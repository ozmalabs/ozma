# Ozma Cast output fixture

Run the HLS output example with a native session offer and the node API URL.
Append `--scene` to include overlays from a scene-capable node.

```sh
cargo run -p ozma-client --example cast_hls -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene \
  --output ./private-cast-media
```

The example uses FFmpeg with libx264 to write `live.m3u8` and a rolling set of
H.264 MPEG-TS segments. It produces video only. Host the playlist and segments
with a receiver-reachable HTTPS endpoint that provides Cast-compatible CORS.
The output directory contains session pixels. Use a private media origin and
authorize access to both the manifest and each segment before sharing it.
A token on the manifest URL alone does not authorize relative segment URLs.

Serve this sender directory over HTTPS. Open `index.html` in a Cast-capable
browser, enter the authorized playlist URL, and choose a receiver. **Stop
casting** ends the receiver session. Playback grants and revocation belong to
the media server. The example does not issue or revoke them.

Use a registered Custom Web Receiver for retained scene overlays or custom
receiver authentication. [Google Cast receiver options](https://developers.google.com/cast/docs/web_receiver) ·
[Google Cast sender integration](https://developers.google.com/cast/docs/web_sender/integrate)
