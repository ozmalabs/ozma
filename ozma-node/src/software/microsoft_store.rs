// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Microsoft Store / MSIX software inventory source.
//!
//! Store packages are externally managed from Ozma's point of view. This module
//! only reads package metadata through PowerShell; it never installs, updates,
//! or patches Store packages in place.
//!
//! Collection is split into a PowerShell command builder, an injectable runner,
//! and a JSON parser so the behaviour can be tested on any platform without
//! Windows or the Microsoft Store.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::model::{SoftwareRecord, SoftwareScope, UpdateState};

/// Stable source name used on every record from this adapter.
const SOURCE_NAME: &str = "microsoft_store";
/// Stable owner name for externally managed Store packages.
const MANAGED_BY: &str = "microsoft_store";

/// Errors produced while collecting or parsing Microsoft Store packages.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The PowerShell output was not valid JSON.
    #[error("invalid Microsoft Store JSON payload: {0}")]
    InvalidJson(String),
    /// The JSON root was neither an object nor an array.
    #[error("Microsoft Store JSON payload must be an object or a list")]
    InvalidRoot,
    /// An injected runner returned an error.
    #[error("Microsoft Store PowerShell runner failed: {0}")]
    Runner(String),
    /// The PowerShell process could not run or exited unsuccessfully.
    #[error("Microsoft Store PowerShell command failed: {0}")]
    Command(String),
}

/// Fields selected from `Get-AppxPackage` and `Get-AppxProvisionedPackage`.
///
/// The list is a constant so every script has the same object shape regardless
/// of the Windows PowerShell version or the package being enumerated.
const PACKAGE_SELECT_FIELDS: &[(&str, &str)] = &[
    ("Name", "$_.Name"),
    ("PackageFamilyName", "$_.PackageFamilyName"),
    ("PackageFullName", "$_.PackageFullName"),
    ("Publisher", "$_.Publisher"),
    ("Version", "$_.Version"),
    ("Architecture", "$_.Architecture"),
    ("InstallLocation", "$_.InstallLocation"),
    ("IsFramework", "$_.IsFramework"),
    ("SignatureKind", "$_.SignatureKind"),
    ("Status", "$_.Status"),
    ("UserProfile", "$_.UserProfile"),
    ("UserSid", "$_.UserSid"),
];

const PROVISIONED_SELECT_FIELDS: &[(&str, &str)] = &[
    (
        "Name",
        "if ($_.DisplayName) { $_.DisplayName } else { $_.PackageName }",
    ),
    (
        "PackageFamilyName",
        "if ($_.PackageFamilyName) { $_.PackageFamilyName } else { \"$($_.PackageName)_$($_.PublisherId)\" }",
    ),
    ("PackageFullName", "$_.PackageFullName"),
    ("Publisher", "$_.PublisherId"),
    ("Version", "$_.Version"),
    ("Architecture", "$_.Architecture"),
    ("InstallLocation", "$_.InstallLocation"),
    ("IsFramework", "$false"),
    ("SignatureKind", "$null"),
    ("Status", "$null"),
    ("UserProfile", "$null"),
    ("UserSid", "$null"),
];

/// Build the PowerShell script used to enumerate Appx/MSIX packages.
///
/// The script always emits one JSON object with `packages` and `provisioned`
/// arrays, and ends with `ConvertTo-Json -Depth 6 -Compress`. When `all_users`
/// is false the script only enumerates the current user's packages. When
/// `provisioned` is false the `provisioned` array is emitted empty instead of
/// querying `Get-AppxProvisionedPackage`.
pub fn build_appx_powershell(all_users: bool, provisioned: bool) -> String {
    let appx_cmdlet = if all_users {
        "Get-AppxPackage -AllUsers"
    } else {
        "Get-AppxPackage"
    };

    let mut script = String::new();
    script.push_str("$ErrorActionPreference = 'Stop'\n");
    // ConvertTo-Json otherwise uses the console code page and can mangle
    // non-ASCII package names.
    script.push_str("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n");
    script.push_str("$result = [ordered]@{}\n");
    script.push_str("$result['packages'] = @(\n");
    script.push_str("    ");
    script.push_str(appx_cmdlet);
    script.push_str(" | Select-Object ");
    script.push_str(&select_object_fields(PACKAGE_SELECT_FIELDS));
    script.push_str("\n)\n");

    if provisioned {
        script.push_str("$result['provisioned'] = @(\n");
        script.push_str("    Get-AppxProvisionedPackage -Online | Select-Object ");
        script.push_str(&select_object_fields(PROVISIONED_SELECT_FIELDS));
        script.push_str("\n)\n");
    } else {
        script.push_str("$result['provisioned'] = @()\n");
    }

    script.push_str("$result | ConvertTo-Json -Depth 6 -Compress");
    script
}

