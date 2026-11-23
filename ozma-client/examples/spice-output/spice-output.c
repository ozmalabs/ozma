/* SPDX-License-Identifier: AGPL-3.0-only */
/*
 * Outbound SPICE display bridge for Ozma scene output.
 *
 * The bridge uses the installed libspice-server QXL interface rather than
 * reimplementing SPICE framing.  It accepts two stdin stream formats:
 *
 *   OZFR  repeated composed RGBA frames.  The bridge diffs successive frames
 *         and sends only changed rectangles, with a full-frame fallback when
 *         the damage is large or too fragmented.
 *
 *   OZSC  a structured scene stream (defined below).  Image resources become
 *         QXL secondary surfaces and placements become draw-copy commands
 *         whose source is SPICE_IMAGE_TYPE_SURFACE.  Rectangular damage and
 *         full-frame records remain draw-copy bitmap updates, so video and
 *         effects that cannot be represented as an image surface still work.
 *
 * The server requires TLS and ticket authentication, binds loopback by
 * default, and exposes a view-only display.  Audio playback is a raw S16
 * stereo PCM channel; the server may encode it to Opus/CELT for capable
 * clients, but this bridge does not accept an Opus stream.
 *
 * See README.md in this directory for the wire format, build/run commands,
 * and the tested/untested matrix.
 */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <glib-unix.h>
#include <glib.h>
#include <signal.h>
#include <spice.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define MAX_SURFACES 64
#define MAX_RESOURCES 63
#define MAX_QUEUE_COMMANDS 256
#define MAX_DIM 4096
#define MAX_FRAME_BYTES (64ULL * 1024 * 1024)
#define MAX_AUDIO_BYTES (2U * 1024 * 1024)
#define MAX_OZSC_RECORD (64U * 1024 * 1024)
#define MAX_INPUT_BUFFER (MAX_FRAME_BYTES + 65536ULL)
#define MAX_RESOURCE_BYTES (256ULL * 1024 * 1024)
#define MAX_QUEUED_BYTES (256ULL * 1024 * 1024)

/* OZSC record types.  Payloads are big-endian; coordinates are signed. */
enum {
    OZSC_SETUP = 1,      /* u32 width, u32 height, u32 bg RGBA */
    OZSC_FRAME_BEGIN = 2,/* u32 frame_id */
    OZSC_IMAGE_DEF = 3,  /* u32 id, u32 w, u32 h, u32 stride, RGBA pixels */
    OZSC_IMAGE_FREE = 4, /* u32 id */
    OZSC_PLACE = 5,      /* u32 id, i32 x, i32 y */
    OZSC_CLEAR = 6,      /* i32 x, i32 y, u32 w, u32 h, u32 bg RGBA */
    OZSC_DAMAGE = 7,     /* i32 x, i32 y, u32 w, u32 h, u32 stride, pixels */
    OZSC_FULL_FRAME = 8, /* u32 w, u32 h, u32 stride, RGBA pixels */
    OZSC_FRAME_END = 9,  /* u32 frame_id */
    OZSC_AUDIO = 10,     /* u32 rate, u16 channels, u16 fmt, u32 frames, PCM */
};

typedef struct {
    int fd;
    SpiceWatchFunc callback;
    void *opaque;
    guint source;
} Watch;

typedef struct {
    SpiceTimerFunc callback;
    void *opaque;
    guint source;
} Timer;

typedef struct {
    int refs;
    uint8_t *data;
} SharedBuf;

typedef enum {
    CMD_SURFACE_CREATE,
    CMD_SURFACE_DESTROY,
    CMD_DRAW,
} CmdKind;

typedef struct PendingCmd {
    struct PendingCmd *next;
    CmdKind kind;
    int counts_as_draw;
    size_t bytes;        /* in-flight bitmap bytes, for backpressure */
    uint32_t surface_id; /* for create release accounting */
    SharedBuf *buf;
    QXLSurfaceCmd surface;
    QXLDrawable drawable;
    QXLImage image;
} PendingCmd;

typedef struct {
    uint32_t id;
    int used;
    uint32_t surface_id;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    SharedBuf *buf;
} Resource;

typedef struct {
    QXLInstance instance;
    QXLInterface qxl;
    SpicePlaybackInstance playback;
    SpicePlaybackInterface playback_iface;
    SpiceServer *server;
    GMainLoop *loop;

    uint8_t *primary;   /* primary canvas memory, owned by the server */
    uint8_t *shadow;    /* latest composed OZFR frame, for damage diffing */
    uint32_t width, height, stride;
    uint32_t background; /* 0xRRGGBBAA */
    int structured;      /* OZSC input */

    GMutex lock;
    PendingCmd *cmd_head;
    PendingCmd *cmd_tail;
    unsigned queue_len;
    unsigned pending_draws;
    size_t queued_bytes;
    size_t resource_bytes;
    uint64_t image_serial;

    int surface_in_use[MAX_SURFACES];
    int surface_releasing[MAX_SURFACES];
    Resource resources[MAX_RESOURCES];

    uint8_t *audio;
    size_t audio_len;
    size_t audio_cap;
    uint32_t audio_rate;
    int audio_started;
    uint64_t audio_records;
    uint64_t audio_bytes;
    uint64_t audio_frames_sent;

    uint64_t stat_surface_create;
    uint64_t stat_surface_destroy;
    uint64_t stat_draw_surface;
    uint64_t stat_draw_bitmap;
    uint64_t stat_draw_fill;
    uint64_t stat_full_frames;
    uint64_t stat_damage_rects;

    int started;
    int qxl_attached;
    int failed;
    int shutting_down;
} Bridge;

typedef struct {
    uint8_t *data;
    size_t len;
    size_t cap;
} InputBuf;

static InputBuf *g_input;
static Bridge *g_bridge;
static void bridge_pump_audio(Bridge *b);
static gboolean pump_audio_idle(gpointer data);

static uint32_t be32(const uint8_t *p) {
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | p[3];
}

static int32_t bes32(const uint8_t *p) {
    return (int32_t)be32(p);
}

static uint16_t be16(const uint8_t *p) {
    return (uint16_t)(((uint16_t)p[0] << 8) | p[1]);
}

static gboolean read_full_fd(int fd, void *ptr, size_t size) {
    uint8_t *p = ptr;
    while (size) {
        ssize_t n = read(fd, p, size);
        if (n < 0 && errno == EINTR)
            continue;
        if (n <= 0)
            return FALSE;
        p += n;
        size -= (size_t)n;
    }
    return TRUE;
}

static SharedBuf *shared_new(size_t size) {
    SharedBuf *buf = calloc(1, sizeof(*buf));
    if (!buf)
        return NULL;
    buf->data = malloc(size ? size : 1);
    if (!buf->data) {
        free(buf);
        return NULL;
    }
    buf->refs = 1;
    return buf;
}

static SharedBuf *shared_ref(SharedBuf *buf) {
    if (buf)
        buf->refs++;
    return buf;
}

static void shared_unref(SharedBuf *buf) {
    if (!buf)
        return;
    if (--buf->refs == 0) {
        free(buf->data);
        free(buf);
    }
}

static uint32_t rgba_to_xrgb(uint32_t rgba) {
    uint8_t r = (uint8_t)(rgba >> 24);
    uint8_t g = (uint8_t)(rgba >> 16);
    uint8_t b = (uint8_t)(rgba >> 8);
    return ((uint32_t)r << 16) | ((uint32_t)g << 8) | b;
}

/* Convert in-place from RGBA8 to the little-endian BGRX layout used by
 * SPICE_SURFACE_FMT_32_xRGB. */
static void rgba_to_bgrx(uint8_t *pixels, size_t bytes) {
    for (size_t i = 0; i + 3 < bytes; i += 4) {
        uint8_t r = pixels[i];
        pixels[i] = pixels[i + 2];
        pixels[i + 2] = r;
        pixels[i + 3] = 0xff;
    }
}

static void bridge_wakeup(Bridge *b) {
    if (b->started)
        spice_qxl_wakeup(&b->instance);
}

