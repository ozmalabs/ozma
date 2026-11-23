#!/usr/bin/env node
// Replay bridge for browser scene events. It accepts JSONL on stdin and emits
// JSONL presentation events. A production adapter can replace this process
// with a QUIC producer without changing the event contract.

import readline from "node:readline";

const budget = Number(process.argv[2] ?? 4 * 1024 * 1024);
const resources = new Map();
let used = 0;
let presentedRevision = 0;
let pending;

function output(event) {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

function tryPresent() {
  if (!pending || pending.revision <= presentedRevision) return;
  if (!pending.images.every(image => resources.has(image.resource_id))) return;
  presentedRevision = pending.revision;
  output({ type: "presented", commit: pending });
  pending = undefined;
}

function resource(event) {
  if (typeof event.id !== "number" || typeof event.payload_base64 !== "string") {
    throw new Error("resource requires numeric id and payload_base64");
  }
  const bytes = Buffer.from(event.payload_base64, "base64");
  if (bytes.length < 8 || bytes.readBigUInt64BE(0) !== BigInt(event.id)) {
    throw new Error("resource framing does not match its id");
  }
  const existing = resources.get(event.id);
  if (existing) {
    if (!existing.equals(bytes)) throw new Error("resource id was reused with different bytes");
    return;
  }
  if (used + bytes.length > budget) throw new Error("scene resource cache budget exceeded");
  resources.set(event.id, bytes);
  used += bytes.length;
  output({ type: "resource_cached", id: event.id, bytes: bytes.length });
  tryPresent();
}

const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", line => {
  try {
    const event = JSON.parse(line);
    if (event.type === "resource") resource(event);
    else if (event.type === "scene_commit") {
      const commit = event.commit;
      if (!commit || !Number.isInteger(commit.revision) || !Array.isArray(commit.images)) {
        throw new Error("invalid scene_commit");
      }
      if (commit.revision > presentedRevision) pending = commit;
      tryPresent();
    } else if (event.type === "video_frame") {
      output({ type: "video_frame", surface_id: event.surface_id, frame: event.frame });
    }
  } catch (error) {
    output({ type: "error", message: error.message });
  }
});
