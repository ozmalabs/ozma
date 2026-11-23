// SPDX-License-Identifier: AGPL-3.0-only
//! Produce a live HLS video feed from a native Ozma session.

mod output_support;

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use output_support::SourceArgs;
use ozma_client::frame_output::ComposedFrame;
use tokio::{io::AsyncWriteExt, process::Command, sync::watch};

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tokio::fs::create_dir_all(&args.output).await?;
    let mut client = output_support::connect(&args.source).await?;
    let (send, receive) = watch::channel(None);
    let mut revision = 0;
    let result = tokio::select! {
        result = output_support::consume(&mut client, |width, height, video, scene| {
            revision += 1;
            send.send_replace(Some(Arc::new(output_support::compose(revision, width, height, video, scene)?)));
            Ok(())
        }) => result,
        result = encode_hls(receive, args.output, args.fps) => result,
        signal = tokio::signal::ctrl_c() => signal.map_err(Into::into),
    };
    let _ = client.close().await;
    result
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
