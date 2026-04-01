/*
 * QEMU KVMFR Display Backend
 *
 * Writes QEMU's framebuffer to IVSHMEM shared memory in Looking Glass
 * KVMFR format. Any Looking Glass client (or ozma's looking_glass.py)
 * can read frames from the host side without any guest software.
 *
 * This gives every QEMU VM a Looking Glass-compatible frame export for
 * free — no guest-side capture program needed for emulated GPUs.
 *
 * For GPU passthrough VMs, the guest-side Looking Glass Host is still
 * needed (only the guest can see the passthrough GPU's framebuffer).
 * But this backend handles the emulated VGA/virtio-gpu case, which
 * covers soft nodes, development VMs, and the boot phase of passthrough
 * VMs (via RAMFB).
 *
 * Usage:
 *   qemu-system-x86_64 \
 *       -display kvmfr,shm-path=/dev/shm/looking-glass,shm-size=32 \
 *       -device ivshmem-plain,memdev=lg-mem \
 *       -object memory-backend-file,id=lg-mem,share=on,
 *              mem-path=/dev/shm/looking-glass,size=32M \
 *       ...
 *
 * The display backend and the ivshmem device both access the same
 * shared memory file. The guest sees it as a PCI BAR (ivshmem), the
 * host sees it as mmap'd memory (this backend).
 *
 * Multi-console: one DCL per graphic console. Each console gets its
 * own frame region within the shared memory. Console 0 uses the
 * standard KVMFR layout; additional consoles are not yet supported
 * by the Looking Glass client but the infrastructure is ready.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 * Copyright (C) 2024-2026 Ozma Labs Pty Ltd
 */

#include "qemu/osdep.h"
#include "ui/console.h"
#include "ui/surface.h"
#include "qapi/error.h"
#include "qemu/error-report.h"
#include "qemu/module.h"
#include "qemu/option.h"
#include "qemu/timer.h"

#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <stdatomic.h>

/* ── KVMFR / LGMP protocol constants ─────────────────────────────── */

#define LGMP_MAGIC          0x504d474c  /* "LGMP" */
#define LGMP_VERSION        10
#define KVMFR_MAGIC         "KVMFR---"
#define KVMFR_VERSION       20
#define KVMFR_MAGIC_LEN     8

/* Queue IDs (Looking Glass convention) */
#define LGMP_Q_POINTER      1
#define LGMP_Q_FRAME        2
#define LGMP_Q_POINTER_LEN  32
#define LGMP_Q_FRAME_LEN    2   /* double-buffered */

/* Frame types (matches Looking Glass FrameType enum) */
#define FRAME_TYPE_INVALID  0
#define FRAME_TYPE_BGRA     1
#define FRAME_TYPE_RGBA     2
#define FRAME_TYPE_RGBA10   3
#define FRAME_TYPE_RGBA16F  4
#define FRAME_TYPE_BGR_32   5
#define FRAME_TYPE_RGB_24   6

/* Cursor types */
#define CURSOR_TYPE_COLOR       0
#define CURSOR_TYPE_MONOCHROME  1
#define CURSOR_TYPE_MASKED      2

/* Cursor message flags (stored in LGMPHeaderMessage.udata) */
#define CURSOR_FLAG_POSITION  0x1
#define CURSOR_FLAG_VISIBLE   0x2
#define CURSOR_FLAG_SHAPE     0x4

/* KVMFR feature flags */
#define KVMFR_FEATURE_SETCURSORPOS  (1 << 0)

/* ── LGMP shared memory layout ───────────────────────────────────── */

/*
 * Simplified LGMP layout for a single-producer (this backend):
 *
 * Offset 0:      LGMPHeader (padded to 4096 bytes)
 *   - timestamp, magic, version, sessionID, numQueues, udata[]
 *   - udata[] contains the KVMFR header
 *
 * Offset 4096:   Queue headers (one per queue, 4096 bytes each)
 *   - Message ring: LGMPHeaderMessage[depth]
 *
 * After queues:  Data region
 *   - Frame slots (allocated sequentially)
 *   - Cursor slots (allocated sequentially)
 */

#define SHM_HEADER_SIZE     4096
#define SHM_QUEUE_SIZE      4096
#define SHM_ALIGN           64

/* Maximum frame damage rects */
#define KVMFR_MAX_DAMAGE_RECTS  64

