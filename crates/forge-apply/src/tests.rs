use std::fs;
use std::path::Path;

use forge_schema::SystemDecl;
use tempfile::TempDir;

use super::*;

fn decl(extra: &str) -> SystemDecl {
    SystemDecl::parse(&format!("hostname = \"novara\"\nlibc = \"musl\"\n{extra}")).unwrap()
}

/// A root with the OpenRC scripts the tests may enable.
fn root() -> TempDir {
    let t = tempfile::tempdir().unwrap();
    let init = t.path().join("etc/init.d");
    fs::create_dir_all(&init).unwrap();
    for s in ["sshd", "chronyd", "networking"] {
        fs::write(init.join(s), "#!/sbin/openrc-run\n").unwrap();
    }
    t
}

/// The key that signs every test repository, and the policy that trusts it.
fn test_key() -> SecretKey {
    SecretKey::from_seed([9; 32])
}

fn trust_of(keys: &[&SecretKey]) -> Trust {
    let mut ring = Keyring::empty();
    for k in keys {
        ring.insert(k.public());
    }
    Trust::Require(ring)
}

fn opts() -> Options {
    Options { rebuild: false, trust: trust_of(&[&test_key()]), fetch: Default::default() }
}

fn plan(d: &SystemDecl, r: &Path) -> Plan {
    plan_system(d, r, opts()).unwrap()
}

fn read(r: &Path, rel: &str) -> String {
    fs::read_to_string(r.join(rel)).unwrap()
}

#[test]
fn fresh_root_gets_basic_settings() {
    let r = root();
    let p = plan(&decl(""), r.path());
    let sections: Vec<_> = p.changes.iter().map(|c| c.section).collect();
    for s in ["hostname", "timezone", "locale", "keymap"] {
        assert!(sections.contains(&s), "missing {s}");
    }
    assert!(p.changes.iter().all(|c| c.kind == Kind::Add));
    apply(&p).unwrap();
    assert_eq!(read(r.path(), "etc/hostname"), "novara\n");
    assert_eq!(read(r.path(), "etc/locale.conf"), "LANG=en_US.UTF-8\n");
    assert_eq!(read(r.path(), "etc/conf.d/keymaps"), "keymap=\"us\"\n");
    assert_eq!(
        fs::read_link(r.path().join("etc/localtime")).unwrap(),
        Path::new("/usr/share/zoneinfo/UTC")
    );
}

#[test]
fn second_apply_has_nothing_to_do() {
    let r = root();
    let d = decl("kernel_modules = [\"btrfs\"]\n[init]\nservices = [\"sshd\"]\n");
    apply(&plan(&d, r.path())).unwrap();
    assert!(plan(&d, r.path()).is_empty());
}

#[test]
fn rebuild_reapplies_everything() {
    let r = root();
    let d = decl("[init]\nservices = [\"sshd\"]\n");
    apply(&plan(&d, r.path())).unwrap();
    let p = plan_system(&d, r.path(), Options { rebuild: true, ..opts() }).unwrap();
    assert!(!p.changes.is_empty());
    assert!(p.changes.iter().all(|c| c.kind == Kind::Modify));
    assert!(p.changes.iter().all(|c| c.summary.ends_with("(re-apply)")));
    apply(&p).unwrap();
}

#[test]
fn changed_hostname_is_a_modify() {
    let r = root();
    apply(&plan(&decl(""), r.path())).unwrap();
    let d = SystemDecl::parse("hostname = \"other\"\nlibc = \"musl\"\n").unwrap();
    let p = plan(&d, r.path());
    assert_eq!(p.changes.len(), 1);
    assert_eq!(p.changes[0].kind, Kind::Modify);
    apply(&p).unwrap();
    assert_eq!(read(r.path(), "etc/hostname"), "other\n");
}

#[test]
fn enabling_services_links_them_into_the_default_runlevel() {
    let r = root();
    let d = decl("[init]\nservices = [\"sshd\", \"chronyd\"]\n");
    apply(&plan(&d, r.path())).unwrap();
    for s in ["sshd", "chronyd"] {
        let link = r.path().join(format!("etc/runlevels/default/{s}"));
        assert_eq!(fs::read_link(link).unwrap(), Path::new(&format!("/etc/init.d/{s}")));
    }
    assert!(read(r.path(), "var/lib/forge/state.toml").contains("sshd"));
}

