// SPDX-License-Identifier: AGPL-3.0-only
//! Dimensions, values, and the aperture meet.
//!
//! An [`Aperture`] maps each [`Dimension`] to the set of values that are
//! currently admissible. The meet of two apertures intersects those sets. The
//! representation is intentionally small:
//!
//! * [`Value::Number`] is a **maximum** for numeric dimensions.
//! * [`Value::Bool`] is a capability flag (`false` is more restrictive).
//! * [`Value::Set`] is an unordered set of admissible tokens; the meet is the
//!   intersection.
//! * [`Value::Enum`] is an ordered token for dimensions with an explicit
//!   restriction order. Higher rank wins the meet (the stricter requirement).
//! * [`Value::Text`] is opaque and only meets an equal value.
//! * [`Value::Bottom`] means the dimension has no admissible value. An
//!   aperture containing a bottom value is empty.
//!
//! Unknown dimensions and unknown enum tokens are carried through and only
//! match an equal value; a mismatch produces [`Value::Bottom`]. This is the
//! fail-closed rule that lets newer policies add dimensions without changing
//! the meaning of older ones.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ApertureError;

/// A capability or security dimension.
///
/// The starting set follows `docs/aperture-policy.md` §2.1, with `codec`
/// included from that table. `Unknown` preserves dimensions introduced by a
/// newer controller or policy manifest.
///
/// `codec`, `audio`, and `permissions` are set-valued. `audio` uses opaque
/// tokens for the channel layout, sample rate, and codec; adapters define the
/// token spelling. `permissions` uses permission names, so an adapter can
/// convert `ozma-protocol`'s `PermissionSet` to and from [`Value::Set`] at the
/// boundary without making this crate depend on the protocol crate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dimension {
    Width,
    Height,
    Fps,
    Codec,
    BitrateKbps,
    Displays,
    Hdr,
    Bus,
    MaxPayload,
    LatencyClass,
    Audio,
    Source,
    Privacy,
    Permissions,
    AuthStrength,
    Trust,
    Consent,
    Zone,
    /// A dimension this build does not know about. It is carried through
    /// untouched and fails closed on any unequal value. Use [`Value::Text`] for
    /// opaque string policy values; numbers, booleans, and string sets are also
    /// preserved.
    Unknown(String),
}

impl Dimension {
    /// The wire/policy key for this dimension.
    pub fn as_str(&self) -> &str {
        match self {
            Dimension::Width => "width",
            Dimension::Height => "height",
            Dimension::Fps => "fps",
            Dimension::Codec => "codec",
            Dimension::BitrateKbps => "bitrate_kbps",
            Dimension::Displays => "displays",
            Dimension::Hdr => "hdr",
            Dimension::Bus => "bus",
            Dimension::MaxPayload => "max_payload",
            Dimension::LatencyClass => "latency_class",
            Dimension::Audio => "audio",
            Dimension::Source => "source",
            Dimension::Privacy => "privacy",
            Dimension::Permissions => "permissions",
            Dimension::AuthStrength => "auth_strength",
            Dimension::Trust => "trust",
            Dimension::Consent => "consent",
            Dimension::Zone => "zone",
            Dimension::Unknown(name) => name,
        }
    }

