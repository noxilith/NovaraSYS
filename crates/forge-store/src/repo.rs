//! Package repositories: an `index.toml` listing archives, plus the archives,
//! plus a signature `index.toml.sig`.
//!
//! Only local directories are supported for now. The index records each
//! archive's SHA-256, and installing checks it. The index is signed with
//! ed25519 (see [`crate::sign`]) and by default must verify against a keyring
//! before anything is installed from it, so the hashes it lists can be
//! trusted. The index carries a `serial` that grows with every publish, which
//! lets a client notice that it is being shown an older index (a rollback).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use forge_schema::Libc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::archive::read_manifest;
use crate::id::{StoreId, hex, is_hex};
use crate::sign::{Keyring, SecretKey, SigFile, VerifyError};
use crate::{StoreError, io_ctx};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub libc: Libc,
    /// Full SHA-256 of the build inputs (see [`crate::Manifest::hash`]).
    pub hash: String,
    /// File name of the archive inside the repository directory.
    pub archive: String,
    /// SHA-256 of the archive file.
    pub archive_sha256: String,
    /// Store ids of run-time dependencies.
    #[serde(default)]
    pub deps: Vec<String>,
}

impl IndexEntry {
    pub fn id(&self) -> Result<StoreId, StoreError> {
        StoreId::new(&self.name, &self.version, &self.hash)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// Grows with every publish. Signed along with the rest of the index, so a
    /// client can refuse an index older than one it has already seen.
    #[serde(default)]
    pub serial: u64,
    #[serde(default, rename = "package")]
    pub packages: Vec<IndexEntry>,
}

fn safe_file_name(n: &str) -> bool {
    !n.is_empty() && !n.starts_with('.') && !n.contains(['/', '\\', '\0'])
}

/// Compare versions numerically where they contain numbers ("1.10" > "1.9").
/// How package versions are ordered (numeric parts first, `_rN` revisions after).
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    cmp_versions(a, b)
}

fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let d = c.is_ascii_digit();
            match out.last_mut() {
                Some((is_d, t)) if *is_d == d => t.push(c),
                _ => out.push((d, c.to_string())),
            }
        }
        out
    }
    let (pa, pb) = (parts(a), parts(b));
    for (x, y) in pa.iter().zip(&pb) {
        let ord = if x.0 && y.0 {
            let (nx, ny) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
            nx.len().cmp(&ny.len()).then_with(|| nx.cmp(ny))
        } else {
            x.1.cmp(&y.1)
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    pa.len().cmp(&pb.len())
}

impl Index {
    pub fn parse(text: &str, path: &Path) -> Result<Self, StoreError> {
        let bad = |message: String| StoreError::BadIndex { path: path.to_path_buf(), message };
        let idx: Index = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
        for e in &idx.packages {
            e.id().map_err(|err| bad(err.to_string()))?;
            if !safe_file_name(&e.archive) {
                return Err(bad(format!("unsafe archive file name `{}`", e.archive)));
            }
            if !is_hex(&e.archive_sha256, 64) {
                return Err(bad(format!("{}: bad archive_sha256", e.name)));
            }
            for d in &e.deps {
                if StoreId::parse(d).is_none() {
                    return Err(bad(format!("{}: bad dependency id `{d}`", e.name)));
                }
            }
        }
        Ok(idx)
    }

    pub fn render(&self) -> String {
        let mut sorted = self.clone();
        sorted
            .packages
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| cmp_versions(&a.version, &b.version)));
        toml::to_string(&sorted).expect("index serializes")
    }

    /// Add an entry, replacing one with the same store id.
    pub fn insert(&mut self, entry: IndexEntry) {
        match self.packages.iter().position(|p| p.id().ok() == entry.id().ok()) {
            Some(i) => self.packages[i] = entry,
            None => self.packages.push(entry),
        }
    }

    pub fn by_id(&self, id: &StoreId) -> Option<&IndexEntry> {
        self.packages.iter().find(|p| p.id().ok().as_ref() == Some(id))
    }

    /// The newest version of `name` built for `libc`.
    pub fn latest(&self, name: &str, libc: Libc) -> Option<&IndexEntry> {
        self.packages
            .iter()
            .filter(|p| p.name == name && p.libc == libc)
            .max_by(|a, b| cmp_versions(&a.version, &b.version))
    }
}