#[test]
fn only_services_forge_enabled_are_disabled_again() {
    let r = root();
    // `networking` was enabled by someone else (e.g. the live ISO).
    let rl = r.path().join("etc/runlevels/default");
    fs::create_dir_all(&rl).unwrap();
    symlink("/etc/init.d/networking", rl.join("networking")).unwrap();

    apply(&plan(&decl("[init]\nservices = [\"sshd\"]\n"), r.path())).unwrap();
    let p = plan(&decl(""), r.path());
    let removals: Vec<_> = p.changes.iter().filter(|c| c.kind == Kind::Remove).collect();
    assert_eq!(removals.len(), 1);
    assert!(removals[0].summary.contains("sshd"));
    apply(&p).unwrap();

    assert!(rl.join("sshd").symlink_metadata().is_err());
    assert!(rl.join("networking").symlink_metadata().is_ok(), "must not touch unmanaged services");
    assert!(plan(&decl(""), r.path()).is_empty());
}

#[test]
fn removals_come_after_everything_else() {
    let r = root();
    apply(&plan(&decl("[init]\nservices = [\"sshd\"]\n"), r.path())).unwrap();
    let p = plan(
        &SystemDecl::parse("hostname = \"x\"\nlibc = \"musl\"\n[init]\nservices = [\"chronyd\"]\n")
            .unwrap(),
        r.path(),
    );
    let kinds: Vec<_> = p.changes.iter().map(|c| c.kind).collect();
    assert_eq!(kinds.last(), Some(&Kind::Remove));
    assert!(kinds.iter().rev().skip(1).all(|k| *k != Kind::Remove));
}

#[test]
fn removing_all_kernel_modules_removes_the_file() {
    let r = root();
    apply(&plan(&decl("kernel_modules = [\"btrfs\"]\n"), r.path())).unwrap();
    assert!(read(r.path(), "etc/modules-load.d/forge.conf").contains("btrfs"));
    let p = plan(&decl(""), r.path());
    assert!(p.changes.iter().any(|c| c.kind == Kind::Remove && c.section == "kernel modules"));
    apply(&p).unwrap();
    assert!(!r.path().join("etc/modules-load.d/forge.conf").exists());
}

#[test]
fn missing_service_is_a_planning_error() {
    let r = root();
    let d = decl("[init]\nservices = [\"nosuchd\"]\n");
    assert!(matches!(
        plan_system(&d, r.path(), opts()),
        Err(PlanError::ServiceMissing(s)) if s == "nosuchd"
    ));
}

#[test]
fn rejects_unsafe_values() {
    let r = root();
    for extra in [
        "timezone = \"../../etc/passwd\"\n",
        "timezone = \"/etc/passwd\"\n",
        "locale = \"en US\"\n",
        "keymap = \"us; rm\"\n",
        "kernel_modules = [\"a b\"]\n",
    ] {
        assert!(plan_system(&decl(extra), r.path(), opts()).is_err(), "{extra}");
    }
    let bad_host = SystemDecl::parse("hostname = \"a/b\"\nlibc = \"musl\"\n").unwrap();
    assert!(plan_system(&bad_host, r.path(), opts()).is_err());
}

#[test]
fn users_are_pending_not_applied() {
    let r = root();
    let d = decl("[[users]]\nname = \"alice\"\n");
    let p = plan(&d, r.path());
    assert!(p.pending.iter().any(|x| x.section == "users" && x.summary.contains("alice")));
    apply(&p).unwrap();
    assert!(plan(&d, r.path()).is_empty());
}

// ---- packages -------------------------------------------------------------

use forge_store::{Keyring, LocalRepo, PackOptions, Profile, SecretKey, Store, Trust, pack_dir};