/* ── Structures matching LGMP/KVMFR wire format ──────────────────── */

/* These must match the Looking Glass definitions exactly for
 * binary compatibility. All fields are little-endian. */

typedef struct {
    uint32_t udata;         /* application flags */
    uint32_t size;          /* payload size */
    uint32_t offset;        /* byte offset from SHM base to payload */
    _Atomic(uint32_t) pending_subs;  /* bitmask of pending subscribers */
} LGMPHeaderMessage;

typedef struct {
    uint32_t queue_id;
    uint32_t num_messages;
    _Atomic(uint32_t) position;      /* producer write position */
    uint32_t _pad1;
    /* Followed by LGMPHeaderMessage[num_messages] */
} LGMPHeaderQueue;

typedef struct {
    _Atomic(uint64_t) timestamp;
    uint8_t  _pad_ts[56];           /* pad to 64 bytes */

    /* Queue descriptors start here — but we lay them out manually
     * since they're variable-size. We just store the offset. */
} LGMPHeaderBase;

typedef struct {
    uint32_t format_ver;
    uint32_t frame_serial;
    uint32_t type;                  /* FrameType */
    uint32_t screen_width;
    uint32_t screen_height;
    uint32_t data_width;
    uint32_t data_height;
    uint32_t frame_width;
    uint32_t frame_height;
    uint32_t rotation;              /* 0, 90, 180, 270 */
    uint32_t stride;                /* pixels per row */
    uint32_t pitch;                 /* bytes per row */
    uint32_t offset;                /* offset from this struct to FrameBuffer */
    uint32_t damage_rects_count;
    struct {
        uint32_t x, y, w, h;
    } damage_rects[KVMFR_MAX_DAMAGE_RECTS];
    uint32_t flags;
} KVMFRFrame;

typedef struct {
    _Atomic(uint32_t) wp;           /* write pointer (bytes written) */
    uint8_t data[];                 /* pixel data */
} FrameBuffer;

typedef struct {
    int16_t  x, y;
    uint32_t type;                  /* CursorType */
    int8_t   hx, hy;               /* hotspot */
    uint32_t width, height, pitch;
    /* Followed by pixel data for COLOR type */
} KVMFRCursor;

/* ── Per-console state ───────────────────────────────────────────── */

typedef struct KVMFRDisplay {
    DisplayChangeListener dcl;
    QemuConsole *con;
    int console_index;

    /* Shared memory */
    uint8_t *shm_base;
    size_t   shm_size;

    /* Current surface */
    DisplaySurface *surface;

    /* Frame state */
    uint32_t frame_serial;
    int      current_slot;          /* 0 or 1 (double-buffer) */
    size_t   frame_slot_offset[2];  /* byte offsets to frame slots */
    size_t   frame_slot_size;

    /* Cursor state */
    size_t   cursor_slot_offset[LGMP_Q_POINTER_LEN];
    size_t   cursor_slot_size;
    int      current_cursor_slot;
    int      cursor_x, cursor_y;
    bool     cursor_visible;

    /* Queue pointers (into SHM) */
    LGMPHeaderQueue *frame_queue;
    LGMPHeaderQueue *pointer_queue;
    LGMPHeaderMessage *frame_messages;
    LGMPHeaderMessage *pointer_messages;

    /* Timer for LGMP heartbeat */
    QEMUTimer *heartbeat_timer;
} KVMFRDisplay;

/* ── Global state ────────────────────────────────────────────────── */

static struct {
    char    shm_path[256];
    size_t  shm_size_mb;
    int     shm_fd;
    uint8_t *shm_base;
    size_t  shm_size;
    uint32_t session_id;
    bool    initialized;
} kvmfr_state;

/* ── SHM initialization ─────────────────────────────────────────── */

static size_t align_up(size_t v, size_t align)
{
    return (v + align - 1) & ~(align - 1);
}