static void queue_cmd(Bridge *b, PendingCmd *cmd) {
    cmd->next = NULL;
    g_mutex_lock(&b->lock);
    if (b->cmd_tail)
        b->cmd_tail->next = cmd;
    else
        b->cmd_head = cmd;
    b->cmd_tail = cmd;
    b->queue_len++;
    if (cmd->counts_as_draw)
        b->pending_draws++;
    b->queued_bytes += cmd->bytes;
    g_mutex_unlock(&b->lock);
}

static unsigned queue_length(Bridge *b) {
    g_mutex_lock(&b->lock);
    unsigned n = b->queue_len;
    g_mutex_unlock(&b->lock);
    return n;
}

static int backpressured(Bridge *b) {
    g_mutex_lock(&b->lock);
    int full = b->queue_len >= MAX_QUEUE_COMMANDS ||
               b->pending_draws >= MAX_QUEUE_COMMANDS ||
               (uint64_t)b->queued_bytes >= MAX_QUEUED_BYTES;
    g_mutex_unlock(&b->lock);
    return full;
}

static uint32_t alloc_surface(Bridge *b) {
    uint32_t id = 0;
    g_mutex_lock(&b->lock);
    for (uint32_t i = 1; i < MAX_SURFACES; i++) {
        if (!b->surface_in_use[i] && !b->surface_releasing[i]) {
            b->surface_in_use[i] = 1;
            id = i;
            break;
        }
    }
    g_mutex_unlock(&b->lock);
    return id;
}

static PendingCmd *new_bitmap_draw(Bridge *b, uint32_t dest_surface, int32_t x, int32_t y,
                                   uint32_t w, uint32_t h, uint32_t stride, SharedBuf *buf) {
    PendingCmd *cmd = calloc(1, sizeof(*cmd));
    if (!cmd)
        return NULL;
    cmd->kind = CMD_DRAW;
    cmd->counts_as_draw = 1;
    cmd->bytes = (size_t)stride * h;
    cmd->buf = shared_ref(buf);

    QXLImage *image = &cmd->image;
    memset(image, 0, sizeof(*image));
    QXL_SET_IMAGE_ID(image, QXL_IMAGE_GROUP_DRIVER_DONT_CACHE, ++b->image_serial);
    image->descriptor.type = SPICE_IMAGE_TYPE_BITMAP;
    image->descriptor.width = w;
    image->descriptor.height = h;
    image->bitmap.format = SPICE_BITMAP_FMT_32BIT;
    image->bitmap.flags = QXL_BITMAP_DIRECT | QXL_BITMAP_TOP_DOWN;
    image->bitmap.x = w;
    image->bitmap.y = h;
    image->bitmap.stride = stride;
    image->bitmap.data = (QXLPHYSICAL)(uintptr_t)buf->data;

    QXLDrawable *d = &cmd->drawable;
    d->release_info.id = (uintptr_t)cmd;
    d->surface_id = dest_surface;
    d->type = QXL_DRAW_COPY;
    d->effect = QXL_EFFECT_OPAQUE;
    d->surfaces_dest[0] = d->surfaces_dest[1] = d->surfaces_dest[2] = -1;
    d->mm_time = (uint32_t)(g_get_monotonic_time() / 1000);
    d->bbox = (QXLRect){.top = y, .left = x, .bottom = y + (int32_t)h, .right = x + (int32_t)w};
    d->u.copy.src_bitmap = (QXLPHYSICAL)(uintptr_t)image;
    d->u.copy.src_area = (QXLRect){.top = 0, .left = 0, .bottom = (int32_t)h,
                                   .right = (int32_t)w};
    d->u.copy.rop_descriptor = SPICE_ROPD_OP_PUT;
    b->stat_draw_bitmap++;
    if (dest_surface == 0) {
        if (x == 0 && y == 0 && w == b->width && h == b->height)
            b->stat_full_frames++;
        else
            b->stat_damage_rects++;
    }
    return cmd;
}

static PendingCmd *new_surface_draw(Bridge *b, const Resource *res, int32_t x, int32_t y) {
    PendingCmd *cmd = calloc(1, sizeof(*cmd));
    if (!cmd)
        return NULL;
    cmd->kind = CMD_DRAW;
    cmd->counts_as_draw = 1;

    QXLImage *image = &cmd->image;
    memset(image, 0, sizeof(*image));
    QXL_SET_IMAGE_ID(image, QXL_IMAGE_GROUP_DRIVER_DONT_CACHE, ++b->image_serial);
    image->descriptor.type = SPICE_IMAGE_TYPE_SURFACE;
    image->descriptor.width = res->width;
    image->descriptor.height = res->height;
    image->surface_image.surface_id = res->surface_id;

    QXLDrawable *d = &cmd->drawable;
    d->release_info.id = (uintptr_t)cmd;
    d->surface_id = 0;
    d->type = QXL_DRAW_COPY;
    d->effect = QXL_EFFECT_OPAQUE;
    d->mm_time = (uint32_t)(g_get_monotonic_time() / 1000);
    d->bbox = (QXLRect){.top = y, .left = x, .bottom = y + (int32_t)res->height,
                        .right = x + (int32_t)res->width};
    d->surfaces_dest[0] = (int32_t)res->surface_id;
    d->surfaces_dest[1] = d->surfaces_dest[2] = -1;
    d->surfaces_rects[0] = (QXLRect){.top = 0, .left = 0, .bottom = (int32_t)res->height,
                                     .right = (int32_t)res->width};
    d->u.copy.src_bitmap = (QXLPHYSICAL)(uintptr_t)image;
    d->u.copy.src_area = d->surfaces_rects[0];
    d->u.copy.rop_descriptor = SPICE_ROPD_OP_PUT;
    b->stat_draw_surface++;
    return cmd;
}

static PendingCmd *new_surface_create(Bridge *b, uint32_t surface_id, uint32_t w, uint32_t h,
                                      uint32_t stride, SharedBuf *buf) {
    PendingCmd *cmd = calloc(1, sizeof(*cmd));
    if (!cmd)
        return NULL;
    cmd->kind = CMD_SURFACE_CREATE;
    cmd->surface_id = surface_id;
    cmd->buf = shared_ref(buf);

    QXLSurfaceCmd *s = &cmd->surface;
    s->release_info.id = (uintptr_t)cmd;
    s->surface_id = surface_id;
    s->type = QXL_SURFACE_CMD_CREATE;
    /* Keep the supplied pixels.  Without this flag libspice-server zeroes the
     * host buffer while creating the surface, which would also corrupt the
     * resource cache and later placements.  The client receives the surface
     * on demand when a draw references it. */
    s->flags = QXL_SURF_FLAG_KEEP_DATA;
    s->u.surface_create.format = SPICE_SURFACE_FMT_32_xRGB;
    s->u.surface_create.width = w;
    s->u.surface_create.height = h;
    s->u.surface_create.stride = (int32_t)stride;
    s->u.surface_create.data = (QXLPHYSICAL)(uintptr_t)buf->data;
    b->stat_surface_create++;
    return cmd;
}

static PendingCmd *new_surface_destroy(Bridge *b, uint32_t surface_id) {
    PendingCmd *cmd = calloc(1, sizeof(*cmd));
    if (!cmd)
        return NULL;
    cmd->kind = CMD_SURFACE_DESTROY;
    cmd->surface_id = surface_id;
    QXLSurfaceCmd *s = &cmd->surface;
    s->release_info.id = (uintptr_t)cmd;
    s->surface_id = surface_id;
    s->type = QXL_SURFACE_CMD_DESTROY;
    b->stat_surface_destroy++;
    return cmd;
}

static PendingCmd *new_fill(Bridge *b, int32_t x, int32_t y, uint32_t w, uint32_t h,
                            uint32_t xrgb) {
    PendingCmd *cmd = calloc(1, sizeof(*cmd));
    if (!cmd)
        return NULL;
    cmd->kind = CMD_DRAW;
    cmd->counts_as_draw = 1;
    QXLDrawable *d = &cmd->drawable;
    d->release_info.id = (uintptr_t)cmd;
    d->surface_id = 0;
    d->type = QXL_DRAW_FILL;
    d->effect = QXL_EFFECT_OPAQUE;
    d->surfaces_dest[0] = d->surfaces_dest[1] = d->surfaces_dest[2] = -1;
    d->mm_time = (uint32_t)(g_get_monotonic_time() / 1000);
    d->bbox = (QXLRect){.top = y, .left = x, .bottom = y + (int32_t)h, .right = x + (int32_t)w};
    d->u.fill.brush.type = SPICE_BRUSH_TYPE_SOLID;
    d->u.fill.brush.u.color = xrgb;
    d->u.fill.rop_descriptor = SPICE_ROPD_OP_PUT;
    d->u.fill.mask.flags = 0;
    d->u.fill.mask.bitmap = 0;
    b->stat_draw_fill++;
    return cmd;
}

