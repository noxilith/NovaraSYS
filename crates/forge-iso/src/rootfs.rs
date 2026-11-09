//! Assembling a root filesystem tree from repository packages.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use forge_schema::Libc;
use forge_store::{LocalRepo, Store, Trust};

use crate::{IsoError, io_ctx};

/// Resolve `names` (and their dependencies) in the repositories, verify each archive
/// as it is installed into `store_dir`, and return the store paths in dependency order.
/// Indexes are checked against `trust` like any other consumer.
pub fn fetch(repos: &[PathBuf], trust: &Trust, names: &[String], libc: Libc, store_dir: &Path) -> Result<Vec<PathBuf>, IsoError> {
    let store = Store::new(store_dir);
    let loaded: Vec<(LocalRepo, forge_store::Index)> = repos
        .iter()
        .map(|r| {
            let repo = LocalRepo::new(r);
            let index = repo.load(trust)?.index;
            Ok((repo, index))
        })
        .collect::<Result<_, forge_store::StoreError>>()?;

    // One merged view, so a package can depend on another repository's package.
    let mut merged = forge_store::Index::default();
    for (_, idx) in &loaded {
        for e in &idx.packages {
            merged.insert(e.clone());
        }
    }
    let entries = forge_store::resolve(&merged, names, libc)?;
    let mut paths = Vec::new();
    for e in &entries {
        let (repo, _) = loaded
            .iter()
            .find(|(_, idx)| idx.packages.iter().any(|p| p == e))
            .expect("resolved entries come from a loaded repository");
        let id = store.install(repo.open_archive(e)?, Some(&e.archive_sha256))?;
        paths.push(store.path(&id));
    }
    Ok(paths)
}

/// Copy the package trees into `dest`. Two packages providing the same file is an
/// error (like profiles), and `dest` must start out empty.
pub fn merge(trees: &[PathBuf], dest: &Path) -> Result<(), IsoError> {
    fs::create_dir_all(dest).map_err(io_ctx(format!("creating {}", dest.display())))?;
    if fs::read_dir(dest).map_err(io_ctx("reading the root directory"))?.next().is_some() {
        return Err(IsoError::Invalid { what: "root directory", value: format!("{} is not empty", dest.display()) });
    }
    let mut files = BTreeMap::<PathBuf, bool>::new(); // path -> is a BusyBox applet link
    for t in trees {
        copy_into(t, dest, Path::new(""), &mut files)?;
    }
    Ok(())
}

