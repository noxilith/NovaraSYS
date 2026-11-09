//! Profiles: the set of packages one user (or the system) can see.
//!
//! A profile directory holds numbered generations (`gen-N/`, a tree of
//! symlinks into the store, plus `gen-N.toml` describing it) and `current`, a
//! symlink to the active generation. Switching builds a complete new
//! generation first and only then flips `current`, so a profile is never
//! half-updated and any earlier generation can be restored instantly.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::id::StoreId;
use crate::manifest::Kind;
use crate::store::Store;
use crate::{StoreError, io_ctx};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileGen {
    pub id: u32,
    /// Seconds since the Unix epoch.
    pub created: u64,
    /// Store ids of the packages in this generation.
    pub packages: Vec<String>,
}

pub struct Profile {
    dir: PathBuf,
}

fn parse_ids(strs: &[String]) -> Result<BTreeSet<StoreId>, StoreError> {
    strs.iter()
        .map(|s| {
            StoreId::parse(s).ok_or_else(|| StoreError::Invalid { what: "store id", value: s.clone() })
        })
        .collect()
}

impl Profile {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Profile { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The symlink to the active generation; put its subdirectories on `PATH`.
    pub fn current_link(&self) -> PathBuf {
        self.dir.join("current")
    }

    fn gen_dir(&self, id: u32) -> PathBuf {
        self.dir.join(format!("gen-{id}"))
    }

    fn gen_meta(&self, id: u32) -> PathBuf {
        self.dir.join(format!("gen-{id}.toml"))
    }

