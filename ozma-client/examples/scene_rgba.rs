// SPDX-License-Identifier: AGPL-3.0-only
//! Export a composed native Ozma session to an external display backend.

mod output_support;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use output_support::SourceArgs;
use ozma_client::frame_output::ComposedFrame;
use tokio::{io::AsyncWriteExt, sync::watch};

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    source: SourceArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut client = output_support::connect(&args.source).await?;
    let (send, receive) = watch::channel(None);
    let mut revision = 0;
    let result = tokio::select! {
        result = output_support::consume_av(&mut client, |width, height, video, scene| {
            revision += 1;
            let frame = output_support::compose(revision, width, height, video, scene)?;
            send.send_replace(Some(Arc::new(frame)));
            Ok(())
        }, |_, _, _, _| Ok(())) => result,
        result = write_frames(receive) => result,
        signal = tokio::signal::ctrl_c() => signal.map_err(Into::into),
    };
    let _ = client.disconnect().await;
    result
}

async fn write_frames(mut frames: watch::Receiver<Option<Arc<ComposedFrame>>>) -> Result<()> {
    let mut output = tokio::io::stdout();
    let mut shape = None;
    while frames.changed().await.is_ok() {
        let frame = frames
            .borrow_and_update()
            .clone()
            .context("no composed frame")?;
        if let Some((width, height)) = shape {
            anyhow::ensure!(
                (width, height) == (frame.width, frame.height),
                "display size changed; restart the output"
            );
        } else {
            shape = Some((frame.width, frame.height));
        }
        let mut header = [0; 16];
        header[..4].copy_from_slice(b"OZFR");
        header[4..8].copy_from_slice(&frame.width.to_be_bytes());
        header[8..12].copy_from_slice(&frame.height.to_be_bytes());
        header[12..].copy_from_slice(&(frame.width * 4).to_be_bytes());
        let write = async {
            output.write_all(&header).await?;
            output.write_all(&frame.rgba).await?;
            output.flush().await
        };
        if let Err(error) = write.await {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }
    }
    Ok(())
}
