// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side state machine for one Display-channel connection.
//!
//! A [`DisplaySession`] consumes decoded [`DisplayMessage`] values and maintains
//! the state a renderer needs:
//!
//! - the current canonical [`DisplayDefinitionDocument`];
//! - the latest decoded field values, keyed by stable field id;
//! - the latest accepted scene commit and its revision; and
//! - data-update sequence numbers and the applied definition hash.
//!
//! Definition state is only mutated after every check succeeds. A rejected
//! definition, a failed patch, or an invalid data update leaves the previous
//! definition and field values untouched, so callers never observe a partial
//! update.
//!
//! ```no_run
//! use ozma_display_protocol::{DisplayMessage, DisplaySession};
//!
//! let mut session = DisplaySession::new(1024 * 1024);
//! # let message: DisplayMessage = unimplemented!();
//! let event = session.apply(message)?;
//! # Ok::<(), ozma_display_protocol::SessionError>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};

use ozma_display_definition::{DefinitionError, DisplayDefinitionDocument, MAX_PATCH_BYTES};
use ozma_protocol::display_method::{ActionDef, FieldDef};
use serde_json::Value;

use crate::{convert, proto, DisplayMessage};

/// Default maximum size of a stored definition, matching the definition
/// crate's document limit (1 MiB).
pub const DEFAULT_MAX_DEFINITION_BYTES: usize = ozma_display_definition::MAX_DEFINITION_BYTES;

/// The result of applying one [`DisplayMessage`] to a [`DisplaySession`].
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayEvent {
    /// A full definition was validated and installed.
    DefinitionApplied {
        /// Definition identifier.
        id: String,
        /// Definition version.
        version: u32,
    },

    /// A full definition or patch failed validation. No state was changed.
    DefinitionRejected {
        /// Identifier claimed by the rejected message.
        id: String,
        /// Version claimed by the rejected message.
        version: u32,
        /// Human-readable explanation of the rejection.
        reason: String,
    },

    /// A `DefinitionRef` matched the definition already cached by the session.
    DefinitionCacheHit {
        /// Definition identifier.
        id: String,
        /// Definition version.
        version: u32,
    },

    /// The peer must send a full definition before the referenced version can
    /// be used.
    NeedDefinition {
        /// Definition identifier.
        id: String,
        /// Version that is required.
        version: u32,
        /// Expectation hash supplied by a `DefinitionRef`. Empty when a patch
        /// named a missing base definition, because the base hash is unknown.
        hash: Vec<u8>,
    },

    /// A patch was applied to the cached definition and installed.
    DefinitionPatched {
        /// Definition identifier.
        id: String,
        /// Version that was patched.
        from: u32,
        /// New definition version.
        to: u32,
    },

    /// Changed field values were decoded and applied.
    DataUpdated {
        /// Definition version the update belongs to.
        definition_version: u32,
        /// Field ids whose decoded value actually changed, in message order.
        changed_field_ids: Vec<u32>,
    },

    /// A scene commit advanced the scene revision.
    SceneUpdated {
        /// Accepted scene revision.
        revision: u64,
    },

    /// A complete scene snapshot was received and forwarded to the renderer.
    ///
    /// Snapshot interpretation belongs to the scene codec; the session does not
    /// cache it as the latest commit.
    SceneSnapshot(proto::SceneSnapshot),

    /// A scene commit was older than (or equal to) the latest accepted
    /// revision and was ignored.
    SceneIgnored {
        /// Revision carried by the ignored commit.
        revision: u64,
        /// Revision currently held by the session.
        latest: u64,
    },

    /// An action invocation to forward to the application.
    Action(proto::ActionInvoke),

    /// A pointer, key, semantic, or action interaction to forward to the
    /// application.
    Interaction(proto::InteractionEvent),
}

