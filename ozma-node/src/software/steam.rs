// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Steam software-source adapter.
//!
//! Steam is an inventory *source*: this module reads Valve's own metadata and
//! never scans or modifies game directories. Discovery and manifest parsing are
//! pure-std operations so tests do not need Steam installed.
//!
//! # Valve KeyValues notes
//!
//! The parser accepts quoted and bare keys/values, nested objects, `//` line
//! comments, `/* ... */` block comments, escaped quotes/backslashes, numeric
//! keys, and a leading UTF-8 BOM. When a key appears more than once in the same
//! object, the **last** value wins (a plain `insert` into a `BTreeMap`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map as JsonMap, Value as JsonValue};

use super::model::{SoftwareRecord, SoftwareScope, UpdateState};

/// A parsed KeyValues value: either a string leaf or a nested object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VdfValue {
    /// A quoted or bare scalar value.
    String(String),
    /// A nested `{ ... }` object.
    Object(VdfMap),
}

/// One KeyValues object. Keys are stored sorted for deterministic iteration.
pub type VdfMap = BTreeMap<String, VdfValue>;

/// The subset of an `appmanifest_<appid>.acf` file Ozma reports.
///
/// Numeric fields are parsed when possible but the original strings are always
/// available in [`AppManifest::raw`], so odd or missing values are preserved
/// rather than causing a panic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppManifest {
    pub appid: String,
    pub name: String,
    pub installdir: String,
    pub buildid: String,
    pub state_flags: Option<u64>,
    pub size_on_disk: Option<u64>,
    pub last_updated: Option<u64>,
    pub last_played: Option<u64>,
    /// `SharedDepots` entries as depot id -> manifest id.
    pub shared_depots: BTreeMap<String, String>,
    /// The complete parsed `AppState` object, including unmodelled fields.
    pub raw: VdfMap,
}

/// Errors produced while reading or parsing Steam metadata.
#[derive(Debug, thiserror::Error)]
pub enum SteamError {
    /// The input is not valid Valve KeyValues.
    #[error("malformed Valve KeyValues at line {line}, column {column}: {message}")]
    Parse {
        line: usize,
        column: usize,
        message: String,
    },
    /// A required object/key was absent or had the wrong shape.
    #[error("invalid Steam metadata structure: {0}")]
    Structure(String),
    /// A metadata file could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl SteamError {
    fn parse(line: usize, column: usize, message: impl Into<String>) -> Self {
        Self::Parse {
            line,
            column,
            message: message.into(),
        }
    }
}

/// Platform family used by [`discover_steam_roots_in`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteamPlatform {
    Linux,
    MacOs,
    Windows,
    Other,
}

impl SteamPlatform {
    /// The platform this binary was compiled for.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// Collect Steam inventory from `roots`, or from auto-discovered roots.
///
/// Passing `Some(&[])` deliberately collects nothing. Passing `None` calls
/// [`discover_steam_roots`]. Malformed or unreadable manifests are logged and
/// skipped; one bad file never fails the whole collection.
///
/// Duplicate AppIDs are resolved by processing libraries in discovery order and
/// keeping the first successfully parsed manifest.
pub fn collect(
    roots: Option<&[PathBuf]>,
    user_id: &str,
) -> Result<Vec<SoftwareRecord>, SteamError> {
    let roots = match roots {
        Some(roots) => roots.to_vec(),
        None => discover_steam_roots(),
    };

    let mut libraries = Vec::new();
    let mut seen_libraries = BTreeSet::new();
    for root in roots {
        add_library(&mut libraries, &mut seen_libraries, &root);

        let library_vdf = root.join("steamapps").join("libraryfolders.vdf");
        match std::fs::read_to_string(&library_vdf) {
            Ok(text) => match parse_library_folders(&text) {
                Ok(extra_libraries) => {
                    for library in extra_libraries {
                        add_library(&mut libraries, &mut seen_libraries, &library);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        path = %library_vdf.display(),
                        error = %error,
                        "ignoring malformed Steam libraryfolders.vdf"
                    );
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    path = %library_vdf.display(),
                    "Steam libraryfolders.vdf not present"
                );
            }
            Err(error) => {
                tracing::warn!(
                    path = %library_vdf.display(),
                    error = %error,
                    "unable to read Steam libraryfolders.vdf"
                );
            }
        }
    }

    let mut records = Vec::new();
    let mut seen_appids = BTreeSet::new();

    for library in libraries {
        let steamapps = library.join("steamapps");
        let entries = match std::fs::read_dir(&steamapps) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::debug!(
                    path = %steamapps.display(),
                    error = %error,
                    "Steam library has no readable steamapps directory"
                );
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(error = %error, "unable to read a Steam library entry");
                    continue;
                }
            };
            let manifest_path = entry.path();
            if !is_app_manifest(&manifest_path) {
                continue;
            }