/* Queue a full-frame bitmap of the latest composed state.  This is used for
 * the first frame and whenever a new display client is initialized, because
 * libspice-server may attach the QXL worker after the client's initial
 * surface push. */
static gboolean resync_idle(gpointer data) {
    Bridge *b = data;
    if (!b->shadow || !b->width || !b->height)
        return G_SOURCE_REMOVE;
    size_t bytes = (size_t)b->stride * b->height;
    SharedBuf *buf = shared_new(bytes);
    if (!buf)
        return G_SOURCE_REMOVE;
    memcpy(buf->data, b->shadow, bytes);
    PendingCmd *cmd = new_bitmap_draw(b, 0, 0, 0, b->width, b->height, b->stride, buf);
    shared_unref(buf);
    if (cmd) {
        queue_cmd(b, cmd);
        bridge_wakeup(b);
    }
    return G_SOURCE_REMOVE;
}

/* ------------------------------------------------------------------ */
/* QXL callbacks                                                       */

static void release_resource(QXLInstance *qin, struct QXLReleaseInfoExt release) {
    Bridge *b = (Bridge *)qin;
    PendingCmd *cmd = (PendingCmd *)(uintptr_t)(release.info ? release.info->id : 0);
    if (!cmd)
        return;
    if (cmd->kind == CMD_SURFACE_CREATE && cmd->surface_id < MAX_SURFACES) {
        g_mutex_lock(&b->lock);
        b->surface_in_use[cmd->surface_id] = 0;
        b->surface_releasing[cmd->surface_id] = 0;
        g_mutex_unlock(&b->lock);
    }
    g_mutex_lock(&b->lock);
    if (cmd->counts_as_draw && b->pending_draws)
        b->pending_draws--;
    if (cmd->bytes <= b->queued_bytes)
        b->queued_bytes -= cmd->bytes;
    else
        b->queued_bytes = 0;
    g_mutex_unlock(&b->lock);
    shared_unref(cmd->buf);
    free(cmd);
}

static int get_command(QXLInstance *qin, struct QXLCommandExt *out) {
    Bridge *b = (Bridge *)qin;
    if (!g_atomic_int_get(&b->qxl_attached))
        return 0;
    g_mutex_lock(&b->lock);
    PendingCmd *cmd = b->cmd_head;
    if (!cmd) {
        g_mutex_unlock(&b->lock);
        return 0;
    }
    b->cmd_head = cmd->next;
    if (!b->cmd_head)
        b->cmd_tail = NULL;
    if (b->queue_len)
        b->queue_len--;
    g_mutex_unlock(&b->lock);

    memset(out, 0, sizeof(*out));
    out->group_id = 0;
    out->flags = 0;
    if (cmd->kind == CMD_DRAW) {
        out->cmd.data = (QXLPHYSICAL)(uintptr_t)&cmd->drawable;
        out->cmd.type = QXL_CMD_DRAW;
    } else {
        out->cmd.data = (QXLPHYSICAL)(uintptr_t)&cmd->surface;
        out->cmd.type = QXL_CMD_SURFACE;
    }
    return 1;
}

static int req_cmd_notification(QXLInstance *qin) {
    Bridge *b = (Bridge *)qin;
    if (!g_atomic_int_get(&b->qxl_attached))
        return 1;
    return queue_length(b) ? 0 : 1;
}

static void attached_worker(QXLInstance *qin) {
    Bridge *b = (Bridge *)qin;
    QXLDevMemSlot slot = {0};
    slot.slot_group_id = 0;
    slot.slot_id = 0;
    slot.generation = 0;
    slot.virt_start = 0;
    slot.virt_end = UINTPTR_MAX;
    slot.addr_delta = 0;
    spice_qxl_add_memslot(qin, &slot);

    QXLDevSurfaceCreate surface = {0};
    surface.width = b->width;
    surface.height = b->height;
    surface.stride = (int32_t)b->stride;
    surface.format = SPICE_SURFACE_FMT_32_xRGB;
    surface.type = QXL_SURF_TYPE_PRIMARY;
    surface.mem = (QXLPHYSICAL)(uintptr_t)b->primary;
    spice_qxl_create_primary_surface(qin, 0, &surface);

    /* Commands may only be handed to the server once the worker has created
     * the primary surface.  Anything queued before this point stays in the
     * bridge queue. */
    g_atomic_int_set(&b->qxl_attached, 1);
    spice_qxl_wakeup(qin);
    g_idle_add(resync_idle, b);
}

static void get_init_info(QXLInstance *qin, QXLDevInitInfo *info) {
    (void)qin;
    memset(info, 0, sizeof(*info));
    info->num_memslots_groups = 1;
    info->num_memslots = 1;
    info->memslot_gen_bits = 8;
    info->memslot_id_bits = 8;
    info->qxl_ram_size = 16 * 1024 * 1024;
    info->n_surfaces = MAX_SURFACES;
}

static int get_cursor_command(QXLInstance *qin, struct QXLCommandExt *cmd) {
    (void)qin;
    (void)cmd;
    return 0;
}
static int req_cursor_notification(QXLInstance *qin) {
    (void)qin;
    return 1;
}
static void set_compression_level(QXLInstance *qin, int level) {
    (void)qin;
    (void)level;
}
static void notify_update(QXLInstance *qin, uint32_t update) {
    (void)qin;
    (void)update;
}
static int flush_resources(QXLInstance *qin) {
    (void)qin;
    return 0;
}
static void async_complete(QXLInstance *qin, uint64_t cookie) {
    (void)qin;
    (void)cookie;
}
static void update_area_complete(QXLInstance *qin, uint32_t sid, QXLRect *r, uint32_t n) {
    (void)qin;
    (void)sid;
    (void)r;
    (void)n;
}
static void set_client_capabilities(QXLInstance *qin, uint8_t client_present,
                                    uint8_t caps[SPICE_CAPABILITIES_SIZE]) {
    (void)qin;
    (void)client_present;
    (void)caps;
}
static int client_monitors_config(QXLInstance *qin, VDAgentMonitorsConfig *config) {
    (void)qin;
    (void)config;
    return 0;
}

static gboolean timer_dispatch(gpointer data) {
    Timer *t = data;
    t->source = 0;
    t->callback(t->opaque);
    return G_SOURCE_REMOVE;
}
static SpiceTimer *timer_add(SpiceTimerFunc f, void *o) {
    Timer *t = calloc(1, sizeof(*t));
    if (t) {
        t->callback = f;
        t->opaque = o;
    }
    return (SpiceTimer *)t;
}
static void timer_start(SpiceTimer *timer, uint32_t ms) {
    Timer *t = (Timer *)timer;
    if (t->source)
        g_source_remove(t->source);
    t->source = g_timeout_add(ms, timer_dispatch, t);
}
static void timer_cancel(SpiceTimer *timer) {
    Timer *t = (Timer *)timer;
    if (t->source) {
        g_source_remove(t->source);
        t->source = 0;
    }
}
static void timer_remove(SpiceTimer *timer) {
    timer_cancel(timer);
    free(timer);
}