/// Render a PowerShell `Select-Object` property list with a stable ordering.
fn select_object_fields(fields: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (index, (name, expression)) in fields.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n        ");
        }
        out.push('@');
        out.push('{');
        out.push_str("Name='");
        out.push_str(name);
        out.push_str("';Expression={");
        out.push_str(expression);
        out.push_str("}}");
    }
    out
}

/// Parse Appx/MSIX JSON produced by [`build_appx_powershell`].
///
/// Accepts a wrapper object such as `{"packages": [...], "provisioned": [...]}`,
/// a bare list, or a single package object. Wrapper keys and package fields are
/// matched case-insensitively. Missing fields and non-string numbers are
/// tolerated. Entries under `provisioned` always use
/// [`SoftwareScope::Provisioned`]; every other entry uses `scope`.
///
/// Invalid JSON and non-object/non-array roots return a [`StoreError`].
pub fn parse_appx_packages(
    payload: &str,
    scope: SoftwareScope,
) -> Result<Vec<SoftwareRecord>, StoreError> {
    let payload = payload.trim_start_matches('\u{feff}');
    if payload.trim().is_empty() {
        return Ok(Vec::new());
    }

    let root: Value = serde_json::from_str(payload)
        .map_err(|error| StoreError::InvalidJson(error.to_string()))?;

    if !root.is_object() && !root.is_array() {
        return Err(StoreError::InvalidRoot);
    }

    let mut records = Vec::new();
    collect_entries(&root, scope, &mut records);
    Ok(deduplicate_records(records))
}

/// Collect installed and provisioned Store packages with an injected runner.
///
/// The runner receives the PowerShell script and returns its standard output.
/// This is the testable path and works on any platform.
pub fn collect_with_runner(
    runner: &dyn Fn(&str) -> Result<String, String>,
) -> Result<Vec<SoftwareRecord>, StoreError> {
    let script = build_appx_powershell(true, true);
    let payload = runner(&script).map_err(StoreError::Runner)?;
    parse_appx_packages(&payload, SoftwareScope::User)
}

/// Collect Store packages using the platform default runner.
///
/// On non-Windows hosts this returns an empty inventory without invoking
/// PowerShell. On Windows it runs `powershell -NoProfile -NonInteractive
/// -Command <script>` and parses stdout. It never mutates packages.
#[cfg(not(windows))]
pub fn collect() -> Result<Vec<SoftwareRecord>, StoreError> {
    Ok(Vec::new())
}

/// Collect Store packages using the platform default runner.
///
/// On non-Windows hosts this returns an empty inventory without invoking
/// PowerShell. On Windows it first tries all-user and provisioned packages
/// (which usually need elevation). If that fails, it retries for the current
/// user so a non-elevated node still reports the packages it can see. It never
/// mutates packages.
#[cfg(windows)]
pub fn collect() -> Result<Vec<SoftwareRecord>, StoreError> {
    match run_powershell(&build_appx_powershell(true, true))
        .and_then(|payload| parse_appx_packages(&payload, SoftwareScope::User))
    {
        Ok(records) => Ok(records),
        Err(all_users_error) => run_powershell(&build_appx_powershell(false, false))
            .and_then(|payload| parse_appx_packages(&payload, SoftwareScope::User))
            .map_err(|current_user_error| {
                StoreError::Command(format!(
                    "all-users query failed ({all_users_error}); \
                         current-user fallback failed ({current_user_error})"
                ))
            }),
    }
}

