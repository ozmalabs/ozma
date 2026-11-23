// SPDX-License-Identifier: AGPL-3.0-only
//! Produce a live HLS video feed from a native Ozma session.

mod output_support;

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use output_support::SourceArgs;
use ozma_client::frame_output::ComposedFrame;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{mpsc, watch},
};

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    source: SourceArgs,
    /// Private output directory for the live playlist and media segments.
    #[arg(long)]
    output: PathBuf,
    /// Frame rate of the output stream.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,
    /// Also convert the session's Opus packets into an AAC HLS audio track.
    #[arg(long)]
    audio: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tokio::fs::create_dir_all(&args.output).await?;
    let mut client = output_support::connect(&args.source).await?;
    if args.audio {
        anyhow::ensure!(
            client
                .session()
                .is_some_and(|session| session.spec.audio.enabled)
                && client
                    .permissions()
                    .contains(ozma_protocol::Permission::Audio),
            "the session offer must grant audio and enable audio capture"
        );
    }
    let (send, receive) = watch::channel(None);
    let (audio_send, audio_receive) = mpsc::channel(64);
    let mut revision = 0;
    let result = tokio::select! {
        result = output_support::consume_av(&mut client, |width, height, video, scene| {
            revision += 1;
            send.send_replace(Some(Arc::new(output_support::compose(revision, width, height, video, scene)?)));
            Ok(())
        }, |packet, pts, rate, channels| {
            if args.audio {
                audio_send.try_send((packet.to_vec(), pts, rate, channels))
                    .context("queuing Cast audio")?;
            }
            Ok(())
        }) => result,
        result = encode_hls(receive, args.output.clone(), args.fps) => result,
        result = encode_audio_hls(audio_receive, args.output.clone(), args.audio) => result,
        result = publish_master(&args.output, args.audio) => result,
        signal = tokio::signal::ctrl_c() => signal.map_err(Into::into),
    };
    let _ = client.disconnect().await;
    result
}

async fn publish_master(output: &std::path::Path, enabled: bool) -> Result<()> {
    if enabled {
        while !tokio::fs::try_exists(output.join("live.m3u8")).await?
            || !tokio::fs::try_exists(output.join("audio.m3u8")).await?
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let staging = output.join("master.m3u8.tmp");
        tokio::fs::write(
            &staging,
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Ozma\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,AUDIO=\"audio\"\nlive.m3u8\n",
        ).await?;
        tokio::fs::rename(staging, output.join("master.m3u8")).await?;
    }
    std::future::pending().await
}

/// Convert raw Opus access units into an Ogg Opus stream for FFmpeg, then
/// expose AAC segments as a separate HLS audio rendition.
async fn encode_audio_hls(
    mut packets: mpsc::Receiver<(Vec<u8>, u64, u32, u8)>,
    output: PathBuf,
    enabled: bool,
) -> Result<()> {
    if !enabled {
        return std::future::pending::<Result<()>>().await;
    }
    let mut ffmpeg = Command::new("ffmpeg")
        .current_dir(output)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "ogg",
            "-i",
            "pipe:0",
            "-vn",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_list_size",
            "4",
            "-hls_flags",
            "delete_segments+independent_segments+temp_file",
            "-hls_segment_filename",
            "audio%06d.ts",
            "audio.m3u8",
        ])
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg Opus-to-AAC HLS audio encoder")?;
    let mut stdin = ffmpeg.stdin.take().context("opening audio ffmpeg stdin")?;
    let serial = 0x4f5a4d41u32;
    let mut sequence = 0u32;
    stdin
        .write_all(&ogg_page(
            &mut sequence,
            serial,
            0,
            true,
            false,
            &opus_head(),
        ))
        .await?;
    stdin
        .write_all(&ogg_page(
            &mut sequence,
            serial,
            0,
            false,
            false,
            &opus_tags(),
        ))
        .await?;
    while let Some((packet, pts, rate, channels)) = packets.recv().await {
        if rate != 48_000 || channels != 2 {
            continue;
        }
        stdin
            .write_all(&ogg_page(
                &mut sequence,
                serial,
                pts.saturating_add(20_000).saturating_mul(u64::from(rate)) / 1_000_000,
                false,
                false,
                &packet,
            ))
            .await?;
    }
    let _ = stdin.shutdown().await;
    let status = ffmpeg.wait().await?;
    anyhow::ensure!(status.success(), "audio ffmpeg exited with {status}");
    Ok(())
}

fn opus_head() -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.extend_from_slice(&[1, 2, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]);
    v
}
fn opus_tags() -> Vec<u8> {
    let mut v = b"OpusTags".to_vec();
    v.extend_from_slice(&4u32.to_le_bytes());
    v.extend_from_slice(b"Ozma");
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}
fn ogg_page(
    sequence: &mut u32,
    serial: u32,
    granule: u64,
    bos: bool,
    continued: bool,
    packet: &[u8],
) -> Vec<u8> {
    let mut segments = Vec::new();
    let mut left = packet.len();
    while left >= 255 {
        segments.push(255);
        left -= 255;
    }
    segments.push(left as u8);
    let mut page = b"OggS".to_vec();
    page.extend_from_slice(&[0, 0]);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&sequence.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes());
    page.push(segments.len() as u8);
    page.extend_from_slice(&segments);
    page.extend_from_slice(packet);
    if bos {
        page[5] |= 2;
    }
    if continued {
        page[5] |= 1;
    }
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    *sequence += 1;
    page
}

fn ogg_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

async fn encode_hls(
    mut frames: watch::Receiver<Option<Arc<ComposedFrame>>>,
    output: PathBuf,
    fps: u32,
) -> Result<()> {
    let (width, height) = loop {
        if let Some(frame) = frames.borrow_and_update().as_ref() {
            break (frame.width, frame.height);
        }
        frames.changed().await?;
    };
    let size = format!("{width}x{height}");
    let rate = fps.to_string();
    let mut ffmpeg = Command::new("ffmpeg")
        .current_dir(output)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-video_size",
            &size,
            "-framerate",
            &rate,
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-maxrate",
            "2400k",
            "-bufsize",
            "2400k",
            "-pix_fmt",
            "yuv420p",
            "-g",
            &rate,
            "-sc_threshold",
            "0",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_list_size",
            "4",
            "-hls_flags",
            "delete_segments+independent_segments+temp_file",
            "-hls_segment_filename",
            "segment%06d.ts",
            "live.m3u8",
        ])
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg with libx264 and HLS support")?;
    let mut stdin = ffmpeg.stdin.take().context("opening ffmpeg stdin")?;
    let mut clock = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(fps)));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = clock.tick() => {
                let frame = frames.borrow_and_update().clone().context("output frame unavailable")?;
                anyhow::ensure!(
                    (frame.width, frame.height) == (width, height),
                    "output dimensions changed; restart the HLS stream"
                );
                stdin.write_all(&frame.rgba).await.context("feeding ffmpeg")?;
            }
            status = ffmpeg.wait() => anyhow::bail!("ffmpeg exited with {}", status?),
        }
    }
}
