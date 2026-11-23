// SPDX-License-Identifier: AGPL-3.0-only
//! Export native scene image placements and encoded video to the loopback Xpra
//! development peer.

#[allow(dead_code)] // The structured adapter does not decode the video background.
mod output_support;

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use anyhow::{bail, Context, Result};
use clap::Parser;
use openh264::formats::YUVSource;
use output_support::SourceArgs;
use ozma_client::{
    xpra_peer::{XpraPeerServer, XpraSceneFrame},
    ClientEvent, VideoAssembler,
};
use ozma_protocol::{
    xpra_output::{VideoCodec, XpraVideoFrame, FULL_DISPLAY_WINDOW_ID},
    ChannelKind,
};
use tokio::sync::{mpsc, watch};

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    source: SourceArgs,
    /// Local Xpra endpoint for a modern view-only peer.
    #[arg(long, default_value = "127.0.0.1:14500")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.listen.ip().is_loopback(),
        "the Xpra prototype binds to loopback"
    );
    let server = XpraPeerServer::bind(args.listen).await?;
    let mut client = output_support::connect(&args.source).await?;
    let (publish, mut receive) = watch::channel(None);
    let (video, mut video_receive) = mpsc::channel(8);
    eprintln!("Xpra scene output listening on {}", server.local_addr()?);

    let result = tokio::select! {
        result = async {
            if client.scene().is_none() {
                bail!("scene profile required; use --scene and a scene-capable node");
            }
            let mut assembler = VideoAssembler::default();
            let mut decoder = openh264::decoder::Decoder::new()?;
            let mut dimensions = None;
            let mut started = false;
            let mut last_sequence = None;
            loop {
                match client.recv_event().await? {
                    ClientEvent::ScenePresented { .. } => {
                        let scene = client.scene().context("missing native scene")?;
                        let commit = scene.presented().clone();
                        let mut resources = BTreeMap::new();
                        for item in &commit.images {
                            if let std::collections::btree_map::Entry::Vacant(entry) =
                                resources.entry(item.resource_id)
                            {
                                let png = scene.image(item.resource_id).context("scene image missing")?;
                                entry.insert(png.to_vec());
                            }
                        }
                        publish.send_replace(Some(Arc::new(XpraSceneFrame { commit, resources })));
                    }
                    ClientEvent::Media(packet) if packet.kind == ChannelKind::Video => {
                        let Some(frame) = assembler.push(&packet.payload)? else {
                            continue;
                        };
                        if frame.track_id != 0 {
                            continue;
                        }
                        if !frame.keyframe
                            && last_sequence.and_then(|last: u64| last.checked_add(1))
                                != Some(frame.sequence)
                        {
                            started = false;
                            last_sequence = None;
                            continue;
                        }
                        if frame.keyframe {
                            let Some(decoded) = decoder.decode(&frame.data)? else {
                                continue;
                            };
                            let (width, height) = decoded.dimensions();
                            anyhow::ensure!(
                                width <= 16_384 && height <= 16_384,
                                "Xpra video frame exceeds 16384 pixels"
                            );
                            dimensions = Some((width as u32, height as u32));
                            started = true;
                        }
                        if !started {
                            continue;
                        }
                        let Some((width, height)) = dimensions else {
                            continue;
                        };
                        last_sequence = Some(frame.sequence);
                        // The Xpra peer is a latest-state viewer. Drop an
                        // access unit if it cannot keep up; the encoder waits
                        // for the next keyframe before restarting the stream.
                        if video
                            .try_send(XpraVideoFrame {
                                node_id: FULL_DISPLAY_WINDOW_ID,
                                codec: VideoCodec::H264,
                                width,
                                height,
                                keyframe: frame.keyframe,
                                data: frame.data,
                            })
                            .is_err()
                        {
                            started = false;
                        }
                    }
                    ClientEvent::Error { error } => return Err(error.into()),
                    ClientEvent::Closed { reason } => bail!("node connection closed: {reason:?}"),
                    ClientEvent::Bye { reason } => bail!("node ended session: {reason:?}"),
                    _ => {}
                }
            }
        } => result,
        result = async {
            while receive.borrow().is_none() {
                receive.changed().await?;
            }
            loop {
                if let Err(error) = server
                    .serve_with_video(receive.clone(), &mut video_receive)
                    .await
                {
                    eprintln!("Xpra peer ended: {error}");
                }
            }
        } => result,
        signal = tokio::signal::ctrl_c() => signal.map_err(Into::into),
    };
    let _ = client.disconnect().await;
    result
}