#[cfg(windows)]
fn run_powershell(script: &str) -> Result<String, StoreError> {
    use std::os::windows::process::CommandExt;

    // CREATE_NO_WINDOW keeps a service/GUI wrapper from flashing a console.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| StoreError::Command(format!("failed to start powershell: {error}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(StoreError::Command(format!(
            "exit status {}: {detail}",
            output.status
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Walk a decoded JSON value, appending records for package-shaped objects.
fn collect_entries(value: &Value, scope: SoftwareScope, out: &mut Vec<SoftwareRecord>) {
    match value {
        Value::Array(items) => {
            for item in items {
                if let Value::Object(entry) = item {
                    if let Some(record) = record_from_entry(entry, scope) {
                        out.push(record);
                    }
                }
            }
        }
        Value::Object(map) => {
            let mut found_section = false;

            for (raw_key, nested) in map {
                let key = normalize_key(raw_key);
                let is_container = nested.is_array() || nested.is_object();

                if is_container && is_provisioned_section(&key) {
                    found_section = true;
                    collect_entries(nested, SoftwareScope::Provisioned, out);
                } else if is_container && is_package_section(&key) {
                    found_section = true;
                    collect_entries(nested, scope, out);
                }
            }

            // A plain package object is not wrapped in a section.
            if !found_section {
                if let Some(record) = record_from_entry(map, scope) {
                    out.push(record);
                }
            }
        }
        _ => {}
    }
}

fn is_package_section(key: &str) -> bool {
    matches!(
        key,
        "packages" | "appx" | "appxpackages" | "value" | "items" | "results" | "data"
    )
}

fn is_provisioned_section(key: &str) -> bool {
    matches!(key, "provisioned" | "provisionedpackages")
}

/// Convert one Appx package object into a normalized record.
///
/// Returns `None` when the object has no usable name. Framework packages are
/// intentionally kept; the caller may filter them when presenting inventory.
fn record_from_entry(entry: &Map<String, Value>, scope: SoftwareScope) -> Option<SoftwareRecord> {
    let fields: BTreeMap<String, &Value> = entry
        .iter()
        .map(|(key, value)| (normalize_key(key), value))
        .collect();

    let package_family_name = text_field(&fields, &["PackageFamilyName"]);
    let package_full_name = text_field(&fields, &["PackageFullName"]);

    let name = [
        text_field(&fields, &["Name"]),
        text_field(&fields, &["DisplayName"]),
        text_field(&fields, &["PackageName"]),
        package_family_name.clone(),
        package_full_name.clone(),
    ]
    .into_iter()
    .find(|candidate| !candidate.is_empty())?;

    let source_id = if package_family_name.is_empty() {
        name.clone()
    } else {
        package_family_name.clone()
    };

    let mut record = SoftwareRecord::new(name, SOURCE_NAME);
    record.source_id = source_id;
    record.version = text_field(&fields, &["Version"]);
    record.install_path = text_field(&fields, &["InstallLocation"]);
    record.scope = scope;
    record.user_id = text_field(&fields, &["UserSid", "UserProfile"]);
    record.architecture = lookup(&fields, &["Architecture"])
        .map(normalize_architecture)
        .unwrap_or_default();
    record.managed_by = MANAGED_BY.to_string();
    record.update_state = UpdateState::ExternalManaged;

    record
        .identifiers
        .insert("package_family_name".to_string(), package_family_name);
    record
        .identifiers
        .insert("package_full_name".to_string(), package_full_name);

    record.metadata.insert(
        "publisher".to_string(),
        Value::String(text_field(&fields, &["Publisher", "PublisherId"])),
    );
    record.metadata.insert(
        "is_framework".to_string(),
        Value::Bool(bool_field(&fields, &["IsFramework"])),
    );
    record.metadata.insert(
        "signature_kind".to_string(),
        Value::String(text_field(&fields, &["SignatureKind"])),
    );
    record.metadata.insert(
        "status".to_string(),
        Value::String(text_field(&fields, &["Status"])),
    );

    Some(record)
}

/// Look up a field by any of its case-insensitive aliases.
fn lookup<'a>(fields: &BTreeMap<String, &'a Value>, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| {
        fields
            .get(&normalize_key(name))
            .copied()
            .filter(|value| !value.is_null())
    })
}

