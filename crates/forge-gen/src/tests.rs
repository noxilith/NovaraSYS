use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::*;

/// A fake top-level volume: `top/@` (live) and `top/@snapshots`, plus a boot dir.
struct Machine {
    dir: TempDir,
}

impl Machine {
    fn new() -> Self {
        let m = Machine { dir: tempfile::tempdir().unwrap() };
        fs::create_dir_all(m.top().join("@snapshots")).unwrap();
        fs::create_dir_all(m.top().join("@/etc")).unwrap();
        fs::write(m.top().join("@/etc/version"), "v1\n").unwrap();
        m
    }
    fn top(&self) -> std::path::PathBuf {
        self.dir.path().join("top")
    }
    fn conf(&self) -> std::path::PathBuf {
        self.dir.path().join("boot/limine.conf")
    }
    fn ctx<'a>(&self, backend: &'a dyn Backend, booted: &str) -> Ctx<'a> {
        Ctx {
            backend,
            live: self.top().join("@"),
            snaps: self.top().join("@snapshots"),
            top: Some(self.top()),
            limine_conf: self.conf(),
            booted_subvol: Some(booted.to_string()),
            boot: BootInfo {
                kernel: "vmlinuz-6.1".into(),
                initramfs: "initramfs-6.1.img".into(),
                cmdline: "root=UUID=abc init=/sbin/openrc-init".into(),
            },
            now: 1000,
        }
    }
    fn set_version(&self, v: &str) {
        fs::write(self.top().join("@/etc/version"), format!("{v}\n")).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.top().join(rel)).unwrap()
    }
    fn conf_text(&self) -> String {
        fs::read_to_string(self.conf()).unwrap()
    }
}

struct FailingBackend;
impl Backend for FailingBackend {
    fn snapshot(&self, _: &Path, _: &Path, _: bool) -> io::Result<()> {
        Err(io::Error::other("disk on fire"))
    }
    fn delete(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn create_snapshots_writes_metadata_and_menu() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    let g = create(&ctx, "first").unwrap();
    assert_eq!(g.id, 1);
    assert_eq!(m.read("@snapshots/gen-1/etc/version"), "v1\n");
    assert_eq!(current(&ctx).unwrap(), Some(1));
    let listed = list(&ctx).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].description, "first");
    assert_eq!(listed[0].kernel, "vmlinuz-6.1");
    let conf = m.conf_text();
    assert!(conf.contains("//Generation 1: first"));
    assert!(conf.contains("rootflags=subvol=@snapshots/gen-1 ro"));
    assert!(conf.contains("init=/sbin/openrc-init"), "init is pinned in the cmdline");
}

#[test]
fn a_generation_is_frozen_and_ids_increase() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    create(&ctx, "one").unwrap();
    m.set_version("v2");
    let g2 = create(&ctx, "two").unwrap();
    assert_eq!(g2.id, 2);
    assert_eq!(m.read("@snapshots/gen-1/etc/version"), "v1\n");
    assert_eq!(m.read("@snapshots/gen-2/etc/version"), "v2\n");
    assert_eq!(current(&ctx).unwrap(), Some(2));
    assert_eq!(list(&ctx).unwrap().iter().map(|g| g.id).collect::<Vec<_>>(), [1, 2]);
}

#[test]
fn each_generation_keeps_the_kernel_it_was_made_with() {
    let m = Machine::new();
    create(&m.ctx(&CopyBackend, "@"), "old kernel").unwrap();
    let mut ctx = m.ctx(&CopyBackend, "@");
    ctx.boot.kernel = "vmlinuz-6.2".into();
    ctx.boot.initramfs = "initramfs-6.2.img".into();
    create(&ctx, "new kernel").unwrap();
    let conf = m.conf_text();
    let g1 = conf.split("//Generation 1").nth(1).unwrap();
    assert!(g1.contains("vmlinuz-6.1"), "generation 1 must still boot kernel 6.1");
    assert!(conf.contains("path: boot():/vmlinuz-6.2"), "the live entry uses the newest kernel");
}

#[test]
fn create_refuses_when_booted_from_a_generation() {
    let m = Machine::new();
    create(&m.ctx(&CopyBackend, "@"), "one").unwrap();
    let r = create(&m.ctx(&CopyBackend, "@snapshots/gen-1"), "nope");
    assert!(matches!(r, Err(GenError::BootedFromGeneration(1))));
}

#[test]
fn remove_refuses_current_and_booted_generation() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    create(&ctx, "one").unwrap();
    create(&ctx, "two").unwrap();
    assert!(matches!(remove(&ctx, 2), Err(GenError::Protected { id: 2, .. })));
    let booted_one = m.ctx(&CopyBackend, "@snapshots/gen-1");
    assert!(matches!(remove(&booted_one, 1), Err(GenError::Protected { id: 1, .. })));
    assert!(matches!(remove(&ctx, 9), Err(GenError::NoSuchGeneration(9))));
}

#[test]
fn remove_deletes_snapshot_metadata_and_menu_entry() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    create(&ctx, "one").unwrap();
    create(&ctx, "two").unwrap();
    let r = remove(&ctx, 1).unwrap();
    assert_eq!(r.removed, [1]);
    assert!(!m.top().join("@snapshots/gen-1").exists());
    assert!(!m.top().join("@snapshots/gen-1.toml").exists());
    assert!(!m.conf_text().contains("Generation 1"));
    assert!(m.conf_text().contains("Generation 2"));
}

