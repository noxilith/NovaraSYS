//! Garbage collection: delete store entries nothing refers to any more.
//!
//! Roots are every package in every profile generation (system and users) and
//! every package a system generation (snapshot) needs. Anything reachable from
//! a root through run-time dependencies is kept; the rest is removed.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::id::StoreId;
use crate::store::{Store, remove_tree};
use crate::{StoreError, io_ctx};

/// Temporary install directories older than this are leftovers of a crashed
/// install. Younger ones may belong to an install that is still running.
const STALE_TMP: Duration = Duration::from_secs(3600);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub kept: Vec<StoreId>,
    pub removed: Vec<StoreId>,
    pub stale_tmp: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct SnapshotMeta {
    #[serde(default)]
    store_paths: Vec<String>,
}

/// Packages that system generations (`gen-N.toml` in the snapshots directory)
/// say they need. Entries that are not store ids are ignored.
pub fn roots_from_snapshots(dir: &Path) -> Result<BTreeSet<StoreId>, StoreError> {
    let mut roots = BTreeSet::new();
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(roots),
        Err(e) => return Err(io_ctx(format!("reading {}", dir.display()))(e)),
    };
    for e in rd.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("gen-") && name.ends_with(".toml")) {
            continue;
        }
        let text = fs::read_to_string(e.path()).map_err(io_ctx(format!("reading {}", e.path().display())))?;
        let meta: SnapshotMeta = toml::from_str(&text).map_err(|err| StoreError::Invalid {
            what: "generation metadata",
            value: format!("{}: {err}", e.path().display()),
        })?;
        roots.extend(meta.store_paths.iter().filter_map(|s| StoreId::parse(s)));
    }
    Ok(roots)
}