fn text_field(fields: &BTreeMap<String, &Value>, names: &[&str]) -> String {
    lookup(fields, names).map(value_to_text).unwrap_or_default()
}

fn bool_field(fields: &BTreeMap<String, &Value>, names: &[&str]) -> bool {
    match lookup(fields, names) {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes"
        ),
        _ => false,
    }
}

/// Convert a JSON scalar to text, tolerating numbers and booleans.
fn value_to_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.trim().to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// Normalize architecture strings and the numeric `ProcessorArchitecture` codes.
fn normalize_architecture(value: &Value) -> String {
    match value {
        Value::Number(number) => number
            .as_i64()
            .map(architecture_name)
            .unwrap_or_else(|| number.to_string()),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                String::new()
            } else if let Ok(code) = text.parse::<i64>() {
                architecture_name(code)
            } else {
                normalize_architecture_name(text)
            }
        }
        _ => String::new(),
    }
}

fn architecture_name(code: i64) -> String {
    match code {
        0 => "x86".to_string(),
        5 => "arm".to_string(),
        9 => "x64".to_string(),
        11 => "neutral".to_string(),
        12 => "arm64".to_string(),
        other => other.to_string(),
    }
}

fn normalize_architecture_name(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "x64" | "amd64" | "x86_64" => "x64".to_string(),
        "arm64" | "aarch64" => "arm64".to_string(),
        "x86" | "i386" | "i686" | "win32" => "x86".to_string(),
        "neutral" | "anycpu" => "neutral".to_string(),
        "arm" => "arm".to_string(),
        _ => normalized,
    }
}

/// Remove punctuation and case so field/wrapper names match consistently.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// Drop records that repeat `(source_id, version, scope, user_id, architecture)`.
fn deduplicate_records(records: Vec<SoftwareRecord>) -> Vec<SoftwareRecord> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();

    for record in records {
        let key = (
            record.source_id.clone(),
            record.version.clone(),
            scope_key(record.scope),
            record.user_id.clone(),
            record.architecture.clone(),
        );
        if seen.insert(key) {
            result.push(record);
        }
    }

    result
}

