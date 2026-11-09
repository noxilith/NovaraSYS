use std::collections::{BTreeMap, BTreeSet};

use forge_schema::Libc;
use serde::{Deserialize, Serialize};

use crate::StoreError;
use crate::id::{StoreId, is_hex};

/// Where the manifest sits inside an archive and inside a store entry.
pub const MANIFEST_PATH: &str = ".forge/manifest.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: Kind,
    /// Permission bits; normalized when unpacked (never setuid/setgid).
    #[serde(default)]
    pub mode: u32,
    /// Files only: SHA-256 of the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Symlinks only: where the link points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Describes one package build: what it is, what it needs, and every file in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub libc: Libc,
    /// Full SHA-256 of the build inputs; names the store entry.
    pub hash: String,
    /// Store ids of the packages this one needs at run time.
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default, rename = "file")]
    pub files: Vec<FileEntry>,
}

/// A relative path with only normal components: no `..`, no leading `/`, no
/// empty parts, and nothing under the reserved `.forge` directory.
pub(crate) fn safe_rel_path(p: &str) -> bool {
    !p.is_empty()
        && !p.contains('\0')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        && p.split('/').next() != Some(".forge")
}

impl Manifest {
    pub fn id(&self) -> Result<StoreId, StoreError> {
        StoreId::new(&self.name, &self.version, &self.hash)
    }

    pub fn dep_ids(&self) -> Result<Vec<StoreId>, StoreError> {
        self.deps
            .iter()
            .map(|d| {
                StoreId::parse(d)
                    .ok_or_else(|| StoreError::Invalid { what: "dependency id", value: d.clone() })
            })
            .collect()
    }

    pub fn validate(&self) -> Result<(), StoreError> {
        self.id()?;
        self.dep_ids()?;
        let mut kinds: BTreeMap<&str, Kind> = BTreeMap::new();
        for f in &self.files {
            if !safe_rel_path(&f.path) {
                return Err(StoreError::UnsafeArchive(format!("unsafe path `{}`", f.path)));
            }
            if kinds.insert(&f.path, f.kind).is_some() {
                return Err(StoreError::UnsafeArchive(format!("duplicate path `{}`", f.path)));
            }
            let bad = |what: &'static str| StoreError::Invalid { what, value: f.path.clone() };
            match f.kind {
                Kind::File => {
                    if !f.sha256.as_deref().is_some_and(|h| is_hex(h, 64)) || f.target.is_some() {
                        return Err(bad("file entry (needs a sha256, no target)"));
                    }
                }
                Kind::Dir => {
                    if f.sha256.is_some() || f.target.is_some() {
                        return Err(bad("directory entry"));
                    }
                }
                Kind::Symlink => {
                    let ok = f.target.as_deref().is_some_and(|t| !t.is_empty() && !t.contains('\0'));
                    if !ok || f.sha256.is_some() {
                        return Err(bad("symlink entry (needs a target, no sha256)"));
                    }
                }
            }
        }
        // Every entry's parent must be a listed directory, so unpacking never
        // has to create intermediate directories it has not vetted.
        let dirs: BTreeSet<&str> =
            kinds.iter().filter(|(_, k)| **k == Kind::Dir).map(|(p, _)| *p).collect();
        for f in &self.files {
            if let Some((parent, _)) = f.path.rsplit_once('/')
                && !dirs.contains(parent) {
                    return Err(StoreError::UnsafeArchive(format!(
                        "`{}` has no listed parent directory",
                        f.path
                    )));
                }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn file(path: &str) -> FileEntry {
        FileEntry { path: path.into(), kind: Kind::File, mode: 0o644, sha256: Some(H.into()), target: None }
    }
    fn dir(path: &str) -> FileEntry {
        FileEntry { path: path.into(), kind: Kind::Dir, mode: 0o755, sha256: None, target: None }
    }
    fn manifest(files: Vec<FileEntry>) -> Manifest {
        Manifest { name: "a".into(), version: "1".into(), libc: Libc::Musl, hash: H.into(), deps: vec![], files }
    }

    #[test]
    fn accepts_a_well_formed_manifest() {
        manifest(vec![dir("usr"), dir("usr/bin"), file("usr/bin/a")]).validate().unwrap();
    }

    #[test]
    fn rejects_unsafe_paths() {
        for p in ["/etc/passwd", "../x", "a/../b", "a//b", "./a", ".forge/manifest.toml", "a/", ""] {
            assert!(manifest(vec![file(p)]).validate().is_err(), "{p:?}");
        }
    }

    #[test]
    fn rejects_duplicates_and_missing_parents() {
        assert!(manifest(vec![dir("a"), dir("a")]).validate().is_err());
        assert!(manifest(vec![file("a/b")]).validate().is_err());
        assert!(manifest(vec![dir("a"), file("a/b/c")]).validate().is_err());
    }

    #[test]
    fn rejects_inconsistent_entries() {
        let mut f = file("a");
        f.sha256 = None;
        assert!(manifest(vec![f]).validate().is_err());
        let mut l = file("l");
        l.kind = Kind::Symlink;
        l.sha256 = None;
        assert!(manifest(vec![l.clone()]).validate().is_err(), "symlink needs a target");
        l.target = Some("a".into());
        manifest(vec![l]).validate().unwrap();
    }

    #[test]
    fn rejects_bad_dependency_ids() {
        let mut m = manifest(vec![]);
        m.deps = vec!["../../etc".into()];
        assert!(m.validate().is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let m = manifest(vec![dir("usr"), file("usr/a")]);
        let text = toml::to_string(&m).unwrap();
        assert_eq!(toml::from_str::<Manifest>(&text).unwrap(), m);
    }
}