static gboolean watch_dispatch(gint fd, GIOCondition condition, gpointer data) {
    Watch *w = data;
    int events = 0;
    if (condition & (G_IO_IN | G_IO_PRI | G_IO_HUP | G_IO_ERR | G_IO_NVAL))
        events |= SPICE_WATCH_EVENT_READ;
    if (condition & G_IO_OUT)
        events |= SPICE_WATCH_EVENT_WRITE;
    w->callback(fd, events, w->opaque);
    return G_SOURCE_CONTINUE;
}
static SpiceWatch *watch_add(int fd, int mask, SpiceWatchFunc f, void *o) {
    Watch *w = calloc(1, sizeof(*w));
    if (!w)
        return NULL;
    w->fd = fd;
    w->callback = f;
    w->opaque = o;
    GIOCondition condition = (mask & SPICE_WATCH_EVENT_READ ? G_IO_IN : 0) |
                             (mask & SPICE_WATCH_EVENT_WRITE ? G_IO_OUT : 0);
    w->source = g_unix_fd_add(fd, condition | G_IO_HUP | G_IO_ERR, watch_dispatch, w);
    return (SpiceWatch *)w;
}
static void watch_update(SpiceWatch *watch, int mask) {
    Watch *w = (Watch *)watch;
    if (w->source)
        g_source_remove(w->source);
    GIOCondition condition = (mask & SPICE_WATCH_EVENT_READ ? G_IO_IN : 0) |
                             (mask & SPICE_WATCH_EVENT_WRITE ? G_IO_OUT : 0);
    w->source = g_unix_fd_add(w->fd, condition | G_IO_HUP | G_IO_ERR, watch_dispatch, w);
}
static void watch_remove(SpiceWatch *watch) {
    Watch *w = (Watch *)watch;
    if (w->source)
        g_source_remove(w->source);
    free(w);
}

static const char *channel_event_name(int event) {
    switch (event) {
    case SPICE_CHANNEL_EVENT_CONNECTED:
        return "connected";
    case SPICE_CHANNEL_EVENT_INITIALIZED:
        return "initialized";
    case SPICE_CHANNEL_EVENT_DISCONNECTED:
        return "disconnected";
    default:
        return "event";
    }
}

static void channel_event(int event, SpiceChannelEventInfo *info) {
    if (!info)
        return;
    fprintf(stderr,
            "[spice-output] channel %s type=%d id=%d connection=%d tls=%d\n",
            channel_event_name(event), info->type, info->id, info->connection_id,
            (info->flags & SPICE_CHANNEL_EVENT_FLAG_TLS) ? 1 : 0);
    if (g_bridge && event == SPICE_CHANNEL_EVENT_CONNECTED &&
        info->type == SPICE_CHANNEL_MAIN) {
        fprintf(stderr, "[spice-output] viewer connected (one authenticated viewer is "
                        "enforced by libspice-server)\n");
    }
    /* The display channel can finish initializing before the QXL worker has
     * created its primary surface, so explicitly give every new viewer the
     * latest composed frame from the main loop. */
    if (g_bridge && event == SPICE_CHANNEL_EVENT_INITIALIZED &&
        info->type == SPICE_CHANNEL_DISPLAY) {
        g_idle_add(resync_idle, g_bridge);
    }
    if (g_bridge && event == SPICE_CHANNEL_EVENT_INITIALIZED &&
        info->type == SPICE_CHANNEL_PLAYBACK) {
        g_idle_add(pump_audio_idle, g_bridge);
    }
}

/* ------------------------------------------------------------------ */
/* Audio                                                               */

static void bridge_pump_audio(Bridge *b) {
    if (!b->started || !b->audio_started || !b->audio_len)
        return;
    for (;;) {
        uint32_t *frame = NULL;
        uint32_t num_samples = 0;
        spice_server_playback_get_buffer(&b->playback, &frame, &num_samples);
        if (!frame || num_samples == 0)
            break;
        size_t need = (size_t)num_samples * 4;
        memset(frame, 0, need);
        size_t take = b->audio_len < need ? b->audio_len : need;
        memcpy(frame, b->audio, take);
        if (take) {
            memmove(b->audio, b->audio + take, b->audio_len - take);
            b->audio_len -= take;
        }
        spice_server_playback_put_samples(&b->playback, frame);
        if (!b->audio_frames_sent)
            fprintf(stderr, "[spice-output] audio: first PCM frame sent to viewer\n");
        b->audio_frames_sent++;
        if (take < need)
            break; /* do not spin with silence when the producer is behind */
    }
}

static gboolean pump_audio_idle(gpointer data) {
    bridge_pump_audio(data);
    return G_SOURCE_REMOVE;
}

static void audio_append(Bridge *b, const uint8_t *pcm, size_t bytes) {
    if (!bytes)
        return;
    if (b->audio_len + bytes > MAX_AUDIO_BYTES) {
        /* Drop the oldest half rather than growing without bound. */
        size_t drop = b->audio_len / 2;
        memmove(b->audio, b->audio + drop, b->audio_len - drop);
        b->audio_len -= drop;
        if (b->audio_len + bytes > MAX_AUDIO_BYTES) {
            fprintf(stderr, "[spice-output] dropping audio: playback consumer is behind\n");
            return;
        }
    }
    if (b->audio_len + bytes > b->audio_cap) {
        size_t cap = b->audio_cap ? b->audio_cap : 65536;
        while (cap < b->audio_len + bytes)
            cap *= 2;
        uint8_t *p = realloc(b->audio, cap);
        if (!p)
            return;
        b->audio = p;
        b->audio_cap = cap;
    }
    memcpy(b->audio + b->audio_len, pcm, bytes);
    b->audio_len += bytes;
    bridge_pump_audio(b);
}

/* ------------------------------------------------------------------ */
/* Structured scene records                                            */

static void shadow_fill(Bridge *b, int32_t x, int32_t y, uint32_t w, uint32_t h,
                        uint32_t xrgb) {
    if (!b->shadow)
        return;
    for (uint32_t row = 0; row < h; row++) {
        uint8_t *d = b->shadow + (size_t)(y + (int32_t)row) * b->stride + (size_t)x * 4;
        for (uint32_t col = 0; col < w; col++) {
            d[col * 4] = (uint8_t)(xrgb & 0xff);
            d[col * 4 + 1] = (uint8_t)((xrgb >> 8) & 0xff);
            d[col * 4 + 2] = (uint8_t)((xrgb >> 16) & 0xff);
            d[col * 4 + 3] = 0xff;
        }
    }
}

static void shadow_blit(Bridge *b, int32_t x, int32_t y, uint32_t w, uint32_t h,
                        const uint8_t *src, uint32_t src_stride) {
    if (!b->shadow)
        return;
    for (uint32_t row = 0; row < h; row++)
        memcpy(b->shadow + (size_t)(y + (int32_t)row) * b->stride + (size_t)x * 4,
               src + (size_t)row * src_stride, (size_t)w * 4);
}

static Resource *find_resource(Bridge *b, uint32_t id) {
    for (int i = 0; i < MAX_RESOURCES; i++) {
        if (b->resources[i].used && b->resources[i].id == id)
            return &b->resources[i];
    }
    return NULL;
}

static Resource *alloc_resource(Bridge *b, uint32_t id) {
    for (int i = 0; i < MAX_RESOURCES; i++) {
        if (!b->resources[i].used) {
            memset(&b->resources[i], 0, sizeof(b->resources[i]));
            b->resources[i].used = 1;
            b->resources[i].id = id;
            return &b->resources[i];
        }
    }
    return NULL;
}

static void resource_free(Bridge *b, Resource *res) {
    if (!res || !res->used)
        return;
    if (res->surface_id) {
        PendingCmd *destroy = new_surface_destroy(b, res->surface_id);
        if (destroy) {
            g_mutex_lock(&b->lock);
            if (res->surface_id < MAX_SURFACES)
                b->surface_releasing[res->surface_id] = 1;
            g_mutex_unlock(&b->lock);
            queue_cmd(b, destroy);
        }
    }
    size_t bytes = (size_t)res->stride * res->height;
    if (bytes <= b->resource_bytes)
        b->resource_bytes -= bytes;
    else
        b->resource_bytes = 0;
    shared_unref(res->buf);
    memset(res, 0, sizeof(*res));
}