/// How much to trust a repository's index.
#[derive(Debug, Clone)]
pub enum Trust {
    /// The index must carry a valid signature from a key in this keyring.
    Require(Keyring),
    /// Use the index as it is, without checking any signature. Explicitly
    /// insecure: anyone who can alter the index can substitute any package.
    AllowUnsigned,
}

impl Default for Trust {
    /// Fail closed: nothing is trusted until a keyring says so.
    fn default() -> Self {
        Trust::Require(Keyring::empty())
    }
}

/// An index that was loaded under a [`Trust`] policy.
#[derive(Debug)]
pub struct Loaded {
    pub index: Index,
    /// Id of the key that signed it; `None` only under [`Trust::AllowUnsigned`].
    pub signed_by: Option<String>,
}

/// A repository in a local directory.
#[derive(Debug, Clone)]
pub struct LocalRepo {
    dir: PathBuf,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes).map_err(io_ctx(format!("writing {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(io_ctx(format!("saving {}", path.display())))
}

impl LocalRepo {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        LocalRepo { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.toml")
    }

    fn sig_path(&self) -> PathBuf {
        self.dir.join("index.toml.sig")
    }

    fn read_index_bytes(&self) -> Result<Vec<u8>, StoreError> {
        let p = self.index_path();
        fs::read(&p).map_err(|e| StoreError::BadIndex { path: p, message: format!("cannot read: {e}") })
    }

    /// Whether the repository has a signature file.
    pub fn is_signed(&self) -> bool {
        self.sig_path().exists()
    }

    /// The index WITHOUT checking any signature. For tools that inspect a
    /// repository; anything that installs from it must use [`LocalRepo::load`].
    pub fn index_unverified(&self) -> Result<Index, StoreError> {
        let p = self.index_path();
        let bytes = self.read_index_bytes()?;
        let text = String::from_utf8(bytes)
            .map_err(|_| StoreError::BadIndex { path: p.clone(), message: "not valid UTF-8".into() })?;
        Index::parse(&text, &p)
    }

    /// Load the index under `trust`. With [`Trust::Require`] the signature is
    /// checked against the keyring, and the index is parsed from the very bytes
    /// that were verified (reading the file twice would let it change in between).
    pub fn load(&self, trust: &Trust) -> Result<Loaded, StoreError> {
        let bytes = self.read_index_bytes()?;
        let sig = match fs::read_to_string(self.sig_path()) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_ctx(format!("reading {}", self.sig_path().display()))(e)),
        };
        verify_index(&self.dir, &self.index_path(), &bytes, sig.as_deref(), trust)
    }

    pub fn archive_path(&self, e: &IndexEntry) -> PathBuf {
        self.dir.join(&e.archive)
    }

    pub fn open_archive(&self, e: &IndexEntry) -> Result<File, StoreError> {
        let p = self.archive_path(e);
        File::open(&p).map_err(io_ctx(format!("opening {}", p.display())))
    }

    /// Write `index` and, if there is a signer, its signature (over the exact
    /// bytes written). The index is saved first, so a reader that catches the
    /// two files between the renames sees a mismatch and fails closed.
    fn save(&self, index: &Index, signer: Option<&SecretKey>) -> Result<(), StoreError> {
        let text = index.render();
        write_atomic(&self.index_path(), text.as_bytes())?;
        if let Some(key) = signer {
            write_atomic(&self.sig_path(), key.sign_index(text.as_bytes()).render().as_bytes())?;
        }
        Ok(())
    }

    /// Add a package archive to the repository: copy it in and record it in
    /// the index (creating the index if there is none), bumping the index's
    /// serial number.
    ///
    /// Signing rules, so a signature can never quietly become meaningless:
    /// - a signed repository can only be extended with `signer` (publishing
    ///   without it would leave a stale signature), and only if its current
    ///   index really carries a valid signature from that key (so a tampered
    ///   index is not signed on top of);
    /// - an existing UNSIGNED index is not signed as a side effect of
    ///   publishing; check it and use [`LocalRepo::sign`] for that.
    pub fn publish(&self, archive: &Path, signer: Option<&SecretKey>) -> Result<IndexEntry, StoreError> {
        let manifest = read_manifest(File::open(archive).map_err(io_ctx(format!("opening {}", archive.display())))?)?;
        let id = manifest.id()?;

        let existing = match fs::read(self.index_path()) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_ctx(format!("reading {}", self.index_path().display()))(e)),
        };
        match (signer, self.is_signed(), existing.is_some()) {
            (None, true, _) => return Err(StoreError::SignedRepoNeedsKey(self.dir.clone())),
            (Some(key), true, true) => {
                let mut ring = Keyring::empty();
                ring.insert(key.public());
                self.load(&Trust::Require(ring)).map_err(|e| StoreError::BadSignature {
                    repo: self.dir.clone(),
                    reason: format!("the existing index is not validly signed by this key, so it will not be extended: {e}"),
                })?;
            }
            (Some(_), false, true) => return Err(StoreError::UnsignedIndex(self.dir.clone())),
            _ => {}
        }

        let mut f = File::open(archive).map_err(io_ctx(format!("opening {}", archive.display())))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf).map_err(io_ctx(format!("reading {}", archive.display())))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }

        fs::create_dir_all(&self.dir).map_err(io_ctx(format!("creating {}", self.dir.display())))?;
        let file_name = format!("{id}.tar.zst");
        let dest = self.dir.join(&file_name);
        if fs::canonicalize(archive).ok() != fs::canonicalize(&dest).ok() {
            fs::copy(archive, &dest).map_err(io_ctx(format!("copying to {}", dest.display())))?;
        }

        let entry = IndexEntry {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            libc: manifest.libc,
            hash: manifest.hash.clone(),
            archive: file_name,
            archive_sha256: hex(&hasher.finalize()),
            deps: manifest.deps.clone(),
        };
        let mut index = match &existing {
            Some(bytes) => {
                let text = std::str::from_utf8(bytes).map_err(|_| StoreError::BadIndex {
                    path: self.index_path(),
                    message: "not valid UTF-8".into(),
                })?;
                Index::parse(text, &self.index_path())?
            }
            None => Index::default(),
        };
        index.insert(entry.clone());
        index.serial += 1;
        self.save(&index, signer)?;
        Ok(entry)
    }

    /// Sign the current index with `signer` (bumping its serial number). This
    /// blesses whatever the index says, so check it first; use it to sign a
    /// repository that was not signed, or to re-sign with a new key.
    pub fn sign(&self, signer: &SecretKey) -> Result<(), StoreError> {
        let mut index = self.index_unverified()?;
        index.serial += 1;
        self.save(&index, Some(signer))
    }

    /// Like [`LocalRepo::sign`], but the new serial is at least `min_serial`. A repository rebuilt from scratch
    /// would otherwise start counting again, and clients (which refuse an index older than one they saw) would
    /// reject it as a rollback.
    pub fn sign_at_least(&self, signer: &SecretKey, min_serial: u64) -> Result<(), StoreError> {
        let mut index = self.index_unverified()?;
        index.serial = (index.serial + 1).max(min_serial);
        self.save(&index, Some(signer))
    }
}

