//! End-to-end tests of `forge build`: they run the real binary, which really
//! creates the sandbox. They are skipped (with a note) on machines where
//! unprivileged user namespaces are unavailable.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use std::collections::BTreeMap;

use forge_build::sandbox::{Bind, SandboxConfig};
use tempfile::TempDir;

const FORGE: &str = env!("CARGO_BIN_EXE_forge");

fn sandbox_available() -> bool {
    Command::new(FORGE).arg("__sandbox-probe").status().is_ok_and(|s| s.success())
}

macro_rules! require_sandbox {
    () => {
        if !sandbox_available() {
            eprintln!("skipping: unprivileged user namespaces are not available here");
            return;
        }
    };
}

fn sha256(path: &Path) -> String {
    let out = Command::new("sha256sum").arg(path).output().expect("sha256sum");
    String::from_utf8(out.stdout).unwrap().split_whitespace().next().unwrap().to_string()
}

/// A scratch area with a source tarball, a recipe, and output directories.
struct Fx {
    dir: TempDir,
}

impl Fx {
    fn new() -> Self {
        Fx { dir: tempfile::tempdir().unwrap() }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// `src.tar.gz` holding `proj-1.0/<files>`; returns its path.
    fn tarball(&self, files: &[(&str, &str)]) -> PathBuf {
        let mut b = tar::Builder::new(Vec::new());
        for (path, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(0);
            h.set_cksum();
            b.append_data(&mut h, format!("proj-1.0/{path}"), content.as_bytes()).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&b.into_inner().unwrap()).unwrap();
        let path = self.p("proj-1.0.tar.gz");
        fs::write(&path, gz.finish().unwrap()).unwrap();
        path
    }

    /// Write `<name>/recipe.toml` with a file:// source and the given extra TOML.
    fn recipe(&self, name: &str, source: &Path, extra: &str) -> PathBuf {
        let dir = self.p(&format!("recipes/{name}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("recipe.toml"),
            format!(
                "name = \"{name}\"\nversion = \"1.0\"\nlibc = [\"musl\", \"glibc\"]\n\n[[source]]\nurl = \"file://{}\"\nsha256 = \"{}\"\n\n{extra}",
                source.display(),
                sha256(source)
            ),
        )
        .unwrap();
        dir
    }

    fn forge(&self, args: &[&str]) -> Output {
        Command::new(FORGE)
            .args(args)
            .env("FORGE_TEST_SECRET", "must-not-leak-into-the-sandbox")
            .output()
            .expect("run forge")
    }

    /// `forge build <recipe> --libc <libc> --tools host ...` with scratch directories.
    fn build_in(&self, recipe: &Path, libc: &str, out: &str, extra: &[&str]) -> Output {
        let (out, work, cache) = (self.p(out), self.p("work"), self.p("cache"));
        let mut args = vec![
            "build",
            recipe.to_str().unwrap(),
            "--libc",
            libc,
            "--tools",
            "host",
            "--out",
            out.to_str().unwrap(),
            "--work-dir",
            work.to_str().unwrap(),
            "--source-cache",
            cache.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.forge(&args)
    }

    fn build(&self, recipe: &Path) -> Output {
        self.build_in(recipe, "musl", "out", &[])
    }

    fn archives(&self, out: &str) -> Vec<PathBuf> {
        let Ok(rd) = fs::read_dir(self.p(out)) else { return vec![] };
        let mut v: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
        v.sort();
        v
    }

    fn work_dir_entries(&self) -> usize {
        fs::read_dir(self.p("work")).map_or(0, Iterator::count)
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// Unpack a package archive into a scratch store and read one file from it.
fn read_from_archive(fx: &Fx, archive: &Path, file: &str) -> String {
    let store = fx.p("read-store");
    let o = fx.forge(&["store", "--store", store.to_str().unwrap(), "install", archive.to_str().unwrap(), "--unverified"]);
    assert!(o.status.success(), "{}", text(&o));
    let entry = fs::read_dir(&store).unwrap().filter_map(Result::ok).map(|e| e.path()).find(|p| {
        p.file_name().unwrap().to_string_lossy().starts_with(archive.file_name().unwrap().to_string_lossy().split('.').next().unwrap())
    });
    fs::read_to_string(entry.expect("installed entry").join(file)).unwrap()
}

const SIMPLE: &str = "[build]\ncompile = \"cp data.txt built.txt\"\ninstall = \"mkdir -p $DESTDIR/usr/share/proj && cp built.txt $DESTDIR/usr/share/proj/data.txt\"\n";

// ---- tests -------------------------------------------------------------------

#[test]
fn builds_a_package_from_source_inside_the_sandbox() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "hello from the source\n")]);
    let recipe = fx.recipe("proj", &src, SIMPLE);
    let o = fx.build(&recipe);
    assert!(o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("==> forge: compile") && t.contains("==> forge: install"), "{t}");
    assert!(t.contains("not reproducible"), "host tools are flagged: {t}");

    let archives = fx.archives("out");
    assert_eq!(archives.len(), 1, "{archives:?}");
    assert!(archives[0].file_name().unwrap().to_string_lossy().starts_with("proj-1.0-"));
    assert!(t.contains(&sha256(&archives[0])), "the printed checksum matches the file");
    assert_eq!(read_from_archive(&fx, &archives[0], "usr/share/proj/data.txt"), "hello from the source\n");
    assert_eq!(fx.work_dir_entries(), 0, "the temporary build directory is removed");
}

#[test]
fn the_same_inputs_give_a_byte_identical_package() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "reproducible\n")]);
    let recipe = fx.recipe("proj", &src, SIMPLE);
    assert!(fx.build_in(&recipe, "musl", "out1", &[]).status.success());
    assert!(fx.build_in(&recipe, "musl", "out2", &[]).status.success());
    let (a, b) = (fx.archives("out1"), fx.archives("out2"));
    assert_eq!(a[0].file_name(), b[0].file_name(), "same store id");
    assert_eq!(fs::read(&a[0]).unwrap(), fs::read(&b[0]).unwrap(), "same bytes");
}

