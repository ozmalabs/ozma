// SPDX-License-Identifier: AGPL-3.0-only
//! Dump a 128-byte EDID base block for a virtual display.
//!
//! Raw bytes are the default so they can be fed straight to a libevdi spike
//! harness:
//!
//! ```sh
//! cargo run -p ozma-display --example dump_edid -- \
//!   laptop-1 "Remote laptop" 1920 1200 60 > /tmp/edid.bin
//! ```
//!
//! Pass `--hex` for a human-readable annotation instead.

use std::io::{self, Write};
use std::process::ExitCode;

use ozma_display::{build_edid, DisplayMode, VirtualDisplaySpec};

fn usage() -> ExitCode {
    eprintln!(
        "usage: dump_edid <display-id> <name> <width> <height> <refresh-hz> [--hex]\n\
         \n\
         Writes the 128-byte EDID base block to stdout (binary by default)."
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 5 && args.len() != 6 {
        return usage();
    }
    let hex = args.len() == 6 && args[5] == "--hex";
    if args.len() == 6 && !hex {
        return usage();
    }

    let parse = |value: &str, label: &str| -> Result<u32, ExitCode> {
        value.parse::<u32>().map_err(|_| {
            eprintln!("invalid {label}: {value}");
            usage()
        })
    };

    let width = match parse(&args[2], "width") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let height = match parse(&args[3], "height") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let refresh = match parse(&args[4], "refresh") {
        Ok(value) => value,
        Err(code) => return code,
    };

    let mode = DisplayMode::new(width, height, refresh);
    let spec = VirtualDisplaySpec::new(args[0].as_str(), args[1].as_str(), (300, 190), mode);
    let edid = match build_edid(&spec) {
        Ok(edid) => edid,
        Err(error) => {
            eprintln!("cannot build EDID: {error}");
            return ExitCode::from(1);
        }
    };

    if hex {
        for (index, byte) in edid.as_bytes().iter().enumerate() {
            if index % 16 == 0 {
                if index != 0 {
                    println!();
                }
                print!("{index:04x}: ");
            }
            print!("{byte:02x} ");
        }
        println!();
        println!(
            "length={} checksum=0x{:02x} valid={} preferred={}",
            edid.as_bytes().len(),
            edid.checksum(),
            edid.checksum_valid(),
            edid.preferred_mode()
        );
        return ExitCode::SUCCESS;
    }

    let mut stdout = io::stdout().lock();
    match stdout.write_all(edid.as_bytes()).and_then(|_| stdout.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("write failed: {error}");
            ExitCode::from(1)
        }
    }
}
