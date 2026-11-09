//! Helpers for building packages in tests.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use forge_schema::Libc;

use crate::archive::{PackOptions, Packed, pack_dir};

/// Build a package from `(path, content)` pairs under `scratch`, returning the
/// archive bytes. Files under a `bin/` directory are made executable.
pub fn make_pkg(
    scratch: &Path,
    name: &str,
    version: &str,
    files: &[(&str, &str)],
    deps: &[&str],
) -> (Vec<u8>, Packed) {
    let src = scratch.join(format!("src-{name}-{version}"));
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(&src).unwrap();
    for (path, content) in files {
        let p = src.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        if path.contains("bin/") {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let out = scratch.join(format!("{name}-{version}.tar.zst"));
    let opts = PackOptions {
        name: name.into(),
        version: version.into(),
        libc: Libc::Musl,
        hash: None,
        deps: deps.iter().map(|d| d.to_string()).collect(),
    };
    let packed = pack_dir(&src, &opts, &out).unwrap();
    (fs::read(&out).unwrap(), packed)
}