/// Work out what to install for the wanted package names: the newest version
/// of each for `libc`, plus everything they depend on, with dependencies
/// listed before the packages that need them.
pub fn resolve(index: &Index, names: &[String], libc: Libc) -> Result<Vec<IndexEntry>, StoreError> {
    fn visit(
        index: &Index,
        entry: &IndexEntry,
        libc: Libc,
        done: &mut BTreeSet<StoreId>,
        active: &mut BTreeSet<StoreId>,
        out: &mut Vec<IndexEntry>,
    ) -> Result<(), StoreError> {
        let id = entry.id()?;
        if done.contains(&id) {
            return Ok(());
        }
        if !active.insert(id.clone()) {
            return Err(StoreError::Cycle(id.to_string()));
        }
        for dep in &entry.deps {
            let missing = || StoreError::DependencyMissing { pkg: id.to_string(), dep: dep.clone() };
            let dep_id = StoreId::parse(dep).ok_or_else(missing)?;
            let dep_entry = index.by_id(&dep_id).filter(|d| d.libc == libc).ok_or_else(missing)?;
            visit(index, dep_entry, libc, done, active, out)?;
        }
        active.remove(&id);
        done.insert(id);
        out.push(entry.clone());
        Ok(())
    }

    let mut by_name: BTreeMap<&str, &IndexEntry> = BTreeMap::new();
    for n in names {
        let e = index.latest(n, libc).ok_or_else(|| StoreError::PackageNotFound(n.clone()))?;
        by_name.insert(n, e);
    }
    let (mut done, mut active, mut out) = (BTreeSet::new(), BTreeSet::new(), Vec::new());
    for e in by_name.values() {
        visit(index, e, libc, &mut done, &mut active, &mut out)?;
    }
    Ok(out)
}