    /// All generations, oldest first.
    pub fn generations(&self) -> Result<Vec<ProfileGen>, StoreError> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_ctx(format!("reading {}", self.dir.display()))(e)),
        };
        let mut ids: Vec<u32> = rd
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| n.strip_prefix("gen-")?.strip_suffix(".toml")?.parse().ok())
            .collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|id| {
                let p = self.gen_meta(id);
                let text = fs::read_to_string(&p).map_err(io_ctx(format!("reading {}", p.display())))?;
                toml::from_str(&text).map_err(|e| StoreError::Invalid {
                    what: "profile generation metadata",
                    value: format!("{}: {e}", p.display()),
                })
            })
            .collect()
    }

    /// The active generation, if any.
    pub fn current(&self) -> Result<Option<u32>, StoreError> {
        match fs::read_link(self.current_link()) {
            Ok(t) => Ok(t.to_str().and_then(|s| s.strip_prefix("gen-")).and_then(|n| n.parse().ok())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_ctx(format!("reading {}", self.current_link().display()))(e)),
        }
    }

    /// Packages in the active generation (empty if there is none yet).
    pub fn current_packages(&self) -> Result<BTreeSet<StoreId>, StoreError> {
        let Some(cur) = self.current()? else { return Ok(BTreeSet::new()) };
        match self.generations()?.into_iter().find(|g| g.id == cur) {
            Some(g) => parse_ids(&g.packages),
            None => Ok(BTreeSet::new()),
        }
    }

    /// Packages in *every* generation: all of them must be kept by garbage
    /// collection, because any generation can be rolled back to.
    pub fn all_packages(&self) -> Result<BTreeSet<StoreId>, StoreError> {
        let mut all = BTreeSet::new();
        for g in self.generations()? {
            all.extend(parse_ids(&g.packages)?);
        }
        Ok(all)
    }

    /// Make `packages` the visible set. Returns the new generation number, or
    /// `None` if the active generation already holds exactly this set.
    pub fn switch(&self, store: &Store, packages: &BTreeSet<StoreId>, now: u64) -> Result<Option<u32>, StoreError> {
        if self.current()?.is_some() && &self.current_packages()? == packages {
            return Ok(None);
        }
        fs::create_dir_all(&self.dir).map_err(io_ctx(format!("creating {}", self.dir.display())))?;
        let id = self.generations()?.last().map_or(1, |g| g.id + 1);
        let tmp = self.dir.join(format!("gen-{id}.tmp"));
        let _ = fs::remove_dir_all(&tmp);

        let built = self.build_tree(store, packages, &tmp);
        if let Err(e) = built {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        let meta = ProfileGen {
            id,
            created: now,
            packages: packages.iter().map(|p| p.as_str().to_string()).collect(),
        };
        let meta_tmp = self.dir.join(format!("gen-{id}.toml.tmp"));
        let finish = (|| -> Result<(), StoreError> {
            fs::write(&meta_tmp, toml::to_string(&meta).expect("serializes"))
                .map_err(io_ctx(format!("writing {}", meta_tmp.display())))?;
            fs::rename(&meta_tmp, self.gen_meta(id)).map_err(io_ctx("saving generation metadata"))?;
            fs::rename(&tmp, self.gen_dir(id)).map_err(io_ctx("saving the generation"))?;
            self.set_current(id)
        })();
        if let Err(e) = finish {
            let _ = fs::remove_file(&meta_tmp);
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Ok(Some(id))
    }

    fn build_tree(&self, store: &Store, packages: &BTreeSet<StoreId>, tmp: &Path) -> Result<(), StoreError> {
        fs::create_dir_all(tmp).map_err(io_ctx(format!("creating {}", tmp.display())))?;
        // path -> (owner, kind, is a BusyBox applet link). An applet link is only a fallback: a real package's
        // file replaces it, and it never replaces a real file.
        let mut owners: BTreeMap<String, (StoreId, Kind, bool)> = BTreeMap::new();
        for pkg in packages {
            let manifest = store.manifest(pkg)?;
            for f in &manifest.files {
                let dest = tmp.join(&f.path);
                let applet = f.kind == Kind::Symlink
                    && f.target.as_deref().is_some_and(|t| Path::new(t).file_name().is_some_and(|n| n == "busybox"));
                match (owners.get(&f.path), f.kind) {
                    // Directories are shared: both packages just need it to exist.
                    (Some((_, Kind::Dir, _)), Kind::Dir) => {}
                    (Some(_), _) if applet => {}
                    (Some((_, _, true)), _) => {
                        fs::remove_file(&dest).map_err(io_ctx(format!("replacing {}", dest.display())))?;
                        symlink(store.path(pkg).join(&f.path), &dest)
                            .map_err(io_ctx(format!("linking {}", dest.display())))?;
                        owners.insert(f.path.clone(), (pkg.clone(), f.kind, false));
                    }
                    (Some((owner, _, _)), _) => {
                        return Err(StoreError::Conflict {
                            path: f.path.clone(),
                            a: owner.clone(),
                            b: pkg.clone(),
                        });
                    }
                    (None, Kind::Dir) => {
                        fs::create_dir(&dest).map_err(io_ctx(format!("creating {}", dest.display())))?;
                        owners.insert(f.path.clone(), (pkg.clone(), Kind::Dir, false));
                    }
                    (None, _) => {
                        symlink(store.path(pkg).join(&f.path), &dest)
                            .map_err(io_ctx(format!("linking {}", dest.display())))?;
                        owners.insert(f.path.clone(), (pkg.clone(), f.kind, applet));
                    }
                }
            }
        }
        Ok(())
    }

    /// Make generation `id` the active one (this is also how you roll back).
    pub fn set_current(&self, id: u32) -> Result<(), StoreError> {
        if !self.gen_dir(id).is_dir() {
            return Err(StoreError::NoSuchGeneration(id));
        }
        let tmp = self.dir.join(format!("current.tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        symlink(format!("gen-{id}"), &tmp).map_err(io_ctx(format!("linking {}", tmp.display())))?;
        fs::rename(&tmp, self.current_link()).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            io_ctx("activating the generation")(e)
        })
    }

    pub fn remove_generation(&self, id: u32) -> Result<(), StoreError> {
        if self.current()? == Some(id) {
            return Err(StoreError::CurrentGeneration(id));
        }
        if !self.gen_dir(id).exists() && !self.gen_meta(id).exists() {
            return Err(StoreError::NoSuchGeneration(id));
        }
        fs::remove_dir_all(self.gen_dir(id)).or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
            .map_err(io_ctx(format!("removing generation {id}")))?;
        fs::remove_file(self.gen_meta(id)).or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
            .map_err(io_ctx(format!("removing generation {id} metadata")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_pkg;

    struct Fx {
        d: tempfile::TempDir,
        store: Store,
        profile: Profile,
    }

    impl Fx {
        fn new() -> Self {
            let d = tempfile::tempdir().unwrap();
            let store = Store::new(d.path().join("store"));
            let profile = Profile::new(d.path().join("profile"));
            Fx { d, store, profile }
        }
        fn add(&self, name: &str, files: &[(&str, &str)]) -> StoreId {
            let (bytes, _) = make_pkg(self.d.path(), name, "1.0", files, &[]);
            self.store.install(&bytes[..], None).unwrap()
        }
        fn set(ids: &[&StoreId]) -> BTreeSet<StoreId> {
            ids.iter().map(|i| (*i).clone()).collect()
        }
    }


    /// A package holding real files plus symlinks (`links`: path -> target), packed from a scratch tree.
    fn add_with_links(fx: &Fx, name: &str, files: &[(&str, &str)], links: &[(&str, &str)]) -> StoreId {
        let src = fx.d.path().join(format!("src-links-{name}"));
        for (p, c) in files {
            fs::create_dir_all(src.join(p).parent().unwrap()).unwrap();
            fs::write(src.join(p), c).unwrap();
        }
        for (p, t) in links {
            fs::create_dir_all(src.join(p).parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(t, src.join(p)).unwrap();
        }
        let out = fx.d.path().join(format!("{name}.tar.zst"));
        let opts = crate::PackOptions { name: name.into(), version: "1.0".into(), libc: forge_schema::Libc::Musl, hash: None, deps: vec![] };
        crate::pack_dir(&src, &opts, &out).unwrap();
        fx.store.install(fs::File::open(&out).unwrap(), None).unwrap()
    }

    #[test]
    fn a_real_package_replaces_a_busybox_applet_link_in_either_order() {
        // The set is ordered by id: "bc" sorts before "busybox", "zzbc" after it, so both orders are exercised.
        for real in ["bc", "zzbc"] {
            let fx = Fx::new();
            let bb = add_with_links(&fx, "busybox", &[("bin/busybox", "BB")], &[("usr/bin/bc", "../../bin/busybox"), ("bin/ls", "busybox")]);
            let bc = fx.add(real, &[("usr/bin/bc", "REAL")]);
            let ids = Fx::set(&[&bb, &bc]);
            fx.profile.switch(&fx.store, &ids, 1).unwrap();
            let cur = fx.profile.current_link();
            assert_eq!(fs::read_to_string(cur.join("usr/bin/bc")).unwrap(), "REAL", "the real bc wins");
            assert_eq!(fs::read_link(cur.join("bin/ls")).unwrap(), Path::new(&format!("{}", fx.store.path(&bb).join("bin/ls").display())), "other applets stay");
        }
    }

    #[test]
    fn two_real_files_at_one_path_are_still_a_conflict() {
        let fx = Fx::new();
        let a = fx.add("a", &[("usr/bin/x", "1")]);
        let b = fx.add("b", &[("usr/bin/x", "2")]);
        assert!(fx.profile.switch(&fx.store, &Fx::set(&[&a, &b]), 1).is_err());
        let c = add_with_links(&fx, "c", &[], &[("usr/bin/x", "elsewhere")]);
        assert!(fx.profile.switch(&fx.store, &Fx::set(&[&a, &c]), 2).is_err(), "a link to anything but busybox is no applet");
    }
    #[test]
    fn links_package_files_into_the_profile() {
        let fx = Fx::new();
        let kitty = fx.add("kitty", &[("usr/bin/kitty", "k")]);
        let n = fx.profile.switch(&fx.store, &Fx::set(&[&kitty]), 10).unwrap();
        assert_eq!(n, Some(1));
        let link = fx.profile.current_link().join("usr/bin/kitty");
        assert_eq!(fs::read_to_string(&link).unwrap(), "k", "readable through the profile");
        assert_eq!(fs::read_link(&link).unwrap(), fx.store.path(&kitty).join("usr/bin/kitty"));
        assert_eq!(fx.profile.current().unwrap(), Some(1));
        assert!(!fx.profile.current_link().join(".forge").exists(), "internal files are not exposed");
    }

    #[test]
    fn packages_share_directories_but_not_files() {
        let fx = Fx::new();
        let a = fx.add("a", &[("usr/bin/a", "a")]);
        let b = fx.add("b", &[("usr/bin/b", "b")]);
        fx.profile.switch(&fx.store, &Fx::set(&[&a, &b]), 10).unwrap();
        let bin = fx.profile.current_link().join("usr/bin");
        assert!(bin.join("a").exists() && bin.join("b").exists());
    }

    #[test]
    fn a_file_provided_by_two_packages_is_a_conflict_and_changes_nothing() {
        let fx = Fx::new();
        let a = fx.add("a", &[("usr/bin/tool", "from a")]);
        let b = fx.add("b", &[("usr/bin/tool", "from b")]);
        fx.profile.switch(&fx.store, &Fx::set(&[&a]), 10).unwrap();
        let err = fx.profile.switch(&fx.store, &Fx::set(&[&a, &b]), 11).unwrap_err();
        assert!(matches!(&err, StoreError::Conflict { path, .. } if path == "usr/bin/tool"), "{err}");
        assert_eq!(fx.profile.current().unwrap(), Some(1), "still on the old generation");
        assert_eq!(fx.profile.generations().unwrap().len(), 1);
        assert!(!fx.profile.dir().join("gen-2.tmp").exists(), "no half-built generation left");
        assert_eq!(fs::read_to_string(fx.profile.current_link().join("usr/bin/tool")).unwrap(), "from a");
    }

    #[test]
    fn switching_to_the_same_set_creates_no_new_generation() {
        let fx = Fx::new();
        let a = fx.add("a", &[("x", "1")]);
        assert_eq!(fx.profile.switch(&fx.store, &Fx::set(&[&a]), 1).unwrap(), Some(1));
        assert_eq!(fx.profile.switch(&fx.store, &Fx::set(&[&a]), 2).unwrap(), None);
        assert_eq!(fx.profile.generations().unwrap().len(), 1);
    }

    #[test]
    fn rollback_restores_the_previous_set_instantly() {
        let fx = Fx::new();
        let a = fx.add("a", &[("usr/bin/a", "a")]);
        let b = fx.add("b", &[("usr/bin/b", "b")]);
        fx.profile.switch(&fx.store, &Fx::set(&[&a]), 1).unwrap();
        fx.profile.switch(&fx.store, &Fx::set(&[&a, &b]), 2).unwrap();
        assert!(fx.profile.current_link().join("usr/bin/b").exists());
        fx.profile.set_current(1).unwrap();
        assert!(!fx.profile.current_link().join("usr/bin/b").exists(), "b is gone from the view");
        assert!(fx.profile.current_link().join("usr/bin/a").exists());
        assert_eq!(fx.profile.current_packages().unwrap(), Fx::set(&[&a]));
        assert_eq!(fx.profile.all_packages().unwrap(), Fx::set(&[&a, &b]), "b is still protected from gc");
    }

    #[test]
    fn removing_packages_removes_them_from_the_view() {
        let fx = Fx::new();
        let a = fx.add("a", &[("usr/bin/a", "a")]);
        let b = fx.add("b", &[("usr/bin/b", "b")]);
        fx.profile.switch(&fx.store, &Fx::set(&[&a, &b]), 1).unwrap();
        fx.profile.switch(&fx.store, &Fx::set(&[&a]), 2).unwrap();
        assert!(!fx.profile.current_link().join("usr/bin/b").exists());
        fx.profile.switch(&fx.store, &BTreeSet::new(), 3).unwrap();
        assert!(fx.profile.current_packages().unwrap().is_empty());
    }

    #[test]
    fn a_package_missing_from_the_store_is_an_error() {
        let fx = Fx::new();
        let ghost = StoreId::parse("ghost-1-0123456789abcdef0123456789abcdef").unwrap();
        let err = fx.profile.switch(&fx.store, &Fx::set(&[&ghost]), 1).unwrap_err();
        assert!(matches!(err, StoreError::NotInStore(_)));
        assert!(fx.profile.generations().unwrap().is_empty());
        assert_eq!(fx.profile.current().unwrap(), None);
    }

    #[test]
    fn generations_can_be_removed_except_the_current_one() {
        let fx = Fx::new();
        let a = fx.add("a", &[("x", "1")]);
        let b = fx.add("b", &[("y", "1")]);
        fx.profile.switch(&fx.store, &Fx::set(&[&a]), 1).unwrap();
        fx.profile.switch(&fx.store, &Fx::set(&[&b]), 2).unwrap();
        assert!(matches!(fx.profile.remove_generation(2), Err(StoreError::CurrentGeneration(2))));
        fx.profile.remove_generation(1).unwrap();
        assert_eq!(fx.profile.generations().unwrap().len(), 1);
        assert!(!fx.profile.dir().join("gen-1").exists());
        assert!(matches!(fx.profile.remove_generation(1), Err(StoreError::NoSuchGeneration(1))));
        assert!(matches!(fx.profile.set_current(9), Err(StoreError::NoSuchGeneration(9))));
    }

    #[test]
    fn an_empty_profile_has_no_generations() {
        let fx = Fx::new();
        assert_eq!(fx.profile.current().unwrap(), None);
        assert!(fx.profile.generations().unwrap().is_empty());
        assert!(fx.profile.current_packages().unwrap().is_empty());
    }
}