    /// True when this is a numeric maximum dimension.
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Dimension::Width
                | Dimension::Height
                | Dimension::Fps
                | Dimension::BitrateKbps
                | Dimension::Displays
                | Dimension::MaxPayload
        )
    }

    /// True when this is a boolean dimension.
    pub fn is_boolean(&self) -> bool {
        matches!(self, Dimension::Hdr)
    }

    /// True when this is a set-valued dimension.
    pub fn is_set(&self) -> bool {
        matches!(
            self,
            Dimension::Codec | Dimension::Audio | Dimension::Permissions
        )
    }

    /// True when values of this dimension use the explicit restriction order.
    pub fn is_ordered(&self) -> bool {
        matches!(
            self,
            Dimension::Bus
                | Dimension::LatencyClass
                | Dimension::Source
                | Dimension::Privacy
                | Dimension::AuthStrength
                | Dimension::Trust
                | Dimension::Consent
                | Dimension::Zone
        )
    }

    /// The explicit restriction order for an ordered dimension, from least to
    /// most restrictive. The meet takes the highest-ranked value.
    ///
    /// The orders encode a policy decision, not a universal truth:
    ///
    /// * `bus`: network < usb3 < usb2
    /// * `latency_class`: bulk < interactive < realtime
    /// * `source`: network < display < client-camera < local-camera
    /// * `privacy`: allow < consent-required < deny
    /// * `auth_strength`: none < password < ssh-key < passkey < local
    /// * `trust`: untrusted < relay < wan < lan < local
    /// * `consent`: none < required < granted < denied
    /// * `zone`: public < lan < vpn < loopback
    ///
    /// `auth_strength` and `trust` rank the *stronger/stricter requirement*
    /// higher, so the meet requires the strongest authentication and the most
    /// trusted connection. `consent` ranks `granted` above `required` because a
    /// grant satisfies a pending requirement, while `denied` remains the most
    /// restrictive value. `zone`, `bus`, `source`, and `latency_class` rank the
    /// narrower/privacy-preserving choice higher so a meet never silently
    /// widens exposure or relaxes a latency promise.
    pub fn restriction_order(&self) -> Option<&'static [&'static str]> {
        match self {
            Dimension::Bus => Some(&["network", "usb3", "usb2"]),
            Dimension::LatencyClass => Some(&["bulk", "interactive", "realtime"]),
            Dimension::Source => Some(&["network", "display", "client-camera", "local-camera"]),
            Dimension::Privacy => Some(&["allow", "consent-required", "deny"]),
            Dimension::AuthStrength => Some(&["none", "password", "ssh-key", "passkey", "local"]),
            Dimension::Trust => Some(&["untrusted", "relay", "wan", "lan", "local"]),
            Dimension::Consent => Some(&["none", "required", "granted", "denied"]),
            Dimension::Zone => Some(&["public", "lan", "vpn", "loopback"]),
            _ => None,
        }
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Dimension {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "width" => Dimension::Width,
            "height" => Dimension::Height,
            "fps" => Dimension::Fps,
            "codec" => Dimension::Codec,
            "bitrate_kbps" => Dimension::BitrateKbps,
            "displays" => Dimension::Displays,
            "hdr" => Dimension::Hdr,
            "bus" => Dimension::Bus,
            "max_payload" => Dimension::MaxPayload,
            "latency_class" => Dimension::LatencyClass,
            "audio" => Dimension::Audio,
            "source" => Dimension::Source,
            "privacy" => Dimension::Privacy,
            "permissions" => Dimension::Permissions,
            "auth_strength" => Dimension::AuthStrength,
            "trust" => Dimension::Trust,
            "consent" => Dimension::Consent,
            "zone" => Dimension::Zone,
            other => Dimension::Unknown(other.to_string()),
        })
    }
}

impl Serialize for Dimension {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Dimension {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(value.parse().expect("Dimension::from_str is infallible"))
    }
}

/// A value in an aperture.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    /// The dimension has no admissible value. This is the bottom of the meet.
    Bottom,
    /// A maximum for a numeric dimension.
    Number(u64),
    /// A boolean capability flag; `false` is more restrictive.
    Bool(bool),
    /// An ordered token for a dimension with a restriction order.
    Enum(String),
    /// A set of admissible tokens. The meet is the intersection. Preference
    /// order (for example codec preference) is not encoded here; consumers may
    /// choose a stable preference among the effective tokens. Sets serialize
    /// as JSON arrays, while the loader also accepts a single string for a
    /// one-element set.
    Set(BTreeSet<String>),
    /// An opaque token. Only an equal token meets.
    Text(String),
}

impl Value {
    /// Construct a numeric maximum.
    pub fn number(value: u64) -> Self {
        Value::Number(value)
    }

    /// Construct a boolean value.
    pub fn boolean(value: bool) -> Self {
        Value::Bool(value)
    }

    /// Construct an ordered enum token.
    pub fn enumeration(value: impl Into<String>) -> Self {
        Value::Enum(value.into())
    }