/// Build a package and publish it to `<root>/srv/repo`; returns its store id.
fn publish(root: &Path, name: &str, files: &[(&str, &str)], deps: &[&str]) -> String {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let src = scratch.path().join("src");
    fs::create_dir_all(&src).unwrap();
    for (path, content) in files {
        let p = src.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        if path.contains("bin/") {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let opts = PackOptions {
        name: name.into(),
        version: "1.0".into(),
        libc: forge_schema::Libc::Musl,
        hash: None,
        deps: deps.iter().map(|d| d.to_string()).collect(),
    };
    let out = scratch.path().join("pkg.tar.zst");
    pack_dir(&src, &opts, &out).unwrap();
    LocalRepo::new(root.join("srv/repo")).publish(&out, Some(&test_key())).unwrap().id().unwrap().to_string()
}

fn pkg_decl(packages: &[&str]) -> SystemDecl {
    let list = packages.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", ");
    decl(&format!("repos = [\"/srv/repo\"]\npackages = [{list}]\n"))
}

fn system_profile(r: &Path) -> Profile {
    Profile::new(r.join("forge/profiles/system"))
}

fn view(r: &Path, rel: &str) -> std::path::PathBuf {
    system_profile(r).current_link().join(rel)
}

#[test]
fn declared_packages_are_installed_and_linked_with_their_dependencies() {
    let r = root();
    let lib = publish(r.path(), "libfoo", &[("usr/lib/libfoo.so", "lib")], &[]);
    publish(r.path(), "hello", &[("usr/bin/hello", "#!/bin/sh\n")], &[&lib]);

    let p = plan(&pkg_decl(&["hello"]), r.path());
    let pkgs: Vec<_> = p.changes.iter().filter(|c| c.section == "packages").collect();
    assert_eq!(pkgs.len(), 2, "the dependency is pulled in: {pkgs:?}");
    assert!(pkgs.iter().all(|c| c.kind == Kind::Add && c.summary.contains("install from")));
    apply(&p).unwrap();

    let store = Store::new(r.path().join("forge/store"));
    assert_eq!(store.list().unwrap().len(), 2);
    assert_eq!(fs::read_to_string(view(r.path(), "usr/bin/hello")).unwrap(), "#!/bin/sh\n");
    assert_eq!(fs::read_to_string(view(r.path(), "usr/lib/libfoo.so")).unwrap(), "lib");
    assert!(read(r.path(), "etc/profile.d/forge.sh").contains("forge/profiles/system/current/usr/bin"));
    assert!(plan(&pkg_decl(&["hello"]), r.path()).is_empty(), "converged");
}

#[test]
fn removing_a_package_unlinks_it_but_keeps_it_in_the_store_until_gc() {
    let r = root();
    publish(r.path(), "a", &[("usr/bin/a", "a")], &[]);
    publish(r.path(), "b", &[("usr/bin/b", "b")], &[]);
    apply(&plan(&pkg_decl(&["a", "b"]), r.path())).unwrap();

    let p = plan(&pkg_decl(&["a"]), r.path());
    let removals: Vec<_> = p.changes.iter().filter(|c| c.kind == Kind::Remove && c.section == "packages").collect();
    assert_eq!(removals.len(), 1);
    assert!(removals[0].summary.starts_with("b-"));
    apply(&p).unwrap();

    assert!(!view(r.path(), "usr/bin/b").exists());
    assert!(view(r.path(), "usr/bin/a").exists());
    assert_eq!(Store::new(r.path().join("forge/store")).list().unwrap().len(), 2, "still stored");
    assert_eq!(system_profile(r.path()).generations().unwrap().len(), 2, "the old set is a generation");
    assert!(plan(&pkg_decl(&["a"]), r.path()).is_empty());
}

#[test]
fn a_package_already_in_the_store_is_only_linked() {
    let r = root();
    publish(r.path(), "a", &[("usr/bin/a", "a")], &[]);
    apply(&plan(&pkg_decl(&["a"]), r.path())).unwrap();
    apply(&plan(&pkg_decl(&[]), r.path())).unwrap();
    assert!(!view(r.path(), "usr/bin/a").exists());

    // Break the repository: the package must still come back from the store.
    for e in fs::read_dir(r.path().join("srv/repo")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "zst") {
            fs::remove_file(p).unwrap();
        }
    }
    let p = plan(&pkg_decl(&["a"]), r.path());
    assert!(p.changes.iter().any(|c| c.summary.contains("already in the store")));
    apply(&p).unwrap();
    assert!(view(r.path(), "usr/bin/a").exists());
}

#[test]
fn packages_without_a_repository_are_a_planning_error() {
    let r = root();
    assert!(matches!(
        plan_system(&decl("packages = [\"kitty\"]\n"), r.path(), opts()),
        Err(PlanError::NoRepo)
    ));
}

#[test]
fn unknown_packages_and_missing_indexes_are_planning_errors_and_change_nothing() {
    let r = root();
    publish(r.path(), "a", &[("usr/bin/a", "a")], &[]);
    let err = plan_system(&pkg_decl(&["nosuchpkg"]), r.path(), opts()).unwrap_err();
    assert!(err.to_string().contains("nosuchpkg"), "{err}");

    let d = decl("repos = [\"/nowhere\"]\npackages = [\"a\"]\n");
    assert!(matches!(plan_system(&d, r.path(), opts()), Err(PlanError::Store(_))));
    assert!(!r.path().join("forge").exists());
}