#[test]
fn the_sandbox_hides_the_host_and_blocks_the_network_and_protects_the_system_directories() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    // Each check exits non-zero if the sandbox is leakier than promised, which fails the build.
    let checks = r#"[build]
compile = '''
[ "$(id -u)" = 0 ] || { echo "not root inside"; exit 1; }
[ ! -e /home ] || { echo "host /home is visible"; exit 1; }
[ ! -e /boot ] || { echo "host /boot is visible"; exit 1; }
[ "$HOME" = /build/home ] || { echo "HOME leaked: $HOME"; exit 1; }
[ "$PATH" = /usr/bin:/usr/sbin:/bin:/sbin ] || { echo "PATH leaked: $PATH"; exit 1; }
[ -z "${FORGE_TEST_SECRET:-}" ] || { echo "the host environment leaked in"; exit 1; }
[ "$(cat /etc/hostname)" = novara-build ] || { echo "wrong hostname"; exit 1; }
if touch /usr/oops 2>/dev/null; then echo "/usr is writable"; exit 1; fi
if touch /etc/oops 2>/dev/null; then echo "/etc is writable"; exit 1; fi
if touch /bin/oops 2>/dev/null; then echo "/bin is writable"; exit 1; fi
touch /build/ok /tmp/ok || { echo "the build and tmp directories must be writable"; exit 1; }
[ "$(grep -c '^00000000' /proc/net/route)" = 0 ] || { echo "there is a default route"; exit 1; }
if (exec 3<>/dev/tcp/1.1.1.1/80) 2>/dev/null; then echo "the network is reachable"; exit 1; fi
procs=$(ls /proc | grep -c '^[0-9][0-9]*$')
[ "$procs" -lt 30 ] || { echo "can see $procs processes, so the host's are visible"; exit 1; }
echo isolated > built.txt
'''
install = "mkdir -p $DESTDIR/r && cp built.txt $DESTDIR/r/result"
"#;
    let recipe = fx.recipe("isolation", &src, checks);
    let o = fx.build(&recipe);
    assert!(o.status.success(), "the sandbox leaked:\n{}", text(&o));
    assert_eq!(read_from_archive(&fx, &fx.archives("out")[0], "r/result"), "isolated\n");
}