/// Errors returned while applying a message that could not be validated.
///
/// Definition validation failures are reported as
/// [`DisplayEvent::DefinitionRejected`] or [`DisplayEvent::NeedDefinition`]
/// instead, because they are part of normal reconnect and recovery flow. Data
/// updates are atomic and return an error without changing any field value.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A data update arrived before any definition was installed.
    #[error("data update received before a display definition was applied")]
    NoDefinition,

    /// A data update targets a different definition version than the cached
    /// document.
    #[error(
        "data update is for definition version {update_version}, but the cached definition is version {cached_version}"
    )]
    DefinitionVersionMismatch {
        /// Version carried by the update.
        update_version: u32,
        /// Version of the cached definition.
        cached_version: u32,
    },

    /// A data update references a field id that is not in the cached field
    /// table.
    #[error("data update references unknown field id {0}")]
    UnknownField(u32),

    /// A data update contains the same field id more than once.
    #[error("data update contains duplicate value for field id {0}")]
    DuplicateField(u32),

    /// A field value did not match the type declared by the cached definition.
    #[error("data update for field id {field_id} could not be decoded")]
    InvalidFieldValue {
        /// Field id whose value was rejected.
        field_id: u32,
        /// Underlying definition error.
        #[source]
        source: DefinitionError,
    },
}

/// State for one client connection to a Display channel.
#[derive(Debug)]
pub struct DisplaySession {
    max_definition_bytes: usize,
    definition: Option<DisplayDefinitionDocument>,
    applied_hash: Option<Vec<u8>>,
    definition_json: Option<Vec<u8>>,
    field_values: BTreeMap<u32, Value>,
    latest_scene_commit: Option<proto::SceneCommit>,
    scene_revision: Option<u64>,
    last_data_sequence: Option<u64>,
}

impl Default for DisplaySession {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_DEFINITION_BYTES)
    }
}

impl DisplaySession {
    /// Creates an empty session.
    ///
    /// `max_definition_bytes` bounds both the received `definition_json` and
    /// the canonical bytes retained after validation. Use
    /// [`DEFAULT_MAX_DEFINITION_BYTES`] for the standard 1 MiB limit, or a
    /// smaller value to honor a device resource budget.
    pub fn new(max_definition_bytes: usize) -> Self {
        Self {
            max_definition_bytes,
            definition: None,
            applied_hash: None,
            definition_json: None,
            field_values: BTreeMap::new(),
            latest_scene_commit: None,
            scene_revision: None,
            last_data_sequence: None,
        }
    }

    /// Maximum definition size accepted by this session.
    pub fn max_definition_bytes(&self) -> usize {
        self.max_definition_bytes
    }

    /// The currently cached definition, if any.
    pub fn definition(&self) -> Option<&DisplayDefinitionDocument> {
        self.definition.as_ref()
    }

    /// Identifier of the cached definition.
    pub fn definition_id(&self) -> Option<&str> {
        self.definition.as_ref().map(DisplayDefinitionDocument::id)
    }

    /// Version of the cached definition.
    pub fn definition_version(&self) -> Option<u32> {
        self.definition
            .as_ref()
            .map(DisplayDefinitionDocument::version)
    }

    /// Hash that was applied with the cached definition.
    pub fn applied_hash(&self) -> Option<&[u8]> {
        self.applied_hash.as_deref()
    }

    /// Canonical JSON bytes of the cached definition.
    pub fn applied_definition_json(&self) -> Option<&[u8]> {
        self.definition_json.as_deref()
    }

    /// Latest decoded field values, keyed by stable field id.
    pub fn field_values(&self) -> &BTreeMap<u32, Value> {
        &self.field_values
    }

    /// Latest decoded value for one field id.
    pub fn field_value(&self, field_id: u32) -> Option<&Value> {
        self.field_values.get(&field_id)
    }

    /// Latest accepted scene commit.
    pub fn latest_scene_commit(&self) -> Option<&proto::SceneCommit> {
        self.latest_scene_commit.as_ref()
    }

    /// Latest accepted scene revision.
    pub fn scene_revision(&self) -> Option<u64> {
        self.scene_revision
    }

    /// Sequence number of the most recently applied data update.
    pub fn last_data_sequence(&self) -> Option<u64> {
        self.last_data_sequence
    }