#[test]
fn a_tampered_archive_is_rejected_and_nothing_is_installed() {
    let r = root();
    publish(r.path(), "a", &[("usr/bin/a", "a")], &[]);
    let archive = fs::read_dir(r.path().join("srv/repo"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "zst"))
        .unwrap();
    let mut bytes = fs::read(&archive).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    fs::write(&archive, bytes).unwrap();

    let p = plan(&pkg_decl(&["a"]), r.path());
    let err = apply(&p).unwrap_err();
    assert!(err.summary.contains("install from"), "{err}");
    assert!(Store::new(r.path().join("forge/store")).list().unwrap().is_empty());
    assert_eq!(system_profile(r.path()).current().unwrap(), None);
}

#[test]
fn two_packages_providing_the_same_file_fail_without_changing_the_profile() {
    let r = root();
    publish(r.path(), "a", &[("usr/bin/tool", "from a")], &[]);
    apply(&plan(&pkg_decl(&["a"]), r.path())).unwrap();
    publish(r.path(), "b", &[("usr/bin/tool", "from b")], &[]);

    let err = apply(&plan(&pkg_decl(&["a", "b"]), r.path())).unwrap_err();
    assert!(err.to_string().contains("provided by both"), "{err}");
    assert_eq!(fs::read_to_string(view(r.path(), "usr/bin/tool")).unwrap(), "from a");
    assert_eq!(system_profile(r.path()).generations().unwrap().len(), 1);
}

#[test]
fn non_openrc_init_defers_keymap_and_services() {
    let r = root();
    let d = decl("[init]\nsystem = \"runit\"\nservices = [\"sshd\"]\n");
    let p = plan(&d, r.path());
    assert!(p.changes.iter().all(|c| c.section != "keymap" && c.section != "services"));
    assert!(p.pending.iter().any(|x| x.section == "keymap"));
    assert!(p.pending.iter().any(|x| x.section == "services"));
}

#[test]
fn corrupt_state_file_is_reported_not_ignored() {
    let r = root();
    fs::create_dir_all(r.path().join("var/lib/forge")).unwrap();
    fs::write(r.path().join("var/lib/forge/state.toml"), "managed_services = 3").unwrap();
    assert!(matches!(
        plan_system(&decl(""), r.path(), opts()),
        Err(PlanError::BadState { .. })
    ));
}

// ---- repository trust ------------------------------------------------------

/// A one-package repository at `<root>/srv/repo`, signed by `signer` if given.
fn publish_as(root: &Path, name: &str, signer: Option<&SecretKey>) {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let src = scratch.path().join("src/usr/bin");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join(name), name).unwrap();
    fs::set_permissions(src.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    let opts = PackOptions {
        name: name.into(),
        version: "1.0".into(),
        libc: forge_schema::Libc::Musl,
        hash: None,
        deps: vec![],
    };
    let out = scratch.path().join("pkg.tar.zst");
    pack_dir(&scratch.path().join("src"), &opts, &out).unwrap();
    LocalRepo::new(root.join("srv/repo")).publish(&out, signer).unwrap();
}

fn store_is_empty(r: &Path) -> bool {
    Store::new(r.join("forge/store")).list().unwrap().is_empty()
}

#[test]
fn packages_from_an_unsigned_repository_are_refused_and_nothing_is_touched() {
    let r = root();
    publish_as(r.path(), "a", None);
    for options in [opts(), Options::default()] {
        let err = plan_system(&pkg_decl(&["a"]), r.path(), options).unwrap_err();
        assert!(err.to_string().contains("no signature"), "{err}");
    }
    assert!(store_is_empty(r.path()));
    assert_eq!(system_profile(r.path()).current().unwrap(), None);
}

#[test]
fn allow_unsigned_is_an_explicit_insecure_opt_out() {
    let r = root();
    publish_as(r.path(), "a", None);
    let insecure = Options { trust: Trust::AllowUnsigned, ..Options::default() };
    let p = plan_system(&pkg_decl(&["a"]), r.path(), insecure).unwrap();
    apply(&p).unwrap();
    assert!(view(r.path(), "usr/bin/a").exists());
    // An unsigned index has no trustworthy serial, so none is remembered.
    let state = fs::read_to_string(r.path().join("var/lib/forge/state.toml")).unwrap_or_default();
    assert!(!state.contains("repo_serials"), "{state}");
}

