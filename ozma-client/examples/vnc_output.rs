// SPDX-License-Identifier: AGPL-3.0-only
//! Display a native Ozma session through a loopback VNC endpoint.

mod output_support;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use clap::Parser;
use output_support::SourceArgs;
use ozma_client::vnc::{RfbFrame, RfbServer};
use tokio::sync::watch;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    source: SourceArgs,
    /// Local VNC endpoint.
    #[arg(long, default_value = "127.0.0.1:5900")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let server = RfbServer::bind(args.listen).await?;
    let mut client = output_support::connect(&args.source).await?;
    let (send, mut receive) = watch::channel(None);
    let mut revision = 0;
    eprintln!("VNC listening on {}", server.local_addr()?);

    let result = tokio::select! {
        result = output_support::consume(&mut client, |width, height, video, scene| {
            revision += 1;
            let frame = output_support::compose(revision, width, height, video, scene)?;
            let framebuffer = RfbFrame::new(frame.width.try_into()?, frame.height.try_into()?, frame.rgba)?;
            send.send_replace(Some(Arc::new(framebuffer)));
            Ok(())
        }) => result,
        result = async {
            while receive.borrow().is_none() {
                receive.changed().await?;
            }
            loop {
                if let Err(error) = server.accept_frames(receive.clone()).await {
                    eprintln!("VNC connection ended: {error}");
                }
            }
        } => result,
        signal = tokio::signal::ctrl_c() => signal.map_err(Into::into),
    };
    let _ = client.close().await;
    result
}