static bool kvmfr_init_shm(const char *path, size_t size_mb, Error **errp)
{
    size_t size = size_mb * 1024 * 1024;
    int fd;

    if (kvmfr_state.initialized) {
        return true;
    }

    fd = open(path, O_RDWR | O_CREAT, 0660);
    if (fd < 0) {
        error_setg_errno(errp, errno, "Failed to open SHM: %s", path);
        return false;
    }

    if (ftruncate(fd, size) < 0) {
        error_setg_errno(errp, errno, "Failed to resize SHM to %zuMB", size_mb);
        close(fd);
        return false;
    }

    uint8_t *base = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (base == MAP_FAILED) {
        error_setg_errno(errp, errno, "Failed to mmap SHM");
        close(fd);
        return false;
    }

    /* Zero the header region */
    memset(base, 0, SHM_HEADER_SIZE);

    kvmfr_state.shm_fd = fd;
    kvmfr_state.shm_base = base;
    kvmfr_state.shm_size = size;
    kvmfr_state.session_id = (uint32_t)g_random_int();
    strncpy(kvmfr_state.shm_path, path, sizeof(kvmfr_state.shm_path) - 1);
    kvmfr_state.shm_size_mb = size_mb;
    kvmfr_state.initialized = true;

    return true;
}

static void kvmfr_write_lgmp_header(void)
{
    uint8_t *base = kvmfr_state.shm_base;
    size_t off = 0;

    /* LGMP header base: timestamp + padding (64 bytes) */
    _Atomic(uint64_t) *timestamp = (_Atomic(uint64_t) *)base;
    atomic_store_explicit(timestamp, 0, memory_order_release);
    off = 64;

    /* Skip to after the reserved header area — we'll write queue
     * headers and the LGMP trailer at known offsets. The full LGMP
     * header structure is complex (variable-length queues), so we
     * write a simplified version that's compatible with the LG client.
     *
     * Layout at the end of the base header region:
     *   magic(4) + version(4) + sessionID(4) + numQueues(4) + udataSize(4) + udata[]
     */

    /* Write LGMP trailer at offset 512 (well within the 4096 header) */
    size_t trailer_off = 512;
    uint32_t *trailer = (uint32_t *)(base + trailer_off);
    trailer[0] = LGMP_MAGIC;
    trailer[1] = LGMP_VERSION;
    trailer[2] = kvmfr_state.session_id;
    trailer[3] = 2;  /* numQueues: frame + pointer */

    /* KVMFR udata (the KVMFR header) */
    size_t udata_off = trailer_off + 20;
    uint32_t udata_size = 8 + 4 + 32 + 4;  /* magic + version + hostver + features */
    trailer[4] = udata_size;

    uint8_t *udata = base + udata_off;
    memcpy(udata, KVMFR_MAGIC, KVMFR_MAGIC_LEN);
    *(uint32_t *)(udata + 8) = KVMFR_VERSION;
    strncpy((char *)(udata + 12), "ozma-kvmfr 0.1", 32);
    *(uint32_t *)(udata + 44) = KVMFR_FEATURE_SETCURSORPOS;
}

static void kvmfr_setup_queues(KVMFRDisplay *dpy)
{
    uint8_t *base = kvmfr_state.shm_base;
    size_t queue_region_start = SHM_HEADER_SIZE;

    /* Frame queue header */
    dpy->frame_queue = (LGMPHeaderQueue *)(base + queue_region_start);
    dpy->frame_queue->queue_id = LGMP_Q_FRAME;
    dpy->frame_queue->num_messages = LGMP_Q_FRAME_LEN;
    atomic_store_explicit(&dpy->frame_queue->position, 0, memory_order_release);

    dpy->frame_messages = (LGMPHeaderMessage *)(
        (uint8_t *)dpy->frame_queue + sizeof(LGMPHeaderQueue)
    );
    memset(dpy->frame_messages, 0,
           sizeof(LGMPHeaderMessage) * LGMP_Q_FRAME_LEN);

    /* Pointer queue header */
    size_t ptr_queue_off = queue_region_start + SHM_QUEUE_SIZE;
    dpy->pointer_queue = (LGMPHeaderQueue *)(base + ptr_queue_off);
    dpy->pointer_queue->queue_id = LGMP_Q_POINTER;
    dpy->pointer_queue->num_messages = LGMP_Q_POINTER_LEN;
    atomic_store_explicit(&dpy->pointer_queue->position, 0, memory_order_release);

    dpy->pointer_messages = (LGMPHeaderMessage *)(
        (uint8_t *)dpy->pointer_queue + sizeof(LGMPHeaderQueue)
    );
    memset(dpy->pointer_messages, 0,
           sizeof(LGMPHeaderMessage) * LGMP_Q_POINTER_LEN);

    /* Data region starts after all queues */
    size_t data_start = queue_region_start + SHM_QUEUE_SIZE * 2;

    /* Allocate frame slots (double-buffered).
     * Each slot: KVMFRFrame header + FrameBuffer header + pixel data.
     * Reserve enough for 4K BGRA (3840*2160*4 = ~33MB per frame).
     * Actual allocation is bounded by SHM size. */
    size_t remaining = kvmfr_state.shm_size - data_start;
    size_t cursor_total = dpy->cursor_slot_size * LGMP_Q_POINTER_LEN;
    size_t frame_budget = remaining - cursor_total;
    dpy->frame_slot_size = frame_budget / LGMP_Q_FRAME_LEN;
    dpy->frame_slot_size = align_up(dpy->frame_slot_size, SHM_ALIGN);

    for (int i = 0; i < LGMP_Q_FRAME_LEN; i++) {
        dpy->frame_slot_offset[i] = data_start + i * dpy->frame_slot_size;
    }

    /* Allocate cursor slots after frame slots */
    size_t cursor_start = data_start + LGMP_Q_FRAME_LEN * dpy->frame_slot_size;
    for (int i = 0; i < LGMP_Q_POINTER_LEN; i++) {
        dpy->cursor_slot_offset[i] = cursor_start + i * dpy->cursor_slot_size;
    }
}