static int handle_image_def(Bridge *b, const uint8_t *p, size_t len) {
    if (len < 16)
        return -1;
    uint32_t id = be32(p);
    uint32_t w = be32(p + 4);
    uint32_t h = be32(p + 8);
    uint32_t stride = be32(p + 12);
    if (!id || !w || !h || w > MAX_DIM || h > MAX_DIM || stride != w * 4)
        return -1;
    uint64_t bytes = (uint64_t)stride * h;
    if (bytes > MAX_FRAME_BYTES || len != 16 + bytes)
        return -1;
    Resource *old = find_resource(b, id);
    size_t old_bytes = old ? (size_t)old->stride * old->height : 0;
    if (b->resource_bytes - old_bytes + bytes > MAX_RESOURCE_BYTES) {
        fprintf(stderr, "[spice-output] image resource memory limit reached\n");
        return -1;
    }
    uint32_t surface_id = alloc_surface(b);
    if (!surface_id) {
        fprintf(stderr, "[spice-output] no free QXL surface for image %u\n", id);
        return -1;
    }
    SharedBuf *buf = shared_new((size_t)bytes);
    if (!buf) {
        g_mutex_lock(&b->lock);
        b->surface_in_use[surface_id] = 0;
        g_mutex_unlock(&b->lock);
        return -1;
    }
    memcpy(buf->data, p + 16, (size_t)bytes);
    rgba_to_bgrx(buf->data, (size_t)bytes);

    if (old)
        resource_free(b, old);
    Resource *res = alloc_resource(b, id);
    if (!res) {
        shared_unref(buf);
        g_mutex_lock(&b->lock);
        b->surface_in_use[surface_id] = 0;
        g_mutex_unlock(&b->lock);
        return -1;
    }
    res->surface_id = surface_id;
    res->width = w;
    res->height = h;
    res->stride = stride;
    res->buf = shared_ref(buf);
    b->resource_bytes += (size_t)bytes;

    PendingCmd *create = new_surface_create(b, surface_id, w, h, stride, buf);
    /* Populate the client's copy of the surface once.  Later placements only
     * reference the surface id, so moving an image does not resend pixels. */
    PendingCmd *populate = new_bitmap_draw(b, surface_id, 0, 0, w, h, stride, buf);
    if (!create || !populate) {
        if (create) {
            shared_unref(create->buf);
            free(create);
        }
        if (populate) {
            shared_unref(populate->buf);
            free(populate);
        }
        shared_unref(buf);
        shared_unref(res->buf);
        memset(res, 0, sizeof(*res));
        g_mutex_lock(&b->lock);
        b->surface_in_use[surface_id] = 0;
        g_mutex_unlock(&b->lock);
        return -1;
    }
    queue_cmd(b, create);
    queue_cmd(b, populate);
    shared_unref(buf);
    return 0;
}

static int handle_image_free(Bridge *b, const uint8_t *p, size_t len) {
    if (len != 4)
        return -1;
    Resource *res = find_resource(b, be32(p));
    if (!res) {
        fprintf(stderr, "[spice-output] ignoring free of unknown image %u\n", be32(p));
        return 0;
    }
    resource_free(b, res);
    return 0;
}

static int handle_place(Bridge *b, const uint8_t *p, size_t len) {
    if (len != 12)
        return -1;
    uint32_t id = be32(p);
    int32_t x = bes32(p + 4);
    int32_t y = bes32(p + 8);
    Resource *res = find_resource(b, id);
    if (!res) {
        fprintf(stderr, "[spice-output] ignoring placement of unknown image %u\n", id);
        return 0;
    }
    if (x < 0 || y < 0 || x + (int32_t)res->width > (int32_t)b->width ||
        y + (int32_t)res->height > (int32_t)b->height) {
        fprintf(stderr, "[spice-output] placement of image %u outside display\n", id);
        return -1;
    }
    PendingCmd *cmd = new_surface_draw(b, res, x, y);
    if (!cmd)
        return -1;
    queue_cmd(b, cmd);
    shadow_blit(b, x, y, res->width, res->height, res->buf->data, res->stride);
    return 0;
}

static int handle_clear(Bridge *b, const uint8_t *p, size_t len) {
    if (len != 20)
        return -1;
    int32_t x = bes32(p);
    int32_t y = bes32(p + 4);
    uint32_t w = be32(p + 8);
    uint32_t h = be32(p + 12);
    uint32_t color = be32(p + 16);
    if (x < 0 || y < 0 || !w || !h || x + (int32_t)w > (int32_t)b->width ||
        y + (int32_t)h > (int32_t)b->height)
        return -1;
    if (color == 0)
        color = b->background;
    uint32_t xrgb = rgba_to_xrgb(color);
    PendingCmd *cmd = new_fill(b, x, y, w, h, xrgb);
    if (!cmd)
        return -1;
    queue_cmd(b, cmd);
    shadow_fill(b, x, y, w, h, xrgb);
    return 0;
}

static int handle_bitmap_rect(Bridge *b, int32_t x, int32_t y, uint32_t w, uint32_t h,
                              uint32_t stride, const uint8_t *pixels) {
    if (!w || !h || w > MAX_DIM || h > MAX_DIM || stride != w * 4)
        return -1;
    if (x < 0 || y < 0 || x + (int32_t)w > (int32_t)b->width ||
        y + (int32_t)h > (int32_t)b->height)
        return -1;
    size_t bytes = (size_t)stride * h;
    SharedBuf *buf = shared_new(bytes);
    if (!buf)
        return -1;
    memcpy(buf->data, pixels, bytes);
    rgba_to_bgrx(buf->data, bytes);
    PendingCmd *cmd = new_bitmap_draw(b, 0, x, y, w, h, stride, buf);
    if (!cmd) {
        shared_unref(buf);
        return -1;
    }
    queue_cmd(b, cmd);
    shadow_blit(b, x, y, w, h, buf->data, stride);
    shared_unref(buf);
    return 0;
}

static int handle_damage(Bridge *b, const uint8_t *p, size_t len) {
    if (len < 16)
        return -1;
    int32_t x = bes32(p);
    int32_t y = bes32(p + 4);
    uint32_t w = be32(p + 8);
    uint32_t h = be32(p + 12);
    /* Layout is x,y,w,h,stride, then pixels at offset 20. */
    if (len < 20)
        return -1;
    uint32_t stride = be32(p + 16);
    uint64_t bytes = (uint64_t)stride * h;
    if (len != 20 + bytes)
        return -1;
    return handle_bitmap_rect(b, x, y, w, h, stride, p + 20);
}

static int handle_full_frame(Bridge *b, const uint8_t *p, size_t len) {
    if (len < 12)
        return -1;
    uint32_t w = be32(p);
    uint32_t h = be32(p + 4);
    uint32_t stride = be32(p + 8);
    if (w != b->width || h != b->height || !w || !h || stride != w * 4)
        return -1;
    uint64_t bytes = (uint64_t)stride * h;
    if (bytes > MAX_FRAME_BYTES || len != 12 + bytes)
        return -1;
    return handle_bitmap_rect(b, 0, 0, w, h, stride, p + 12);
}

static int handle_audio(Bridge *b, const uint8_t *p, size_t len) {
    if (len < 12)
        return -1;
    uint32_t rate = be32(p);
    uint16_t channels = be16(p + 4);
    uint16_t fmt = be16(p + 6);
    uint32_t frames = be32(p + 8);
    uint64_t bytes = (uint64_t)frames * channels * 2;
    if (channels != 2 || fmt != SPICE_INTERFACE_AUDIO_FMT_S16 ||
        len != 12 + bytes || rate < 8000 || rate > 192000)
        return -1;
    if (rate != b->audio_rate) {
        b->audio_rate = rate;
        if (b->started && b->audio_started)
            spice_server_set_playback_rate(&b->playback, rate);
    }
    b->audio_records++;
    b->audio_bytes += bytes;
    audio_append(b, p + 12, (size_t)bytes);
    return 0;
}

static int handle_setup(Bridge *b, const uint8_t *p, size_t len) {
    if (len != 12)
        return -1;
    uint32_t w = be32(p);
    uint32_t h = be32(p + 4);
    uint32_t bg = be32(p + 8);
    if (!w || !h || w > MAX_DIM || h > MAX_DIM)
        return -1;
    b->width = w;
    b->height = h;
    b->stride = w * 4;
    b->background = bg;
    b->structured = 1;
    return 0;
}

static int handle_structured_record(Bridge *b, uint32_t type, const uint8_t *p, size_t len) {
    switch (type) {
    case OZSC_SETUP:
        /* The initial SETUP is consumed before the server starts.  Accepting
         * it again would change the display size of a live session. */
        return len == 12 ? 0 : -1;
    case OZSC_FRAME_BEGIN:
    case OZSC_FRAME_END:
        return len == 4 ? 0 : -1;
    case OZSC_IMAGE_DEF:
        return handle_image_def(b, p, len);
    case OZSC_IMAGE_FREE:
        return handle_image_free(b, p, len);
    case OZSC_PLACE:
        return handle_place(b, p, len);
    case OZSC_CLEAR:
        return handle_clear(b, p, len);
    case OZSC_DAMAGE:
        return handle_damage(b, p, len);
    case OZSC_FULL_FRAME:
        return handle_full_frame(b, p, len);
    case OZSC_AUDIO:
        return handle_audio(b, p, len);
    default:
        fprintf(stderr, "[spice-output] unknown OZSC record type %u\n", type);
        return -1;
    }
}

