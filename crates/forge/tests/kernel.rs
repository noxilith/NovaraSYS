//! `forge kernel` argument handling. Nothing here touches the network or builds a kernel.

use std::process::{Command, Output};

fn forge(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forge")).args(args).output().expect("run forge")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn the_configuration_modes_are_mutually_exclusive() {
    let o = forge(&["kernel", "build", "--full", "--auto"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("cannot be used with"), "{}", text(&o));
}

#[test]
fn a_version_and_a_series_cannot_both_be_given() {
    let o = forge(&["kernel", "build", "--version", "7.3.3", "--series", "longterm"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("cannot be used with"), "{}", text(&o));
}

#[test]
fn a_release_candidate_is_refused_before_anything_is_downloaded() {
    let dir = tempfile::tempdir().unwrap();
    let o = forge(&["kernel", "build", "--skip-checks", "--full", "--version", "7.4-rc2", "--work-dir", dir.path().to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(text(&o).contains("not a kernel version"), "{}", text(&o));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing was created");
}

#[test]
fn installing_a_directory_that_is_not_a_staged_kernel_fails_clearly() {
    // Root passes the permission check and reaches the directory check; others stop at the permission check.
    let dir = tempfile::tempdir().unwrap();
    let o = forge(&["kernel", "install", dir.path().to_str().unwrap()]);
    assert!(!o.status.success());
    let t = text(&o);
    assert!(t.contains("needs root") || t.contains("staged kernel directory"), "{t}");
}

#[test]
fn the_help_describes_the_three_modes() {
    let o = forge(&["kernel", "build", "--help"]);
    let t = text(&o);
    for flag in ["--full", "--auto", "--menuconfig", "--also-keep", "--loaded-modules", "--install", "--host-tools", "--repo", "--keys"] {
        assert!(t.contains(flag), "{flag} missing from the help: {t}");
    }
}