fn copy_into(src: &Path, dest: &Path, rel: &Path, files: &mut BTreeMap<PathBuf, bool>) -> Result<(), IsoError> {
    let dir = src.join(rel);
    let mut names: Vec<_> = fs::read_dir(&dir).map_err(io_ctx(format!("reading {}", dir.display())))?.collect::<Result<_, _>>().map_err(io_ctx("reading a directory"))?;
    names.sort_by_key(|e| e.file_name());
    for e in names {
        let r = rel.join(e.file_name());
        if r == Path::new(forge_store::MANIFEST_PATH.split('/').next().unwrap_or(".forge")) {
            continue; // the package's own bookkeeping
        }
        let from = src.join(&r);
        let to = dest.join(&r);
        let meta = fs::symlink_metadata(&from).map_err(io_ctx(format!("reading {}", from.display())))?;
        if meta.is_dir() {
            fs::create_dir_all(&to).map_err(io_ctx(format!("creating {}", to.display())))?;
            // Store trees are read-only; the live root is edited afterwards, so keep directories writable.
            fs::set_permissions(&to, fs::Permissions::from_mode(meta.permissions().mode() | 0o200)).map_err(io_ctx("setting permissions"))?;
            copy_into(src, dest, &r, files)?;
        } else {
            let applet = meta.file_type().is_symlink() && is_applet_link(&from);
            if let Some(&existing_applet) = files.get(&r) {
                // A BusyBox applet link is only a fallback: a real package's file replaces it,
                // and an applet link never replaces a real file.
                if applet {
                    continue;
                } else if existing_applet {
                    fs::remove_file(&to).map_err(io_ctx(format!("replacing {}", to.display())))?;
                } else {
                    return Err(IsoError::Conflict(r));
                }
            }
            files.insert(r.clone(), applet);
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&from).map_err(io_ctx("reading a symlink"))?;
                symlink(target, &to).map_err(io_ctx(format!("creating {}", to.display())))?;
            } else {
                fs::copy(&from, &to).map_err(io_ctx(format!("copying {}", from.display())))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let f = t.path().join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, c).unwrap();
        }
        t
    }

    #[test]
    fn packages_share_directories_and_keep_modes_and_links() {
        let a = tree(&[("bin/tool", "a"), (".forge/manifest.toml", "m")]);
        let b = tree(&[("bin/other", "b"), ("lib/x", "x")]);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(a.path().join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("tool", a.path().join("bin/alias")).unwrap();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("root");
        merge(&[a.path().into(), b.path().into()], &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("bin/other")).unwrap(), "b");
        assert_eq!(fs::metadata(dest.join("bin/tool")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read_link(dest.join("bin/alias")).unwrap(), Path::new("tool"));
        assert!(!dest.join(".forge").exists(), "bookkeeping is not part of the system");
    }

    #[test]
    fn read_only_package_directories_stay_writable_in_the_live_root() {
        let a = tree(&[("etc/x", "1")]);
        fs::set_permissions(a.path().join("etc"), fs::Permissions::from_mode(0o555)).unwrap();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("r");
        merge(&[a.path().into()], &dest).unwrap();
        fs::write(dest.join("etc/new"), "ok").unwrap();
        fs::set_permissions(a.path().join("etc"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_file_in_two_packages_is_a_conflict() {
        let a = tree(&[("bin/x", "1")]);
        let b = tree(&[("bin/x", "2")]);
        let out = tempfile::tempdir().unwrap();
        let err = merge(&[a.path().into(), b.path().into()], &out.path().join("r")).unwrap_err();
        assert!(matches!(err, IsoError::Conflict(p) if p == Path::new("bin/x")));
    }

    #[test]
    fn a_non_empty_destination_is_refused() {
        let a = tree(&[("bin/x", "1")]);
        let out = tree(&[("stale", "")]);
        assert!(merge(&[a.path().into()], out.path()).is_err());
    }
}

/// Is `p` a symlink to BusyBox (`busybox`, `../bin/busybox`, `/bin/busybox`)?
fn is_applet_link(p: &Path) -> bool {
    fs::read_link(p).ok().is_some_and(|t| t.file_name().is_some_and(|n| n == "busybox"))
}

#[cfg(test)]
mod applet_tests {
    use super::*;

    fn pkg(files: &[(&str, &str)], links: &[(&str, &str)]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let f = t.path().join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, c).unwrap();
        }
        for (p, target) in links {
            let f = t.path().join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            symlink(target, f).unwrap();
        }
        t
    }

    #[test]
    fn a_real_package_replaces_a_busybox_applet_link_in_either_order() {
        for busybox_first in [true, false] {
            let bb = pkg(&[("bin/busybox", "BB")], &[("sbin/blkid", "../bin/busybox"), ("bin/ls", "busybox")]);
            let real = pkg(&[("sbin/blkid", "REAL")], &[]);
            let out = tempfile::tempdir().unwrap();
            let dest = out.path().join("r");
            let order: Vec<PathBuf> = if busybox_first { vec![bb.path().into(), real.path().into()] } else { vec![real.path().into(), bb.path().into()] };
            merge(&order, &dest).unwrap();
            assert_eq!(fs::read_to_string(dest.join("sbin/blkid")).unwrap(), "REAL", "busybox_first={busybox_first}");
            assert!(fs::symlink_metadata(dest.join("bin/ls")).unwrap().file_type().is_symlink(), "other applets stay");
        }
    }

    #[test]
    fn two_real_files_still_conflict_and_other_symlinks_are_not_applets() {
        let a = pkg(&[("bin/x", "1")], &[]);
        let b = pkg(&[], &[("bin/x", "elsewhere")]);
        let out = tempfile::tempdir().unwrap();
        assert!(matches!(merge(&[a.path().into(), b.path().into()], &out.path().join("r")), Err(IsoError::Conflict(_))));
    }
}