/* ------------------------------------------------------------------ */
/* OZFR composed frames and damage rectangles                          */

typedef struct {
    int32_t x, y;
    uint32_t w, h;
} Rect;

#define MAX_DAMAGE_RECTS 16

static int rect_row_differs(const uint8_t *a, const uint8_t *b, int width) {
    return memcmp(a, b, (size_t)width * 4) != 0;
}

static int compute_damage(const uint8_t *old_frame, const uint8_t *new_frame, uint32_t width,
                          uint32_t height, uint32_t stride, Rect *rects, int max_rects,
                          uint64_t *changed_pixels) {
    int n = 0;
    uint64_t total_changed = 0;
    if (memcmp(old_frame, new_frame, (size_t)stride * height) == 0) {
        *changed_pixels = 0;
        return 0;
    }

    uint32_t y = 0;
    while (y < height) {
        const uint8_t *orow = old_frame + (size_t)y * stride;
        const uint8_t *nrow = new_frame + (size_t)y * stride;
        if (!rect_row_differs(orow, nrow, (int32_t)width)) {
            y++;
            continue;
        }
        uint32_t top = y;
        uint32_t left = width, right = 0;
        while (y < height) {
            orow = old_frame + (size_t)y * stride;
            nrow = new_frame + (size_t)y * stride;
            if (!rect_row_differs(orow, nrow, (int32_t)width))
                break;
            for (uint32_t x = 0; x < width; x++) {
                if (memcmp(orow + (size_t)x * 4, nrow + (size_t)x * 4, 4) != 0) {
                    if (x < left)
                        left = x;
                    if (x > right)
                        right = x;
                    total_changed++;
                }
            }
            y++;
        }
        if (n >= max_rects)
            return -1; /* too fragmented, caller uses a full frame */
        rects[n++] = (Rect){.x = (int32_t)left, .y = (int32_t)top, .w = right - left + 1,
                            .h = y - top};
    }
    *changed_pixels = total_changed;
    return n;
}

static SharedBuf *crop_frame(const uint8_t *frame, uint32_t stride, Rect r) {
    size_t row = (size_t)r.w * 4;
    SharedBuf *buf = shared_new(row * r.h);
    if (!buf)
        return NULL;
    for (uint32_t y = 0; y < r.h; y++)
        memcpy(buf->data + (size_t)y * row, frame + (size_t)(r.y + (int32_t)y) * stride +
                                                       (size_t)r.x * 4, row);
    return buf;
}

static int handle_ozfr_frame(Bridge *b, uint32_t width, uint32_t height, uint32_t stride,
                             uint8_t *pixels) {
    if (width != b->width || height != b->height || stride != b->stride) {
        fprintf(stderr, "[spice-output] OZFR frame dimensions changed; restart the stream\n");
        return -1;
    }
    rgba_to_bgrx(pixels, (size_t)stride * height);

    Rect rects[MAX_DAMAGE_RECTS];
    uint64_t changed = 0;
    int n = compute_damage(b->shadow, pixels, width, height, stride, rects, MAX_DAMAGE_RECTS,
                           &changed);
    if (changed == 0) {
        free(pixels);
        return 0;
    }

    uint64_t total = (uint64_t)width * height;
    int full = n < 0 || changed * 4 >= total * 3; /* >=75% changed */
    if (full) {
        SharedBuf *buf = shared_new((size_t)stride * height);
        if (!buf) {
            free(pixels);
            return -1;
        }
        memcpy(buf->data, pixels, (size_t)stride * height);
        PendingCmd *cmd = new_bitmap_draw(b, 0, 0, 0, width, height, stride, buf);
        shared_unref(buf);
        if (!cmd) {
            free(pixels);
            return -1;
        }
        queue_cmd(b, cmd);
    } else {
        for (int i = 0; i < n; i++) {
            SharedBuf *buf = crop_frame(pixels, stride, rects[i]);
            if (!buf) {
                free(pixels);
                return -1;
            }
            PendingCmd *cmd = new_bitmap_draw(b, 0, rects[i].x, rects[i].y, rects[i].w,
                                              rects[i].h, rects[i].w * 4, buf);
            shared_unref(buf);
            if (!cmd) {
                free(pixels);
                return -1;
            }
            queue_cmd(b, cmd);
        }
    }
    memcpy(b->shadow, pixels, (size_t)stride * height);
    free(pixels);
    return 0;
}

/* ------------------------------------------------------------------ */
/* Input stream parser                                                 */

static void input_consume(InputBuf *in, size_t n) {
    if (n >= in->len) {
        in->len = 0;
        return;
    }
    memmove(in->data, in->data + n, in->len - n);
    in->len -= n;
}

static int parse_input(Bridge *b, InputBuf *in) {
    size_t off = 0;
    while (off < in->len) {
        const uint8_t *p = in->data + off;
        size_t avail = in->len - off;
        if (!b->structured) {
            if (avail < 16)
                break;
            if (memcmp(p, "OZFR", 4) != 0) {
                fprintf(stderr, "[spice-output] expected OZFR frame\n");
                return -1;
            }
            uint32_t w = be32(p + 4);
            uint32_t h = be32(p + 8);
            uint32_t stride = be32(p + 12);
            if (!w || !h || w > MAX_DIM || h > MAX_DIM || stride != w * 4 ||
                (uint64_t)stride * h > MAX_FRAME_BYTES) {
                fprintf(stderr, "[spice-output] invalid OZFR frame header\n");
                return -1;
            }
            size_t need = 16 + (size_t)stride * h;
            if (avail < need)
                break;
            uint8_t *pixels = malloc((size_t)stride * h);
            if (!pixels)
                return -1;
            memcpy(pixels, p + 16, (size_t)stride * h);
            if (handle_ozfr_frame(b, w, h, stride, pixels) < 0)
                return -1;
            off += need;
        } else {
            if (avail < 8)
                break;
            uint32_t type = be32(p);
            uint32_t len = be32(p + 4);
            if (len > MAX_OZSC_RECORD) {
                fprintf(stderr, "[spice-output] OZSC record too large\n");
                return -1;
            }
            if (avail < 8 + (size_t)len)
                break;
            if (handle_structured_record(b, type, p + 8, len) < 0) {
                fprintf(stderr, "[spice-output] invalid OZSC record type=%u len=%u\n", type, len);
                return -1;
            }
            off += 8 + (size_t)len;
        }
    }
    input_consume(in, off);
    bridge_wakeup(b);
    return 0;
}

static gboolean signal_quit(gpointer data) {
    Bridge *b = data;
    g_main_loop_quit(b->loop);
    return G_SOURCE_REMOVE;
}

static gboolean input_dispatch(gint fd, GIOCondition condition, gpointer data) {
    Bridge *b = data;
    InputBuf *in = g_input;
    (void)fd;
    if (condition & (G_IO_HUP | G_IO_ERR | G_IO_NVAL)) {
        if (in && in->len)
            parse_input(b, in);
        g_main_loop_quit(b->loop);
        return G_SOURCE_REMOVE;
    }
    /* Backpressure: leave bytes in the pipe rather than growing an unbounded
     * command queue.  GLib will call us again while the fd stays readable. */
    if (backpressured(b))
        return G_SOURCE_CONTINUE;

    if (!in->data) {
        in->cap = 65536;
        in->data = malloc(in->cap);
        if (!in->data) {
            b->failed = 1;
            g_main_loop_quit(b->loop);
            return G_SOURCE_REMOVE;
        }
    }
    if (in->len == in->cap) {
        if ((uint64_t)in->cap >= MAX_INPUT_BUFFER) {
            fprintf(stderr, "[spice-output] input record exceeds the size limit\n");
            b->failed = 1;
            g_main_loop_quit(b->loop);
            return G_SOURCE_REMOVE;
        }
        size_t cap = in->cap * 2;
        uint8_t *p = realloc(in->data, cap);
        if (!p) {
            b->failed = 1;
            g_main_loop_quit(b->loop);
            return G_SOURCE_REMOVE;
        }
        in->data = p;
        in->cap = cap;
    }
    ssize_t n = read(STDIN_FILENO, in->data + in->len, in->cap - in->len);
    if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR))
        return G_SOURCE_CONTINUE;
    if (n <= 0) {
        g_main_loop_quit(b->loop);
        return G_SOURCE_REMOVE;
    }
    in->len += (size_t)n;
    if (parse_input(b, in) < 0) {
        b->failed = 1;
        g_main_loop_quit(b->loop);
        return G_SOURCE_REMOVE;
    }
    return G_SOURCE_CONTINUE;
}