fn scope_key(scope: SoftwareScope) -> &'static str {
    match scope {
        SoftwareScope::System => "system",
        SoftwareScope::User => "user",
        SoftwareScope::Provisioned => "provisioned",
        SoftwareScope::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGE: &str = r#"{
        "Name": "Store App",
        "PackageFamilyName": "Store.App_abc",
        "PackageFullName": "Store.App_1.2.3.0_x64__abc",
        "Publisher": "CN=Example",
        "Version": "1.2.3",
        "Architecture": "X64",
        "InstallLocation": "C:\\Program Files\\WindowsApps\\Store.App",
        "IsFramework": false,
        "SignatureKind": "Store",
        "Status": "Ok",
        "UserProfile": "C:\\Users\\matt",
        "UserSid": "S-1-5-21-1234"
    }"#;

    fn wrapper(packages: &str, provisioned: &str) -> String {
        format!(r#"{{"packages":[{packages}],"provisioned":[{provisioned}]}}"#)
    }

    #[test]
    fn command_builder_uses_all_users_without_provisioned_section() {
        let script = build_appx_powershell(true, false);

        assert!(script.contains("Get-AppxPackage -AllUsers"));
        assert!(!script.contains("Get-AppxProvisionedPackage"));
        assert!(script.contains("$result['provisioned'] = @()"));
        assert!(script
            .trim_end()
            .ends_with("ConvertTo-Json -Depth 6 -Compress"));
    }

    #[test]
    fn command_builder_omits_all_users_and_reads_provisioned_packages() {
        let script = build_appx_powershell(false, true);

        assert!(script.contains("Get-AppxPackage | Select-Object"));
        assert!(!script.contains("-AllUsers"));
        assert!(script.contains("Get-AppxProvisionedPackage -Online"));
        assert!(script.contains("$result['packages'] = @("));
        assert!(script.contains("$result['provisioned'] = @("));
        assert!(script
            .trim_end()
            .ends_with("ConvertTo-Json -Depth 6 -Compress"));
    }

    #[test]
    fn command_builder_selects_every_stable_field() {
        let script = build_appx_powershell(true, true);

        for (field, _) in PACKAGE_SELECT_FIELDS {
            assert!(
                script.contains(&format!("Name='{field}'")),
                "missing {field}"
            );
        }
    }

    #[test]
    fn parses_wrapper_list_payload_with_case_insensitive_fields() {
        let payload = r#"{
            "Packages": [{
                "name": "Store App",
                "packagefamilyname": "Store.App_abc",
                "PACKAGEFULLNAME": "Store.App_1.2.3.0_x64__abc",
                "publisher": "CN=Example",
                "version": "1.2.3",
                "architecture": "AMD64",
                "installlocation": "C:\\Apps\\Store.App",
                "isframework": true,
                "signaturekind": "Store",
                "status": "Ok",
                "usersid": "S-1-5-21-1234"
            }],
            "PROVISIONED": []
        }"#;

        let records = parse_appx_packages(payload, SoftwareScope::User).unwrap();

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.name, "Store App");
        assert_eq!(record.source, "microsoft_store");
        assert_eq!(record.source_id, "Store.App_abc");
        assert_eq!(record.version, "1.2.3");
        assert_eq!(record.install_path, "C:\\Apps\\Store.App");
        assert_eq!(record.scope, SoftwareScope::User);
        assert_eq!(record.user_id, "S-1-5-21-1234");
        assert_eq!(record.architecture, "x64");
        assert_eq!(record.managed_by, "microsoft_store");
        assert_eq!(record.update_state, UpdateState::ExternalManaged);
        assert_eq!(
            record
                .identifiers
                .get("package_family_name")
                .map(String::as_str),
            Some("Store.App_abc")
        );
        assert_eq!(
            record
                .identifiers
                .get("package_full_name")
                .map(String::as_str),
            Some("Store.App_1.2.3.0_x64__abc")
        );
        assert_eq!(record.metadata["publisher"], "CN=Example");
        assert!(record.metadata["is_framework"].as_bool().unwrap_or(false));
        assert_eq!(record.metadata["signature_kind"], "Store");
        assert_eq!(record.metadata["status"], "Ok");
    }

    #[test]
    fn parses_bare_list_payload() {
        let payload = format!("[{PACKAGE}]");
        let records = parse_appx_packages(&payload, SoftwareScope::System).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].scope, SoftwareScope::System);
        assert_eq!(records[0].source_id, "Store.App_abc");
    }

    #[test]
    fn parses_single_package_object() {
        let records = parse_appx_packages(PACKAGE, SoftwareScope::User).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "Store App");
        assert_eq!(records[0].source_id, "Store.App_abc");
    }

    #[test]
    fn tolerates_missing_fields_and_numeric_values() {
        let payload = r#"[
            {"Name": "Bare"},
            {"PackageName": "Numeric.Version", "Version": 1.5, "Architecture": 9}
        ]"#;

        let records = parse_appx_packages(payload, SoftwareScope::User).unwrap();

        assert_eq!(records.len(), 2);

        let bare = &records[0];
        assert_eq!(bare.name, "Bare");
        assert_eq!(bare.source_id, "Bare");
        assert_eq!(bare.version, "");
        assert_eq!(bare.install_path, "");
        assert_eq!(bare.user_id, "");
        assert_eq!(bare.architecture, "");
        assert_eq!(bare.metadata["publisher"], "");
        assert!(!bare.metadata["is_framework"].as_bool().unwrap_or(true));
        assert_eq!(bare.metadata["signature_kind"], "");
        assert_eq!(bare.metadata["status"], "");

        let numeric = &records[1];
        assert_eq!(numeric.name, "Numeric.Version");
        assert_eq!(numeric.source_id, "Numeric.Version");
        assert_eq!(numeric.version, "1.5");
        assert_eq!(numeric.architecture, "x64");
    }

    #[test]
    fn provisioned_section_forces_provisioned_scope() {
        let payload = wrapper(
            r#"{"Name":"User App","PackageFamilyName":"User.App_abc","Version":"1.0"}"#,
            r#"{"Name":"Provisioned App","PackageFamilyName":"Provisioned.App_abc","Version":"2.0"}"#,
        );

        let records = parse_appx_packages(&payload, SoftwareScope::User).unwrap();

        assert_eq!(records.len(), 2);
        let user = records
            .iter()
            .find(|record| record.name == "User App")
            .unwrap();
        let provisioned = records
            .iter()
            .find(|record| record.name == "Provisioned App")
            .unwrap();
        assert_eq!(user.scope, SoftwareScope::User);
        assert_eq!(provisioned.scope, SoftwareScope::Provisioned);
    }

    #[test]
    fn deduplicates_identical_identity_tuples() {
        let payload = wrapper(
            r#"{"Name":"Same","PackageFamilyName":"Same.App_abc","Version":"1.0","Architecture":"x64","UserSid":"S-1"},
               {"Name":"Same Again","PackageFamilyName":"Same.App_abc","Version":"1.0","Architecture":"X64","UserSid":"S-1"},
               {"Name":"Same New Version","PackageFamilyName":"Same.App_abc","Version":"2.0","Architecture":"x64","UserSid":"S-1"}"#,
            "",
        );

        let records = parse_appx_packages(&payload, SoftwareScope::User).unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "Same");
        assert_eq!(records[1].version, "2.0");
    }

    #[test]
    fn includes_framework_packages_by_default() {
        let payload = wrapper(
            r#"{"Name":"Framework App","PackageFamilyName":"Framework.App_abc","Version":"1.0","IsFramework":true},
               {"Name":"Regular App","PackageFamilyName":"Regular.App_abc","Version":"1.0","IsFramework":false}"#,
            "",
        );

        let records = parse_appx_packages(&payload, SoftwareScope::User).unwrap();

        assert_eq!(records.len(), 2);
    }

    #[test]
    fn strips_utf8_bom_and_normalizes_architectures() {
        let payload = "\u{feff}[{\"Name\":\"BOM App\",\"Architecture\":\"ARM64\"},{\"Name\":\"Neutral App\",\"Architecture\":\"NEUTRAL\"},{\"Name\":\"Arm App\",\"Architecture\":5}]";

        let records = parse_appx_packages(payload, SoftwareScope::User).unwrap();

        assert_eq!(records.len(), 3);
        assert_eq!(records[0].architecture, "arm64");
        assert_eq!(records[1].architecture, "neutral");
        assert_eq!(records[2].architecture, "arm");
    }

    #[test]
    fn malformed_json_returns_typed_error() {
        let error = parse_appx_packages("{\"packages\": [", SoftwareScope::User).unwrap_err();

        assert!(matches!(error, StoreError::InvalidJson(_)));
    }

    #[test]
    fn non_object_root_returns_typed_error() {
        let error = parse_appx_packages("42", SoftwareScope::User).unwrap_err();

        assert_eq!(error, StoreError::InvalidRoot);
    }

    #[test]
    fn empty_payload_returns_empty_inventory() {
        assert!(parse_appx_packages("   ", SoftwareScope::User)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn collect_with_runner_builds_script_and_parses_user_scope() {
        let payload = wrapper(PACKAGE, "");
        let captured = std::cell::RefCell::new(String::new());
        let runner = |script: &str| {
            *captured.borrow_mut() = script.to_string();
            Ok(payload.clone())
        };

        let records = collect_with_runner(&runner).unwrap();

        let script = captured.borrow();
        assert!(script.contains("Get-AppxPackage -AllUsers"));
        assert!(script.contains("Get-AppxProvisionedPackage -Online"));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].scope, SoftwareScope::User);
        assert_eq!(records[0].source_id, "Store.App_abc");
    }

    #[test]
    fn collect_with_runner_propagates_runner_error() {
        let runner = |_script: &str| Err("powershell unavailable".to_string());

        let error = collect_with_runner(&runner).unwrap_err();

        assert_eq!(
            error,
            StoreError::Runner("powershell unavailable".to_string())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn collect_without_runner_on_non_windows_is_empty() {
        assert!(collect().unwrap().is_empty());
    }
}
