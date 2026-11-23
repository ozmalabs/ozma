// SPDX-License-Identifier: AGPL-3.0-only
//! CPU reference renderer for image overlays on a decoded video frame.

use std::io::Cursor;

use ozma_protocol::scene::{ImagePlacement, SceneReceiver};

const MAX_DECODED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SceneRenderError {
    #[error("frame dimensions do not match its RGBA buffer")]
    InvalidFrame,
    #[error("scene references missing image resource {0}")]
    MissingImage(u64),
    #[error("decoded image exceeds the reference renderer's limits")]
    ImageTooLarge,
    #[error("could not decode PNG resource: {0}")]
    Png(#[from] png::DecodingError),
}

/// Composite the currently presented overlays over an opaque RGBA8 video frame.
/// Validate and decode all visible resources before changing the frame.
pub fn composite_over_video(
    scene: &SceneReceiver,
    width: u32,
    height: u32,
    rgba: &mut [u8],
) -> Result<(), SceneRenderError> {
    if byte_len(width, height) != Some(rgba.len()) {
        return Err(SceneRenderError::InvalidFrame);
    }

    let placements = &scene.presented().images;
    let mut images = std::collections::HashMap::new();
    let mut remaining = MAX_DECODED_BYTES;
    for placement in placements {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            images.entry(placement.resource_id)
        {
            let png = scene
                .image(placement.resource_id)
                .ok_or(SceneRenderError::MissingImage(placement.resource_id))?;
            let image = decode(png, remaining)?;
            remaining -= image.1.len();
            entry.insert(image);
        }
    }

    for placement in placements {
        let (info, pixels) = &images[&placement.resource_id];
        draw(placement, info, pixels, width, height, rgba);
    }
    Ok(())
}

fn byte_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

fn decode(png: &[u8], budget: usize) -> Result<(png::OutputInfo, Vec<u8>), SceneRenderError> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_limits(png::Limits {
        bytes: MAX_DECODED_BYTES,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let length = reader.output_buffer_size();
    if length > budget {
        return Err(SceneRenderError::ImageTooLarge);
    }
    let mut pixels = vec![0; length];
    let info = reader.next_frame(&mut pixels)?;
    Ok((info, pixels))
}

fn draw(
    placement: &ImagePlacement,
    info: &png::OutputInfo,
    pixels: &[u8],
    width: u32,
    height: u32,
    rgba: &mut [u8],
) {
    let left = i64::from(placement.x);
    let top = i64::from(placement.y);
    let right = (left + i64::from(placement.width)).min(i64::from(width));
    let bottom = (top + i64::from(placement.height)).min(i64::from(height));
    let channels = info.color_type.samples();
    for y in top.max(0)..bottom {
        let source_y = (y - top) as u64 * u64::from(info.height) / u64::from(placement.height);
        for x in left.max(0)..right {
            let source_x = (x - left) as u64 * u64::from(info.width) / u64::from(placement.width);
            let source = source_y as usize * info.line_size + source_x as usize * channels;
            let target = (y as usize * width as usize + x as usize) * 4;
            let (rgb, alpha) = match &pixels[source..source + channels] {
                [gray] => ([*gray; 3], 255),
                [gray, alpha] => ([*gray; 3], *alpha),
                [r, g, b] => ([*r, *g, *b], 255),
                [r, g, b, alpha] => ([*r, *g, *b], *alpha),
                _ => unreachable!("PNG is expanded to 8-bit colour"),
            };
            let alpha = u32::from(alpha);
            for (dst, src) in rgba[target..target + 3].iter_mut().zip(rgb) {
                *dst =
                    ((u32::from(src) * alpha + u32::from(*dst) * (255 - alpha) + 127) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::scene::{
        CommitStatus, ImageResource, SceneCommit, SceneSnapshot, VideoSurface,
    };

    #[test]
    fn cached_image_moves_over_video_and_can_be_removed() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[200, 0, 0, 128])
                .unwrap();
        }

        let mut scene = SceneReceiver::new(
            SceneSnapshot {
                revision: 0,
                full_display: VideoSurface { id: 1, channel: 2 },
            },
            encoded.len(),
        );
        let asset = ImageResource {
            id: 7,
            png: &encoded,
        }
        .encode();
        assert_eq!(scene.receive_resource(&asset), Ok(false));

        let video = [0, 0, 200, 255, 0, 0, 100, 255];
        let placements = |x| {
            vec![ImagePlacement {
                node_id: 1,
                resource_id: 7,
                x,
                y: 0,
                width: 2,
                height: 1,
            }]
        };
        for (revision, images, expected) in [
            (1, placements(-1), [100, 0, 100, 255, 0, 0, 100, 255]),
            (2, placements(1), [0, 0, 200, 255, 100, 0, 50, 255]),
            (3, Vec::new(), video),
        ] {
            assert_eq!(
                scene.receive_commit(SceneCommit { revision, images }),
                CommitStatus::Presented
            );
            let mut frame = video;
            composite_over_video(&scene, 2, 1, &mut frame).unwrap();
            assert_eq!(frame, expected);
        }
    }
}
