#!/usr/bin/env python3
"""Generate test streams for the spice-output bridge.

The script writes to stdout and is intended to be piped into the bridge:

    python3 make_test_stream.py --format ozsc | ./spice-output ...

OZSC exercises image surfaces, moves, clears, surface destruction, a bitmap
damage rectangle, and (optionally) raw S16 stereo PCM.  OZFR exercises the
composed-frame path and the bridge's damage-rectangle diffing.
"""
import argparse
import math
import struct
import sys
import time

BG = (30, 40, 60, 255)


def be32(v):
    return struct.pack(">I", v & 0xFFFFFFFF)


def record(rtype, payload=b""):
    return be32(rtype) + be32(len(payload)) + payload


class Ozsc:
    def __init__(self, width, height, background=BG):
        self.width = width
        self.height = height
        self.background = background

    def out(self, data):
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    def start(self):
        bg = (self.background[0] << 24) | (self.background[1] << 16) | \
             (self.background[2] << 8) | self.background[3]
        self.out(b"OZSC" + be32(1))
        self.out(record(1, struct.pack(">III", self.width, self.height, bg)))

    def frame_begin(self, frame_id):
        self.out(record(2, be32(frame_id)))

    def frame_end(self, frame_id):
        self.out(record(9, be32(frame_id)))

    def image_def(self, rid, width, height, pixels):
        self.out(record(3, struct.pack(">IIII", rid, width, height, width * 4) + pixels))

    def image_free(self, rid):
        self.out(record(4, be32(rid)))

    def place(self, rid, x, y):
        self.out(record(5, be32(rid) + struct.pack(">ii", x, y)))

    def clear(self, x, y, width, height, color=0):
        self.out(record(6, struct.pack(">iiIII", x, y, width, height, color)))

    def damage(self, x, y, width, height, pixels):
        self.out(record(7, struct.pack(">iiIII", x, y, width, height, width * 4) + pixels))

    def audio(self, frames, rate=44100):
        pcm = bytearray()
        for i in range(frames):
            v = int(6000 * math.sin(2 * math.pi * 440 * (i / rate)))
            pcm += struct.pack("<hh", v, v)
        self.out(record(10, struct.pack(">IHHI", rate, 2, 1, frames) + pcm))


def image_pair():
    w, h = 80, 60
    left = bytearray()
    for _y in range(h):
        for x in range(w):
            left += bytes((230, 70, 50, 255)) if x < w // 2 else bytes((40, 90, 230, 255))
    small = bytes((40, 220, 80, 255)) * (40 * 40)
    return (w, h, bytes(left)), (40, 40, small)


def emit_audio(stream, chunks=8):
    for _ in range(chunks):
        stream.audio(1024)
        time.sleep(0.1)


def run_ozsc(args):
    big, small_img = image_pair()
    w, h, red_blue = big
    small_w, small_h, small = small_img
    stream = Ozsc(args.width, args.height)
    stream.start()
    stream.frame_begin(0)
    stream.image_def(1, w, h, red_blue)
    stream.image_def(2, small_w, small_h, small)
    stream.place(1, 30, 30)
    stream.place(2, 220, 60)
    stream.frame_end(0)
    if args.audio:
        emit_audio(stream, max(1, int(args.hold * 10)))
    else:
        time.sleep(args.hold)

    stream.frame_begin(1)
    stream.clear(30, 30, w, h)
    stream.place(1, 120, 100)
    stream.frame_end(1)
    if args.audio:
        emit_audio(stream, max(1, int(args.hold * 10)))
    else:
        time.sleep(args.hold)

    stream.frame_begin(2)
    stream.clear(120, 100, w, h)
    stream.clear(220, 60, small_w, small_h)
    stream.image_free(2)
    stream.place(1, 280, 150)
    yellow = bytes((240, 230, 40, 255)) * (60 * 30)
    stream.damage(10, 10, 60, 30, yellow)
    stream.frame_end(2)
    if args.audio:
        emit_audio(stream, max(1, int(args.hold * 10)))
    else:
        time.sleep(args.hold)

    if not args.loop:
        return
    x, y = 280, 150
    step = 0
    while True:
        stream.frame_begin(3 + step)
        stream.clear(x, y, w, h)
        x = 30 if x == 280 else 280
        y = 30 if y == 150 else 150
        stream.place(1, x, y)
        stream.frame_end(3 + step)
        if args.audio:
            emit_audio(stream, 4)
        else:
            time.sleep(0.25)
        step += 1


def ozfr_frame(width, height, step):
    stride = width * 4
    bg = bytes((BG[0], BG[1], BG[2], 255)) * width
    frame = bytearray(bg * height)
    x0 = 40 + (step % 8) * 40
    y0 = 40 + ((step // 8) % 3) * 30
    for y in range(y0, min(y0 + 60, height)):
        for x in range(x0, min(x0 + 60, width)):
            o = y * stride + x * 4
            frame[o:o + 4] = bytes((220, 60, 40, 255))
    return struct.pack(">4sIII", b"OZFR", width, height, stride) + bytes(frame)


def run_ozfr(args):
    width, height = min(args.width, 1024), min(args.height, 768)
    step = 0
    while True:
        sys.stdout.buffer.write(ozfr_frame(width, height, step))
        sys.stdout.buffer.flush()
        step += 1
        if not args.loop and step >= 12:
            return
        time.sleep(args.hold)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--format", choices=("ozsc", "ozfr"), default="ozsc")
    parser.add_argument("--width", type=int, default=400)
    parser.add_argument("--height", type=int, default=240)
    parser.add_argument("--hold", type=float, default=2.0,
                        help="seconds between scene revisions (default 2.0)")
    parser.add_argument("--audio", action="store_true",
                        help="interleave 44.1 kHz S16 stereo PCM (OZSC only)")
    parser.add_argument("--loop", action="store_true", help="repeat the moving-image scene")
    args = parser.parse_args()
    try:
        if args.format == "ozsc":
            run_ozsc(args)
        else:
            run_ozfr(args)
    except BrokenPipeError:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