    /// Applies one decoded Display message.
    ///
    /// Definition messages always produce an event, even when rejected. Data
    /// updates are validated as a whole before any value is stored and return
    /// [`SessionError`] on failure. Scene commits at or below the current
    /// revision are reported as [`DisplayEvent::SceneIgnored`].
    pub fn apply(&mut self, message: DisplayMessage) -> Result<DisplayEvent, SessionError> {
        match message {
            DisplayMessage::Definition(definition) => Ok(self.apply_definition(definition)),
            DisplayMessage::DefinitionRef(reference) => Ok(self.apply_definition_ref(reference)),
            DisplayMessage::DefinitionPatch(patch) => Ok(self.apply_definition_patch(patch)),
            DisplayMessage::DataUpdate(update) => self.apply_data_update(update),
            DisplayMessage::SceneCommit(commit) => Ok(self.apply_scene_commit(commit)),
            DisplayMessage::SceneSnapshot(snapshot) => Ok(DisplayEvent::SceneSnapshot(snapshot)),
            DisplayMessage::ActionInvoke(invoke) => Ok(DisplayEvent::Action(invoke)),
            DisplayMessage::InteractionEvent(event) => Ok(DisplayEvent::Interaction(event)),
        }
    }

    fn apply_definition(&mut self, definition: proto::DisplayDefinition) -> DisplayEvent {
        let proto::DisplayDefinition {
            id,
            version,
            hash,
            definition_json,
            definition_zstd,
            fields,
            actions,
        } = definition;

        // Rule for senders that populate both forms: the uncompressed
        // `definition_json` always wins and `definition_zstd` is ignored. This
        // keeps the choice deterministic without requiring both encodings to
        // agree, which would make otherwise valid messages fail.
        let definition_json = match uncompressed_or_zstd(
            definition_json,
            &definition_zstd,
            self.max_definition_bytes,
            "definition",
        ) {
            Ok(bytes) => bytes,
            Err(reason) => return rejected(&id, version, reason),
        };

        let document = match DisplayDefinitionDocument::parse(&definition_json) {
            Ok(document) => document,
            Err(source) => return rejected(&id, version, source.to_string()),
        };

        if document.id() != id {
            return rejected(
                &id,
                version,
                format!(
                    "definition JSON id `{}` does not match message id `{id}`",
                    document.id()
                ),
            );
        }
        if document.version() != version {
            return rejected(
                &id,
                version,
                format!(
                    "definition JSON version {} does not match message version {version}",
                    document.version()
                ),
            );
        }
        if document.hash().as_slice() != hash.as_slice() {
            return rejected(
                &id,
                version,
                "definition JSON hash does not match the message hash".to_owned(),
            );
        }

        if let Err(reason) = check_proto_tables(&document, &fields, &actions) {
            return rejected(&id, version, reason);
        }

        let canonical = match document.canonical_json() {
            Ok(canonical) => canonical,
            Err(source) => return rejected(&id, version, source.to_string()),
        };
        if canonical.len() > self.max_definition_bytes {
            return rejected(
                &id,
                version,
                format!(
                    "canonical definition is {} bytes, exceeding the {}-byte session budget",
                    canonical.len(),
                    self.max_definition_bytes
                ),
            );
        }

        self.definition = Some(document);
        self.applied_hash = Some(hash);
        self.definition_json = Some(canonical);
        self.field_values.clear();
        self.last_data_sequence = None;

        DisplayEvent::DefinitionApplied { id, version }
    }

    fn apply_definition_ref(&self, reference: proto::DefinitionRef) -> DisplayEvent {
        let proto::DefinitionRef { id, version, hash } = reference;

        let cached = self
            .definition
            .as_ref()
            .is_some_and(|document| document.id() == id && document.version() == version)
            && self.applied_hash.as_deref() == Some(hash.as_slice());

        if cached {
            DisplayEvent::DefinitionCacheHit { id, version }
        } else {
            DisplayEvent::NeedDefinition { id, version, hash }
        }
    }

