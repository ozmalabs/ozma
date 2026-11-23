// SPDX-License-Identifier: AGPL-3.0-only
//! Consume the composed `OZFR` stream and write desktop frames for an RDP
//! bridge.  This is intentionally a bridge seam, not an RDP wire server yet.
//!
//! Example:
//!   ozma-client-scene-rgba | cargo run -p ozma-client --example rdp_output -- --output-dir /tmp/ozma-rdp

use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::Parser;
use ozma_client::rdp::{RdpFrameLayout, RdpFrameReader};

#[derive(Debug, Parser)]
struct Args {
    /// Directory receiving numbered 32-bit BMP desktop frames.
    #[arg(long)]
    output_dir: PathBuf,
    /// Stop after this many frames (zero means until EOF).
    #[arg(long, default_value_t = 0)]
    max_frames: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    fs::create_dir_all(&args.output_dir).context("creating RDP bridge output directory")?;
    let stdin = io::stdin();
    let mut reader = RdpFrameReader::new(stdin.lock());
    let mut count = 0_u64;
    while let Some((index, layout, rgba)) = reader.next_frame().context("reading OZFR frame")? {
        let path = args.output_dir.join(format!("frame-{index:08}.bmp"));
        write_bmp(&path, layout, &rgba).with_context(|| format!("writing {}", path.display()))?;
        count += 1;
        if args.max_frames != 0 && count >= args.max_frames {
            break;
        }
    }
    eprintln!("RDP frame bridge wrote {count} frame(s)");
    Ok(())
}

fn write_bmp(path: &PathBuf, layout: RdpFrameLayout, rgba: &[u8]) -> Result<()> {
    let row_bytes = layout.width as usize * 4;
    let image_bytes = row_bytes * layout.height as usize;
    let file_size = 54 + image_bytes;
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(b"BM")?;
    out.write_all(&(file_size as u32).to_le_bytes())?;
    out.write_all(&[0; 4])?;
    out.write_all(&(54_u32).to_le_bytes())?;
    out.write_all(&(40_u32).to_le_bytes())?;
    out.write_all(&(layout.width as i32).to_le_bytes())?;
    out.write_all(&(layout.height as i32).to_le_bytes())?;
    out.write_all(&(1_u16).to_le_bytes())?;
    out.write_all(&(32_u16).to_le_bytes())?;
    out.write_all(&[0; 4])?;
    out.write_all(&(image_bytes as u32).to_le_bytes())?;
    out.write_all(&[0; 16])?;
    for y in (0..layout.height as usize).rev() {
        let row = &rgba[y * layout.stride as usize..y * layout.stride as usize + row_bytes];
        for pixel in row.chunks_exact(4) {
            out.write_all(&[pixel[2], pixel[1], pixel[0], pixel[3]])?;
        }
    }
    out.flush()?;
    Ok(())
}