#[test]
fn a_repository_signed_by_an_untrusted_key_is_refused() {
    let r = root();
    let stranger = SecretKey::from_seed([77; 32]);
    publish_as(r.path(), "a", Some(&stranger));
    let err = plan_system(&pkg_decl(&["a"]), r.path(), opts()).unwrap_err();
    assert!(err.to_string().contains("not among"), "{err}");
    assert!(store_is_empty(r.path()));
}

#[test]
fn an_index_altered_after_signing_is_refused() {
    let r = root();
    publish_as(r.path(), "a", Some(&test_key()));
    let index = r.path().join("srv/repo/index.toml");
    let text = fs::read_to_string(&index).unwrap();
    let hash_line = text.lines().find(|l| l.starts_with("archive_sha256")).unwrap().to_string();
    // The attacker points the index at a different archive hash.
    fs::write(&index, text.replace(&hash_line, &format!("archive_sha256 = \"{}\"", "0".repeat(64)))).unwrap();
    let err = plan_system(&pkg_decl(&["a"]), r.path(), opts()).unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
    assert!(store_is_empty(r.path()));
}

#[test]
fn the_serial_of_a_signed_repository_is_remembered() {
    let r = root();
    publish_as(r.path(), "a", Some(&test_key()));
    apply(&plan(&pkg_decl(&["a"]), r.path())).unwrap();
    let state = read(r.path(), "var/lib/forge/state.toml");
    assert!(state.contains("[repo_serials]") && state.contains("\"/srv/repo\" = 1"), "{state}");
}

#[test]
fn an_older_but_genuinely_signed_index_is_refused_as_a_rollback() {
    let r = root();
    let repo = r.path().join("srv/repo");
    publish_as(r.path(), "a", Some(&test_key())); // serial 1
    let (old_index, old_sig) = (fs::read(repo.join("index.toml")).unwrap(), fs::read(repo.join("index.toml.sig")).unwrap());
    publish_as(r.path(), "b", Some(&test_key())); // serial 2
    apply(&plan(&pkg_decl(&["a"]), r.path())).unwrap(); // remembers serial 2

    // The attacker replays the old index with its (valid!) old signature.
    fs::write(repo.join("index.toml"), &old_index).unwrap();
    fs::write(repo.join("index.toml.sig"), &old_sig).unwrap();
    let err = plan_system(&pkg_decl(&["a"]), r.path(), opts()).unwrap_err();
    assert!(err.to_string().contains("went backwards") && err.to_string().contains("serial 1"), "{err}");

    // Allowing unsigned skips the signature and with it the serial; that is its documented cost.
    let insecure = Options { trust: Trust::AllowUnsigned, ..Options::default() };
    assert!(plan_system(&pkg_decl(&["a"]), r.path(), insecure).is_ok());
}

#[test]
fn the_same_serial_again_is_not_a_rollback_and_newer_ones_are_remembered() {
    let r = root();
    publish_as(r.path(), "a", Some(&test_key()));
    apply(&plan(&pkg_decl(&["a"]), r.path())).unwrap();
    plan_system(&pkg_decl(&["a"]), r.path(), opts()).expect("the same serial is fine");
    publish_as(r.path(), "b", Some(&test_key()));
    apply(&plan(&pkg_decl(&["a", "b"]), r.path())).unwrap();
    assert!(read(r.path(), "var/lib/forge/state.toml").contains("\"/srv/repo\" = 2"));
}

#[test]
fn repositories_are_only_consulted_when_packages_are_declared() {
    let r = root();
    publish_as(r.path(), "a", None); // unsigned, but nothing is asked of it
    let p = plan_system(&pkg_decl(&[]), r.path(), opts()).unwrap();
    assert!(p.changes.iter().all(|c| c.section != "packages"));
}

// ---- repositories served over HTTP(S) ---------------------------------------------------------------

use forge_store::{Fetch, FetchHandle, StoreError as SErr};
use std::sync::{Arc, Mutex};

/// Serves `<root>/srv/repo` as `https://example.test/repo/...` and remembers what was asked for.
struct Served {
    dir: std::path::PathBuf,
    log: Mutex<Vec<String>>,
}

impl Served {
    fn path(&self, url: &str) -> Result<std::path::PathBuf, SErr> {
        self.log.lock().unwrap().push(url.to_string());
        let name = url
            .strip_prefix("https://example.test/repo/")
            .ok_or_else(|| SErr::Fetch { url: url.into(), reason: "unknown host".into() })?;
        Ok(self.dir.join(name))
    }
}

