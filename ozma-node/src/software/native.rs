// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Native package-manager inventory.
//!
//! These collectors mirror the Python agent's `collect_app_inventory`: `dpkg`
//! (falling back to `rpm`) and `snap` on Linux, Homebrew on macOS, and winget on
//! Windows. They only query the package manager; they never install or update.

use std::process::Command;

use super::model::{SoftwareRecord, SoftwareScope, UpdateState};

/// Collect installed packages from the platform's native package managers.
pub fn collect() -> Vec<SoftwareRecord> {
    if cfg!(target_os = "linux") {
        collect_linux()
    } else if cfg!(target_os = "macos") {
        collect_macos()
    } else if cfg!(target_os = "windows") {
        collect_windows()
    } else {
        Vec::new()
    }
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: a service/GUI wrapper must not flash a console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    if !output.status.success() && output.stdout.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn collect_linux() -> Vec<SoftwareRecord> {
    let mut records = Vec::new();

    if let Some(text) = run(
        "dpkg-query",
        &["-W", "-f=${Package}\t${Version}\t${Status}\n"],
    ) {
        records.extend(parse_dpkg_output(&text));
    }
    if records.is_empty() {
        if let Some(text) = run(
            "rpm",
            &["-qa", "--queryformat", "%{NAME}\t%{VERSION}-%{RELEASE}\n"],
        ) {
            records.extend(parse_rpm_output(&text));
        }
    }
    if let Some(text) = run("snap", &["list", "--unicode=never"]) {
        records.extend(parse_snap_output(&text));
    }

    records
}

fn collect_macos() -> Vec<SoftwareRecord> {
    match run("brew", &["list", "--versions"]) {
        Some(text) => parse_brew_output(&text),
        None => Vec::new(),
    }
}

fn collect_windows() -> Vec<SoftwareRecord> {
    match run(
        "winget",
        &[
            "list",
            "--accept-source-agreements",
            "--disable-interactivity",
        ],
    ) {
        Some(text) => parse_winget_output(&text),
        None => Vec::new(),
    }
}

fn native_record(name: &str, version: &str, source: &str, source_id: &str) -> SoftwareRecord {
    SoftwareRecord {
        name: name.to_owned(),
        source: source.to_owned(),
        source_id: source_id.to_owned(),
        version: version.to_owned(),
        scope: SoftwareScope::System,
        managed_by: source.to_owned(),
        update_state: UpdateState::Unknown,
        ..SoftwareRecord::default()
    }
}

/// Parse `dpkg-query -W -f=${Package}\t${Version}\t${Status}` output.
pub fn parse_dpkg_output(text: &str) -> Vec<SoftwareRecord> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let version = fields.next()?.trim();
            let status = fields.next().unwrap_or("").trim();
            if name.is_empty() || !status.contains("installed") {
                return None;
            }
            if status.contains("not-installed") || status.contains("config-files") {
                return None;
            }
            Some(native_record(name, version, "dpkg", name))
        })
        .collect()
}

/// Parse `rpm -qa --queryformat %{NAME}\t%{VERSION}-%{RELEASE}\n` output.
pub fn parse_rpm_output(text: &str) -> Vec<SoftwareRecord> {
    text.lines()
        .filter_map(|line| {
            let (name, version) = line.split_once('\t')?;
            let name = name.trim();
            let version = version.trim();
            if name.is_empty() {
                return None;
            }
            Some(native_record(name, version, "rpm", name))
        })
        .collect()
}

/// Parse `snap list --unicode=never` output (the first row is a header).
pub fn parse_snap_output(text: &str) -> Vec<SoftwareRecord> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let version = fields.next()?;
            Some(native_record(name, version, "snap", name))
        })
        .collect()
}

/// Parse `brew list --versions` output.
pub fn parse_brew_output(text: &str) -> Vec<SoftwareRecord> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let versions: Vec<&str> = fields.collect();
            if versions.is_empty() {
                return None;
            }
            Some(native_record(name, &versions.join(","), "brew", name))
        })
        .collect()
}

/// Parse `winget list` table output.
///
/// The table is position-based so names containing spaces survive. Column
/// starts are taken from the header, which is aligned with every data row. If no
/// header can be found, fall back to whitespace splitting for simple rows.
pub fn parse_winget_output(text: &str) -> Vec<SoftwareRecord> {
    let lines: Vec<&str> = text.lines().collect();
    let Some((header_index, columns)) = find_winget_columns(&lines) else {
        return parse_winget_rows_whitespace(&lines);
    };

    let mut records = Vec::new();
    for line in lines.iter().skip(header_index + 1) {
        if line.trim().is_empty() || is_winget_separator(line) {
            continue;
        }
        let name = slice_column(line, &columns, "name").trim();
        let id = slice_column(line, &columns, "id").trim();
        let version = slice_column(line, &columns, "version").trim();
        if name.is_empty() || version.is_empty() {
            continue;
        }
        let source_id = if id.is_empty() { name } else { id };
        records.push(native_record(name, version, "winget", source_id));
    }
    records
}