#[test]
fn recipe_steps_see_the_documented_environment() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe(
        "envtest",
        &src,
        r#"[build]
compile = '''
{
echo "LIBC=$LIBC"
echo "TARGET=$TARGET"
echo "DESTDIR=$DESTDIR"
echo "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
echo "TZ=$TZ LC_ALL=$LC_ALL"
echo "JOBS=$JOBS"
echo "pwd=$(pwd)"
echo "src=$(cat data.txt)"
echo "recipe_files=$(ls $RECIPE_DIR | tr '\n' ' ')"
echo "sources=$(ls $SOURCES | tr '\n' ' ')"
} > built.txt
'''
install = "mkdir -p $DESTDIR/r && cp built.txt $DESTDIR/r/env"
"#,
    );
    let o = fx.build_in(&recipe, "glibc", "out", &["--jobs", "3"]);
    assert!(o.status.success(), "{}", text(&o));
    let env = read_from_archive(&fx, &fx.archives("out")[0], "r/env");
    for line in [
        "LIBC=glibc",
        "TARGET=x86_64-linux-gnu",
        "DESTDIR=/build/dest",
        "SOURCE_DATE_EPOCH=315532800",
        "TZ=UTC LC_ALL=C",
        "JOBS=3",
        "pwd=/build/extract/proj-1.0",
        "src=x",
        "recipe_files=recipe.toml",
        "sources=proj-1.0.tar.gz",
    ] {
        assert!(env.contains(line), "missing `{line}` in:\n{env}");
    }
}

#[test]
fn a_failing_step_fails_the_build_and_leaves_nothing_behind() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe("bad", &src, "[build]\ncompile = \"echo about-to-fail; exit 7\"\ninstall = \"mkdir -p $DESTDIR/x && touch $DESTDIR/x/f\"\n");
    let o = fx.build(&recipe);
    assert!(!o.status.success());
    let t = text(&o);
    assert!(t.contains("about-to-fail") && t.contains("the build failed"), "{t}");
    assert!(!t.contains("==> forge: install"), "no later step runs after a failure: {t}");
    assert!(fx.archives("out").is_empty(), "no archive is produced");
    assert_eq!(fx.work_dir_entries(), 0, "the build directory is cleaned up");
}

#[test]
fn keep_work_leaves_the_build_directory_for_inspection() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe("bad", &src, "[build]\ncompile = \"exit 1\"\n");
    let o = fx.build_in(&recipe, "musl", "out", &["--keep-work"]);
    assert!(!o.status.success());
    assert_eq!(fx.work_dir_entries(), 1, "kept: {}", text(&o));
}

#[test]
fn a_recipe_that_forgets_destdir_fails_instead_of_silently_succeeding() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    // A classic bug: installing straight into /usr. That must fail (read-only), not vanish.
    let recipe = fx.recipe("forgetful", &src, "[build]\ninstall = \"mkdir -p /usr/share/oops && cp data.txt /usr/share/oops/\"\n");
    let o = fx.build(&recipe);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(fx.archives("out").is_empty());
}

#[test]
fn an_empty_destdir_is_an_error() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe("empty", &src, "[build]\ninstall = \"true\"\n");
    let o = fx.build(&recipe);
    assert!(!o.status.success());
    assert!(text(&o).contains("DESTDIR empty"), "{}", text(&o));
    assert!(fx.archives("out").is_empty());
}