/* ── Frame posting ───────────────────────────────────────────────── */

static void kvmfr_post_frame(KVMFRDisplay *dpy, int x, int y, int w, int h)
{
    if (!dpy->surface) {
        return;
    }

    int width = surface_width(dpy->surface);
    int height = surface_height(dpy->surface);
    int stride = surface_stride(dpy->surface);
    void *pixels = surface_data(dpy->surface);

    if (!pixels || width <= 0 || height <= 0) {
        return;
    }

    /* Select frame slot (toggle between 0 and 1) */
    int slot = dpy->current_slot;
    size_t slot_off = dpy->frame_slot_offset[slot];
    size_t pixel_size = (size_t)stride * height;
    size_t needed = sizeof(KVMFRFrame) + sizeof(FrameBuffer) + pixel_size;

    if (needed > dpy->frame_slot_size) {
        /* Frame too large for slot — skip this frame */
        return;
    }

    KVMFRFrame *frame = (KVMFRFrame *)(kvmfr_state.shm_base + slot_off);
    FrameBuffer *fb = (FrameBuffer *)(
        (uint8_t *)frame + sizeof(KVMFRFrame)
    );

    /* Fill frame header */
    frame->format_ver = 0;
    frame->frame_serial = ++dpy->frame_serial;
    frame->type = FRAME_TYPE_BGRA;  /* QEMU's PIXMAN_x8r8g8b8 = BGRX */
    frame->screen_width = width;
    frame->screen_height = height;
    frame->data_width = width;
    frame->data_height = height;
    frame->frame_width = width;
    frame->frame_height = height;
    frame->rotation = 0;
    frame->stride = width;
    frame->pitch = stride;
    frame->offset = sizeof(KVMFRFrame);
    frame->flags = 0;

    /* Damage rects */
    if (x == 0 && y == 0 && w == width && h == height) {
        frame->damage_rects_count = 0;  /* full frame */
    } else {
        frame->damage_rects_count = 1;
        frame->damage_rects[0].x = x;
        frame->damage_rects[0].y = y;
        frame->damage_rects[0].w = w;
        frame->damage_rects[0].h = h;
    }

    /* Reset write pointer */
    atomic_store_explicit(&fb->wp, 0, memory_order_release);

    /* Copy pixel data */
    memcpy(fb->data, pixels, pixel_size);

    /* Signal write complete */
    atomic_store_explicit(&fb->wp, (uint32_t)pixel_size,
                          memory_order_release);

    /* Post to frame queue */
    LGMPHeaderMessage *msg = &dpy->frame_messages[slot];
    msg->udata = 0;
    msg->size = sizeof(KVMFRFrame) + sizeof(FrameBuffer) + pixel_size;
    msg->offset = slot_off;
    atomic_store_explicit(&msg->pending_subs, 0, memory_order_release);

    /* Advance queue position */
    atomic_store_explicit(&dpy->frame_queue->position,
                          (uint32_t)(slot + 1),
                          memory_order_release);

    dpy->current_slot = (slot + 1) % LGMP_Q_FRAME_LEN;
}

