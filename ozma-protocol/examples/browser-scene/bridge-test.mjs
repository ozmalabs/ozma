#!/usr/bin/env node
import { spawn } from "node:child_process";

const bridge = spawn(process.execPath, [new URL("./bridge.mjs", import.meta.url).pathname, "128"], {
  stdio: ["pipe", "pipe", "inherit"],
});
let output = "";
bridge.stdout.on("data", chunk => { output += chunk; });
const png = Buffer.from("89504e470d0a1a0a fixture", "hex");
const framed = Buffer.alloc(8 + png.length);
framed.writeBigUInt64BE(7n);
png.copy(framed, 8);
const write = event => bridge.stdin.write(`${JSON.stringify(event)}\n`);
write({ type: "scene_commit", commit: { revision: 1, images: [{ node_id: 1, resource_id: 7, x: 0, y: 100, width: 320, height: 180 }] } });
write({ type: "resource", id: 7, payload_base64: framed.toString("base64") });
write({ type: "scene_commit", commit: { revision: 2, images: [{ node_id: 1, resource_id: 7, x: 0, y: 25, width: 320, height: 180 }] } });
write({ type: "video_frame", surface_id: 1, frame: 3 });
bridge.stdin.end();
bridge.on("close", code => {
  const events = output.trim().split("\n").map(JSON.parse);
  const revisions = events.filter(event => event.type === "presented").map(event => event.commit.revision);
  if (code !== 0 || revisions.join(",") !== "1,2" || !events.some(event => event.type === "video_frame")) {
    console.error(output);
    process.exit(1);
  }
  console.log("Browser bridge passed: out-of-order resource, placement revisions, independent video");
});