#[test]
fn a_source_with_the_wrong_checksum_is_refused_before_anything_runs() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe("tampered", &src, SIMPLE);
    let toml = fs::read_to_string(recipe.join("recipe.toml")).unwrap();
    fs::write(recipe.join("recipe.toml"), toml.replace(&sha256(&src), &"0".repeat(64))).unwrap();
    let o = fx.build(&recipe);
    assert!(!o.status.success());
    assert!(text(&o).contains("checksum"), "{}", text(&o));
    assert!(!text(&o).contains("==> forge:"), "no build step ran");
}

/// Regression: the sandbox's scratch /tmp used to be mounted after the binds, so
/// anything bound below /tmp (a store or repository kept under /tmp) vanished.
#[test]
fn a_bind_below_tmp_is_visible_and_not_hidden_by_the_scratch_tmp() {
    require_sandbox!();
    let fx = Fx::new();
    let data = fx.p("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("hello"), "visible\n").unwrap();
    let root = fx.p("root");
    fs::create_dir_all(&root).unwrap();

    let (mut binds, mut symlinks) = (Vec::new(), Vec::new());
    for dir in ["/bin", "/sbin", "/lib", "/lib64", "/usr/bin", "/usr/sbin", "/usr/lib", "/usr/lib32", "/usr/lib64", "/usr/libexec"] {
        match fs::symlink_metadata(dir) {
            Ok(m) if m.file_type().is_symlink() => {
                symlinks.push((dir.to_string(), fs::read_link(dir).unwrap().to_string_lossy().into_owned()));
            }
            Ok(m) if m.is_dir() => binds.push(Bind { src: dir.into(), dst: dir.into(), writable: false }),
            _ => {}
        }
    }
    binds.push(Bind { src: data, dst: "/tmp/below-tmp/data".into(), writable: false });
    let cfg = SandboxConfig {
        root_dir: root,
        binds,
        symlinks,
        files: vec![],
        env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        cwd: "/".into(),
        // It must be readable, read-only, and the rest of /tmp must stay a writable scratch area.
        script: "cat /tmp/below-tmp/data/hello && ! touch /tmp/below-tmp/data/x 2>/dev/null && touch /tmp/scratch-ok".into(),
        hostname: "t".into(),
    };
    let cfg_path = fx.p("sandbox.toml");
    fs::write(&cfg_path, toml::to_string(&cfg).unwrap()).unwrap();
    let o = fx.forge(&["__sandbox", cfg_path.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("visible"), "{}", text(&o));
}

#[test]
fn a_source_that_cannot_be_downloaded_fails_the_build_before_anything_runs() {
    require_sandbox!();
    let fx = Fx::new();
    let recipe = fx.p("recipes/remote");
    fs::create_dir_all(&recipe).unwrap();
    // Port 1 on localhost refuses connections immediately: no DNS, no real network.
    fs::write(
        recipe.join("recipe.toml"),
        format!("name = \"remote\"\nversion = \"1\"\nlibc = [\"musl\"]\n[[source]]\nurl = \"http://127.0.0.1:1/r-1.tar.gz\"\nsha256 = \"{}\"\n", "ab".repeat(32)),
    )
    .unwrap();
    let o = fx.build(&recipe);
    assert!(!o.status.success());
    let t = text(&o);
    assert!(t.contains("cannot download") && t.contains("127.0.0.1:1"), "{t}");
    assert!(!t.contains("==> forge:"), "no build step ran");
    assert!(fx.archives("out").is_empty());
}

#[test]
fn unsupported_source_url_schemes_are_refused() {
    require_sandbox!();
    let fx = Fx::new();
    let recipe = fx.p("recipes/ftp");
    fs::create_dir_all(&recipe).unwrap();
    fs::write(
        recipe.join("recipe.toml"),
        format!("name = \"ftp\"\nversion = \"1\"\nlibc = [\"musl\"]\n[[source]]\nurl = \"ftp://example.org/r-1.tar.gz\"\nsha256 = \"{}\"\n", "ab".repeat(32)),
    )
    .unwrap();
    let o = fx.build(&recipe);
    assert!(!o.status.success());
    assert!(text(&o).contains("unsupported source url"), "{}", text(&o));
}

/// The real thing: build with BusyBox and GNU make from the store and no host
/// tools. It needs a stage-0 repository, which `scripts/bootstrap-base.sh`
/// makes (and which needs a compiler and the network), so it only runs when
/// `FORGE_TEST_STAGE0_REPO` points at one:
///
///   FORGE_TEST_STAGE0_REPO=repo/stage0-<stamp> cargo test -p forge --test build store_tools
#[test]
fn store_tools_build_with_a_real_shell_and_make_from_the_stage0_repository() {
    require_sandbox!();
    let Some(repo) = std::env::var_os("FORGE_TEST_STAGE0_REPO") else {
        eprintln!("skipping: set FORGE_TEST_STAGE0_REPO to a repository made by scripts/bootstrap-base.sh");
        return;
    };
    let libc = std::env::var("FORGE_TEST_STAGE0_LIBC").unwrap_or_else(|_| "musl".into());
    // A relative path is relative to the workspace root, not to crates/forge.
    let repo = PathBuf::from(repo);
    let repo = if repo.is_absolute() { repo } else { Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(repo) };
    let repo = fs::canonicalize(&repo).unwrap_or_else(|e| panic!("stage-0 repository {}: {e}", repo.display()));
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/store-tools");

    let fx = Fx::new();
    // The fixture names the real packages; the stage-0 repository holds the host-built
    // `-stage0` ones, so build from a copy that asks for those.
    let recipe = fx.p("store-tools-stage0");
    fs::create_dir_all(&recipe).unwrap();
    fs::copy(fixture.join("Makefile"), recipe.join("Makefile")).unwrap();
    let recipe_text = fs::read_to_string(fixture.join("recipe.toml")).unwrap().replace("\"busybox\", \"make\"", "\"busybox-stage0\", \"make-stage0\"");
    fs::write(recipe.join("recipe.toml"), recipe_text).unwrap();
    let (store, out, work) = (fx.p("store"), fx.p("out"), fx.p("work"));
    let o = fx.forge(&[
        "build", recipe.to_str().unwrap(), "--libc", &libc, "--tools", "store",
        "--store", store.to_str().unwrap(), "--repo", repo.to_str().unwrap(), "--allow-unsigned",
        "--out", out.to_str().unwrap(), "--work-dir", work.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!text(&o).contains("not reproducible"), "store tools are reproducible: {}", text(&o));

    let report = read_from_archive(&fx, &fx.archives("out")[0], "check/result.txt");
    assert!(report.contains("make: 4.4.1"), "{report}");
    assert!(report.contains("busybox"), "the shell is BusyBox from the store: {report}");
    assert!(report.contains("host libc present: no"), "{report}");
    assert!(report.contains("host gcc present: no"), "{report}");

    // Only what the recipe asked for was installed into the store.
    let stored: Vec<String> = fs::read_dir(&store)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(stored.len(), 2, "{stored:?}");
    assert!(stored.iter().any(|s| s.starts_with("busybox-stage0-")) && stored.iter().any(|s| s.starts_with("make-stage0-")));
}

#[test]
fn a_recipe_for_another_libc_is_refused() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe("glibconly", &src, SIMPLE);
    let toml = fs::read_to_string(recipe.join("recipe.toml")).unwrap();
    fs::write(recipe.join("recipe.toml"), toml.replace("[\"musl\", \"glibc\"]", "[\"glibc\"]")).unwrap();
    let o = fx.build_in(&recipe, "musl", "out", &[]);
    assert!(!o.status.success());
    assert!(text(&o).contains("does not support musl"), "{}", text(&o));
}

#[test]
fn per_libc_overrides_change_what_is_built() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe(
        "overridden",
        &src,
        r#"[build]
# Steps run under `sh -eu`, so a variable that may be unset needs a default.
compile = "echo \"base flags=${EXTRA:-}\" > built.txt"
install = "mkdir -p $DESTDIR/r && cp built.txt $DESTDIR/r/f"

[libc_override.musl]
compile = "echo \"musl flags=$EXTRA\" > built.txt"
[libc_override.musl.env]
EXTRA = "-Os"
"#,
    );
    assert!(fx.build_in(&recipe, "musl", "musl-out", &[]).status.success());
    assert!(fx.build_in(&recipe, "glibc", "glibc-out", &[]).status.success());
    let (m, g) = (fx.archives("musl-out"), fx.archives("glibc-out"));
    assert_eq!(read_from_archive(&fx, &m[0], "r/f"), "musl flags=-Os\n");
    assert_eq!(read_from_archive(&fx, &g[0], "r/f"), "base flags=\n");
    assert_ne!(m[0].file_name(), g[0].file_name(), "different libc, different package");
}

#[test]
fn files_next_to_the_recipe_are_visible_and_part_of_the_hash() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let build = "[build]\ncompile = \"ls $RECIPE_DIR | tr '\\\\n' ' ' > built.txt\"\ninstall = \"mkdir -p $DESTDIR/r && cp built.txt $DESTDIR/r/f\"\n";
    let recipe = fx.recipe("patched", &src, build);
    assert!(fx.build_in(&recipe, "musl", "out1", &[]).status.success());
    fs::write(recipe.join("fix.patch"), "--- a\n+++ b\n").unwrap();
    assert!(fx.build_in(&recipe, "musl", "out2", &[]).status.success());
    let (a, b) = (fx.archives("out1"), fx.archives("out2"));
    assert_ne!(a[0].file_name(), b[0].file_name(), "adding a patch changes the package id");
    assert!(read_from_archive(&fx, &b[0], "r/f").contains("fix.patch"));
}