            let text = match std::fs::read_to_string(&manifest_path) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(
                        path = %manifest_path.display(),
                        error = %error,
                        "skipping unreadable Steam app manifest"
                    );
                    continue;
                }
            };

            let manifest = match parse_app_manifest(&text) {
                Ok(manifest) => manifest,
                Err(error) => {
                    tracing::warn!(
                        path = %manifest_path.display(),
                        error = %error,
                        "skipping malformed Steam app manifest"
                    );
                    continue;
                }
            };

            let appid = manifest.appid.trim();
            if appid.is_empty() {
                tracing::warn!(
                    path = %manifest_path.display(),
                    "skipping Steam app manifest without an appid"
                );
                continue;
            }
            if !seen_appids.insert(appid.to_string()) {
                tracing::debug!(
                    appid,
                    path = %manifest_path.display(),
                    "duplicate Steam AppID; keeping the first manifest"
                );
                continue;
            }

            records.push(record_from_manifest(
                &manifest,
                &library,
                &manifest_path,
                user_id,
            ));
        }
    }

    Ok(records)
}

fn record_from_manifest(
    manifest: &AppManifest,
    library: &Path,
    manifest_path: &Path,
    user_id: &str,
) -> SoftwareRecord {
    let appid = manifest.appid.trim();
    let name = if manifest.name.trim().is_empty() {
        format!("Steam App {appid}")
    } else {
        manifest.name.clone()
    };
    let install_path = library
        .join("steamapps")
        .join("common")
        .join(&manifest.installdir);

    let mut record = SoftwareRecord::new(name, "steam");
    record.source_id = appid.to_string();
    record.version = manifest.buildid.clone();
    record.install_path = install_path.to_string_lossy().into_owned();
    record.scope = SoftwareScope::User;
    record.user_id = user_id.to_string();
    record.managed_by = "steam".to_string();
    record.update_state = UpdateState::ExternalManaged;
    record
        .identifiers
        .insert("steam_appid".to_string(), appid.to_string());

    let metadata = &mut record.metadata;
    metadata.insert("appid".to_string(), JsonValue::String(appid.to_string()));
    metadata.insert(
        "installdir".to_string(),
        JsonValue::String(manifest.installdir.clone()),
    );
    metadata.insert(
        "buildid".to_string(),
        JsonValue::String(manifest.buildid.clone()),
    );
    insert_numeric_metadata(
        metadata,
        "state_flags",
        manifest.state_flags,
        raw_field(&manifest.raw, "StateFlags"),
    );
    insert_numeric_metadata(
        metadata,
        "size_on_disk",
        manifest.size_on_disk,
        raw_field(&manifest.raw, "SizeOnDisk"),
    );
    insert_numeric_metadata(
        metadata,
        "last_updated",
        manifest.last_updated,
        raw_field(&manifest.raw, "LastUpdated"),
    );
    insert_numeric_metadata(
        metadata,
        "last_played",
        manifest.last_played,
        raw_field(&manifest.raw, "LastPlayed"),
    );
    metadata.insert(
        "library_path".to_string(),
        JsonValue::String(library.to_string_lossy().into_owned()),
    );
    metadata.insert(
        "manifest_path".to_string(),
        JsonValue::String(manifest_path.to_string_lossy().into_owned()),
    );
    if !manifest.shared_depots.is_empty() {
        let depots: JsonMap<String, JsonValue> = manifest
            .shared_depots
            .iter()
            .map(|(depot, manifest_id)| (depot.clone(), JsonValue::String(manifest_id.clone())))
            .collect();
        metadata.insert("shared_depots".to_string(), JsonValue::Object(depots));
    }

    record
}

fn insert_numeric_metadata(
    metadata: &mut BTreeMap<String, JsonValue>,
    key: &str,
    parsed: Option<u64>,
    raw: Option<String>,
) {
    if let Some(value) = parsed {
        metadata.insert(key.to_string(), JsonValue::from(value));
    } else if let Some(raw) = raw.filter(|value| !value.trim().is_empty()) {
        metadata.insert(key.to_string(), JsonValue::String(raw));
    }
}