    fn apply_definition_patch(&mut self, patch: proto::DefinitionPatch) -> DisplayEvent {
        let proto::DefinitionPatch {
            definition_id,
            base_version,
            version,
            patch_json,
            hash,
            patch_zstd,
        } = patch;

        // As with full definitions, `patch_json` wins when both forms are
        // present and `patch_zstd` is ignored.
        let patch_limit = MAX_PATCH_BYTES.min(self.max_definition_bytes);
        let patch_zstd = patch_zstd.as_deref().unwrap_or_default();
        let patch_json =
            match uncompressed_or_zstd(patch_json, patch_zstd, patch_limit, "definition patch") {
                Ok(bytes) => bytes,
                Err(reason) => return rejected(&definition_id, version, reason),
            };

        let Some(document) = self.definition.as_ref() else {
            return DisplayEvent::NeedDefinition {
                id: definition_id,
                version: base_version,
                // The patch carries the hash of its result, not of the missing
                // base document.
                hash: Vec::new(),
            };
        };

        if document.id() != definition_id {
            return rejected(
                &definition_id,
                version,
                format!(
                    "cached definition id `{}` does not match patch id `{definition_id}`",
                    document.id()
                ),
            );
        }
        if document.version() != base_version {
            return DisplayEvent::NeedDefinition {
                id: definition_id,
                version: base_version,
                hash: Vec::new(),
            };
        }

        let from = document.version();
        let patched = match document.apply_patch(&patch_json) {
            Ok(patched) => patched,
            Err(source) => return rejected(&definition_id, version, source.to_string()),
        };

        if patched.id() != definition_id {
            return rejected(
                &definition_id,
                version,
                format!(
                    "patched definition id `{}` does not match patch id `{definition_id}`",
                    patched.id()
                ),
            );
        }
        if patched.version() != version {
            return rejected(
                &definition_id,
                version,
                format!(
                    "patched definition version {} does not match patch version {version}",
                    patched.version()
                ),
            );
        }
        if patched.hash().as_slice() != hash.as_slice() {
            return rejected(
                &definition_id,
                version,
                "patched definition hash does not match the patch hash".to_owned(),
            );
        }

        let canonical = match patched.canonical_json() {
            Ok(canonical) => canonical,
            Err(source) => return rejected(&definition_id, version, source.to_string()),
        };
        if canonical.len() > self.max_definition_bytes {
            return rejected(
                &definition_id,
                version,
                format!(
                    "patched definition is {} bytes, exceeding the {}-byte session budget",
                    canonical.len(),
                    self.max_definition_bytes
                ),
            );
        }

        // Keep values whose field still exists and still decodes under the new
        // field table; drop anything the patch invalidated.
        let mut retained = BTreeMap::new();
        for (field_id, value) in std::mem::take(&mut self.field_values) {
            if patched.field(field_id).is_some()
                && patched.encode_field_value(field_id, &value).is_ok()
            {
                retained.insert(field_id, value);
            }
        }

        self.definition = Some(patched);
        self.applied_hash = Some(hash);
        self.definition_json = Some(canonical);
        self.field_values = retained;
        self.last_data_sequence = None;

        DisplayEvent::DefinitionPatched {
            id: definition_id,
            from,
            to: version,
        }
    }

    fn apply_data_update(
        &mut self,
        update: proto::DataUpdate,
    ) -> Result<DisplayEvent, SessionError> {
        let proto::DataUpdate {
            definition_version,
            values,
            sequence,
            timestamp_micros: _,
        } = update;

        let Some(document) = self.definition.as_ref() else {
            return Err(SessionError::NoDefinition);
        };
        if document.version() != definition_version {
            return Err(SessionError::DefinitionVersionMismatch {
                update_version: definition_version,
                cached_version: document.version(),
            });
        }

        // Validate the whole message before mutating any cached value.
        let mut seen = BTreeSet::new();
        let mut decoded = Vec::with_capacity(values.len());
        for value in &values {
            if !seen.insert(value.field_id) {
                return Err(SessionError::DuplicateField(value.field_id));
            }
            if document.field(value.field_id).is_none() {
                return Err(SessionError::UnknownField(value.field_id));
            }
            let json = document.decode_field_value(value).map_err(|source| {
                SessionError::InvalidFieldValue {
                    field_id: value.field_id,
                    source,
                }
            })?;
            decoded.push((value.field_id, json));
        }

        let mut changed_field_ids = Vec::new();
        for (field_id, value) in decoded {
            if self.field_values.get(&field_id) != Some(&value) {
                self.field_values.insert(field_id, value);
                changed_field_ids.push(field_id);
            }
        }

        if let Some(sequence) = sequence {
            self.last_data_sequence = Some(sequence);
        }

        Ok(DisplayEvent::DataUpdated {
            definition_version,
            changed_field_ids,
        })
    }

    fn apply_scene_commit(&mut self, commit: proto::SceneCommit) -> DisplayEvent {
        if let Some(latest) = self.scene_revision {
            if commit.revision <= latest {
                return DisplayEvent::SceneIgnored {
                    revision: commit.revision,
                    latest,
                };
            }
        }

        let revision = commit.revision;
        self.scene_revision = Some(revision);
        self.latest_scene_commit = Some(commit);

        DisplayEvent::SceneUpdated { revision }
    }
}