fn is_winget_separator(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| character == '-' || character == ' ' || character == '+')
}

/// Return the header row and its `(column name, starting byte)` pairs.
fn find_winget_columns(lines: &[&str]) -> Option<(usize, Vec<(String, usize)>)> {
    for (index, line) in lines.iter().enumerate() {
        let columns = winget_header_columns(line);
        let has = |wanted: &str| columns.iter().any(|(name, _)| name == wanted);
        if has("name") && has("id") && has("version") {
            return Some((index, columns));
        }
    }
    None
}

fn winget_header_columns(header: &str) -> Vec<(String, usize)> {
    let mut columns = Vec::new();
    let mut start = None;
    for (index, character) in header.char_indices() {
        if character.is_whitespace() {
            if let Some(begin) = start.take() {
                columns.push((header[begin..index].to_ascii_lowercase(), begin));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        columns.push((header[begin..].trim_end().to_ascii_lowercase(), begin));
    }
    columns
}

fn slice_column<'a>(line: &'a str, columns: &[(String, usize)], target: &str) -> &'a str {
    let Some(index) = columns.iter().position(|(name, _)| name == target) else {
        return "";
    };
    let start = columns[index].1;
    let end = columns
        .get(index + 1)
        .map(|(_, position)| *position)
        .unwrap_or(line.len());
    if start >= line.len() {
        return "";
    }
    line.get(start..end.min(line.len())).unwrap_or("")
}

fn parse_winget_rows_whitespace(lines: &[&str]) -> Vec<SoftwareRecord> {
    lines
        .iter()
        .skip(3)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 2 {
                return None;
            }
            let version = fields.last()?;
            let name = fields[..fields.len() - 1].join(" ");
            if name.is_empty() {
                return None;
            }
            Some(native_record(&name, version, "winget", &name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpkg_keeps_only_installed_packages() {
        let text = "vim\t9.0-1\tinstall ok installed\n\
                    removed\t1.0\tdeinstall ok config-files\n\
                    broken\t2.0\tinstall ok not-installed\n";
        let records = parse_dpkg_output(text);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "vim");
        assert_eq!(records[0].version, "9.0-1");
        assert_eq!(records[0].source, "dpkg");
        assert_eq!(records[0].source_id, "vim");
        assert_eq!(records[0].scope, SoftwareScope::System);
    }

    #[test]
    fn rpm_parses_tab_separated_rows() {
        let records = parse_rpm_output("bash\t5.2.26-1\ncurl\t8.5.0-1\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].name, "curl");
        assert_eq!(records[1].source, "rpm");
        assert_eq!(records[1].version, "8.5.0-1");
    }

    #[test]
    fn snap_skips_header_and_parses_name_version() {
        let text = "Name     Version  Rev  Tracking  Publisher  Notes\n\
                    core22   20240111 123  latest/stable canonical -\n\
                    firefox  123.0    456  latest/stable mozilla -\n";
        let records = parse_snap_output(text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "core22");
        assert_eq!(records[1].version, "123.0");
        assert_eq!(records[1].source, "snap");
    }

    #[test]
    fn brew_joins_multiple_versions() {
        let records = parse_brew_output("python@3.12 3.12.1 3.12.2\ngit 2.43.0\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "python@3.12");
        assert_eq!(records[0].version, "3.12.1,3.12.2");
        assert_eq!(records[0].source, "brew");
    }

    #[test]
    fn winget_uses_header_positions_for_names_with_spaces() {
        let text = format!(
            "{:<24} {:<25} {:<9} {:<9} {}\n{:<24} {:<25} {:<9} {:<9} {}\n{:<24} {:<25} {:<9} {:<9} {}\n",
            "Name",
            "Id",
            "Version",
            "Available",
            "Source",
            "7-Zip",
            "7zip.7zip",
            "24.09",
            "24.09",
            "winget",
            "Microsoft Visual Studio",
            "Microsoft.VisualStudio",
            "17.9.0",
            "",
            "winget",
        );
        let records = parse_winget_output(&text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "7-Zip");
        assert_eq!(records[0].source_id, "7zip.7zip");
        assert_eq!(records[0].version, "24.09");
        assert_eq!(records[1].name, "Microsoft Visual Studio");
        assert_eq!(records[1].source_id, "Microsoft.VisualStudio");
        assert_eq!(records[1].version, "17.9.0");
    }
}