fn add_library(libraries: &mut Vec<PathBuf>, seen: &mut BTreeSet<PathBuf>, path: &Path) {
    let key = dedupe_key(path);
    if seen.insert(key) {
        libraries.push(path.to_path_buf());
    }
}

fn is_app_manifest(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with("appmanifest_") && name.ends_with(".acf")
}

/// Discover Steam roots for the current user and platform.
///
/// Returns only existing directories, deduplicated by canonical path when
/// possible. This never fails: an unsupported platform simply yields no roots
/// unless `STEAM_ROOT` or `STEAM_PATH` points at an existing directory.
pub fn discover_steam_roots() -> Vec<PathBuf> {
    let env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let home = home_from_env(&env);
    let roots = discover_steam_roots_in(home.as_deref(), &env, SteamPlatform::current());

    #[cfg(windows)]
    let roots = add_windows_registry_root(roots);

    roots
}

#[cfg(windows)]
fn add_windows_registry_root(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: BTreeSet<PathBuf> = roots.iter().map(|root| dedupe_key(root)).collect();
    if let Some(path) = windows_registry_steam_path() {
        push_existing_dir(&mut roots, &mut seen, path);
    }
    roots
}

/// Testable core of [`discover_steam_roots`].
///
/// `home`, `env`, and `platform` are injected so discovery can be exercised on
/// Linux for every platform without touching the real environment.
pub fn discover_steam_roots_in(
    home: Option<&Path>,
    env: &BTreeMap<String, String>,
    platform: SteamPlatform,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    for key in ["STEAM_ROOT", "STEAM_PATH"] {
        if let Some(path) = env_get_ci(env, key).and_then(env_path) {
            candidates.push(path);
        }
    }

    match platform {
        SteamPlatform::Linux => {
            if let Some(home) = home {
                candidates.push(home.join(".steam/steam"));
                candidates.push(home.join(".local/share/Steam"));
                candidates.push(home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"));
            }
        }
        SteamPlatform::MacOs => {
            if let Some(home) = home {
                candidates.push(home.join("Library/Application Support/Steam"));
            }
        }
        SteamPlatform::Windows => {
            if let Some(program_files) = env_get_ci(env, "ProgramFiles(x86)").and_then(env_path) {
                candidates.push(program_files.join("Steam"));
            }
            if let Some(program_files) = env_get_ci(env, "ProgramFiles").and_then(env_path) {
                candidates.push(program_files.join("Steam"));
            }
        }
        SteamPlatform::Other => {}
    }

    let mut roots = Vec::new();
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        push_existing_dir(&mut roots, &mut seen, candidate);
    }
    roots
}

fn push_existing_dir(roots: &mut Vec<PathBuf>, seen: &mut BTreeSet<PathBuf>, path: PathBuf) {
    if !path.is_dir() {
        return;
    }
    let key = dedupe_key(&path);
    if seen.insert(key) {
        roots.push(path);
    }
}

fn dedupe_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn env_get_ci<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str).or_else(|| {
        env.iter().find_map(|(candidate, value)| {
            candidate
                .eq_ignore_ascii_case(key)
                .then_some(value.as_str())
        })
    })
}

fn env_path(value: &str) -> Option<PathBuf> {
    let value = value.trim().trim_matches('"');
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn home_from_env(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(home) = env_get_ci(env, "HOME").filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(home) = env_get_ci(env, "USERPROFILE").filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(home));
    }
    match (env_get_ci(env, "HOMEDRIVE"), env_get_ci(env, "HOMEPATH")) {
        (Some(drive), Some(rest)) => Some(PathBuf::from(format!("{drive}{rest}"))),
        _ => None,
    }
}