/// Check `index_bytes` against `sig_text` under `trust` and parse them. `repo` names the repository in
/// errors (a directory, or a URL); `index_path` is what parse errors point at. The index is parsed from the
/// very bytes that were verified: reading it twice would let it change in between.
pub(crate) fn verify_index(
    repo: &Path,
    index_path: &Path,
    bytes: &[u8],
    sig_text: Option<&str>,
    trust: &Trust,
) -> Result<Loaded, StoreError> {
    let parse = |bytes: &[u8]| -> Result<Index, StoreError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| StoreError::BadIndex { path: index_path.to_path_buf(), message: "not valid UTF-8".into() })?;
        Index::parse(text, index_path)
    };
    let Trust::Require(ring) = trust else {
        return Ok(Loaded { index: parse(bytes)?, signed_by: None });
    };
    let Some(sig_text) = sig_text else {
        return Err(StoreError::Unsigned(repo.to_path_buf()));
    };
    let bad = |reason: String| StoreError::BadSignature { repo: repo.to_path_buf(), reason };
    let sig = SigFile::parse(sig_text).map_err(|e| bad(format!("unreadable signature file: {e}")))?;
    let key_id = ring.verify_index(bytes, &sig).map_err(|e| match e {
        VerifyError::UnknownKey(key_id) => StoreError::UnknownKey { repo: repo.to_path_buf(), key_id, known: ring.ids().len() },
        VerifyError::Malformed(m) => bad(format!("malformed signature: {m}")),
        VerifyError::Invalid => bad("the signature does not match the index (it was altered after signing, or signed with a different key)".into()),
    })?;
    Ok(Loaded { index: parse(bytes)?, signed_by: Some(key_id) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_pkg;

    fn entry(name: &str, version: &str, deps: &[&str]) -> IndexEntry {
        let h = crate::sha256_hex(format!("{name}{version}").as_bytes());
        IndexEntry {
            name: name.into(),
            version: version.into(),
            libc: Libc::Musl,
            hash: h,
            archive: format!("{name}-{version}.tar.zst"),
            archive_sha256: "0".repeat(64),
            deps: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn id_of(e: &IndexEntry) -> String {
        e.id().unwrap().to_string()
    }

    fn index(entries: Vec<IndexEntry>) -> Index {
        Index { packages: entries, ..Default::default() }
    }

    fn names(v: &[IndexEntry]) -> Vec<String> {
        v.iter().map(|e| format!("{}-{}", e.name, e.version)).collect()
    }

    #[test]
    fn picks_the_newest_version_numerically() {
        let idx = index(vec![entry("a", "1.9", &[]), entry("a", "1.10", &[]), entry("a", "1.2", &[])]);
        assert_eq!(idx.latest("a", Libc::Musl).unwrap().version, "1.10");
        assert!(idx.latest("a", Libc::Glibc).is_none(), "wrong libc");
        assert!(idx.latest("b", Libc::Musl).is_none());
    }

    #[test]
    fn resolves_dependencies_before_dependents() {
        let lib = entry("lib", "1", &[]);
        let mid = entry("mid", "1", &[&id_of(&lib)]);
        let app = entry("app", "1", &[&id_of(&mid), &id_of(&lib)]);
        let idx = index(vec![app, mid, lib]);
        let order = names(&resolve(&idx, &["app".into()], Libc::Musl).unwrap());
        assert_eq!(order, ["lib-1", "mid-1", "app-1"]);
    }

    #[test]
    fn shared_dependencies_appear_once() {
        let lib = entry("lib", "1", &[]);
        let a = entry("a", "1", &[&id_of(&lib)]);
        let b = entry("b", "1", &[&id_of(&lib)]);
        let idx = index(vec![a, b, lib]);
        let r = resolve(&idx, &["a".into(), "b".into()], Libc::Musl).unwrap();
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn unknown_packages_and_missing_dependencies_are_errors() {
        let idx = index(vec![entry("a", "1", &["ghost-1-0123456789abcdef0123456789abcdef"])]);
        assert!(matches!(resolve(&idx, &["nope".into()], Libc::Musl), Err(StoreError::PackageNotFound(n)) if n == "nope"));
        assert!(matches!(resolve(&idx, &["a".into()], Libc::Musl), Err(StoreError::DependencyMissing { .. })));
    }

    #[test]
    fn a_dependency_built_for_another_libc_does_not_count() {
        let mut lib = entry("lib", "1", &[]);
        lib.libc = Libc::Glibc;
        let app = entry("app", "1", &[&id_of(&lib)]);
        let idx = index(vec![app, lib]);
        assert!(matches!(resolve(&idx, &["app".into()], Libc::Musl), Err(StoreError::DependencyMissing { .. })));
    }

    #[test]
    fn dependency_cycles_are_detected() {
        let mut a = entry("a", "1", &[]);
        let b = entry("b", "1", &[&id_of(&a)]);
        a.deps = vec![id_of(&b)];
        let idx = index(vec![a, b]);
        assert!(matches!(resolve(&idx, &["a".into()], Libc::Musl), Err(StoreError::Cycle(_))));
    }

    #[test]
    fn index_round_trips_and_rejects_unsafe_content() {
        let idx = index(vec![entry("b", "1", &[]), entry("a", "2", &[])]);
        let text = idx.render();
        assert!(text.find("name = \"a\"").unwrap() < text.find("name = \"b\"").unwrap(), "sorted");
        assert_eq!(Index::parse(&text, Path::new("i")).unwrap().packages.len(), 2);

        let bad = text.replace("a-2.tar.zst", "../../etc/passwd");
        assert!(Index::parse(&bad, Path::new("i")).is_err(), "path traversal in an archive name");
        let bad = text.replace(&"0".repeat(64), "xyz");
        assert!(Index::parse(&bad, Path::new("i")).is_err());
    }

    #[test]
    fn publish_builds_an_index_and_updates_it() {
        let d = tempfile::tempdir().unwrap();
        let repo = LocalRepo::new(d.path().join("repo"));
        assert!(repo.index_unverified().is_err(), "no index yet");

        let (_, lib) = make_pkg(d.path(), "lib", "1", &[("usr/lib/x", "x")], &[]);
        let e1 = repo.publish(&d.path().join("lib-1.tar.zst"), None).unwrap();
        assert_eq!(e1.archive_sha256, lib.archive_sha256);

        let (_, _app) = make_pkg(d.path(), "app", "1", &[("usr/bin/app", "a")], &[lib.id.as_str()]);
        let e2 = repo.publish(&d.path().join("app-1.tar.zst"), None).unwrap();
        assert_eq!(e2.deps, vec![lib.id.to_string()]);

        let idx = repo.index_unverified().unwrap();
        assert_eq!(idx.packages.len(), 2);
        assert!(repo.archive_path(&e1).is_file());
        let order = names(&resolve(&idx, &["app".into()], Libc::Musl).unwrap());
        assert_eq!(order, ["lib-1", "app-1"]);

        // Publishing the same archive again replaces the entry, not duplicates it.
        repo.publish(&d.path().join("lib-1.tar.zst"), None).unwrap();
        assert_eq!(repo.index_unverified().unwrap().packages.len(), 2);
    }

    #[test]
    fn publish_rejects_a_file_that_is_not_a_package() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("junk.tar.zst"), "not an archive").unwrap();
        let repo = LocalRepo::new(d.path().join("repo"));
        assert!(repo.publish(&d.path().join("junk.tar.zst"), None).is_err());
        assert!(!repo.dir().join("index.toml").exists());
    }

    // ---- signing -------------------------------------------------------------

    fn key(n: u8) -> SecretKey {
        SecretKey::from_seed([n; 32])
    }

    fn ring(keys: &[&SecretKey]) -> Trust {
        let mut r = Keyring::empty();
        for k in keys {
            r.insert(k.public());
        }
        Trust::Require(r)
    }

    /// A repository in `d` holding one published package, signed by `signer` if given.
    fn repo_with_package(d: &Path, name: &str, signer: Option<&SecretKey>) -> LocalRepo {
        make_pkg(d, name, "1", &[("usr/bin/x", name)], &[]);
        let repo = LocalRepo::new(d.join(format!("repo-{name}")));
        repo.publish(&d.join(format!("{name}-1.tar.zst")), signer).unwrap();
        repo
    }

    #[test]
    fn a_signed_repository_loads_under_the_right_key() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let repo = repo_with_package(d.path(), "a", Some(&k));
        assert!(repo.is_signed());
        let loaded = repo.load(&ring(&[&k])).unwrap();
        assert_eq!(loaded.signed_by, Some(k.public().id()));
        assert_eq!(loaded.index.packages.len(), 1);
        assert_eq!(loaded.index.serial, 1);
    }

    #[test]
    fn every_publish_raises_the_signed_serial() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let repo = repo_with_package(d.path(), "a", Some(&k));
        make_pkg(d.path(), "b", "1", &[("usr/bin/y", "b")], &[]);
        repo.publish(&d.path().join("b-1.tar.zst"), Some(&k)).unwrap();
        let loaded = repo.load(&ring(&[&k])).unwrap();
        assert_eq!((loaded.index.serial, loaded.index.packages.len()), (2, 2));
    }

    #[test]
    fn an_unsigned_repository_is_refused_unless_explicitly_allowed() {
        let d = tempfile::tempdir().unwrap();
        let repo = repo_with_package(d.path(), "a", None);
        assert!(!repo.is_signed());
        assert!(matches!(repo.load(&Trust::default()), Err(StoreError::Unsigned(_))), "the default fails closed");
        assert!(matches!(repo.load(&ring(&[&key(1)])), Err(StoreError::Unsigned(_))));
        let loaded = repo.load(&Trust::AllowUnsigned).unwrap();
        assert_eq!(loaded.signed_by, None, "an unsigned index is never reported as signed");
    }

    #[test]
    fn an_index_altered_after_signing_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let repo = repo_with_package(d.path(), "a", Some(&k));
        let path = repo.dir().join("index.toml");
        let original = fs::read_to_string(&path).unwrap();
        let hash = original.lines().find(|l| l.starts_with("archive_sha256")).unwrap().to_string();
        for tampered in [
            original.replace(&hash, &format!("archive_sha256 = \"{}\"", "0".repeat(64))), // substituted archive
            format!("{original}\n"),                                                       // trailing whitespace
            original.replace("serial = 1", "serial = 99"),                                 // forged serial
            original.replace("name = \"a\"", "name = \"b\""),                              // renamed package
        ] {
            fs::write(&path, &tampered).unwrap();
            let err = repo.load(&ring(&[&k])).unwrap_err();
            assert!(matches!(&err, StoreError::BadSignature { reason, .. } if reason.contains("does not match")), "{err}");
        }
        fs::write(&path, &original).unwrap();
        assert!(repo.load(&ring(&[&k])).is_ok(), "restoring the original makes it valid again");
    }

    #[test]
    fn a_signature_by_a_key_that_is_not_trusted_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (signer, other) = (key(1), key(2));
        let repo = repo_with_package(d.path(), "a", Some(&signer));
        let err = repo.load(&ring(&[&other])).unwrap_err();
        assert!(matches!(&err, StoreError::UnknownKey { key_id, known: 1, .. } if *key_id == signer.public().id()), "{err}");
        assert!(matches!(repo.load(&Trust::default()), Err(StoreError::UnknownKey { known: 0, .. })), "an empty keyring trusts nothing");
    }

    #[test]
    fn a_signature_does_not_transfer_to_another_repositorys_index() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let a = repo_with_package(d.path(), "a", Some(&k));
        let b = repo_with_package(d.path(), "b", Some(&k));
        fs::copy(a.dir().join("index.toml.sig"), b.dir().join("index.toml.sig")).unwrap();
        assert!(matches!(b.load(&ring(&[&k])), Err(StoreError::BadSignature { .. })));
    }

    #[test]
    fn damaged_signature_files_are_refused_not_ignored() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let repo = repo_with_package(d.path(), "a", Some(&k));
        let sig = repo.dir().join("index.toml.sig");
        let good = fs::read_to_string(&sig).unwrap();
        for damaged in ["", "not toml {{{", "version = 1\n", &good.replace("signature = \"", "signature = \"00")] {
            fs::write(&sig, damaged).unwrap();
            assert!(repo.load(&ring(&[&k])).is_err(), "{damaged:?}");
        }
    }

    #[test]
    fn allow_unsigned_never_checks_a_signature_even_a_bad_one() {
        // Documented behaviour of the insecure mode: the signature is ignored.
        let d = tempfile::tempdir().unwrap();
        let repo = repo_with_package(d.path(), "a", Some(&key(1)));
        fs::write(repo.dir().join("index.toml.sig"), "garbage").unwrap();
        let loaded = repo.load(&Trust::AllowUnsigned).unwrap();
        assert_eq!(loaded.signed_by, None);
    }

    #[test]
    fn publishing_without_the_key_to_a_signed_repository_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let repo = repo_with_package(d.path(), "a", Some(&k));
        make_pkg(d.path(), "b", "1", &[("usr/bin/y", "b")], &[]);
        let err = repo.publish(&d.path().join("b-1.tar.zst"), None).unwrap_err();
        assert!(matches!(err, StoreError::SignedRepoNeedsKey(_)), "{err}");
        assert_eq!(repo.load(&ring(&[&k])).unwrap().index.packages.len(), 1, "nothing changed, still valid");
    }

    #[test]
    fn publishing_with_a_different_key_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let repo = repo_with_package(d.path(), "a", Some(&key(1)));
        make_pkg(d.path(), "b", "1", &[("usr/bin/y", "b")], &[]);
        let err = repo.publish(&d.path().join("b-1.tar.zst"), Some(&key(2))).unwrap_err();
        assert!(matches!(err, StoreError::BadSignature { .. }), "{err}");
    }

    #[test]
    fn a_tampered_signed_index_is_not_signed_on_top_of() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let repo = repo_with_package(d.path(), "a", Some(&k));
        let path = repo.dir().join("index.toml");
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, original.replace("name = \"a\"", "name = \"evil\"")).unwrap();
        make_pkg(d.path(), "b", "1", &[("usr/bin/y", "b")], &[]);
        assert!(matches!(
            repo.publish(&d.path().join("b-1.tar.zst"), Some(&k)),
            Err(StoreError::BadSignature { .. })
        ));
    }

    #[test]
    fn an_existing_unsigned_index_is_not_signed_as_a_side_effect_of_publishing() {
        let d = tempfile::tempdir().unwrap();
        let k = key(1);
        let repo = repo_with_package(d.path(), "a", None);
        make_pkg(d.path(), "b", "1", &[("usr/bin/y", "b")], &[]);
        let err = repo.publish(&d.path().join("b-1.tar.zst"), Some(&k)).unwrap_err();
        assert!(matches!(err, StoreError::UnsignedIndex(_)), "{err}");
        assert!(!repo.is_signed(), "still unsigned");

        // The explicit command does it, after which publishing works.
        repo.sign(&k).unwrap();
        assert!(repo.load(&ring(&[&k])).is_ok());
        repo.publish(&d.path().join("b-1.tar.zst"), Some(&k)).unwrap();
        assert_eq!(repo.load(&ring(&[&k])).unwrap().index.packages.len(), 2);
    }

    #[test]
    fn signing_raises_the_serial_and_can_rotate_the_key() {
        let d = tempfile::tempdir().unwrap();
        let (old, new) = (key(1), key(2));
        let repo = repo_with_package(d.path(), "a", Some(&old));
        let before = repo.load(&ring(&[&old])).unwrap().index.serial;
        repo.sign(&new).unwrap();
        assert!(matches!(repo.load(&ring(&[&old])), Err(StoreError::UnknownKey { .. })), "the old key no longer signs it");
        let after = repo.load(&ring(&[&new])).unwrap().index.serial;
        assert!(after > before, "{after} > {before}");
    }

    #[test]
    fn serial_defaults_to_zero_for_indexes_that_predate_signing() {
        let idx = Index::parse("[[package]]\nname = \"a\"\nversion = \"1\"\nlibc = \"musl\"\nhash = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\narchive = \"a.tar.zst\"\narchive_sha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n", Path::new("i")).unwrap();
        assert_eq!(idx.serial, 0);
    }
}

#[cfg(test)]
mod serial_floor_tests {
    use super::*;
    use crate::testutil::make_pkg;

    #[test]
    fn signing_at_least_never_lowers_and_can_raise_the_serial() {
        let d = tempfile::tempdir().unwrap();
        let key = SecretKey::from_seed([5; 32]);
        make_pkg(d.path(), "a", "1", &[("usr/bin/a", "a")], &[]);
        let repo = LocalRepo::new(d.path().join("r"));
        repo.publish(&d.path().join("a-1.tar.zst"), Some(&key)).unwrap();
        assert_eq!(repo.index_unverified().unwrap().serial, 1);
        repo.sign_at_least(&key, 40).unwrap();
        assert_eq!(repo.index_unverified().unwrap().serial, 40, "raised to the floor");
        repo.sign_at_least(&key, 10).unwrap();
        assert_eq!(repo.index_unverified().unwrap().serial, 41, "a lower floor only means the usual +1");
        let ring = {
            let mut r = Keyring::empty();
            r.insert(key.public());
            Trust::Require(r)
        };
        assert_eq!(repo.load(&ring).unwrap().index.serial, 41, "and it verifies");
    }
}