#[test]
fn remove_all_skips_protected_generations() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    for d in ["a", "b", "c"] {
        create(&ctx, d).unwrap();
    }
    let r = remove_all(&ctx).unwrap();
    assert_eq!(r.removed, [1, 2]);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].0, 3);
    assert_eq!(list(&ctx).unwrap().len(), 1);
}

#[test]
fn rollback_stages_into_next_and_points_the_menu_at_it() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    create(&ctx, "good state").unwrap();
    m.set_version("broken");
    assert_eq!(rollback(&ctx, 1).unwrap(), RollbackOutcome::Staged);

    assert_eq!(m.read("@.next/etc/version"), "v1\n");
    assert_eq!(m.read("@/etc/version"), "broken\n", "@ is untouched until promotion");
    assert!(m.conf_text().contains("rootflags=subvol=@.next rw"));
    assert!(matches!(create(&ctx, "x"), Err(GenError::RollbackStaged)));
    assert_eq!(promote(&ctx).unwrap(), Promotion::WaitingForReboot);
}

#[test]
fn promote_replaces_live_root_and_keeps_a_backup() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    create(&ctx, "good state").unwrap();
    m.set_version("broken");
    rollback(&ctx, 1).unwrap();

    // "Reboot" into @.next; the next menu rewrite promotes it.
    let on_next = m.ctx(&CopyBackend, "@.next");
    rewrite_limine(&on_next).unwrap();
    assert_eq!(m.read("@/etc/version"), "v1\n");
    assert_eq!(m.read("@.old-1000/etc/version"), "broken\n", "nothing is ever lost");
    assert!(m.conf_text().contains("rootflags=subvol=@ rw"));
    assert!(!m.conf_text().contains("subvol=@.next"));

    // Cannot clean up while still running on @.next; fine once booted into @.
    assert!(matches!(cleanup_next(&on_next), Err(GenError::BootedFromNext)));
    cleanup_next(&m.ctx(&CopyBackend, "@")).unwrap();
    assert!(!m.top().join("@.next").exists());
    assert!(matches!(cleanup_next(&m.ctx(&CopyBackend, "@")), Err(GenError::NothingToCleanUp)));
}

#[test]
fn rollback_from_a_booted_generation_replaces_live_in_one_step() {
    let m = Machine::new();
    create(&m.ctx(&CopyBackend, "@"), "good").unwrap();
    m.set_version("broken");
    let r = rollback(&m.ctx(&CopyBackend, "@snapshots/gen-1"), 1).unwrap();
    assert_eq!(r, RollbackOutcome::Replaced);
    assert_eq!(m.read("@/etc/version"), "v1\n");
    assert!(!m.top().join("@.next").exists());
    assert_eq!(m.read("@.old-1000/etc/version"), "broken\n");
}

#[test]
fn rollback_needs_the_top_level_volume_and_a_real_generation() {
    let m = Machine::new();
    create(&m.ctx(&CopyBackend, "@"), "one").unwrap();
    let mut ctx = m.ctx(&CopyBackend, "@");
    assert!(matches!(rollback(&ctx, 7), Err(GenError::NoSuchGeneration(7))));
    ctx.top = None;
    assert!(matches!(rollback(&ctx, 1), Err(GenError::NeedTop)));
}

#[test]
fn a_failed_copy_never_leaves_the_system_without_a_live_root() {
    let m = Machine::new();
    create(&m.ctx(&CopyBackend, "@"), "good").unwrap();
    // Stage a rollback by hand so promote has something to copy.
    fs::create_dir_all(m.top().join("@.next")).unwrap();
    fs::write(m.top().join("@snapshots/.rollback"), "staged\n").unwrap();

    let r = promote(&m.ctx(&FailingBackend, "@.next"));
    assert!(matches!(r, Err(GenError::Io { .. })));
    assert_eq!(m.read("@/etc/version"), "v1\n", "@ must be back in place");
    assert!(!m.top().join("@.old-1000").exists());
}

#[test]
fn a_failed_snapshot_leaves_no_generation_behind() {
    let m = Machine::new();
    let r = create(&m.ctx(&FailingBackend, "@"), "x");
    assert!(r.is_err());
    assert!(list(&m.ctx(&CopyBackend, "@")).unwrap().is_empty());
    assert!(!m.conf().exists());
}

#[test]
fn missing_or_corrupt_metadata_is_reported() {
    let m = Machine::new();
    let ctx = m.ctx(&CopyBackend, "@");
    create(&ctx, "one").unwrap();
    fs::write(m.top().join("@snapshots/gen-1.toml"), "id = \"nope\"").unwrap();
    assert!(matches!(list(&ctx), Err(GenError::BadMeta { id: 1, .. })));
    fs::remove_file(m.top().join("@snapshots/gen-1.toml")).unwrap();
    assert!(matches!(list(&ctx), Err(GenError::MissingMeta(1))));
}

#[test]
fn unsafe_boot_info_is_rejected_before_anything_changes() {
    let m = Machine::new();
    let mut ctx = m.ctx(&CopyBackend, "@");
    ctx.boot.kernel = "../../etc/passwd".into();
    assert!(matches!(create(&ctx, "x"), Err(GenError::InvalidBoot(_))));
    assert!(list(&m.ctx(&CopyBackend, "@")).unwrap().is_empty());
}