/// Extracts the JSON payload from a definition or patch message.
///
/// A non-empty uncompressed payload always wins; a compressed payload is only
/// used when the uncompressed form is empty. If both are present they do not
/// need to agree, so a sender may include a compressed form for older readers
/// without making its message invalid.
fn uncompressed_or_zstd(
    json: Vec<u8>,
    zstd: &[u8],
    limit: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    if !json.is_empty() {
        if json.len() > limit {
            return Err(format!(
                "{what} JSON is {} bytes, exceeding the {limit}-byte session budget",
                json.len()
            ));
        }
        return Ok(json);
    }

    if zstd.is_empty() {
        return Err(format!(
            "{what} carries neither an uncompressed JSON payload nor a zstd payload"
        ));
    }

    decompress_zstd(zstd, limit, what)
}

/// Decompresses a zstd payload while never allocating more than `limit` bytes
/// of output.
fn decompress_zstd(compressed: &[u8], limit: usize, what: &str) -> Result<Vec<u8>, String> {
    // Reject a frame that announces an oversized result before allocating the
    // output buffer. Frames without a recorded size are still bounded by the
    // capacity passed to `decompress`.
    if let Some(size) = zstd::bulk::Decompressor::upper_bound(compressed) {
        if size > limit {
            return Err(format!(
                "compressed {what} expands to {size} bytes, exceeding the {limit}-byte session budget"
            ));
        }
    }

    let mut decompressor = zstd::bulk::Decompressor::new().map_err(|error| {
        format!("could not initialize zstd decompression for the {what}: {error}")
    })?;
    let decoded = decompressor
        .decompress(compressed, limit)
        .map_err(|error| format!("could not decompress the {what} with zstd: {error}"))?;

    if decoded.len() > limit {
        return Err(format!(
            "decompressed {what} is {} bytes, exceeding the {limit}-byte session budget",
            decoded.len()
        ));
    }

    Ok(decoded)
}

/// Cross-checks the protobuf field and action tables carried by a definition
/// against the tables compiled from its JSON document.
///
/// An empty protobuf table is accepted so that legacy and JSON-only senders
/// keep working. Non-empty tables must match the compiled tables by stable id
/// and by every compiled attribute: key, type, unit, scale, action parameter
/// schema, permissions, and confirmation requirement. Table order is not
/// significant because ids define each entry.
fn check_proto_tables(
    document: &DisplayDefinitionDocument,
    proto_fields: &[proto::FieldDef],
    proto_actions: &[proto::ActionDef],
) -> Result<(), String> {
    if !proto_fields.is_empty() {
        let mut supplied = BTreeMap::<u32, FieldDef>::new();
        for field in proto_fields {
            let field = convert::field_def_from_proto(field)
                .map_err(|error| format!("invalid protobuf field table: {error}"))?;
            let id = field.id;
            if supplied.insert(id, field).is_some() {
                return Err(format!(
                    "protobuf field table contains duplicate field id {id}"
                ));
            }
        }

        let compiled = document
            .fields()
            .iter()
            .cloned()
            .map(|field| (field.id, field))
            .collect::<BTreeMap<_, _>>();
        if supplied != compiled {
            return Err(
                "protobuf field table does not match the definition's compiled field table"
                    .to_owned(),
            );
        }
    }

    if !proto_actions.is_empty() {
        let mut supplied = BTreeMap::<u32, ActionDef>::new();
        for action in proto_actions {
            let action = convert::action_def_from_proto(action)
                .map_err(|error| format!("invalid protobuf action table: {error}"))?;
            let id = action.id;
            if supplied.insert(id, action).is_some() {
                return Err(format!(
                    "protobuf action table contains duplicate action id {id}"
                ));
            }
        }

        let compiled = document
            .actions()
            .iter()
            .cloned()
            .map(|action| (action.id, action))
            .collect::<BTreeMap<_, _>>();
        if supplied != compiled {
            return Err(
                "protobuf action table does not match the definition's compiled action table"
                    .to_owned(),
            );
        }
    }

    Ok(())
}

fn rejected(id: &str, version: u32, reason: impl Into<String>) -> DisplayEvent {
    DisplayEvent::DefinitionRejected {
        id: id.to_owned(),
        version,
        reason: reason.into(),
    }
}