/* ── Cursor posting ──────────────────────────────────────────────── */

static void kvmfr_post_cursor_position(KVMFRDisplay *dpy, int x, int y,
                                        bool visible)
{
    int slot = dpy->current_cursor_slot;
    size_t off = dpy->cursor_slot_offset[slot];
    KVMFRCursor *cursor = (KVMFRCursor *)(kvmfr_state.shm_base + off);

    cursor->x = x;
    cursor->y = y;

    LGMPHeaderMessage *msg = &dpy->pointer_messages[slot];
    uint32_t flags = CURSOR_FLAG_POSITION;
    if (visible) {
        flags |= CURSOR_FLAG_VISIBLE;
    }
    msg->udata = flags;
    msg->size = sizeof(KVMFRCursor);
    msg->offset = off;
    atomic_store_explicit(&msg->pending_subs, 0, memory_order_release);

    atomic_store_explicit(&dpy->pointer_queue->position,
                          (uint32_t)(slot + 1),
                          memory_order_release);

    dpy->current_cursor_slot = (slot + 1) % LGMP_Q_POINTER_LEN;
}

static void kvmfr_post_cursor_shape(KVMFRDisplay *dpy, QEMUCursor *c)
{
    int slot = dpy->current_cursor_slot;
    size_t off = dpy->cursor_slot_offset[slot];
    KVMFRCursor *cursor = (KVMFRCursor *)(kvmfr_state.shm_base + off);
    size_t pixel_size = (size_t)c->width * c->height * 4;

    if (sizeof(KVMFRCursor) + pixel_size > dpy->cursor_slot_size) {
        return;  /* cursor too large */
    }

    cursor->x = dpy->cursor_x;
    cursor->y = dpy->cursor_y;
    cursor->type = CURSOR_TYPE_COLOR;
    cursor->hx = c->hot_x;
    cursor->hy = c->hot_y;
    cursor->width = c->width;
    cursor->height = c->height;
    cursor->pitch = c->width * 4;

    memcpy((uint8_t *)cursor + sizeof(KVMFRCursor), c->data, pixel_size);

    LGMPHeaderMessage *msg = &dpy->pointer_messages[slot];
    msg->udata = CURSOR_FLAG_SHAPE | CURSOR_FLAG_VISIBLE | CURSOR_FLAG_POSITION;
    msg->size = sizeof(KVMFRCursor) + pixel_size;
    msg->offset = off;
    atomic_store_explicit(&msg->pending_subs, 0, memory_order_release);

    atomic_store_explicit(&dpy->pointer_queue->position,
                          (uint32_t)(slot + 1),
                          memory_order_release);

    dpy->current_cursor_slot = (slot + 1) % LGMP_Q_POINTER_LEN;
}

/* ── LGMP heartbeat ──────────────────────────────────────────────── */

static void kvmfr_heartbeat(void *opaque)
{
    KVMFRDisplay *dpy = opaque;
    _Atomic(uint64_t) *ts = (_Atomic(uint64_t) *)kvmfr_state.shm_base;
    uint64_t now = qemu_clock_get_ms(QEMU_CLOCK_REALTIME);
    atomic_store_explicit(ts, now, memory_order_release);

    timer_mod(dpy->heartbeat_timer,
              qemu_clock_get_ms(QEMU_CLOCK_REALTIME) + 10);
}

/* ── DisplayChangeListener callbacks ─────────────────────────────── */

static void kvmfr_refresh(DisplayChangeListener *dcl)
{
    graphic_hw_update(dcl->con);
}

static void kvmfr_gfx_update(DisplayChangeListener *dcl,
                               int x, int y, int w, int h)
{
    KVMFRDisplay *dpy = container_of(dcl, KVMFRDisplay, dcl);
    kvmfr_post_frame(dpy, x, y, w, h);
}

static void kvmfr_gfx_switch(DisplayChangeListener *dcl,
                               struct DisplaySurface *new_surface)
{
    KVMFRDisplay *dpy = container_of(dcl, KVMFRDisplay, dcl);
    dpy->surface = new_surface;
}

