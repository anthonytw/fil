//! What makes the 1.x wire extensible (DESIGN.md section 8.5, "Versioning"; `docs/v1/wire.md`).
//!
//! - **Unknown types are skipped.** Every message is a postcard enum value whose length is known
//!   from outside (the frame, a MUX record's length, the payload), so a receiver tells a type it
//!   does not know by its variant index alone ([`classify`]) and skips the rest.
//! - **Fields are appended.** A 1.x release adds fields only at the end of a message, in one
//!   struct whose fields all have defaults, read from the bytes after the 1.0 fields with
//!   [`tail`]: an older receiver ignores them, a newer one reads the default when they are absent.
//! - **Features are negotiated.** Each side sends a [`FeatureSet`] right after its hello
//!   (`ControlMsg::Features`); a feature is used only when both sides have it
//!   ([`FeatureSet::common`]).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::limits::{MAX_FEATURE_BYTES, MAX_ROW_EXT};

/// What a receiver makes of a payload by its variant index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A type this build knows: decode it.
    Known,
    /// A type from a later 1.x build: skip it. Carries the index, for the log (never the bytes).
    Unknown(u32),
    /// Not even a variant index.
    Malformed,
}

/// Classifies `payload`, a postcard enum value, against an enum with `known` variants.
pub fn classify(payload: &[u8], known: u32) -> Kind {
    match postcard::take_from_bytes::<u32>(payload) {
        Ok((index, _)) if index < known => Kind::Known,
        Ok((index, _)) => Kind::Unknown(index),
        Err(_) => Kind::Malformed,
    }
}

/// Decodes a known message from `payload` and ignores the bytes after it (fields of a later 1.x
/// build, appended at the end). `None` when the message itself does not decode.
pub fn decode_prefix<T: DeserializeOwned>(payload: &[u8]) -> Option<(T, &[u8])> {
    postcard::take_from_bytes::<T>(payload).ok()
}

/// Appended fields that are present but do not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("malformed appended fields")]
pub struct BadTail;

/// The fields a release appended to a message, decoded from `rest`, the bytes after the fields
/// before them: the default when there are none (an older peer), and anything after them ignored
/// (a newer one). A tail that is present but does not decode is an error.
pub fn tail<T: DeserializeOwned + Default>(rest: &[u8]) -> Result<(T, &[u8]), BadTail> {
    if rest.is_empty() {
        return Ok((T::default(), rest));
    }
    postcard::take_from_bytes::<T>(rest).map_err(|_| BadTail)
}

/// One optional feature of the 1.x wire: a bit number in a [`FeatureSet`]. A number is never
/// reused, and a retired feature keeps its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Feature(pub u16);

impl Feature {
    /// The highest bit a set can carry.
    pub const MAX: u16 = (MAX_FEATURE_BYTES * 8 - 1) as u16;
}

/// The features this build has. 1.0 defines none: everything in it is the baseline every 1.x
/// peer speaks. Add a feature here, with its number, when a 1.x release adds one.
pub const OURS: &[Feature] = &[];

/// A set of features: a bitmap of up to [`MAX_FEATURE_BYTES`] bytes, bit `n` (least significant
/// first) for feature `n`. A longer one does not decode; bits a build does not know are kept on
/// decode and dropped by [`FeatureSet::common`] against this build's own.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub struct FeatureSet(Vec<u8>);

impl<'de> Deserialize<'de> for FeatureSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Vec::deserialize(d)?;
        if bytes.len() > MAX_FEATURE_BYTES {
            return Err(serde::de::Error::custom("feature set too long"));
        }
        Ok(Self(bytes))
    }
}

impl FeatureSet {
    /// The empty set: a peer that sent none, or a build with no features.
    pub fn none() -> Self {
        Self(Vec::new())
    }

    /// What this build advertises.
    pub fn ours() -> Self {
        Self::of(OURS)
    }

    /// A set holding `features`. Panics on a feature past [`Feature::MAX`] (a programming error).
    pub fn of(features: &[Feature]) -> Self {
        let mut s = Self::none();
        for &f in features {
            s.insert(f);
        }
        s
    }