/* ------------------------------------------------------------------ */
/* Startup                                                             */

static void random_ticket(char *out, size_t n) {
    static const char chars[] = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    uint8_t raw[64];
    size_t got = 0;
    FILE *f = fopen("/dev/urandom", "rb");
    if (f) {
        got = fread(raw, 1, sizeof(raw), f);
        fclose(f);
    }
    if (!got) {
        for (size_t i = 0; i < sizeof(raw); i++)
            raw[i] = (uint8_t)(rand() & 0xff);
        got = sizeof(raw);
    }
    if (n) {
        for (size_t i = 0; i + 1 < n; i++)
            out[i] = chars[raw[i % got] % (sizeof(chars) - 1)];
        out[n - 1] = 0;
    }
}

static char *read_password_file(const char *path) {
    FILE *f = fopen(path, "r");
    if (!f)
        return NULL;
    char line[256];
    if (!fgets(line, sizeof(line), f)) {
        fclose(f);
        return NULL;
    }
    fclose(f);
    size_t n = strcspn(line, "\r\n");
    line[n] = 0;
    if (!n)
        return NULL;
    return strdup(line);
}

static int read_initial_stream(Bridge *b, int *mode_structured) {
    uint8_t magic[4];
    if (!read_full_fd(STDIN_FILENO, magic, sizeof(magic))) {
        fprintf(stderr, "[spice-output] stdin is empty\n");
        return -1;
    }
    if (memcmp(magic, "OZFR", 4) == 0) {
        uint8_t header[12];
        if (!read_full_fd(STDIN_FILENO, header, sizeof(header)))
            return -1;
        uint32_t w = be32(header);
        uint32_t h = be32(header + 4);
        uint32_t stride = be32(header + 8);
        uint64_t bytes = (uint64_t)stride * h;
        if (!w || !h || w > MAX_DIM || h > MAX_DIM || stride != w * 4 ||
            bytes > MAX_FRAME_BYTES) {
            fprintf(stderr, "[spice-output] invalid initial OZFR frame\n");
            return -1;
        }
        b->width = w;
        b->height = h;
        b->stride = stride;
        b->structured = 0;
        b->primary = malloc((size_t)bytes);
        b->shadow = malloc((size_t)bytes);
        if (!b->primary || !b->shadow)
            return -1;
        if (!read_full_fd(STDIN_FILENO, b->primary, (size_t)bytes))
            return -1;
        rgba_to_bgrx(b->primary, (size_t)bytes);
        memcpy(b->shadow, b->primary, (size_t)bytes);
        *mode_structured = 0;
        return 0;
    }
    if (memcmp(magic, "OZSC", 4) == 0) {
        uint8_t version[4];
        if (!read_full_fd(STDIN_FILENO, version, sizeof(version)) || be32(version) != 1) {
            fprintf(stderr, "[spice-output] unsupported OZSC version\n");
            return -1;
        }
        for (;;) {
            uint8_t rh[8];
            if (!read_full_fd(STDIN_FILENO, rh, sizeof(rh)))
                return -1;
            uint32_t type = be32(rh);
            uint32_t len = be32(rh + 4);
            if (len > MAX_OZSC_RECORD)
                return -1;
            uint8_t *payload = malloc(len ? len : 1);
            if (!payload)
                return -1;
            if (len && !read_full_fd(STDIN_FILENO, payload, len)) {
                free(payload);
                return -1;
            }
            if (type == OZSC_SETUP) {
                int rc = handle_setup(b, payload, len);
                free(payload);
                if (rc < 0) {
                    fprintf(stderr, "[spice-output] invalid OZSC SETUP\n");
                    return -1;
                }
                break;
            }
            free(payload);
            fprintf(stderr, "[spice-output] OZSC stream must start with SETUP\n");
            return -1;
        }
        uint64_t bytes = (uint64_t)b->stride * b->height;
        b->primary = malloc((size_t)bytes);
        b->shadow = malloc((size_t)bytes);
        if (!b->primary || !b->shadow)
            return -1;
        uint32_t bg = rgba_to_xrgb(b->background);
        for (uint32_t y = 0; y < b->height; y++) {
            for (uint32_t x = 0; x < b->width; x++) {
                size_t o = (size_t)y * b->stride + (size_t)x * 4;
                uint8_t *p = b->primary + o;
                p[0] = (uint8_t)(bg & 0xff);
                p[1] = (uint8_t)((bg >> 8) & 0xff);
                p[2] = (uint8_t)((bg >> 16) & 0xff);
                p[3] = 0xff;
            }
        }
        memcpy(b->shadow, b->primary, (size_t)bytes);
        *mode_structured = 1;
        return 0;
    }
    fprintf(stderr, "[spice-output] expected OZFR or OZSC on stdin\n");
    return -1;
}

