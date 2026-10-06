//! Strongly typed identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::hash::{Hash, HashParseError};

macro_rules! uuid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self { Self(Uuid::new_v4()) }
            pub fn parse(s: &str) -> Result<Self, uuid::Error> {
                // Only the canonical lowercase hyphenated form is accepted so that an ID has
                // exactly one spelling (it is used as a path component in remote storage).
                let u = Uuid::parse_str(s)?;
                if u.hyphenated().to_string() != s {
                    return Err(Uuid::parse_str("x").unwrap_err());
                }
                Ok(Self(u))
            }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0.hyphenated()) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0.hyphenated())
            }
        }
        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> { Self::parse(s) }
        }
    };
}

uuid_id!(
    /// Stable application identity of an instance. Independent of display name and local path.
    InstanceId
);
uuid_id!(
    /// Identity of one computer/installation of the app.
    DeviceId
);
uuid_id!(
    /// One play session (launch → game exit).
    SessionId
);
uuid_id!(
    /// A background job (scan, upload, restore, ...).
    JobId
);
uuid_id!(
    /// A restore/apply transaction.
    TxId
);

/// Snapshot identity: the BLAKE3 hash of the canonical manifest bytes. Immutable and self-verifying.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub Hash);

impl SnapshotId {
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }
    pub fn short(&self) -> String {
        self.0.short()
    }
}
impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl fmt::Debug for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SnapshotId({})", self.0.short())
    }
}
impl FromStr for SnapshotId {
    type Err = HashParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(SnapshotId(Hash::from_hex(s)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_one_canonical_spelling() {
        let id = InstanceId::new();
        let s = id.to_string();
        assert_eq!(InstanceId::parse(&s).unwrap(), id);
        assert!(InstanceId::parse(&s.to_uppercase()).is_err());
        assert!(InstanceId::parse(&s.replace('-', "")).is_err());
        assert!(InstanceId::parse("../../etc/passwd").is_err());
    }
}
