/*
 * QEMU display backend: v4l2loopback output
 * SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Writes QEMU's display framebuffer directly to a v4l2loopback device.
 * No VNC, no SPICE, no intermediate protocol. The VM's display appears
 * as a V4L2 capture device that ffmpeg, OBS, or ozma can read.
 *
 * Usage:
 *   qemu-system-x86_64 ... -display v4l2,device=/dev/video10
 *
 * Or with auto-created v4l2loopback:
 *   qemu-system-x86_64 ... -display v4l2
 *
 * The ozma soft node uses this to stream the VM's display without VNC.
 * The v4l2 device looks identical to a real HDMI capture card — the
 * controller can't tell the difference between a virtual and hardware node.
 *
 * Build:
 *   Build as a QEMU module, or compile standalone and LD_PRELOAD.
 *   See dev/qemu-v4l2/build.sh
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <linux/videodev2.h>

/*
 * This is a standalone implementation that runs as a separate process.
 * It reads the QEMU framebuffer via shared memory or VNC and writes
 * to v4l2loopback. A proper QEMU module would use the DisplayChangeListener
 * API directly — this is the pragmatic version that works without
 * recompiling QEMU.
 *
 * Architecture:
 *   QEMU -display vnc=unix:/tmp/qemu-vnc.sock (local socket, not TCP)
 *     → this process connects to the unix socket VNC
 *     → decodes the framebuffer
 *     → writes to /dev/videoN (v4l2loopback)
 *
 * The VNC is on a unix socket — no TCP, no network. It's essentially
 * shared memory with VNC framing. Near-zero overhead.
 *
 * For the real QEMU module version (requires QEMU source tree):
 * see qemu-display-v4l2-module.c (TODO)
 */

#include <sys/socket.h>
#include <sys/un.h>

#define V4L2_BUF_COUNT 2
#define DEFAULT_WIDTH 1024
#define DEFAULT_HEIGHT 768
#define DEFAULT_FPS 30

struct ozma_v4l2_output {
    int fd;
    int width;
    int height;
    int stride;
    void *buffers[V4L2_BUF_COUNT];
    size_t buf_size;
    int current_buf;
};

static int v4l2_open(struct ozma_v4l2_output *out, const char *device,
                      int width, int height) {
    out->fd = open(device, O_RDWR);
    if (out->fd < 0) {
        perror("v4l2: open");
        return -1;
    }

    out->width = width;
    out->height = height;
    out->stride = width * 4; /* BGRA */
    out->buf_size = out->stride * height;
    out->current_buf = 0;

    /* Set format */
    struct v4l2_format fmt = {0};
    fmt.type = V4L2_BUF_TYPE_VIDEO_OUTPUT;
    fmt.fmt.pix.width = width;
    fmt.fmt.pix.height = height;
    fmt.fmt.pix.pixelformat = V4L2_PIX_FMT_BGR32;
    fmt.fmt.pix.sizeimage = out->buf_size;
    fmt.fmt.pix.field = V4L2_FIELD_NONE;

    if (ioctl(out->fd, VIDIOC_S_FMT, &fmt) < 0) {
        perror("v4l2: VIDIOC_S_FMT");
        close(out->fd);
        return -1;
    }

    return 0;
}

static int v4l2_write_frame(struct ozma_v4l2_output *out,
                             const void *data, size_t size) {
    size_t to_write = size < out->buf_size ? size : out->buf_size;
    ssize_t written = write(out->fd, data, to_write);
    return written > 0 ? 0 : -1;
}

static void v4l2_close(struct ozma_v4l2_output *out) {
    if (out->fd >= 0) {
        close(out->fd);
        out->fd = -1;
    }
}

/*
 * Main: connect to QEMU VNC unix socket, decode frames, write to v4l2.
 *
 * For a production version, this would be a proper QEMU display module
 * that hooks into DisplayChangeListener directly. This standalone version
 * demonstrates the concept and works today.
 */
