#!/usr/bin/env node
// Forward browser scene JSONL to the node's live browser scene socket.
//
// The fixture page writes the same records to its downloadable replay. This
// bridge replays those records over the WebSocket instead of writing them to a
// file, so the node can feed a live browser scene to scene-capable sessions.
//
//   node live.mjs --node ws://127.0.0.1:7382 --file ozma-scene.jsonl
//   node live.mjs --node ws://127.0.0.1:7382 --token "$OZMA_API_TOKEN" < ozma-scene.jsonl
//
// Only resource, scene_commit, and video_frame records are forwarded. Video is
// metadata-only; this script does not encode or send browser video frames.

import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import https from "node:https";
import readline from "node:readline";

function usage() {
  console.error(`usage: node live.mjs [--node ws://127.0.0.1:7382] [--path /api/v1/browser/scene/ws]
                         [--token TOKEN] [--file ozma-scene.jsonl]

Reads fixture JSONL from --file or stdin and forwards resource, scene_commit,
and video_frame records to the node WebSocket.`);
}

function parseArgs(argv) {
  const options = {
    node: process.env.OZMA_NODE ?? "ws://127.0.0.1:7382",
    path: "/api/v1/browser/scene/ws",
    token: process.env.OZMA_API_TOKEN,
    file: undefined,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      usage();
      process.exit(0);
    } else if (arg === "--node") {
      options.node = argv[++index];
    } else if (arg === "--path") {
      options.path = argv[++index];
    } else if (arg === "--token") {
      options.token = argv[++index];
    } else if (arg === "--file") {
      options.file = argv[++index];
    } else if (!arg.startsWith("-") && !options.file) {
      options.file = arg;
    } else {
      usage();
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return options;
}

function encodeFrame(opcode, payload) {
  const data = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const mask = crypto.randomBytes(4);
  let header;
  if (data.length < 126) {
    header = Buffer.alloc(2);
    header[1] = 0x80 | data.length;
  } else if (data.length < 65536) {
    header = Buffer.alloc(4);
    header[1] = 0x80 | 126;
    header.writeUInt16BE(data.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[1] = 0x80 | 127;
    header.writeBigUInt64BE(BigInt(data.length), 2);
  }
  header[0] = 0x80 | opcode;
  const masked = Buffer.alloc(data.length);
  for (let index = 0; index < data.length; index += 1) {
    masked[index] = data[index] ^ mask[index % 4];
  }
  return Buffer.concat([header, mask, masked]);
}

class LiveSocket {
  constructor(url, token) {
    this.url = url;
    this.token = token;
    this.socket = undefined;
    this.buffer = Buffer.alloc(0);
    this.pending = [];
    this.acks = 0;
    this.errors = 0;
    this.closed = false;
  }

  connect() {
    const target = new URL(this.url);
    if (target.protocol === "http:") target.protocol = "ws:";
    if (target.protocol === "https:") target.protocol = "wss:";
    if (target.protocol !== "ws:" && target.protocol !== "wss:") {
      throw new Error(`unsupported WebSocket URL: ${this.url}`);
    }
    const secure = target.protocol === "wss:";
    const headers = {
      Connection: "Upgrade",
      Upgrade: "websocket",
      "Sec-WebSocket-Key": crypto.randomBytes(16).toString("base64"),
      "Sec-WebSocket-Version": "13",
    };
    if (this.token) headers.Authorization = `Bearer ${this.token}`;

    return new Promise((resolve, reject) => {
      const request = (secure ? https : http).request({
        hostname: target.hostname,
        port: target.port || (secure ? 443 : 80),
        path: `${target.pathname}${target.search}`,
        method: "GET",
        headers,
      });
      request.on("upgrade", (response, socket, head) => {
        if (response.statusCode !== 101) {
          reject(new Error(`WebSocket handshake returned ${response.statusCode}`));
          socket.destroy();
          return;
        }
        this.socket = socket;
        this.buffer = head;
        socket.on("data", chunk => {
          this.buffer = Buffer.concat([this.buffer, chunk]);
          this.consume();
        });
        socket.on("close", () => {
          this.closed = true;
          console.error(`live scene socket closed (${this.acks} ack, ${this.errors} error)`);
          process.exitCode = this.errors === 0 ? 0 : 1;
        });
        socket.on("error", error => {
          console.error(`live scene socket error: ${error.message}`);
          process.exitCode = 1;
        });
        for (const text of this.pending.splice(0)) this.writeText(text);
        resolve();
      });
      request.on("response", response => {
        response.resume();
        reject(new Error(`WebSocket handshake returned ${response.statusCode}`));
      });
      request.on("error", reject);
      request.end();
    });
  }

  writeText(text) {
    if (!this.socket || this.closed) {
      this.pending.push(text);
      return;
    }
    this.socket.write(encodeFrame(0x1, text));
  }

  consume() {
    while (true) {
      if (this.buffer.length < 2) return;
      const opcode = this.buffer[0] & 0x0f;
      const masked = (this.buffer[1] & 0x80) !== 0;
      let length = this.buffer[1] & 0x7f;
      let offset = 2;
      if (length === 126) {
        if (this.buffer.length < 4) return;
        length = this.buffer.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        if (this.buffer.length < 10) return;
        length = Number(this.buffer.readBigUInt64BE(2));
        offset = 10;
      }
      let mask;
      if (masked) {
        if (this.buffer.length < offset + 4) return;
        mask = this.buffer.subarray(offset, offset + 4);
        offset += 4;
      }
      if (this.buffer.length < offset + length) return;
      let payload = this.buffer.subarray(offset, offset + length);
      if (masked) {
        payload = Buffer.from(payload);
        for (let index = 0; index < payload.length; index += 1) {
          payload[index] ^= mask[index % 4];
        }
      }
      this.buffer = this.buffer.subarray(offset + length);

      if (opcode === 0x8) {
        this.socket?.end();
      } else if (opcode === 0x9) {
        this.socket?.write(encodeFrame(0xa, payload));
      } else if (opcode === 0x1) {
        const text = payload.toString();
        console.log(`<- ${text}`);
        try {
          const event = JSON.parse(text);
          if (event.type === "ack") this.acks += 1;
          if (event.type === "error") this.errors += 1;
        } catch {
          // The node only sends JSON events; print an unexpected frame as-is.
        }
      }
    }
  }

  close() {
    if (this.socket && !this.closed) {
      this.socket.write(encodeFrame(0x8, Buffer.alloc(0)));
      this.socket.end();
    }
  }
}

const options = parseArgs(process.argv.slice(2));
const target = new URL(options.node);
if (target.protocol === "http:") target.protocol = "ws:";
if (target.protocol === "https:") target.protocol = "wss:";
if (!target.pathname || target.pathname === "/") target.pathname = options.path;

const socket = new LiveSocket(target.toString(), options.token);
await socket.connect();
console.log(`live scene socket connected to ${target}`);

const input = options.file ? fs.createReadStream(options.file) : process.stdin;
if (options.file) {
  input.on("error", error => {
    console.error(`could not read ${options.file}: ${error.message}`);
    process.exit(1);
  });
}

const lines = readline.createInterface({ input, crlfDelay: Infinity });
lines.on("line", line => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let event;
  try {
    event = JSON.parse(trimmed);
  } catch (error) {
    console.error(`ignoring invalid JSONL: ${error.message}`);
    return;
  }
  if (!["resource", "scene_commit", "video_frame"].includes(event.type)) return;
  socket.writeText(JSON.stringify(event));
});
lines.on("close", () => {
  // Give acks a moment to arrive before closing the development connection.
  setTimeout(() => socket.close(), 250);
});