impl Fetch for Served {
    fn get(&self, url: &str) -> Result<Vec<u8>, SErr> {
        fs::read(self.path(url)?).map_err(|e| SErr::Fetch { url: url.into(), reason: e.to_string() })
    }
    fn download(&self, url: &str, dest: &Path) -> Result<(), SErr> {
        fs::copy(self.path(url)?, dest).map(drop).map_err(|e| SErr::Fetch { url: url.into(), reason: e.to_string() })
    }
}

fn remote_decl(names: &[&str]) -> SystemDecl {
    let list: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    SystemDecl::parse(&format!(
        "hostname = \"x\"\nlibc = \"musl\"\nrepos = [\"https://example.test/repo\"]\npackages = [{}]\n",
        list.join(", ")
    ))
    .unwrap()
}

fn remote_opts(r: &Path) -> (Options, Arc<Served>) {
    let served = Arc::new(Served { dir: r.join("srv/repo"), log: Default::default() });
    let opts = Options { rebuild: false, trust: trust_of(&[&test_key()]), fetch: FetchHandle(served.clone()) };
    (opts, served)
}

#[test]
fn packages_from_a_remote_repository_are_downloaded_verified_and_linked() {
    let r = root();
    publish_as(r.path(), "tool", Some(&test_key()));
    let (opts, served) = remote_opts(r.path());
    let plan = plan_system(&remote_decl(&["tool"]), r.path(), opts).unwrap();
    // Planning reads the index and its signature only: no archive is fetched before you agree.
    let asked = served.log.lock().unwrap().clone();
    assert_eq!(asked, ["https://example.test/repo/index.toml", "https://example.test/repo/index.toml.sig"]);
    assert!(store_is_empty(r.path()), "planning changes nothing");
    let shown: Vec<String> = plan.changes.iter().map(|c| c.to_string()).collect();
    assert!(shown.iter().any(|s| s.contains("https://example.test/repo/tool-1.0-")), "{shown:?}");

    apply(&plan).unwrap();
    let asked = served.log.lock().unwrap().clone();
    assert!(asked.last().unwrap().ends_with(".tar.zst"), "the archive was downloaded when applying: {asked:?}");
    assert!(!store_is_empty(r.path()));
    let cur = system_profile(r.path()).current().unwrap().unwrap();
    assert!(r.path().join(format!("forge/profiles/system/gen-{cur}/usr/bin/tool")).exists());
    assert!(!r.path().join("forge/store/.downloads").read_dir().map(|mut d| d.next().is_some()).unwrap_or(false), "no download debris");
}

#[test]
fn a_tampered_remote_index_is_refused_and_nothing_is_touched() {
    let r = root();
    publish_as(r.path(), "tool", Some(&test_key()));
    let idx = r.path().join("srv/repo/index.toml");
    let text = fs::read_to_string(&idx).unwrap();
    fs::write(&idx, format!("{text}\n# tampered\n")).unwrap();
    let (opts, served) = remote_opts(r.path());
    let err = plan_system(&remote_decl(&["tool"]), r.path(), opts).unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
    assert!(store_is_empty(r.path()));
    assert!(served.log.lock().unwrap().iter().all(|u| !u.ends_with(".tar.zst")), "no archive was fetched");
}

#[test]
fn a_remote_archive_that_does_not_match_the_signed_index_installs_nothing() {
    let r = root();
    publish_as(r.path(), "tool", Some(&test_key()));
    let (opts, _) = remote_opts(r.path());
    let plan = plan_system(&remote_decl(&["tool"]), r.path(), opts).unwrap();
    for e in fs::read_dir(r.path().join("srv/repo")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "zst") {
            fs::write(&p, b"not the package the index promised").unwrap();
        }
    }
    assert!(apply(&plan).is_err());
    assert!(store_is_empty(r.path()));
}

#[test]
fn the_serial_of_a_remote_repository_is_remembered_under_its_url() {
    let r = root();
    publish_as(r.path(), "tool", Some(&test_key()));
    let (opts, _) = remote_opts(r.path());
    let plan = plan_system(&remote_decl(&["tool"]), r.path(), opts).unwrap();
    apply(&plan).unwrap();
    let state = fs::read_to_string(r.path().join("var/lib/forge/state.toml")).unwrap_or_default();
    assert!(state.contains("https://example.test/repo"), "{state}");
}