int main(int argc, char **argv) {
    int port = 5930;
    int max_viewers = 1;
    char *listen_addr = NULL;
    char *cert_file = NULL;
    char *key_file = NULL;
    char *ca_file = NULL;
    char *password_file = NULL;
    GError *error = NULL;

    GOptionEntry entries[] = {
        {"port", 'p', 0, G_OPTION_ARG_INT, &port, "TLS port (default 5930)", "PORT"},
        {"tls-port", 's', 0, G_OPTION_ARG_INT, &port, "Alias for --port", "PORT"},
        {"listen", 0, 0, G_OPTION_ARG_STRING, &listen_addr, "Listen address (default 127.0.0.1)",
         "ADDR"},
        {"cert", 0, 0, G_OPTION_ARG_FILENAME, &cert_file, "TLS certificate chain (PEM)",
         "FILE"},
        {"key", 0, 0, G_OPTION_ARG_FILENAME, &key_file, "TLS private key (PEM)", "FILE"},
        {"ca", 0, 0, G_OPTION_ARG_FILENAME, &ca_file,
         "CA certificate for TLS (defaults to --cert)", "FILE"},
        {"password-file", 0, 0, G_OPTION_ARG_FILENAME, &password_file,
         "Viewer ticket file (first line); random ticket if omitted", "FILE"},
        {"max-viewers", 0, 0, G_OPTION_ARG_INT, &max_viewers,
         "Maximum viewers; libspice-server 0.16 enforces 1", "N"},
        {NULL}};

    GOptionContext *ctx = g_option_context_new("- outbound SPICE scene bridge");
    g_option_context_add_main_entries(ctx, entries, NULL);
    if (!g_option_context_parse(ctx, &argc, &argv, &error)) {
        fprintf(stderr, "%s\n", error->message);
        g_error_free(error);
        g_option_context_free(ctx);
        return 2;
    }
    g_option_context_free(ctx);

    /* Compatibility: the previous example accepted a positional loopback
     * port.  Treat a single remaining argument as the TLS port. */
    if (argc == 2) {
        char *end = NULL;
        errno = 0;
        long v = strtol(argv[1], &end, 10);
        if (!errno && end && !*end && v >= 1 && v <= 65535) {
            port = (int)v;
            argc = 1;
        }
    }
    if (argc != 1) {
        fprintf(stderr, "usage: %s [--port PORT] --cert CERT --key KEY [--ca CA] "
                        "[--password-file FILE] [--listen ADDR] [--max-viewers 1] < scene\n",
                argv[0]);
        return 2;
    }

    if (!cert_file)
        cert_file = getenv("SPICE_OUTPUT_CERT");
    if (!key_file)
        key_file = getenv("SPICE_OUTPUT_KEY");
    if (!ca_file)
        ca_file = getenv("SPICE_OUTPUT_CA");
    if (!password_file)
        password_file = getenv("SPICE_OUTPUT_PASSWORD_FILE");
    if (!cert_file || !key_file) {
        fprintf(stderr,
                "[spice-output] TLS is required: pass --cert/--key (or SPICE_OUTPUT_CERT/"
                "SPICE_OUTPUT_KEY)\n");
        return 2;
    }
    if (!ca_file)
        ca_file = cert_file;
    if (port < 1 || port > 65535) {
        fprintf(stderr, "[spice-output] invalid port\n");
        return 2;
    }
    if (max_viewers != 1) {
        fprintf(stderr,
                "[spice-output] libspice-server 0.16 supports one authenticated viewer per "
                "instance; --max-viewers must be 1\n");
        return 2;
    }
    if (!listen_addr)
        listen_addr = "127.0.0.1";

    char generated[17];
    char *password = NULL;
    int password_allocated = 0;
    if (password_file) {
        password = read_password_file(password_file);
        if (!password) {
            fprintf(stderr, "[spice-output] cannot read password file %s\n", password_file);
            return 2;
        }
        password_allocated = 1;
    } else {
        random_ticket(generated, sizeof(generated));
        password = generated;
        fprintf(stderr, "[spice-output] generated viewer ticket: %s\n", password);
    }

    Bridge b = {0};
    g_bridge = &b;
    g_mutex_init(&b.lock);
    b.audio_rate = SPICE_INTERFACE_PLAYBACK_FREQ;
    b.loop = g_main_loop_new(NULL, FALSE);

    InputBuf in = {0};
    g_input = &in;

    int structured = 0;
    if (read_initial_stream(&b, &structured) < 0)
        return 1;
    (void)structured;

    if (fcntl(STDIN_FILENO, F_SETFL, fcntl(STDIN_FILENO, F_GETFL) | O_NONBLOCK) < 0) {
        fprintf(stderr, "[spice-output] cannot make stdin non-blocking\n");
        return 1;
    }

    b.server = spice_server_new();
    if (!b.server)
        return 1;
    spice_server_set_name(b.server, "Ozma scene output");
    /* This bridge implements display commands and surfaces, not SPICE video
     * streams.  Keep the server from attempting stream encoding. */
    spice_server_set_streaming_video(b.server, SPICE_STREAM_VIDEO_OFF);
    spice_server_set_addr(b.server,
                          listen_addr,
                          strchr(listen_addr, ':') ? 0 : SPICE_ADDR_FLAG_IPV4_ONLY);
    if (spice_server_set_tls(b.server, port, ca_file, cert_file, key_file, NULL, NULL, NULL)) {
        fprintf(stderr, "[spice-output] spice_server_set_tls failed (check cert/key/CA)\n");
        return 1;
    }
    if (spice_server_set_ticket(b.server, password, 0, 0, 1)) {
        fprintf(stderr, "[spice-output] spice_server_set_ticket failed\n");
        return 1;
    }

    SpiceCoreInterface core = {0};
    core.base.type = SPICE_INTERFACE_CORE;
    core.base.major_version = SPICE_INTERFACE_CORE_MAJOR;
    core.base.minor_version = SPICE_INTERFACE_CORE_MINOR;
    core.timer_add = timer_add;
    core.timer_start = timer_start;
    core.timer_cancel = timer_cancel;
    core.timer_remove = timer_remove;
    core.watch_add = watch_add;
    core.watch_update_mask = watch_update;
    core.watch_remove = watch_remove;
    core.channel_event = channel_event;
    if (spice_server_init(b.server, &core)) {
        fprintf(stderr, "[spice-output] spice_server_init failed\n");
        return 1;
    }

    memset(&b.qxl, 0, sizeof(b.qxl));
    b.qxl.base.type = SPICE_INTERFACE_QXL;
    b.qxl.base.major_version = SPICE_INTERFACE_QXL_MAJOR;
    b.qxl.base.minor_version = SPICE_INTERFACE_QXL_MINOR;
    b.qxl.attached_worker = attached_worker;
    b.qxl.set_compression_level = set_compression_level;
    b.qxl.get_init_info = get_init_info;
    b.qxl.get_command = get_command;
    b.qxl.req_cmd_notification = req_cmd_notification;
    b.qxl.release_resource = release_resource;
    b.qxl.get_cursor_command = get_cursor_command;
    b.qxl.req_cursor_notification = req_cursor_notification;
    b.qxl.notify_update = notify_update;
    b.qxl.flush_resources = flush_resources;
    b.qxl.async_complete = async_complete;
    b.qxl.update_area_complete = update_area_complete;
    b.qxl.set_client_capabilities = set_client_capabilities;
    b.qxl.client_monitors_config = client_monitors_config;
    b.instance.base.sif = &b.qxl.base;
    b.instance.id = 0;
    if (spice_server_add_interface(b.server, &b.instance.base)) {
        fprintf(stderr, "[spice-output] spice_server_add_interface(qxl) failed\n");
        return 1;
    }

    memset(&b.playback_iface, 0, sizeof(b.playback_iface));
    b.playback_iface.base.type = SPICE_INTERFACE_PLAYBACK;
    b.playback_iface.base.major_version = SPICE_INTERFACE_PLAYBACK_MAJOR;
    b.playback_iface.base.minor_version = SPICE_INTERFACE_PLAYBACK_MINOR;
    b.playback.base.sif = &b.playback_iface.base;
    if (spice_server_add_interface(b.server, &b.playback.base)) {
        fprintf(stderr, "[spice-output] spice_server_add_interface(playback) failed\n");
        return 1;
    }
    b.started = 1;
    spice_server_vm_start(b.server);
    spice_server_playback_start(&b.playback);
    b.audio_started = 1;
    bridge_wakeup(&b);

    fprintf(stderr, "[spice-output] listening on spice+tls://%s:%d (ticket auth required)\n",
            listen_addr, port);

    g_unix_fd_add(STDIN_FILENO, G_IO_IN | G_IO_HUP | G_IO_ERR, input_dispatch, &b);
    g_unix_signal_add(SIGINT, signal_quit, &b);
    g_unix_signal_add(SIGTERM, signal_quit, &b);
    g_main_loop_run(b.loop);

    b.shutting_down = 1;
    fprintf(stderr,
            "[spice-output] display summary: surface_create=%llu surface_destroy=%llu "
            "draw_surface=%llu draw_bitmap=%llu draw_fill=%llu full_frames=%llu "
            "damage_rects=%llu\n",
            (unsigned long long)b.stat_surface_create,
            (unsigned long long)b.stat_surface_destroy,
            (unsigned long long)b.stat_draw_surface,
            (unsigned long long)b.stat_draw_bitmap, (unsigned long long)b.stat_draw_fill,
            (unsigned long long)b.stat_full_frames,
            (unsigned long long)b.stat_damage_rects);
    if (b.audio_records)
        fprintf(stderr,
                "[spice-output] audio summary: records=%llu bytes=%llu frames_sent=%llu\n",
                (unsigned long long)b.audio_records, (unsigned long long)b.audio_bytes,
                (unsigned long long)b.audio_frames_sent);
    if (b.audio_started) {
        spice_server_playback_stop(&b.playback);
        b.audio_started = 0;
    }
    spice_qxl_destroy_primary_surface(&b.instance, 0);
    spice_server_destroy(b.server);

    /* Release commands that were never handed to the server.  Commands that
     * were handed off are freed by release_resource. */
    g_mutex_lock(&b.lock);
    PendingCmd *cmd = b.cmd_head;
    b.cmd_head = b.cmd_tail = NULL;
    b.queue_len = 0;
    g_mutex_unlock(&b.lock);
    while (cmd) {
        PendingCmd *next = cmd->next;
        shared_unref(cmd->buf);
        free(cmd);
        cmd = next;
    }

    for (int i = 0; i < MAX_RESOURCES; i++) {
        if (b.resources[i].used)
            shared_unref(b.resources[i].buf);
    }
    free(in.data);
    free(b.audio);
    free(b.primary);
    free(b.shadow);
    if (password_allocated)
        free(password);
    g_main_loop_unref(b.loop);
    g_mutex_clear(&b.lock);
    g_bridge = NULL;
    return b.failed ? 1 : 0;
}