    /// Construct a set value from strings.
    pub fn set<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Value::Set(values.into_iter().map(|v| v.as_ref().to_string()).collect())
    }

    /// Construct an opaque string value.
    pub fn text(value: impl Into<String>) -> Self {
        Value::Text(value.into())
    }

    /// True when this value admits nothing. An empty set is also bottom.
    pub fn is_bottom(&self) -> bool {
        match self {
            Value::Bottom => true,
            Value::Set(values) => values.is_empty(),
            _ => false,
        }
    }

    /// The restriction rank of an ordered enum value, if it is known.
    pub fn restriction_rank(&self, dimension: &Dimension) -> Option<usize> {
        let Value::Enum(value) = self else {
            return None;
        };
        dimension
            .restriction_order()?
            .iter()
            .position(|candidate| *candidate == value)
    }

    /// Would a request for `requested` be admissible under this single value?
    ///
    /// This is the per-dimension half of [`Aperture::allows`]. It is
    /// conservative: a type mismatch, an unknown enum token, or an unknown
    /// dimension only matches an equal value.
    pub fn allows(&self, dimension: &Dimension, requested: &Value) -> bool {
        if self.is_bottom() || requested.is_bottom() {
            return false;
        }
        if self == requested {
            return true;
        }
        match dimension {
            Dimension::Unknown(_) => false,
            dimension if dimension.is_numeric() => match (self, requested) {
                (Value::Number(limit), Value::Number(request)) => request <= limit,
                _ => false,
            },
            dimension if dimension.is_boolean() => match (self, requested) {
                (Value::Bool(limit), Value::Bool(request)) => !request || *limit,
                _ => false,
            },
            dimension if dimension.is_set() => match (self, requested) {
                (Value::Set(allowed), Value::Set(request)) => request.is_subset(allowed),
                _ => false,
            },
            dimension if dimension.is_ordered() => {
                match (
                    self.restriction_rank(dimension),
                    requested.restriction_rank(dimension),
                ) {
                    (Some(limit), Some(request)) => request >= limit,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Meet this value with another value for the same dimension.
    pub fn meet(&self, other: &Value, dimension: &Dimension) -> Value {
        if self == other {
            return self.clone();
        }
        if self.is_bottom() || other.is_bottom() {
            return Value::Bottom;
        }
        match dimension {
            Dimension::Unknown(_) => Value::Bottom,
            dimension if dimension.is_numeric() => match (self, other) {
                (Value::Number(a), Value::Number(b)) => Value::Number((*a).min(*b)),
                _ => Value::Bottom,
            },
            dimension if dimension.is_boolean() => match (self, other) {
                (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a && *b),
                _ => Value::Bottom,
            },
            dimension if dimension.is_set() => match (self, other) {
                (Value::Set(a), Value::Set(b)) => {
                    let intersection: BTreeSet<String> = a.intersection(b).cloned().collect();
                    if intersection.is_empty() {
                        Value::Bottom
                    } else {
                        Value::Set(intersection)
                    }
                }
                _ => Value::Bottom,
            },
            dimension if dimension.is_ordered() => match (self, other) {
                (Value::Enum(_), Value::Enum(_)) => {
                    match (
                        self.restriction_rank(dimension),
                        other.restriction_rank(dimension),
                    ) {
                        (Some(a), Some(b)) => {
                            let order = dimension
                                .restriction_order()
                                .expect("ordered dimension has an order");
                            Value::Enum(order[a.max(b)].to_string())
                        }
                        _ => Value::Bottom,
                    }
                }
                _ => Value::Bottom,
            },
            _ => Value::Bottom,
        }
    }

    pub(crate) fn from_json(
        dimension: &Dimension,
        value: &serde_json::Value,
    ) -> Result<Self, ApertureError> {
        match dimension {
            dimension if dimension.is_numeric() => {
                value.as_u64().map(Value::Number).ok_or_else(|| {
                    ApertureError::invalid_value(dimension, "expected a non-negative integer")
                })
            }
            dimension if dimension.is_boolean() => value
                .as_bool()
                .map(Value::Bool)
                .ok_or_else(|| ApertureError::invalid_value(dimension, "expected a boolean")),
            dimension if dimension.is_set() => match value {
                serde_json::Value::String(single) => Ok(Value::set([single.as_str()])),
                serde_json::Value::Array(items) => {
                    let mut set = BTreeSet::new();
                    for item in items {
                        let item = item.as_str().ok_or_else(|| {
                            ApertureError::invalid_value(
                                dimension,
                                "expected a string or an array of strings",
                            )
                        })?;
                        set.insert(item.to_string());
                    }
                    Ok(Value::Set(set))
                }
                _ => Err(ApertureError::invalid_value(
                    dimension,
                    "expected a string or an array of strings",
                )),
            },
            dimension if dimension.is_ordered() => value
                .as_str()
                .map(|token| Value::Enum(token.to_string()))
                .ok_or_else(|| ApertureError::invalid_value(dimension, "expected a string token")),
            Dimension::Unknown(_) => Ok(match value {
                serde_json::Value::Null => Value::Bottom,
                serde_json::Value::Bool(boolean) => Value::Bool(*boolean),
                serde_json::Value::Number(number) => {
                    number.as_u64().map(Value::Number).ok_or_else(|| {
                        ApertureError::invalid_value(
                            dimension,
                            "expected an unsigned integer, boolean, string, or array of strings",
                        )
                    })?
                }
                serde_json::Value::String(text) => Value::Text(text.clone()),
                serde_json::Value::Array(items) => {
                    let mut set = BTreeSet::new();
                    for item in items {
                        let item = item.as_str().ok_or_else(|| {
                            ApertureError::invalid_value(
                                dimension,
                                "expected an unsigned integer, boolean, string, or array of strings",
                            )
                        })?;
                        set.insert(item.to_string());
                    }
                    Value::Set(set)
                }
                serde_json::Value::Object(_) => {
                    return Err(ApertureError::invalid_value(
                        dimension,
                        "expected an unsigned integer, boolean, string, or array of strings",
                    ))
                }
            }),
            _ => Err(ApertureError::invalid_value(
                dimension,
                "unsupported dimension kind",
            )),
        }
    }
}

impl Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Value::Bottom => serializer.serialize_none(),
            Value::Number(value) => serializer.serialize_u64(*value),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Enum(value) | Value::Text(value) => serializer.serialize_str(value),
            Value::Set(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
        }
    }
}

/// A set of admissible values per dimension.
///
/// An aperture with no entries is unconstrained (all values are admissible).
/// An aperture with a [`Value::Bottom`] entry is empty (nothing is admissible).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Aperture {
    values: BTreeMap<Dimension, Value>,
}

