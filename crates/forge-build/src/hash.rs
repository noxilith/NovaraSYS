//! The build-input hash: it names the package in the store, so it must change
//! whenever anything that could change the output changes, and must not
//! change otherwise.

use std::fs;
use std::path::Path;

use forge_schema::{Effective, Libc, Recipe};
use sha2::{Digest, Sha256};

use crate::{BuildError, io_ctx};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Hash every regular file under `dir` (path and content), in a fixed order,
/// so patches and helper files shipped next to a recipe are part of the input.
fn hash_dir(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<(), BuildError> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(io_ctx(format!("reading {}", dir.display())))?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let rel_path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let meta = fs::symlink_metadata(&path).map_err(io_ctx(format!("reading {}", path.display())))?;
        if meta.is_dir() {
            hash_dir(&path, &rel_path, out)?;
        } else if meta.is_file() {
            let bytes = fs::read(&path).map_err(io_ctx(format!("reading {}", path.display())))?;
            out.push(format!("file {rel_path} {}", sha256(&bytes)));
        } else {
            return Err(BuildError::Recipe(format!(
                "{} is not a regular file or directory; recipe directories may only contain those",
                path.display()
            )));
        }
    }
    Ok(())
}

/// The 64-hex-character hash of everything that goes into one build.
///
/// `tools` names where the build tools come from (`host` builds are not
/// reproducible, so they get a different hash than store-tool builds).
/// `build_deps` and `run_deps` are store ids.
pub fn input_hash(
    recipe: &Recipe,
    recipe_dir: &Path,
    libc: Libc,
    effective: &Effective,
    tools: &str,
    build_deps: &[String],
    run_deps: &[String],
) -> Result<String, BuildError> {
    let mut lines = vec![
        "forge-input-v1".to_string(),
        format!("name {}", recipe.name),
        format!("version {}", recipe.version),
        format!("libc {}", libc.as_str()),
        format!("tools {tools}"),
    ];
    for s in &recipe.sources {
        lines.push(format!("source {} {}", s.url, s.sha256.to_ascii_lowercase()));
    }
    // Phase text goes through a hash each so a multi-line command cannot be
    // confused with the lines around it.
    let b = &effective.build;
    for (name, text) in [
        ("prepare", &b.prepare),
        ("configure", &b.configure),
        ("compile", &b.compile),
        ("install", &b.install),
    ] {
        lines.push(format!("phase {name} {}", text.as_deref().map_or("-".into(), |t| sha256(t.as_bytes()))));
    }
    for (k, v) in &effective.env {
        lines.push(format!("env {k} {}", sha256(v.as_bytes())));
    }
    let mut files = Vec::new();
    hash_dir(recipe_dir, "", &mut files)?;
    lines.extend(files);
    let mut build_deps = build_deps.to_vec();
    build_deps.sort();
    let mut run_deps = run_deps.to_vec();
    run_deps.sort();
    lines.extend(build_deps.iter().map(|d| format!("build-dep {d}")));
    lines.extend(run_deps.iter().map(|d| format!("run-dep {d}")));
    Ok(sha256(lines.join("\n").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn recipe(extra: &str) -> Recipe {
        Recipe::parse(&format!(
            "name = \"p\"\nversion = \"1\"\nlibc = [\"glibc\", \"musl\"]\n[[source]]\nurl = \"file:///x.tar.gz\"\nsha256 = \"{SRC_HASH}\"\n[build]\ncompile = \"make\"\ninstall = \"make install\"\n{extra}"
        ))
        .unwrap()
    }

    fn hash(r: &Recipe, dir: &Path, libc: Libc, tools: &str, bd: &[&str], rd: &[&str]) -> String {
        let eff = r.effective(libc).unwrap();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        input_hash(r, dir, libc, &eff, tools, &s(bd), &s(rd)).unwrap()
    }

    fn recipe_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("recipe.toml"), "x").unwrap();
        d
    }

    #[test]
    fn is_stable_for_identical_inputs() {
        let d = recipe_dir();
        let r = recipe("");
        let h = hash(&r, d.path(), Libc::Musl, "store", &[], &[]);
        assert_eq!(h.len(), 64);
        assert_eq!(h, hash(&r, d.path(), Libc::Musl, "store", &[], &[]));
    }

    #[test]
    fn changes_with_every_kind_of_input() {
        let d = recipe_dir();
        let base = hash(&recipe(""), d.path(), Libc::Musl, "store", &["a-1-00000000000000000000000000000000"], &[]);
        let differs = |h: String| assert_ne!(h, base);

        differs(hash(&recipe(""), d.path(), Libc::Glibc, "store", &["a-1-00000000000000000000000000000000"], &[]));
        differs(hash(&recipe(""), d.path(), Libc::Musl, "host", &["a-1-00000000000000000000000000000000"], &[]));
        differs(hash(&recipe(""), d.path(), Libc::Musl, "store", &["a-2-00000000000000000000000000000000"], &[]));
        differs(hash(&recipe(""), d.path(), Libc::Musl, "store", &["a-1-00000000000000000000000000000000"], &["b-1-11111111111111111111111111111111"]));
        differs(hash(&recipe("[libc_override.musl]\ninstall = \"make install-x\"\n"), d.path(), Libc::Musl, "store", &["a-1-00000000000000000000000000000000"], &[]));
        differs(hash(&recipe("[libc_override.musl.env]\nCFLAGS = \"-Os\"\n"), d.path(), Libc::Musl, "store", &["a-1-00000000000000000000000000000000"], &[]));
        let changed_src = recipe("").sources[0].url.replace("x.tar.gz", "y.tar.gz");
        let mut r = recipe("");
        r.sources[0].url = changed_src;
        differs(hash(&r, d.path(), Libc::Musl, "store", &["a-1-00000000000000000000000000000000"], &[]));

        // A patch (any file next to the recipe) is an input too.
        fs::write(d.path().join("fix.patch"), "--- a\n+++ b\n").unwrap();
        differs(hash(&recipe(""), d.path(), Libc::Musl, "store", &["a-1-00000000000000000000000000000000"], &[]));
    }

    #[test]
    fn dependency_order_does_not_matter() {
        let d = recipe_dir();
        let r = recipe("");
        let (a, b) = ("a-1-00000000000000000000000000000000", "b-1-11111111111111111111111111111111");
        assert_eq!(
            hash(&r, d.path(), Libc::Musl, "store", &[a, b], &[]),
            hash(&r, d.path(), Libc::Musl, "store", &[b, a], &[])
        );
    }

    #[test]
    fn a_symlink_in_the_recipe_directory_is_refused() {
        let d = recipe_dir();
        std::os::unix::fs::symlink("/etc/passwd", d.path().join("sneaky")).unwrap();
        let r = recipe("");
        let eff = r.effective(Libc::Musl).unwrap();
        assert!(input_hash(&r, d.path(), Libc::Musl, &eff, "store", &[], &[]).is_err());
    }
}
