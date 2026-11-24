// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Software source inventory for the Rust node.
//!
//! Sources are ecosystems that own installed software: native package managers,
//! Steam, and the Microsoft Store. They are inventory inputs, not Ozma
//! distribution targets. Steam and Store records are reported as externally
//! managed; this module never edits their application files.
//!
//! The controller consumes the same JSON shape at
//! `POST /api/v1/software/hosts/{host_id}/inventory`.

pub mod microsoft_store;
pub mod model;
pub mod native;
pub mod report;
pub mod steam;

pub use model::{deduplicate, SoftwareRecord, SoftwareScope, UpdateState};
pub use report::{report_inventory, InventoryReporter, ReportError};

use std::path::PathBuf;

/// Injected Microsoft Store PowerShell runner, primarily for tests.
pub type StoreRunner<'a> = &'a dyn Fn(&str) -> Result<String, String>;

/// Collect every available source and deduplicate by `(source, source_id)`.
///
/// A source failure is logged and skipped; it never prevents the other sources
/// from being reported. Callers that need to block a UI thread should run this
/// on a blocking task because native package managers and PowerShell are
/// synchronous processes.
pub fn collect_inventory() -> Vec<SoftwareRecord> {
    collect_inventory_with(None, "", None)
}

/// Collect inventory with explicit Steam roots and an optional Store runner.
///
/// `store_runner` receives the PowerShell script and returns its stdout. This is
/// primarily for tests; production callers pass `None` and let the collector
/// use the platform default.
pub fn collect_inventory_with(
    steam_roots: Option<&[PathBuf]>,
    steam_user_id: &str,
    store_runner: Option<StoreRunner<'_>>,
) -> Vec<SoftwareRecord> {
    let mut records = native::collect();

    match steam::collect(steam_roots, steam_user_id) {
        Ok(mut steam_records) => records.append(&mut steam_records),
        Err(error) => {
            tracing::warn!(error = %error, "Steam software inventory failed");
        }
    }

    let store_records = match store_runner {
        Some(runner) => microsoft_store::collect_with_runner(runner),
        None => microsoft_store::collect(),
    };
    match store_records {
        Ok(mut store_records) => records.append(&mut store_records),
        Err(error) => {
            tracing::warn!(error = %error, "Microsoft Store software inventory failed");
        }
    }

    deduplicate(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_with_injected_store_runner_merges_sources() {
        let payload = r#"{
            "packages": [{
                "Name": "Store App",
                "PackageFamilyName": "Store.App_abc",
                "Version": "1.2.3",
                "Architecture": "X64"
            }],
            "provisioned": []
        }"#;
        let runner = |_script: &str| Ok(payload.to_string());

        let records = collect_inventory_with(Some(&[]), "", Some(&runner));

        assert!(records.iter().any(|record| {
            record.source == "microsoft_store" && record.source_id == "Store.App_abc"
        }));
        // Every record must carry the controller's required fields.
        for record in records {
            assert!(!record.name.is_empty());
            assert!(!record.source.is_empty());
        }
    }
}
