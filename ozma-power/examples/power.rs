// SPDX-License-Identifier: AGPL-3.0-only
//! Query (and optionally start) a libvirt domain through the virsh backend.
//!
//! Usage:
//!
//! ```text
//! cargo run -p ozma-power --example power -- <domain> [on|off|reset|force-off]
//! ```

use std::str::FromStr;

use ozma_power::{LibvirtPower, PowerAction, PowerBackend};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let domain = arguments.next().unwrap_or_else(|| "ozma-vm".to_string());

    let mut power = LibvirtPower::new(domain);
    power.connect().await?;
    println!("{}", power.query_status().await?);

    if let Some(action) = arguments.next() {
        let action = PowerAction::from_str(&action)?;
        match action {
            PowerAction::Start => power.cont().await?,
            PowerAction::Shutdown => power.system_powerdown().await?,
            PowerAction::Reset => power.system_reset().await?,
            PowerAction::Stop => power.stop().await?,
        }
        println!("{action} requested");
    }

    Ok(())
}