#[cfg(windows)]
fn windows_registry_steam_path() -> Option<PathBuf> {
    let output = std::process::Command::new("reg")
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find(|line| line.contains("SteamPath"))?;
    let (_, value) = line.split_once("REG_SZ")?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

/// Parse `steamapps/libraryfolders.vdf` in either the modern object form:
///
/// ```text
/// "libraryfolders" { "0" { "path" "/games/Steam" } }
/// ```
///
/// or the legacy scalar form:
///
/// ```text
/// "libraryfolders" { "0" "/games/Steam" }
/// ```
///
/// Only numeric top-level entries are considered (so metadata keys such as
/// `ContentStatsID` are ignored). Entries without a non-empty path are skipped
/// and repeated paths are deduplicated.
pub fn parse_library_folders(text: &str) -> Result<Vec<PathBuf>, SteamError> {
    let root = Parser::new(text).parse_document()?;
    let map: &VdfMap = match find_key_ci(&root, "libraryfolders") {
        Some(VdfValue::Object(map)) => map,
        Some(VdfValue::String(_)) => {
            return Err(SteamError::Structure(
                "libraryfolders must be an object".to_string(),
            ));
        }
        // Be permissive for callers that pass the inner object directly.
        None => &root,
    };

    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    for (key, value) in map {
        if !is_numeric_key(key) {
            continue;
        }
        let Some(path) = library_path(value) else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let path = PathBuf::from(path);
        if seen.insert(dedupe_key(&path)) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn library_path(value: &VdfValue) -> Option<&str> {
    match value {
        VdfValue::String(path) => Some(path.as_str()),
        VdfValue::Object(fields) => match find_key_ci(fields, "path") {
            Some(VdfValue::String(path)) => Some(path.as_str()),
            _ => None,
        },
    }
}

fn is_numeric_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|character| character.is_ascii_digit())
}

/// Parse an `appmanifest_<appid>.acf` file.
///
/// Missing fields become empty strings or `None`; unrecognised values remain
/// available in [`AppManifest::raw`]. The only structural requirement is an
/// `AppState` object (or an object that directly carries an `appid`).
pub fn parse_app_manifest(text: &str) -> Result<AppManifest, SteamError> {
    let root = Parser::new(text).parse_document()?;
    let state: &VdfMap = match find_key_ci(&root, "appstate") {
        Some(VdfValue::Object(state)) => state,
        Some(VdfValue::String(_)) => {
            return Err(SteamError::Structure(
                "AppState must be an object".to_string(),
            ));
        }
        None => {
            if find_key_ci(&root, "appid").is_some() {
                &root
            } else {
                return Err(SteamError::Structure("missing AppState object".to_string()));
            }
        }
    };

    Ok(AppManifest {
        appid: string_field(state, "appid"),
        name: string_field(state, "name"),
        installdir: string_field(state, "installdir"),
        buildid: string_field(state, "buildid"),
        state_flags: numeric_field(state, "StateFlags"),
        size_on_disk: numeric_field(state, "SizeOnDisk"),
        last_updated: numeric_field(state, "LastUpdated"),
        last_played: numeric_field(state, "LastPlayed"),
        shared_depots: parse_shared_depots(state),
        raw: state.clone(),
    })
}

fn parse_shared_depots(state: &VdfMap) -> BTreeMap<String, String> {
    let Some(VdfValue::Object(depots)) = find_key_ci(state, "SharedDepots") else {
        return BTreeMap::new();
    };
    depots
        .iter()
        .filter_map(|(depot, value)| match value {
            VdfValue::String(manifest_id) => Some((depot.clone(), manifest_id.clone())),
            VdfValue::Object(fields) => {
                let manifest_id = match find_key_ci(fields, "manifest") {
                    Some(VdfValue::String(manifest_id)) => manifest_id.clone(),
                    _ => match find_key_ci(fields, "depot") {
                        Some(VdfValue::String(depot_id)) => depot_id.clone(),
                        _ => return None,
                    },
                };
                Some((depot.clone(), manifest_id))
            }
        })
        .collect()
}

fn string_field(map: &VdfMap, key: &str) -> String {
    match find_key_ci(map, key) {
        Some(VdfValue::String(value)) => value.clone(),
        _ => String::new(),
    }
}

fn raw_field(map: &VdfMap, key: &str) -> Option<String> {
    match find_key_ci(map, key) {
        Some(VdfValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn numeric_field(map: &VdfMap, key: &str) -> Option<u64> {
    string_field(map, key).trim().parse().ok()
}

fn find_key_ci<'a>(map: &'a VdfMap, key: &str) -> Option<&'a VdfValue> {
    map.iter()
        .find_map(|(candidate, value)| candidate.eq_ignore_ascii_case(key).then_some(value))
}

struct Parser<'a> {
    source: &'a str,
    position: usize,
    line: usize,
    column: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source: source.strip_prefix('\u{feff}').unwrap_or(source),
            position: 0,
            line: 1,
            column: 1,
        }
    }

    fn parse_document(mut self) -> Result<VdfMap, SteamError> {
        let mut map = VdfMap::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                None => return Ok(map),
                Some('}') => {
                    return Err(self.error("unexpected '}' at top level"));
                }
                Some(_) => {}
            }

            let key = self.parse_key()?;
            self.skip_trivia()?;
            let value = self.parse_value()?;
            // Deliberate last-wins policy for repeated keys.
            map.insert(key, value);
        }
    }

    fn parse_object(&mut self) -> Result<VdfMap, SteamError> {
        let mut map = VdfMap::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                None => return Err(self.error("unterminated object; expected '}'")),
                Some('}') => {
                    self.bump();
                    return Ok(map);
                }
                Some(_) => {}
            }

            let key = self.parse_key()?;
            self.skip_trivia()?;
            let value = self.parse_value()?;
            map.insert(key, value);
        }
    }

    fn parse_key(&mut self) -> Result<String, SteamError> {
        match self.peek() {
            None => Err(self.error("expected a key, found end of input")),
            Some('"') => self.parse_quoted(),
            Some('{') => Err(self.error("expected a key, found '{'")),
            Some('}') => Err(self.error("expected a key, found '}'")),
            Some(_) => self.parse_bare(),
        }
    }

    fn parse_value(&mut self) -> Result<VdfValue, SteamError> {
        self.skip_trivia()?;
        match self.peek() {
            None => Err(self.error("expected a value, found end of input")),
            Some('{') => {
                self.bump();
                self.parse_object().map(VdfValue::Object)
            }
            Some('"') => self.parse_quoted().map(VdfValue::String),
            Some('}') => Err(self.error("expected a value, found '}'")),
            Some(_) => self.parse_bare().map(VdfValue::String),
        }
    }

    fn parse_quoted(&mut self) -> Result<String, SteamError> {
        debug_assert_eq!(self.peek(), Some('"'));
        self.bump();
        let mut value = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error("unterminated quoted string")),
                Some('"') => return Ok(value),
                Some('\\') => match self.bump() {
                    None => return Err(self.error("unterminated escape sequence")),
                    Some('n') => value.push('\n'),
                    Some('r') => value.push('\r'),
                    Some('t') => value.push('\t'),
                    // Includes \" and \\; unknown escapes are preserved as the
                    // escaped character rather than rejecting otherwise valid data.
                    Some(character) => value.push(character),
                },
                Some(character) => value.push(character),
            }
        }
    }

    fn parse_bare(&mut self) -> Result<String, SteamError> {
        let start = self.position;
        while let Some(character) = self.peek() {
            if character.is_whitespace()
                || character == '\u{feff}'
                || character == '{'
                || character == '}'
                || character == '"'
            {
                break;
            }
            if character == '/'
                && (self.remaining().starts_with("//") || self.remaining().starts_with("/*"))
            {
                break;
            }
            self.bump();
        }
        if self.position == start {
            Err(self.error("expected a token"))
        } else {
            Ok(self.source[start..self.position].to_string())
        }
    }

    fn skip_trivia(&mut self) -> Result<(), SteamError> {
        loop {
            match self.peek() {
                Some(character) if character.is_whitespace() || character == '\u{feff}' => {
                    self.bump();
                }
                Some('/') if self.remaining().starts_with("//") => {
                    while let Some(character) = self.bump() {
                        if character == '\n' {
                            break;
                        }
                    }
                }
                Some('/') if self.remaining().starts_with("/*") => {
                    self.bump();
                    self.bump();
                    loop {
                        if self.remaining().starts_with("*/") {
                            self.bump();
                            self.bump();
                            break;
                        }
                        if self.bump().is_none() {
                            return Err(self.error("unterminated block comment"));
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn remaining(&self) -> &'a str {
        &self.source[self.position..]
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += character.len_utf8();
        if character == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(character)
    }

    fn error(&self, message: impl Into<String>) -> SteamError {
        SteamError::parse(self.line, self.column, message)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos();
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ozma-steam-{label}-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    fn vdf_quote(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    fn app_manifest_text(appid: &str, name: &str, buildid: &str, installdir: &str) -> String {
        format!(
            r#"
"AppState"
{{
  "appid" "{appid}"
  "name" "{name}"
  "installdir" "{installdir}"
  "buildid" "{buildid}"
  "StateFlags" "4"
  "SizeOnDisk" "123456"
  "LastUpdated" "1700000000"
  "LastPlayed" "1700000100"
  "SharedDepots"
  {{
    "228980" "228980"
  }}
}}
"#
        )
    }

    fn write_library_folders(root: &Path, libraries: &[&Path]) {
        let mut text = String::from("\"libraryfolders\"\n{\n");
        for (index, library) in libraries.iter().enumerate() {
            text.push_str(&format!(
                "  \"{index}\"\n  {{\n    \"path\" \"{}\"\n  }}\n",
                vdf_quote(&library.to_string_lossy())
            ));
        }
        text.push_str("}\n");
        write_file(&root.join("steamapps/libraryfolders.vdf"), &text);
    }

    #[test]
    fn parser_supports_quoted_escapes_nested_objects_and_bom() {
        let raw = r#"
"AppState"
{
  "appid" "70"
  "name" "Half-\"Life\" \\ Test"
  "nested" { "key" "value" }
}
"#;
        let text = format!("\u{feff}{raw}");
        let manifest = parse_app_manifest(&text).expect("parse manifest");

        assert_eq!(manifest.appid, "70");
        assert_eq!(manifest.name, r#"Half-"Life" \ Test"#);
        assert!(matches!(
            find_key_ci(&manifest.raw, "nested"),
            Some(VdfValue::Object(_))
        ));
    }

    #[test]
    fn parser_accepts_line_and_block_comments() {
        let text = r#"
// leading line comment
"AppState" /* trailing block comment */
{
  "appid" "70" // another line comment
  "name" "Comment Test" /* a block
  comment spanning lines */
  "buildid" "build-7"
}
"#;
        let manifest = parse_app_manifest(text).expect("parse manifest with comments");

        assert_eq!(manifest.name, "Comment Test");
        assert_eq!(manifest.buildid, "build-7");
    }

    #[test]
    fn parser_rejects_malformed_input() {
        for text in [
            r#""AppState" { "appid" "70""#,
            r#""AppState" { "appid" "unterminated }"#,
            r#""AppState" { /* unterminated comment"#,
            r#""AppState" { } }"#,
        ] {
            assert!(
                parse_app_manifest(text).is_err(),
                "expected malformed input to be rejected: {text}"
            );
        }
    }

    #[test]
    fn parser_keeps_last_duplicate_value_deterministically() {
        let text = r#""AppState" { "appid" "70" "name" "First" "name" "Second" }"#;
        let manifest = parse_app_manifest(text).expect("parse duplicate keys");

        assert_eq!(manifest.name, "Second");
    }

    #[test]
    fn library_folders_supports_modern_and_legacy_forms() {
        let modern = r#"
"libraryfolders"
{
  "0" { "path" "/games/Steam" "label" "" }
  "1" { "path" "/games/SteamTwo" }
  "2" { "label" "no path here" }
  "ContentStatsID" "123456"
  "TimeNextStatsReport" "123456"
}
"#;
        let paths = parse_library_folders(modern).expect("parse modern libraryfolders");
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/games/Steam"),
                PathBuf::from("/games/SteamTwo"),
            ]
        );

        let legacy = r#"
"libraryfolders"
{
  "0" "/games/Steam"
  "1" "/games/SteamTwo"
  "2" "/games/Steam"
  "TimeNextStatsReport" "123456"
  "ContentStatsID" "654321"
}
"#;
        let paths = parse_library_folders(legacy).expect("parse legacy libraryfolders");
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/games/Steam"),
                PathBuf::from("/games/SteamTwo"),
            ]
        );
    }

    #[test]
    fn library_folders_skips_entries_without_paths() {
        let text = r#"
"libraryfolders"
{
  "0" { "label" "missing path" }
  "1" { "path" "" }
  "2" { "path" "/games/Steam" }
}
"#;
        let paths = parse_library_folders(text).expect("parse libraryfolders");
        assert_eq!(paths, vec![PathBuf::from("/games/Steam")]);
    }

    #[test]
    fn app_manifest_maps_every_reported_field() {
        let text = app_manifest_text("70", "Half-Life", "build-1234", "Half-Life");
        let manifest = parse_app_manifest(&text).expect("parse app manifest");

        assert_eq!(manifest.appid, "70");
        assert_eq!(manifest.name, "Half-Life");
        assert_eq!(manifest.installdir, "Half-Life");
        assert_eq!(manifest.buildid, "build-1234");
        assert_eq!(manifest.state_flags, Some(4));
        assert_eq!(manifest.size_on_disk, Some(123_456));
        assert_eq!(manifest.last_updated, Some(1_700_000_000));
        assert_eq!(manifest.last_played, Some(1_700_000_100));
        assert_eq!(
            manifest.shared_depots.get("228980").map(String::as_str),
            Some("228980")
        );
        assert!(!manifest.raw.is_empty());
    }

    #[test]
    fn app_manifest_preserves_missing_and_odd_values() {
        let text = r#""AppState" { "appid" "70" "StateFlags" "not-a-number" }"#;
        let manifest = parse_app_manifest(text).expect("parse odd app manifest");

        assert_eq!(manifest.appid, "70");
        assert_eq!(manifest.name, "");
        assert_eq!(manifest.state_flags, None);
        assert_eq!(manifest.size_on_disk, None);
        assert!(manifest.shared_depots.is_empty());
        assert_eq!(
            raw_field(&manifest.raw, "StateFlags").as_deref(),
            Some("not-a-number")
        );
    }

    #[test]
    fn collect_maps_manifest_fields_and_multiple_libraries() {
        let temp = TempDir::new("collect");
        let root = temp.path().join("root");
        let second = temp.path().join("second");
        std::fs::create_dir_all(root.join("steamapps")).expect("create root steamapps");
        std::fs::create_dir_all(second.join("steamapps")).expect("create second steamapps");
        write_library_folders(&root, &[&root, &second]);
        write_file(
            &root.join("steamapps/appmanifest_70.acf"),
            &app_manifest_text("70", "Half-Life", "build-1", "Half-Life"),
        );
        write_file(
            &second.join("steamapps/appmanifest_220.acf"),
            &app_manifest_text("220", "Half-Life 2", "build-2", "Half-Life 2"),
        );

        let mut records =
            collect(Some(std::slice::from_ref(&root)), "alice").expect("collect Steam inventory");
        records.sort_by(|left, right| left.source_id.cmp(&right.source_id));

        assert_eq!(records.len(), 2);

        let half_life = records
            .iter()
            .find(|record| record.source_id == "70")
            .expect("Half-Life record");
        assert_eq!(half_life.name, "Half-Life");
        assert_eq!(half_life.source, "steam");
        assert_eq!(half_life.version, "build-1");
        assert_eq!(half_life.scope, SoftwareScope::User);
        assert_eq!(half_life.update_state, UpdateState::ExternalManaged);
        assert_eq!(half_life.managed_by, "steam");
        assert_eq!(half_life.user_id, "alice");
        assert_eq!(
            half_life.install_path,
            root.join("steamapps/common/Half-Life")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            half_life.identifiers.get("steam_appid").map(String::as_str),
            Some("70")
        );
        assert_eq!(
            half_life.metadata.get("appid"),
            Some(&JsonValue::String("70".to_string()))
        );
        assert_eq!(
            half_life.metadata.get("installdir"),
            Some(&JsonValue::String("Half-Life".to_string()))
        );
        assert_eq!(
            half_life.metadata.get("buildid"),
            Some(&JsonValue::String("build-1".to_string()))
        );
        assert_eq!(
            half_life.metadata.get("state_flags"),
            Some(&JsonValue::from(4_u64))
        );
        assert_eq!(
            half_life.metadata.get("size_on_disk"),
            Some(&JsonValue::from(123_456_u64))
        );
        assert_eq!(
            half_life.metadata.get("last_updated"),
            Some(&JsonValue::from(1_700_000_000_u64))
        );
        assert_eq!(
            half_life.metadata.get("last_played"),
            Some(&JsonValue::from(1_700_000_100_u64))
        );
        assert_eq!(
            half_life.metadata.get("library_path"),
            Some(&JsonValue::String(root.to_string_lossy().into_owned()))
        );
        assert_eq!(
            half_life.metadata.get("manifest_path"),
            Some(&JsonValue::String(
                root.join("steamapps/appmanifest_70.acf")
                    .to_string_lossy()
                    .into_owned()
            ))
        );
        assert!(half_life.metadata.contains_key("shared_depots"));

        let second_record = records
            .iter()
            .find(|record| record.source_id == "220")
            .expect("Half-Life 2 record");
        assert_eq!(
            second_record.install_path,
            second
                .join("steamapps/common/Half-Life 2")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn collect_skips_malformed_manifests() {
        let temp = TempDir::new("malformed");
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("steamapps")).expect("create steamapps");
        write_file(
            &root.join("steamapps/appmanifest_70.acf"),
            &app_manifest_text("70", "Good App", "build-1", "Good App"),
        );
        write_file(
            &root.join("steamapps/appmanifest_999.acf"),
            r#""AppState" { "appid" "999""#,
        );

        let records = collect(Some(std::slice::from_ref(&root)), "alice").expect("collect");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source_id, "70");
    }

    #[test]
    fn collect_keeps_first_manifest_for_duplicate_appids() {
        let temp = TempDir::new("duplicate");
        let root = temp.path().join("root");
        let second = temp.path().join("second");
        std::fs::create_dir_all(root.join("steamapps")).expect("create root steamapps");
        std::fs::create_dir_all(second.join("steamapps")).expect("create second steamapps");
        write_library_folders(&root, &[&root, &second]);
        write_file(
            &root.join("steamapps/appmanifest_70.acf"),
            &app_manifest_text("70", "First", "build-1", "First"),
        );
        write_file(
            &second.join("steamapps/appmanifest_70.acf"),
            &app_manifest_text("70", "Second", "build-2", "Second"),
        );

        let records = collect(Some(std::slice::from_ref(&root)), "alice").expect("collect");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "First");
        assert_eq!(records[0].version, "build-1");
    }

    #[test]
    fn collect_with_empty_root_list_collects_nothing() {
        let records = collect(Some(&[]), "alice").expect("collect empty roots");
        assert!(records.is_empty());
    }

    #[test]
    fn discovery_uses_synthetic_home_and_environment() {
        let temp = TempDir::new("discover");
        let home = temp.path().join("home");
        let steam = home.join(".steam/steam");
        let local = home.join(".local/share/Steam");
        let flatpak = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
        let env_root = temp.path().join("env-root");
        let missing = temp.path().join("missing");
        std::fs::create_dir_all(&steam).expect("create .steam/steam");
        std::fs::create_dir_all(&local).expect("create local Steam");
        std::fs::create_dir_all(&flatpak).expect("create Flatpak Steam");
        std::fs::create_dir_all(&env_root).expect("create env root");

        let mut env = BTreeMap::new();
        env.insert(
            "STEAM_ROOT".to_string(),
            env_root.to_string_lossy().into_owned(),
        );
        env.insert(
            "STEAM_PATH".to_string(),
            missing.to_string_lossy().into_owned(),
        );

        let roots = discover_steam_roots_in(Some(&home), &env, SteamPlatform::Linux);
        assert_eq!(roots.len(), 4);
        for expected in [&steam, &local, &flatpak, &env_root] {
            assert!(
                roots.contains(expected),
                "expected discovered root {}",
                expected.display()
            );
        }
        assert!(!roots.contains(&missing));
    }

    #[test]
    fn discovery_deduplicates_env_and_default_paths() {
        let temp = TempDir::new("dedupe");
        let home = temp.path().join("home");
        let local = home.join(".local/share/Steam");
        std::fs::create_dir_all(&local).expect("create local Steam");

        let mut env = BTreeMap::new();
        env.insert(
            "STEAM_ROOT".to_string(),
            local.to_string_lossy().into_owned(),
        );

        let roots = discover_steam_roots_in(Some(&home), &env, SteamPlatform::Linux);
        assert_eq!(roots, vec![local]);
    }

    #[test]
    fn discovery_uses_windows_program_files_environment() {
        let temp = TempDir::new("windows");
        let program_files = temp.path().join("Program Files");
        let program_files_x86 = temp.path().join("Program Files (x86)");
        let steam = program_files.join("Steam");
        let steam_x86 = program_files_x86.join("Steam");
        std::fs::create_dir_all(&steam).expect("create Program Files Steam");
        std::fs::create_dir_all(&steam_x86).expect("create Program Files x86 Steam");

        let mut env = BTreeMap::new();
        env.insert(
            "ProgramFiles".to_string(),
            program_files.to_string_lossy().into_owned(),
        );
        env.insert(
            "ProgramFiles(x86)".to_string(),
            program_files_x86.to_string_lossy().into_owned(),
        );

        let roots = discover_steam_roots_in(None, &env, SteamPlatform::Windows);
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&steam));
        assert!(roots.contains(&steam_x86));
    }
}
