use std::fmt;

use sha2::{Digest, Sha256};

use crate::StoreError;

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Versions may not contain `-`, so a store directory name splits unambiguously.
pub(crate) fn valid_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~'))
}

/// Number of hash characters kept in a store directory name.
const SHORT_HASH: usize = 32;

/// The name of a store entry: `<name>-<version>-<first 32 hex chars of the hash>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreId(String);

impl StoreId {
    /// `hash` is the full 64-character SHA-256 hex string.
    pub fn new(name: &str, version: &str, hash: &str) -> Result<Self, StoreError> {
        if !valid_name(name) {
            return Err(StoreError::Invalid { what: "package name", value: name.into() });
        }
        if !valid_version(version) {
            return Err(StoreError::Invalid { what: "package version", value: version.into() });
        }
        if !is_hex(hash, 64) {
            return Err(StoreError::Invalid { what: "package hash", value: hash.into() });
        }
        Ok(StoreId(format!("{name}-{version}-{}", &hash[..SHORT_HASH])))
    }

    /// Parse a store directory name. `None` for anything that is not one.
    pub fn parse(dirname: &str) -> Option<Self> {
        let mut parts = dirname.rsplitn(3, '-');
        let hash = parts.next()?;
        let version = parts.next()?;
        let name = parts.next()?;
        (is_hex(hash, SHORT_HASH) && valid_version(version) && valid_name(name))
            .then(|| StoreId(dirname.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn name(&self) -> &str {
        self.0.rsplitn(3, '-').nth(2).unwrap_or("")
    }

    pub fn version(&self) -> &str {
        self.0.rsplit('-').nth(1).unwrap_or("")
    }
}

impl fmt::Display for StoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn builds_and_parses_ids() {
        let id = StoreId::new("lib-foo", "1.2.3", H).unwrap();
        assert_eq!(id.as_str(), "lib-foo-1.2.3-0123456789abcdef0123456789abcdef");
        assert_eq!(id.name(), "lib-foo");
        assert_eq!(id.version(), "1.2.3");
        assert_eq!(StoreId::parse(id.as_str()), Some(id));
    }

    #[test]
    fn rejects_bad_parts() {
        assert!(StoreId::new("", "1", H).is_err());
        assert!(StoreId::new("a/b", "1", H).is_err());
        assert!(StoreId::new("-a", "1", H).is_err());
        assert!(StoreId::new("a", "1-2", H).is_err());
        assert!(StoreId::new("a", "1", "xyz").is_err());
        assert!(StoreId::new("a", "1", &H.to_uppercase()).is_err());
    }

    #[test]
    fn parse_rejects_non_store_names() {
        for n in [".tmp-1-2", "foo", "foo-1.0", "foo-1.0-xyz", "../x-1-0123456789abcdef0123456789abcdef"] {
            assert_eq!(StoreId::parse(n), None, "{n}");
        }
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
