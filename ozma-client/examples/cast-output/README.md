# Ozma Cast output fixture

Run the HLS output example with a native session offer and the node API URL.
Append `--scene` to include overlays. Append `--audio` when the offer enables
audio capture and grants audio access.

```sh
cargo run -p ozma-client --example cast_hls -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene \
  --output ./private-cast-media
```

The example uses FFmpeg with libx264 to write `live.m3u8`. With `--audio`, it
also writes an AAC rendition and publishes `master.m3u8` when both tracks
are ready. Use an empty private output directory for each session.

Start the loopback media origin in another terminal.

```sh
cargo run -p ozma-client --example cast_media_server -- ./private-cast-media
```

The development origin prints URLs with a five-minute playback grant.
Reverse-proxy `/cast/` through a receiver-reachable HTTPS origin, preserving the
path and forwarding GET, HEAD, OPTIONS, and DELETE. Keep grant-bearing paths
out of proxy logs. Use the printed `live.m3u8` path for video only or
`master.m3u8` for audio and video, with the HTTPS origin in place of loopback.
Manifest and segment requests use the same grant.

Serve this sender directory over HTTPS. Open `index.html` in a Cast-capable
browser, enter the grant-bearing HTTPS playlist URL, and choose a receiver.
**Stop casting** ends the receiver session and revokes the development grant.
The media origin also expires it automatically. Production integration still
needs controller-issued grants and lifecycle handling.

Physical receiver playback and audio sync need validation. A registered
Custom Web Receiver would allow retained scene overlays or custom receiver
behavior. [Google Cast receiver options](https://developers.google.com/cast/docs/web_receiver) ·
[Google Cast sender integration](https://developers.google.com/cast/docs/web_sender/integrate)