#[test]
fn run_dependencies_are_recorded_by_exact_store_id() {
    require_sandbox!();
    let fx = Fx::new();
    // A dependency, published to a repository.
    let dep_tree = fx.p("dep-tree/usr/lib");
    fs::create_dir_all(&dep_tree).unwrap();
    fs::write(dep_tree.join("libdep.txt"), "dep").unwrap();
    let dep_archive = fx.p("libdep.tar.zst");
    let o = fx.forge(&["store", "pack", fx.p("dep-tree").to_str().unwrap(), "--name", "libdep", "--version", "2.0", "--libc", "musl", "--out", dep_archive.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let keys = fx.p("signing");
    let o = fx.forge(&["key", "generate", "test", "--dir", keys.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let trusted = fx.p("trusted");
    let o = fx.forge(&["key", "trust", keys.join("test.pub").to_str().unwrap(), "--dir", trusted.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let o = fx.forge(&["store", "publish", dep_archive.to_str().unwrap(), "--repo", fx.p("repo").to_str().unwrap(), "--sign", keys.join("test.key").to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let dep_id = text(&o).split_whitespace().nth(1).unwrap().to_string();

    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe("app", &src, &format!("[depends]\nrun = [\"libdep\"]\n\n{SIMPLE}"));

    // Without a repository the dependency cannot be resolved.
    let o = fx.build(&recipe);
    assert!(!o.status.success());
    assert!(text(&o).contains("--repo"), "{}", text(&o));

    let o = fx.build_in(&recipe, "musl", "out", &["--repo", fx.p("repo").to_str().unwrap(), "--keys", trusted.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let manifest = forge_store::read_manifest(fs::File::open(&fx.archives("out")[0]).unwrap()).unwrap();
    assert_eq!(manifest.deps, vec![dep_id]);
}

#[test]
fn store_tools_need_the_build_dependencies_to_exist() {
    require_sandbox!();
    let fx = Fx::new();
    let src = fx.tarball(&[("data.txt", "x\n")]);
    let (out, work, cache, store) = (fx.p("out"), fx.p("work"), fx.p("cache"), fx.p("store"));
    let run = |recipe: &Path| {
        fx.forge(&[
            "build", recipe.to_str().unwrap(), "--libc", "musl", "--tools", "store",
            "--store", store.to_str().unwrap(), "--out", out.to_str().unwrap(),
            "--work-dir", work.to_str().unwrap(), "--source-cache", cache.to_str().unwrap(),
        ])
    };

    // A build dependency that no repository provides.
    let missing = fx.recipe("needs-tools", &src, &format!("[depends]\nbuild = [\"gcc\"]\n\n{SIMPLE}"));
    let o = run(&missing);
    assert!(!o.status.success());
    assert!(text(&o).contains("dependencies"), "{}", text(&o));

    // No build dependencies at all: the sandbox has no shell, and says so.
    let bare = fx.recipe("no-tools", &src, SIMPLE);
    let o = run(&bare);
    assert!(!o.status.success());
    assert!(text(&o).contains("cannot run /bin/sh"), "{}", text(&o));
    assert!(fx.archives("out").is_empty());
}

#[test]
fn signed_repositories_are_enforced_by_build() {
    require_sandbox!();
    let fx = Fx::new();
    let tree = fx.p("t/usr/lib");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("x.txt"), "x").unwrap();
    let archive = fx.p("libdep.tar.zst");
    let o = fx.forge(&["store", "pack", fx.p("t").to_str().unwrap(), "--name", "libdep", "--version", "1", "--libc", "musl", "--out", archive.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let repo = fx.p("repo");
    let o = fx.forge(&["store", "publish", archive.to_str().unwrap(), "--repo", repo.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));

    let src = fx.tarball(&[("data.txt", "x\n")]);
    let recipe = fx.recipe("app", &src, &format!("[depends]\nrun = [\"libdep\"]\n\n{SIMPLE}"));
    let empty = fx.p("nokeys");
    fs::create_dir_all(&empty).unwrap();
    let r = repo.to_str().unwrap();
    let o = fx.build_in(&recipe, "musl", "out", &["--repo", r, "--keys", empty.to_str().unwrap()]);
    assert!(!o.status.success(), "unsigned repo must be refused: {}", text(&o));

    let keys = fx.p("signing");
    let o = fx.forge(&["key", "generate", "k", "--dir", keys.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let key = keys.join("k.key");
    let o = fx.forge(&["repo", "sign", r, "--key", key.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let o = fx.build_in(&recipe, "musl", "out", &["--repo", r, "--keys", empty.to_str().unwrap()]);
    assert!(!o.status.success(), "unknown key must be refused: {}", text(&o));

    let trusted = fx.p("trusted");
    let o = fx.forge(&["key", "trust", keys.join("k.pub").to_str().unwrap(), "--dir", trusted.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let o = fx.forge(&["repo", "verify", r, "--keys", trusted.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let o = fx.build_in(&recipe, "musl", "out", &["--repo", r, "--keys", trusted.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));

    let idx = repo.join("index.toml");
    let t = fs::read_to_string(&idx).unwrap();
    fs::write(&idx, format!("{t}\n# tampered\n")).unwrap();
    let o = fx.forge(&["repo", "verify", r, "--keys", trusted.to_str().unwrap()]);
    assert!(!o.status.success(), "{}", text(&o));
}

#[test]
fn generated_secret_keys_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let keys = fx.p("signing");
    let o = fx.forge(&["key", "generate", "k", "--dir", keys.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    let mode = fs::metadata(keys.join("k.key")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let o = fx.forge(&["key", "generate", "k", "--dir", keys.to_str().unwrap()]);
    assert!(!o.status.success());
}
