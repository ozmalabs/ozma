// SPDX-License-Identifier: AGPL-3.0-only

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Args;
use openh264::{decoder::Decoder, formats::YUVSource};
use ozma_client::{
    ClientEvent, NativeClientConfig, NativeSessionClient, NodeApiClient, VideoAssembler,
};
use ozma_protocol::{ChannelKind, SessionOffer};

#[derive(Args)]
pub struct SourceArgs {
    /// Session offer JSON file.
    #[arg(long)]
    offer: PathBuf,
    /// Node API URL for certificate discovery.
    #[arg(long)]
    node: String,
    /// Receive scene overlays as well as video.
    #[arg(long)]
    scene: bool,
}

pub async fn connect(args: &SourceArgs) -> Result<NativeSessionClient> {
    let offer: SessionOffer = serde_json::from_slice(&std::fs::read(&args.offer)?)
        .context("reading session offer JSON")?;
    let node = NodeApiClient::builder(&args.node).build()?;
    let mut config = NativeClientConfig::new();
    if args.scene {
        config.scene_cache_bytes = Some(4 * 1024 * 1024);
    }
    Ok(NativeSessionClient::connect_with_node_api(&offer, &node, config).await?)
}

/// Decode the current video background and re-emit it on scene changes.
/// Receivers get an opaque RGBA8 frame and the current scene snapshot.
pub async fn consume(
    client: &mut NativeSessionClient,
    mut send: impl FnMut(u32, u32, &[u8], Option<&ozma_protocol::scene::SceneReceiver>) -> Result<()>,
) -> Result<()> {
    let mut assembler = VideoAssembler::default();
    let mut decoder = Decoder::new()?;
    let mut rgba = Vec::new();
    let mut dimensions = None;
    let mut last_sequence = None;

    loop {
        match client.recv_event().await? {
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
                    last_sequence = None;
                    continue;
                }
                let Some(decoded) = decoder.decode(&frame.data)? else {
                    last_sequence = Some(frame.sequence);
                    continue;
                };
                let (width, height) = decoded.dimensions();
                anyhow::ensure!(width <= 4096 && height <= 2160, "output exceeds 4096×2160");
                rgba.resize(width * height * 4, 0);
                decoded.write_rgba8(&mut rgba);
                dimensions = Some((width as u32, height as u32));
                last_sequence = Some(frame.sequence);
            }
            ClientEvent::ScenePresented { .. } => {}
            ClientEvent::Error { error } => return Err(error.into()),
            ClientEvent::Bye { reason } => bail!("node ended session: {reason:?}"),
            ClientEvent::Closed { reason } => bail!("node connection closed: {reason:?}"),
            _ => continue,
        }
        if let Some((width, height)) = dimensions {
            send(width, height, &rgba, client.scene())?;
        }
    }
}

pub fn compose(
    revision: u64,
    width: u32,
    height: u32,
    video: &[u8],
    scene: Option<&ozma_protocol::scene::SceneReceiver>,
) -> Result<ozma_client::frame_output::ComposedFrame> {
    use ozma_client::frame_output::{ComposedFrame, DamageRect};
    let mut rgba = video.to_vec();
    if let Some(scene) = scene {
        ozma_client::scene_render::composite_over_video(scene, width, height, &mut rgba)?;
    }
    ComposedFrame::new(
        revision,
        width,
        height,
        rgba,
        vec![DamageRect::full(width, height)],
    )
    .context("invalid composed frame")
}
