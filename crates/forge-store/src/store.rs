use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::archive::unpack;
use crate::id::{StoreId, hex};
use crate::manifest::{MANIFEST_PATH, Manifest};
use crate::{StoreError, io_ctx};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory of immutable package entries, one per [`StoreId`].
pub struct Store {
    root: PathBuf,
}

struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Unpacked entries are read-only, so they must be made writable again before
/// they can be deleted. Only directories matter: deleting a file needs write
/// permission on its directory, not on the file. Symlinks are never followed.
pub(crate) fn make_writable(path: &Path) -> Result<(), StoreError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_ctx(format!("reading {}", path.display()))(e)),
    };
    if !meta.is_dir() {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(io_ctx(format!("unprotecting {}", path.display())))?;
    for e in fs::read_dir(path).map_err(io_ctx(format!("reading {}", path.display())))? {
        make_writable(&e.map_err(io_ctx(format!("reading {}", path.display())))?.path())?;
    }
    Ok(())
}

/// Delete a directory tree even if it contains read-only directories.
pub fn remove_tree(path: &Path) -> Result<(), StoreError> {
    make_writable(path)?;
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(io_ctx(format!("removing {}", path.display()))(e))
        }
        _ => Ok(()),
    }
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, id: &StoreId) -> PathBuf {
        self.root.join(id.as_str())
    }

    pub fn has(&self, id: &StoreId) -> bool {
        self.path(id).join(MANIFEST_PATH).is_file()
    }

    /// All installed entries, sorted.
    pub fn list(&self) -> Result<Vec<StoreId>, StoreError> {
        let rd = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_ctx(format!("reading {}", self.root.display()))(e)),
        };
        let mut ids: Vec<StoreId> = rd
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| StoreId::parse(&n))
            .collect();
        ids.sort();
        Ok(ids)
    }

    pub fn manifest(&self, id: &StoreId) -> Result<Manifest, StoreError> {
        let p = self.path(id).join(MANIFEST_PATH);
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(StoreError::NotInStore(id.clone())),
            Err(e) => return Err(io_ctx(format!("reading {}", p.display()))(e)),
        };
        let m: Manifest = toml::from_str(&text).map_err(|e| StoreError::Invalid {
            what: "stored manifest",
            value: format!("{}: {e}", p.display()),
        })?;
        m.validate()?;
        Ok(m)
    }

    /// Install a package archive. The archive is unpacked into a private
    /// temporary directory and fully verified (manifest, every file hash, and
    /// the archive's own SHA-256 if `expected_sha256` is given) *before* it is
    /// moved into place, so a bad archive never appears in the store.
    /// Installing something already present is a no-op.
    pub fn install<R: Read>(&self, archive: R, expected_sha256: Option<&str>) -> Result<StoreId, StoreError> {
        fs::create_dir_all(&self.root).map_err(io_ctx(format!("creating {}", self.root.display())))?;
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join(format!(".tmp-{}-{n}", std::process::id()));

        let mut reader = HashingReader { inner: archive, hasher: Sha256::new() };
        let verified = (|| -> Result<Manifest, StoreError> {
            let manifest = unpack(&mut reader, &tmp)?;
            // Hash the whole archive, including anything after the last tar entry.
            io::copy(&mut reader, &mut io::sink()).map_err(io_ctx("reading the archive"))?;
            if let Some(expected) = expected_sha256 {
                let actual = hex(&reader.hasher.clone().finalize());
                if !expected.eq_ignore_ascii_case(&actual) {
                    return Err(StoreError::ArchiveHashMismatch { expected: expected.to_string(), actual });
                }
            }
            Ok(manifest)
        })();
        let manifest = match verified {
            Ok(m) => m,
            Err(e) => {
                let _ = remove_tree(&tmp);
                return Err(e);
            }
        };

        let id = manifest.id()?;
        let dest = self.path(&id);
        if dest.symlink_metadata().is_ok() {
            remove_tree(&tmp)?;
            return Ok(id);
        }
        if let Err(e) = fs::rename(&tmp, &dest) {
            let _ = remove_tree(&tmp);
            // Another installer may have won the race; that is fine.
            if self.has(&id) {
                return Ok(id);
            }
            return Err(io_ctx(format!("moving the package into {}", dest.display()))(e));
        }
        Ok(id)
    }

    pub fn remove(&self, id: &StoreId) -> Result<(), StoreError> {
        remove_tree(&self.path(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_pkg;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("store"));
        (d, s)
    }

    #[test]
    fn installs_a_verified_archive_and_lists_it() {
        let (d, s) = store();
        let (bytes, packed) = make_pkg(d.path(), "hello", "1.0", &[("usr/bin/hello", "hi")], &[]);
        let id = s.install(&bytes[..], Some(&packed.archive_sha256)).unwrap();
        assert_eq!(id, packed.id);
        assert!(s.has(&id));
        assert_eq!(s.list().unwrap(), vec![id.clone()]);
        assert_eq!(fs::read_to_string(s.path(&id).join("usr/bin/hello")).unwrap(), "hi");
        assert_eq!(s.manifest(&id).unwrap(), packed.manifest);
        assert!(!s.root().join(".tmp-0").exists());
    }

    #[test]
    fn installing_twice_is_a_noop() {
        let (d, s) = store();
        let (bytes, _) = make_pkg(d.path(), "hello", "1.0", &[("a", "x")], &[]);
        let a = s.install(&bytes[..], None).unwrap();
        let b = s.install(&bytes[..], None).unwrap();
        assert_eq!(a, b);
        assert_eq!(s.list().unwrap().len(), 1);
        let leftovers: Vec<_> = fs::read_dir(s.root()).unwrap().filter_map(Result::ok).collect();
        assert_eq!(leftovers.len(), 1, "no temp directories left behind");
    }

    #[test]
    fn a_wrong_archive_checksum_installs_nothing() {
        let (d, s) = store();
        let (bytes, _) = make_pkg(d.path(), "hello", "1.0", &[("a", "x")], &[]);
        let wrong = "0".repeat(64);
        let err = s.install(&bytes[..], Some(&wrong)).unwrap_err();
        assert!(matches!(err, StoreError::ArchiveHashMismatch { .. }));
        assert!(s.list().unwrap().is_empty());
        assert_eq!(fs::read_dir(s.root()).unwrap().count(), 0, "temp directory cleaned up");
    }

    #[test]
    fn a_corrupt_archive_installs_nothing_and_leaves_no_debris() {
        let (d, s) = store();
        let (mut bytes, _) = make_pkg(d.path(), "hello", "1.0", &[("a", "some content here")], &[]);
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        bytes.truncate(bytes.len() - 3);
        assert!(s.install(&bytes[..], None).is_err());
        assert!(s.list().unwrap().is_empty());
        assert_eq!(fs::read_dir(s.root()).unwrap().count(), 0);
    }

    #[test]
    fn remove_deletes_even_read_only_entries() {
        let (d, s) = store();
        let (bytes, _) = make_pkg(d.path(), "hello", "1.0", &[("usr/bin/a", "x")], &[]);
        let id = s.install(&bytes[..], None).unwrap();
        s.remove(&id).unwrap();
        assert!(!s.has(&id));
        s.remove(&id).unwrap(); // already gone: fine
    }

    #[test]
    fn missing_entries_are_reported() {
        let (_d, s) = store();
        let id = StoreId::parse("a-1-0123456789abcdef0123456789abcdef").unwrap();
        assert!(matches!(s.manifest(&id), Err(StoreError::NotInStore(_))));
        assert!(s.list().unwrap().is_empty(), "a store that does not exist yet is empty");
    }
}