impl Aperture {
    /// Create an unconstrained aperture.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value for a dimension, returning the previous value.
    pub fn insert(&mut self, dimension: Dimension, value: Value) -> Option<Value> {
        self.values.insert(dimension, value)
    }

    /// Builder-style insert.
    pub fn with(mut self, dimension: Dimension, value: Value) -> Self {
        self.insert(dimension, value);
        self
    }

    /// Read the effective value for a dimension.
    pub fn get(&self, dimension: &Dimension) -> Option<&Value> {
        self.values.get(dimension)
    }

    /// Iterate dimensions in stable key order.
    pub fn iter(&self) -> impl Iterator<Item = (&Dimension, &Value)> {
        self.values.iter()
    }

    /// Number of constrained dimensions.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True when this aperture has no dimensions at all.
    pub fn is_unconstrained(&self) -> bool {
        self.values.is_empty()
    }

    /// True when any dimension has no admissible value.
    pub fn is_empty(&self) -> bool {
        self.values.values().any(Value::is_bottom)
    }

    /// The dimensions that are empty, sorted by name.
    pub fn conflicts(&self) -> Vec<String> {
        let mut conflicting: Vec<String> = self
            .values
            .iter()
            .filter(|(_, value)| value.is_bottom())
            .map(|(dimension, _)| dimension.to_string())
            .collect();
        conflicting.sort();
        conflicting
    }

    /// The meet (intersection) of two apertures.
    ///
    /// The operation is commutative, associative, idempotent, and independent
    /// of the order in which apertures are folded.
    pub fn meet(&self, other: &Aperture) -> Aperture {
        let mut values = self.values.clone();
        for (dimension, other_value) in &other.values {
            match values.get(dimension) {
                Some(value) => {
                    let met = value.meet(other_value, dimension);
                    values.insert(dimension.clone(), met);
                }
                None => {
                    values.insert(dimension.clone(), other_value.clone());
                }
            }
        }
        Aperture { values }
    }

    /// Is a requested value admissible under this aperture?
    ///
    /// A dimension absent from the aperture is unconstrained. For a numeric
    /// maximum, a request is admissible when it is no larger than the effective
    /// maximum. For a set, the requested tokens must be a subset of the
    /// effective tokens. For an ordered dimension, the request must be at least
    /// as restrictive as the effective value.
    pub fn allows(&self, dimension: &Dimension, requested: &Value) -> bool {
        match self.values.get(dimension) {
            Some(value) => value.allows(dimension, requested),
            None => !requested.is_bottom(),
        }
    }

    /// True when every value admitted by `self` is admitted by `other`.
    ///
    /// This is the lattice order used by the attenuation invariant: adding a
    /// frame must produce an effective aperture that is no wider than the
    /// previous one.
    pub fn is_no_wider_than(&self, other: &Aperture) -> bool {
        let mut dimensions: BTreeSet<&Dimension> = self.values.keys().collect();
        dimensions.extend(other.values.keys());

        for dimension in dimensions {
            match (self.values.get(dimension), other.values.get(dimension)) {
                (Some(value), Some(_)) => {
                    // A bottom (empty) value is already narrower than anything.
                    if value.is_bottom() {
                        continue;
                    }
                    if !other.allows(dimension, value) {
                        return false;
                    }
                }
                (Some(_), None) => {}
                (None, Some(_)) => return false,
                (None, None) => {}
            }
        }
        true
    }
}

impl FromIterator<(Dimension, Value)> for Aperture {
    fn from_iter<T: IntoIterator<Item = (Dimension, Value)>>(iter: T) -> Self {
        Aperture {
            values: iter.into_iter().collect(),
        }
    }
}

impl<'de> Deserialize<'de> for Aperture {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
        let mut values = BTreeMap::new();

        for (key, value) in raw {
            let dimension: Dimension = key.parse().expect("Dimension::from_str is infallible");
            let value = Value::from_json(&dimension, &value).map_err(serde::de::Error::custom)?;
            values.insert(dimension, value);
        }

        Ok(Aperture { values })
    }
}