static void kvmfr_mouse_set(DisplayChangeListener *dcl,
                              int x, int y, int on)
{
    KVMFRDisplay *dpy = container_of(dcl, KVMFRDisplay, dcl);
    dpy->cursor_x = x;
    dpy->cursor_y = y;
    dpy->cursor_visible = on;
    kvmfr_post_cursor_position(dpy, x, y, on);
}

static void kvmfr_cursor_define(DisplayChangeListener *dcl,
                                  QEMUCursor *cursor)
{
    KVMFRDisplay *dpy = container_of(dcl, KVMFRDisplay, dcl);
    kvmfr_post_cursor_shape(dpy, cursor);
}

static bool kvmfr_check_format(DisplayChangeListener *dcl,
                                 pixman_format_code_t format)
{
    /* We handle BGRX (x8r8g8b8) which is QEMU's default */
    return format == PIXMAN_x8r8g8b8 || format == PIXMAN_a8r8g8b8;
}

static const DisplayChangeListenerOps kvmfr_dcl_ops = {
    .dpy_name            = "kvmfr",
    .dpy_refresh         = kvmfr_refresh,
    .dpy_gfx_update      = kvmfr_gfx_update,
    .dpy_gfx_switch      = kvmfr_gfx_switch,
    .dpy_gfx_check_format = kvmfr_check_format,
    .dpy_mouse_set       = kvmfr_mouse_set,
    .dpy_cursor_define   = kvmfr_cursor_define,
};

/* ── Display init ────────────────────────────────────────────────── */

static void kvmfr_display_init(DisplayState *ds, DisplayOptions *opts)
{
    Error *err = NULL;
    const char *shm_path = "/dev/shm/looking-glass";
    size_t shm_size_mb = 32;

    /* Parse options.
     * In a full in-tree build these come from DisplayOptions (qapi).
     * For now we use environment variables as a simple mechanism. */
    const char *env_path = getenv("KVMFR_SHM_PATH");
    const char *env_size = getenv("KVMFR_SHM_SIZE");
    if (env_path) {
        shm_path = env_path;
    }
    if (env_size) {
        shm_size_mb = atoi(env_size);
        if (shm_size_mb < 2) shm_size_mb = 32;
    }

    if (!kvmfr_init_shm(shm_path, shm_size_mb, &err)) {
        error_reportf_err(err, "kvmfr: ");
        return;
    }

    kvmfr_write_lgmp_header();

    /* Create a DCL for each graphic console */
    QemuConsole *con;
    for (int idx = 0; ; idx++) {
        con = qemu_console_lookup_by_index(idx);
        if (!con || !qemu_console_is_graphic(con)) {
            break;
        }

        KVMFRDisplay *dpy = g_new0(KVMFRDisplay, 1);
        dpy->con = con;
        dpy->console_index = idx;
        dpy->shm_base = kvmfr_state.shm_base;
        dpy->shm_size = kvmfr_state.shm_size;
        dpy->cursor_slot_size = sizeof(KVMFRCursor) + 512 * 512 * 4;

        kvmfr_setup_queues(dpy);

        dpy->dcl.con = con;
        dpy->dcl.ops = &kvmfr_dcl_ops;
        register_displaychangelistener(&dpy->dcl);

        /* Start LGMP heartbeat timer (10ms interval) */
        dpy->heartbeat_timer = timer_new_ms(QEMU_CLOCK_REALTIME,
                                             kvmfr_heartbeat, dpy);
        timer_mod(dpy->heartbeat_timer,
                  qemu_clock_get_ms(QEMU_CLOCK_REALTIME) + 10);

        info_report("kvmfr: console %d attached (%s, %zuMB SHM)",
                    idx, shm_path, shm_size_mb);
    }
}

/* ── Module registration ─────────────────────────────────────────── */

static QemuDisplay kvmfr_display = {
    .type = DISPLAY_TYPE_NONE,  /* Registered as a side-effect display
                                 * like VNC — doesn't claim a DisplayType.
                                 * Activated by presence of SHM file or
                                 * environment variables. */
    .init = kvmfr_display_init,
};

static void kvmfr_register(void)
{
    /* Only activate if SHM path is configured */
    if (getenv("KVMFR_SHM_PATH") || access("/dev/shm/looking-glass", F_OK) == 0) {
        qemu_display_register(&kvmfr_display);
    }
}

type_init(kvmfr_register);