impl Store {
    /// Remove everything not reachable from `roots`. With `dry_run`, only
    /// report what would be removed. If any kept package's manifest cannot be
    /// read the whole collection is refused, so a damaged store never causes
    /// deletions based on a guess.
    pub fn gc(&self, roots: &BTreeSet<StoreId>, dry_run: bool) -> Result<GcReport, StoreError> {
        let mut live: BTreeSet<StoreId> = BTreeSet::new();
        let mut stack: Vec<StoreId> = roots.iter().cloned().collect();
        while let Some(id) = stack.pop() {
            if !live.insert(id.clone()) || !self.has(&id) {
                continue;
            }
            stack.extend(self.manifest(&id)?.dep_ids()?);
        }

        let mut report = GcReport::default();
        for id in self.list()? {
            if live.contains(&id) {
                report.kept.push(id);
            } else {
                if !dry_run {
                    self.remove(&id)?;
                }
                report.removed.push(id);
            }
        }

        if let Ok(rd) = fs::read_dir(self.root()) {
            for e in rd.filter_map(Result::ok) {
                let is_tmp = e.file_name().to_string_lossy().starts_with(".tmp-");
                let age = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
                if is_tmp && age.is_some_and(|a| a > STALE_TMP) {
                    if !dry_run {
                        remove_tree(&e.path())?;
                    }
                    report.stale_tmp.push(e.path());
                }
            }
        }
        report.stale_tmp.sort();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_pkg;
    use std::fs::File;

    struct Fx {
        d: tempfile::TempDir,
        store: Store,
    }

    impl Fx {
        fn new() -> Self {
            let d = tempfile::tempdir().unwrap();
            let store = Store::new(d.path().join("store"));
            Fx { d, store }
        }
        fn add(&self, name: &str, deps: &[&StoreId]) -> StoreId {
            let dep_strs: Vec<&str> = deps.iter().map(|d| d.as_str()).collect();
            let (bytes, _) = make_pkg(self.d.path(), name, "1", &[("usr/share/f", name)], &dep_strs);
            self.store.install(&bytes[..], None).unwrap()
        }
    }

    fn set(ids: &[&StoreId]) -> BTreeSet<StoreId> {
        ids.iter().map(|i| (*i).clone()).collect()
    }

    #[test]
    fn removes_unreferenced_packages_and_keeps_roots_and_their_dependencies() {
        let fx = Fx::new();
        let lib = fx.add("lib", &[]);
        let app = fx.add("app", &[&lib]);
        let orphan = fx.add("orphan", &[]);
        let r = fx.store.gc(&set(&[&app]), false).unwrap();
        assert_eq!(r.removed, vec![orphan.clone()]);
        assert_eq!(r.kept.len(), 2);
        assert!(fx.store.has(&app) && fx.store.has(&lib) && !fx.store.has(&orphan));
    }

    #[test]
    fn follows_dependencies_through_several_levels() {
        let fx = Fx::new();
        let a = fx.add("a", &[]);
        let b = fx.add("b", &[&a]);
        let c = fx.add("c", &[&b]);
        let r = fx.store.gc(&set(&[&c]), false).unwrap();
        assert!(r.removed.is_empty());
        assert_eq!(r.kept.len(), 3);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let fx = Fx::new();
        let orphan = fx.add("orphan", &[]);
        let r = fx.store.gc(&BTreeSet::new(), true).unwrap();
        assert_eq!(r.removed, vec![orphan.clone()]);
        assert!(fx.store.has(&orphan));
    }

    #[test]
    fn an_empty_root_set_collects_everything() {
        let fx = Fx::new();
        fx.add("a", &[]);
        fx.add("b", &[]);
        assert_eq!(fx.store.gc(&BTreeSet::new(), false).unwrap().removed.len(), 2);
        assert!(fx.store.list().unwrap().is_empty());
    }

    #[test]
    fn roots_that_are_not_installed_are_ignored() {
        let fx = Fx::new();
        let a = fx.add("a", &[]);
        let ghost = StoreId::parse("ghost-1-0123456789abcdef0123456789abcdef").unwrap();
        let r = fx.store.gc(&set(&[&ghost]), false).unwrap();
        assert_eq!(r.removed, vec![a]);
    }

    #[test]
    fn a_damaged_manifest_stops_the_collection_before_any_deletion() {
        let fx = Fx::new();
        let lib = fx.add("lib", &[]);
        let app = fx.add("app", &[&lib]);
        let orphan = fx.add("orphan", &[]);
        let mf = fx.store.path(&app).join(crate::MANIFEST_PATH);
        // Store entries are read-only; unlock the file to simulate damage.
        crate::store::make_writable(&fx.store.path(&app)).unwrap();
        fs::set_permissions(&mf, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        fs::write(&mf, "this is not a manifest").unwrap();
        assert!(fx.store.gc(&set(&[&app]), false).is_err());
        assert!(fx.store.has(&orphan), "nothing was deleted");
        assert!(fx.store.has(&lib));
    }

    #[test]
    fn stale_temp_directories_are_cleaned_but_fresh_ones_are_left_alone() {
        let fx = Fx::new();
        fx.add("a", &[]);
        let old = fx.store.root().join(".tmp-1-0");
        let fresh = fx.store.root().join(".tmp-2-0");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&fresh).unwrap();
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        File::open(&old).unwrap().set_modified(two_hours_ago).unwrap();
        let a = fx.store.list().unwrap();
        let dry = fx.store.gc(&set(&[&a[0]]), true).unwrap();
        assert_eq!(dry.stale_tmp, vec![old.clone()]);
        assert!(old.exists(), "dry run keeps it");
        fx.store.gc(&set(&[&a[0]]), false).unwrap();
        assert!(!old.exists());
        assert!(fresh.exists(), "an install may still be running there");
    }

    #[test]
    fn reads_store_paths_from_system_generation_metadata() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("gen-1.toml"),
            "id = 1\nstore_paths = [\"a-1-0123456789abcdef0123456789abcdef\", \"not a store id\"]\n",
        )
        .unwrap();
        fs::write(d.path().join("gen-2.toml"), "id = 2\n").unwrap();
        fs::write(d.path().join("unrelated.toml"), "store_paths = [\"b-1-0123456789abcdef0123456789abcdef\"]").unwrap();
        let roots = roots_from_snapshots(d.path()).unwrap();
        assert_eq!(roots.len(), 1);
        assert!(roots.iter().next().unwrap().as_str().starts_with("a-1-"));
        assert!(roots_from_snapshots(&d.path().join("missing")).unwrap().is_empty());
    }
}