    pub fn insert(&mut self, f: Feature) {
        assert!(f.0 <= Feature::MAX, "feature {} past the cap", f.0);
        let (byte, bit) = (usize::from(f.0 / 8), f.0 % 8);
        if self.0.len() <= byte {
            self.0.resize(byte + 1, 0);
        }
        self.0[byte] |= 1 << bit;
    }

    pub fn has(&self, f: Feature) -> bool {
        let (byte, bit) = (usize::from(f.0 / 8), f.0 % 8);
        self.0.get(byte).is_some_and(|b| b & (1 << bit) != 0)
    }

    /// The features both sets hold: what two peers may use. Trailing zero bytes are dropped.
    pub fn common(&self, other: &FeatureSet) -> FeatureSet {
        let mut v: Vec<u8> = self.0.iter().zip(&other.0).map(|(a, b)| a & b).collect();
        while v.last() == Some(&0) {
            v.pop();
        }
        Self(v)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&b| b == 0)
    }

    /// The raw bitmap (for the golden and the tests).
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// A set from a raw bitmap, as a peer may send it (tests stand in for a later build with it).
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (bytes.len() <= MAX_FEATURE_BYTES).then(|| Self(bytes.to_vec()))
    }
}

/// Room for fields a later 1.x release adds to a row of a list (`SessionSummary`,
/// `OtherDaemon`): a row sits in the middle of its message, so nothing can be appended after its
/// fields. Empty in 1.0; a later release puts its row fields here, encoded as one tail struct,
/// read back with [`tail`]. At most [`MAX_ROW_EXT`] bytes: a longer one does not decode.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub struct RowExt(Vec<u8>);

impl<'de> Deserialize<'de> for RowExt {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Vec::deserialize(d)?;
        if bytes.len() > MAX_ROW_EXT {
            return Err(serde::de::Error::custom("row extension too long"));
        }
        Ok(Self(bytes))
    }
}

impl RowExt {
    /// The extension's bytes, for [`tail`].
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// An extension holding `bytes`, or `None` past [`MAX_ROW_EXT`].
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (bytes.len() <= MAX_ROW_EXT).then(|| Self(bytes.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_index() {
        assert_eq!(classify(&[2, 9, 9], 3), Kind::Known);
        assert_eq!(classify(&[3], 3), Kind::Unknown(3));
        assert_eq!(classify(&[0x80, 0x01], 3), Kind::Unknown(128));
        assert_eq!(classify(&[], 3), Kind::Malformed);
        assert_eq!(classify(&[0x80], 3), Kind::Malformed);
    }

    #[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
    struct Ext {
        x: u32,
        y: bool,
    }

    #[test]
    fn a_tail_defaults_when_absent_and_ignores_what_follows() {
        assert_eq!(tail::<Ext>(&[]).unwrap().0, Ext::default());
        let (e, rest) = tail::<Ext>(&[7, 1, 0xAA, 0xBB]).unwrap();
        assert_eq!(e, Ext { x: 7, y: true });
        assert_eq!(rest, &[0xAA, 0xBB]);
        // Present but cut short, or invalid: an error, not a default.
        assert!(tail::<Ext>(&[7]).is_err());
        assert!(tail::<Ext>(&[7, 2]).is_err());
    }

    #[test]
    fn features_intersect_and_unknown_bits_drop() {
        let a = FeatureSet::of(&[Feature(0), Feature(9), Feature(200)]);
        let b = FeatureSet::of(&[Feature(9), Feature(255)]);
        let c = a.common(&b);
        assert!(c.has(Feature(9)) && !c.has(Feature(0)) && !c.has(Feature(200)));
        assert_eq!(c.as_bytes(), &[0, 2]);
        assert!(FeatureSet::none().common(&a).is_empty());
        assert!(FeatureSet::ours().is_empty(), "1.0 defines no feature");
    }

    #[test]
    fn a_set_longer_than_the_cap_does_not_decode() {
        let ok = postcard::to_allocvec(&vec![0xFFu8; MAX_FEATURE_BYTES]).unwrap();
        assert!(postcard::from_bytes::<FeatureSet>(&ok).is_ok());
        let long = postcard::to_allocvec(&vec![0u8; MAX_FEATURE_BYTES + 1]).unwrap();
        assert!(postcard::from_bytes::<FeatureSet>(&long).is_err());
        assert!(FeatureSet::from_bytes(&[0; MAX_FEATURE_BYTES + 1]).is_none());
    }
}