int main(int argc, char **argv) {
    const char *v4l2_device = "/dev/video10";
    const char *vnc_socket = "/tmp/qemu-vnc.sock";

    for (int i = 1; i < argc; i++) {
        if (strncmp(argv[i], "--device=", 9) == 0)
            v4l2_device = argv[i] + 9;
        else if (strncmp(argv[i], "--vnc=", 6) == 0)
            vnc_socket = argv[i] + 6;
    }

    setbuf(stdout, NULL); /* unbuffered output */
    printf("ozma-qemu-v4l2: vnc=%s → %s\n", vnc_socket, v4l2_device);

    /* Helper: read exactly n bytes */
    #define READ_EXACT(fd, buf, n) do { \
        size_t _total = 0; \
        while (_total < (size_t)(n)) { \
            ssize_t _r = read(fd, (char*)(buf) + _total, (n) - _total); \
            if (_r <= 0) { fprintf(stderr, "read failed at %zu/%d\n", _total, (int)(n)); goto done; } \
            _total += _r; \
        } \
    } while(0)

    /* Open v4l2loopback device */
    struct ozma_v4l2_output v4l2 = { .fd = -1 };
    if (v4l2_open(&v4l2, v4l2_device, DEFAULT_WIDTH, DEFAULT_HEIGHT) < 0) {
        fprintf(stderr, "Failed to open v4l2 device %s\n", v4l2_device);
        fprintf(stderr, "Load v4l2loopback: sudo modprobe v4l2loopback devices=1 "
                "video_nr=10 card_label=OzmaVM\n");
        return 1;
    }

    /* Connect to QEMU VNC unix socket */
    int vnc_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (vnc_fd < 0) {
        perror("socket");
        return 1;
    }

    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, vnc_socket, sizeof(addr.sun_path) - 1);

    if (connect(vnc_fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("connect to VNC socket");
        fprintf(stderr, "Start QEMU with: -vnc unix:%s\n", vnc_socket);
        return 1;
    }

    printf("Connected to VNC socket. Reading frames...\n");

    /* VNC handshake (minimal — just enough to get framebuffer updates) */
    char buf[4096];

    /* Read server protocol version */
    READ_EXACT(vnc_fd, buf, 12);
    /* Send client version */
    write(vnc_fd, "RFB 003.008\n", 12);

    /* Security handshake (VNC 3.8 — assume no auth for local socket) */
    READ_EXACT(vnc_fd, buf, 1); /* number of security types (1 byte) */
    int n_sec = (unsigned char)buf[0];
    READ_EXACT(vnc_fd, buf, n_sec); /* security type bytes */
    buf[0] = 1; /* Select type 1 = None */
    write(vnc_fd, buf, 1);
    READ_EXACT(vnc_fd, buf, 4); /* security result (0 = OK) */

    /* Client init: shared flag */
    buf[0] = 1; /* shared */
    write(vnc_fd, buf, 1);

    /* Server init: read framebuffer info */
    READ_EXACT(vnc_fd, buf, 24); /* ServerInit */
    int fb_width = (unsigned char)buf[0] << 8 | (unsigned char)buf[1];
    int fb_height = (unsigned char)buf[2] << 8 | (unsigned char)buf[3];
    int bpp = (unsigned char)buf[4];
    int name_len = (unsigned char)buf[20] << 24 | (unsigned char)buf[21] << 16 |
                   (unsigned char)buf[22] << 8 | (unsigned char)buf[23];
    READ_EXACT(vnc_fd, buf, name_len); /* desktop name */

    printf("Framebuffer: %dx%d @ %d bpp\n", fb_width, fb_height, bpp);

    /* Resize v4l2 if needed */
    if (fb_width != v4l2.width || fb_height != v4l2.height) {
        v4l2_close(&v4l2);
        v4l2_open(&v4l2, v4l2_device, fb_width, fb_height);
    }

    /* Set pixel format to raw (32-bit BGRA) */
    unsigned char set_pf[20] = {0};
    set_pf[0] = 0; /* SetPixelFormat */
    /* pixel format: 32bpp, 24depth, BGRA */
    set_pf[4] = 32; /* bpp */
    set_pf[5] = 24; /* depth */
    set_pf[6] = 0;  /* big-endian: no */
    set_pf[7] = 1;  /* true-color: yes */
    set_pf[8] = 0; set_pf[9] = 0xFF; /* red-max: 255 */
    set_pf[10] = 0; set_pf[11] = 0xFF; /* green-max */
    set_pf[12] = 0; set_pf[13] = 0xFF; /* blue-max */
    set_pf[14] = 16; /* red-shift */
    set_pf[15] = 8;  /* green-shift */
    set_pf[16] = 0;  /* blue-shift */
    write(vnc_fd, set_pf, 20);

    /* Set encoding to Raw */
    unsigned char set_enc[8] = {0};
    set_enc[0] = 2; /* SetEncodings */
    set_enc[2] = 0; set_enc[3] = 1; /* 1 encoding */
    /* encoding 0 = Raw */
    write(vnc_fd, set_enc, 8);

    /* Allocate framebuffer */
    size_t fb_size = fb_width * fb_height * 4;
    unsigned char *framebuffer = malloc(fb_size);
    if (!framebuffer) { perror("malloc"); return 1; }
    memset(framebuffer, 0, fb_size);

    /* Main loop: request updates, read pixels, write to v4l2 */
    while (1) {
        /* FramebufferUpdateRequest (incremental) */
        unsigned char req[10] = {0};
        req[0] = 3; /* FramebufferUpdateRequest */
        req[1] = 1; /* incremental */
        req[6] = fb_width >> 8; req[7] = fb_width & 0xFF;
        req[8] = fb_height >> 8; req[9] = fb_height & 0xFF;
        write(vnc_fd, req, 10);

        /* Read response */
        unsigned char msg_type;
        if (read(vnc_fd, &msg_type, 1) <= 0) break;

        if (msg_type == 0) { /* FramebufferUpdate */
            unsigned char hdr[3];
            read(vnc_fd, hdr, 3);
            int n_rects = hdr[1] << 8 | hdr[2];

            for (int r = 0; r < n_rects; r++) {
                unsigned char rect[12];
                read(vnc_fd, rect, 12);
                int rx = rect[0] << 8 | rect[1];
                int ry = rect[2] << 8 | rect[3];
                int rw = rect[4] << 8 | rect[5];
                int rh = rect[6] << 8 | rect[7];
                /* encoding type in rect[8..11], should be 0 (raw) */

                /* Read raw pixel data for this rectangle */
                size_t rect_size = rw * rh * 4;
                unsigned char *rect_data = malloc(rect_size);
                size_t total_read = 0;
                while (total_read < rect_size) {
                    ssize_t n = read(vnc_fd, rect_data + total_read,
                                     rect_size - total_read);
                    if (n <= 0) { free(rect_data); goto done; }
                    total_read += n;
                }

                /* Copy rectangle into framebuffer */
                for (int y = 0; y < rh; y++) {
                    memcpy(framebuffer + ((ry + y) * fb_width + rx) * 4,
                           rect_data + y * rw * 4,
                           rw * 4);
                }
                free(rect_data);
            }

            /* Write complete frame to v4l2 */
            v4l2_write_frame(&v4l2, framebuffer, fb_size);
        }
        /* Ignore other message types (bell, clipboard, etc.) */
        else {
            /* Skip unknown messages */
            usleep(1000);
        }
    }

done:
    free(framebuffer);
    v4l2_close(&v4l2);
    close(vnc_fd);
    printf("Disconnected.\n");
    return 0;
}
