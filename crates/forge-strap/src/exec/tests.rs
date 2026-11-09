use std::fs;

use tempfile::TempDir;

use super::*;
use crate::plan;

/// Records every command; answers `blkid` with fixed UUIDs; can be told to fail.
#[derive(Default)]
struct FakeRunner {
    log: Vec<Cmd>,
    fail_on: Option<&'static str>,
}

impl Runner for FakeRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<String, StrapError> {
        self.log.push(cmd.clone());
        if self.fail_on == Some(cmd.program.as_str()) {
            return Err(StrapError::Command {
                cmd: cmd.to_string(),
                status: "exit status: 1".into(),
                stderr: "boom".into(),
            });
        }
        Ok(match cmd.program.as_str() {
            "blkid" if cmd.args.last().map(String::as_str) == Some("/dev/sda2") => "1111-aaaa\n".into(),
            "blkid" => "ABCD-1234\n".into(),
            _ => String::new(),
        })
    }
}

impl FakeRunner {
    fn programs(&self) -> Vec<&str> {
        self.log.iter().map(|c| c.program.as_str()).collect()
    }
    fn find(&self, program: &str) -> Vec<&Cmd> {
        self.log.iter().filter(|c| c.program == program).collect()
    }
}

#[derive(Default)]
struct FakeUi {
    steps: Vec<String>,
    warnings: Vec<String>,
}

impl Ui for FakeUi {
    fn step(&mut self, _: usize, _: usize, text: &str) {
        self.steps.push(text.into());
    }
    fn warn(&mut self, text: &str) {
        self.warnings.push(text.into());
    }
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// A target that already holds what rsync would have copied: a musl
    /// system with a kernel in /boot, plus a Limine directory.
    fn new() -> Self {
        let f = Fixture { dir: tempfile::tempdir().unwrap() };
        let mp = f.mp();
        fs::create_dir_all(mp.join("boot")).unwrap();
        fs::create_dir_all(mp.join("lib")).unwrap();
        fs::write(mp.join("lib/ld-musl-x86_64.so.1"), "").unwrap();
        for k in ["6.9.0", "6.10.2"] {
            fs::write(mp.join(format!("boot/vmlinuz-{k}")), "").unwrap();
            fs::write(mp.join(format!("boot/initramfs-{k}.img")), "").unwrap();
        }
        fs::create_dir_all(f.source().join("boot")).unwrap();
        fs::create_dir_all(f.limine()).unwrap();
        fs::write(f.limine().join("BOOTX64.EFI"), "efi").unwrap();
        fs::write(f.limine().join("limine-bios.sys"), "bios").unwrap();
        f
    }
    fn mp(&self) -> PathBuf {
        self.dir.path().join("target")
    }
    fn limine(&self) -> PathBuf {
        self.dir.path().join("limine")
    }
    fn source(&self) -> PathBuf {
        self.dir.path().join("src")
    }
    fn request(&self, fw: Firmware) -> Request {
        Request {
            plan: plan(fw, "/dev/sda2", Some("/dev/sda1")).unwrap(),
            source: self.source(),
            mount_point: self.mp(),
            top_mount: self.dir.path().join("top"),
            limine_dir: self.limine(),
            hostname: "novara".into(),
            libc: None,
            kernel: None,
            initramfs: None,
            extra_cmdline: "quiet".into(),
            efi_entry: true,
            accounts: Accounts::default(),
        }
    }
}

fn run(req: &Request) -> (Result<(), StrapError>, FakeRunner, FakeUi) {
    let (mut r, mut ui) = (FakeRunner::default(), FakeUi::default());
    let res = execute(req, &mut r, &mut ui);
    (res, r, ui)
}

#[test]
fn uefi_install_runs_the_expected_commands_in_order() {
    let f = Fixture::new();
    let (res, r, ui) = run(&f.request(Firmware::Uefi));
    res.unwrap();
    assert_eq!(ui.steps.len(), 10);
    assert!(ui.warnings.is_empty());

    let p = r.programs();
    let pos = |name: &str| p.iter().position(|x| *x == name).unwrap();
    assert!(pos("mkfs.vfat") < pos("mkfs.btrfs"));
    assert!(pos("mkfs.btrfs") < pos("btrfs"));
    assert!(pos("btrfs") < pos("rsync"));
    assert!(pos("rsync") < pos("blkid"));
    assert!(pos("blkid") < pos("efibootmgr"));
    assert!(pos("efibootmgr") < pos("sync"));
    assert_eq!(*p.last().unwrap(), "umount");

    let c = r.find("mkfs.btrfs");
    assert_eq!(c[0].to_string(), "mkfs.btrfs -f -L novara /dev/sda2");
    assert_eq!(r.find("mkfs.vfat")[0].to_string(), "mkfs.vfat -F 32 -n NOVARA /dev/sda1");
    assert_eq!(r.find("sfdisk")[0].args, ["--part-type", "/dev/sda", "1", ESP_TYPE_GUID]);
    assert_eq!(r.find("btrfs").len(), 4, "one create per subvolume");
}

#[test]
fn every_mount_is_unmounted_in_reverse_order() {
    let f = Fixture::new();
    let (res, r, _) = run(&f.request(Firmware::Uefi));
    res.unwrap();
    let mounts: Vec<String> = r.find("mount").iter().map(|c| c.args.last().unwrap().clone()).collect();
    let umounts: Vec<String> = r.find("umount").iter().map(|c| c.args.last().unwrap().clone()).collect();
    assert_eq!(mounts.len(), umounts.len());
    let mut expected = mounts.clone();
    expected.reverse();
    // The temporary top-level mount is released first, then the target stack in reverse.
    assert_eq!(umounts[0], mounts[0]);
    assert_eq!(umounts[1..], expected[..expected.len() - 1]);
    assert!(mounts.last().unwrap().ends_with("target/boot"));
}

#[test]
fn rsync_copies_the_live_system_and_skips_runtime_and_live_only_paths() {
    let f = Fixture::new();
    let (res, r, _) = run(&f.request(Firmware::Uefi));
    res.unwrap();
    let all = r.find("rsync");
    assert_eq!(all.len(), 2, "one copy for the root, one FAT-safe copy for /boot");
    let rs = all[0];
    assert!(rs.show, "progress goes to the terminal");
    for a in [
        "-aHX",
        "--numeric-ids",
        "--exclude=/proc",
        "--exclude=/dev",
        "--exclude=/.snapshots",
        "--exclude=/fastboot",
        "--exclude=/medium",
        "--exclude=/boot",
    ] {
        assert!(rs.args.contains(&a.to_string()), "missing {a}");
    }
    assert_eq!(rs.args[rs.args.len() - 2], format!("{}/", f.source().display()));
    assert!(rs.args.last().unwrap().ends_with("target/"));

    // /boot goes to a FAT32 partition: no ownership, permissions, ACLs or xattrs.
    let boot = all[1];
    for a in ["-rtD", "--no-perms", "--no-owner", "--no-group"] {
        assert!(boot.args.contains(&a.to_string()), "missing {a}");
    }
    assert!(!boot.args.iter().any(|a| a.contains('A') && a.starts_with('-') && !a.starts_with("--")));
    assert_eq!(boot.args[boot.args.len() - 2], format!("{}/boot/", f.source().display()));
    assert!(boot.args.last().unwrap().ends_with("target/boot/"));
    for d in ["dev", "proc", "sys", "run", "tmp", "mnt"] {
        assert!(f.mp().join(d).is_dir(), "{d} missing");
    }
    let mode = fs::metadata(f.mp().join("tmp")).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o1777);
}

#[test]
fn writes_fstab_with_every_subvolume() {
    let f = Fixture::new();
    run(&f.request(Firmware::Uefi)).0.unwrap();
    let fstab = fs::read_to_string(f.mp().join("etc/fstab")).unwrap();
    for line in [
        "UUID=1111-aaaa / btrfs subvol=@,noatime 0 0",
        "UUID=1111-aaaa /.snapshots btrfs subvol=@snapshots,noatime 0 0",
        "UUID=1111-aaaa /home btrfs subvol=@home,noatime 0 0",
        "UUID=1111-aaaa /forge/store btrfs subvol=@store,noatime 0 0",
        "UUID=ABCD-1234 /boot vfat umask=0077 0 2",
    ] {
        assert!(fstab.contains(line), "missing: {line}");
    }
}

#[test]
fn writes_a_starter_declaration_that_parses_and_never_overwrites_one() {
    let f = Fixture::new();
    let mut req = f.request(Firmware::Uefi);
    req.accounts.user = Some(NewUser { name: "alice".into(), password: "pw".into() });
    run(&req).0.unwrap();
    let text = fs::read_to_string(f.mp().join("etc/forge/system.toml")).unwrap();
    let decl = SystemDecl::parse(&text).unwrap();
    assert_eq!(decl.hostname, "novara");
    assert_eq!(decl.libc, Libc::Musl, "detected from ld-musl");
    assert_eq!(decl.users[0].name, "alice");
    assert_eq!(decl.users[0].groups, ["wheel", "forge", "audio", "video", "input", "users"]);

    fs::write(f.mp().join("etc/forge/system.toml"), "hostname = \"mine\"\nlibc = \"glibc\"\n").unwrap();
    run(&f.request(Firmware::Uefi)).0.unwrap();
    assert!(fs::read_to_string(f.mp().join("etc/forge/system.toml")).unwrap().contains("mine"));
}

#[test]
fn libc_override_wins_and_undetectable_libc_is_an_error() {
    let f = Fixture::new();
    let mut req = f.request(Firmware::Uefi);
    req.libc = Some(Libc::Glibc);
    run(&req).0.unwrap();
    let decl = SystemDecl::parse(&fs::read_to_string(f.mp().join("etc/forge/system.toml")).unwrap()).unwrap();
    assert_eq!(decl.libc, Libc::Glibc);

    let f2 = Fixture::new();
    fs::remove_file(f2.mp().join("lib/ld-musl-x86_64.so.1")).unwrap();
    let (res, _, _) = run(&f2.request(Firmware::Uefi));
    assert!(matches!(res, Err(StrapError::Step { source, .. }) if matches!(*source, StrapError::UnknownLibc)));
}

#[test]
fn uefi_gets_the_fallback_loader_and_a_firmware_entry() {
    let f = Fixture::new();
    let (res, r, _) = run(&f.request(Firmware::Uefi));
    res.unwrap();
    assert_eq!(fs::read_to_string(f.mp().join("boot/EFI/BOOT/BOOTX64.EFI")).unwrap(), "efi");
    let e = r.find("efibootmgr")[0];
    assert_eq!(e.args, ["--create", "--label", "Novara", "--disk", "/dev/sda", "--part", "1", "--loader", "\\EFI\\BOOT\\BOOTX64.EFI"]);
}

#[test]
fn no_efi_entry_flag_never_calls_efibootmgr() {
    let f = Fixture::new();
    let mut req = f.request(Firmware::Uefi);
    req.efi_entry = false;
    let (res, r, _) = run(&req);
    res.unwrap();
    assert!(r.find("efibootmgr").is_empty());
    assert!(f.mp().join("boot/EFI/BOOT/BOOTX64.EFI").is_file());
}

#[test]
fn firmware_entry_failure_is_only_a_warning() {
    let f = Fixture::new();
    let (mut r, mut ui) = (FakeRunner { fail_on: Some("efibootmgr"), ..Default::default() }, FakeUi::default());
    execute(&f.request(Firmware::Uefi), &mut r, &mut ui).unwrap();
    assert_eq!(ui.warnings.len(), 1);
    assert!(ui.warnings[0].contains("removable-media"));
}

#[test]
fn bios_install_copies_the_stage_file_and_installs_to_the_disk() {
    let f = Fixture::new();
    let (res, r, _) = run(&f.request(Firmware::Bios));
    res.unwrap();
    assert_eq!(fs::read_to_string(f.mp().join("boot/limine-bios.sys")).unwrap(), "bios");
    assert_eq!(r.find("limine")[0].to_string(), "limine bios-install /dev/sda");
    assert!(r.find("efibootmgr").is_empty());
    assert!(r.find("sfdisk").is_empty(), "no EFI partition type for BIOS");
}

#[test]
fn limine_conf_boots_the_newest_kernel_from_the_new_root() {
    let f = Fixture::new();
    run(&f.request(Firmware::Uefi)).0.unwrap();
    let conf = fs::read_to_string(f.mp().join("boot/limine.conf")).unwrap();
    assert!(conf.contains("path: boot():/vmlinuz-6.10.2"), "6.10 is newer than 6.9: {conf}");
    assert!(conf.contains("module_path: boot():/initramfs-6.10.2.img"));
    assert!(conf.contains("cmdline: root=UUID=1111-aaaa quiet rootflags=subvol=@ rw"));
}

#[test]
fn missing_kernel_stops_the_install_with_a_hint() {
    let f = Fixture::new();
    for e in fs::read_dir(f.mp().join("boot")).unwrap() {
        fs::remove_file(e.unwrap().path()).unwrap();
    }
    let (res, _, _) = run(&f.request(Firmware::Uefi));
    let msg = res.unwrap_err().to_string();
    assert!(msg.contains("--kernel"), "{msg}");
}

#[test]
fn missing_limine_files_are_reported() {
    let f = Fixture::new();
    fs::remove_file(f.limine().join("BOOTX64.EFI")).unwrap();
    let (res, _, _) = run(&f.request(Firmware::Uefi));
    assert!(matches!(res, Err(StrapError::Step { source, .. }) if matches!(*source, StrapError::LimineMissing(_))));
}

#[test]
fn accounts_are_created_with_passwords_on_stdin_only() {
    let f = Fixture::new();
    let mut req = f.request(Firmware::Uefi);
    req.accounts.user = Some(NewUser { name: "alice".into(), password: "s3cret".into() });
    req.accounts.root_password = Some("r00t".into());
    let (res, r, _) = run(&req);
    res.unwrap();

    let mp = f.mp().display().to_string();
    // Every group is created first (harmless if present), so useradd -G never names a missing one.
    let made: Vec<&str> = r.find("groupadd").iter().map(|c| c.args.last().unwrap().as_str()).collect();
    assert_eq!(made, ["wheel", "forge", "audio", "video", "input", "users"]);
    assert!(r.find("groupadd").iter().all(|c| c.args[..3] == ["--root", mp.as_str(), "-f"]));
    assert_eq!(
        r.find("useradd")[0].args,
        ["--root", &mp, "-m", "-G", "wheel,forge,audio,video,input,users", "-s", "/bin/sh", "alice"],
        "the installer's user is an administrator"
    );
    let ch = r.find("chpasswd");
    assert_eq!(ch.len(), 2);
    assert_eq!(ch[0].stdin.as_deref(), Some("alice:s3cret\n"));
    assert_eq!(ch[1].stdin.as_deref(), Some("root:r00t\n"));
    for c in &r.log {
        assert!(!c.to_string().contains("s3cret") && !c.to_string().contains("r00t"), "password leaked into argv: {c}");
    }
}

#[test]
fn no_accounts_means_no_account_commands() {
    let f = Fixture::new();
    let (res, r, _) = run(&f.request(Firmware::Uefi));
    res.unwrap();
    for p in ["groupadd", "useradd", "chpasswd"] {
        assert!(r.find(p).is_empty());
    }
}

#[test]
fn rejects_bad_account_input_before_touching_anything() {
    let f = Fixture::new();
    for (name, pw) in [("Alice", "x"), ("a b", "x"), ("", "x"), ("../x", "x"), ("alice", ""), ("alice", "a\nb")] {
        let mut req = f.request(Firmware::Uefi);
        req.accounts.user = Some(NewUser { name: name.into(), password: pw.into() });
        let (res, r, _) = run(&req);
        assert!(matches!(res, Err(StrapError::Invalid { .. })), "{name:?}/{pw:?}");
        assert!(r.log.is_empty(), "nothing may run for bad input");
    }
}

#[test]
fn a_failing_step_unmounts_everything_and_names_the_step() {
    let f = Fixture::new();
    let (mut r, mut ui) = (FakeRunner { fail_on: Some("rsync"), ..Default::default() }, FakeUi::default());
    let err = execute(&f.request(Firmware::Uefi), &mut r, &mut ui).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("copy the live system"), "{msg}");
    assert!(msg.contains("incomplete state"));
    let mounts = r.find("mount").len();
    assert_eq!(r.find("umount").len(), mounts, "all mounts released after the failure");
    assert!(r.find("blkid").is_empty(), "nothing runs after the failed step");
}

#[test]
fn preflight_rejects_mounted_and_non_block_devices() {
    let p = plan(Firmware::Uefi, "/dev/sda2", Some("/dev/sda1")).unwrap();
    let block = |_: &str| true;
    assert!(preflight(&p, "/dev/nvme0n1p2 / btrfs rw 0 0\n", &block).is_ok());
    assert!(matches!(
        preflight(&p, "/dev/sda1 /boot vfat rw 0 0\n/dev/nvme0n1p2 / btrfs rw 0 0\n", &block),
        Err(StrapError::Mounted(d)) if d == "/dev/sda1"
    ));
    let only_boot = |d: &str| d == "/dev/sda1";
    assert!(matches!(preflight(&p, "", &only_boot), Err(StrapError::NotBlock(d)) if d == "/dev/sda2"));
}

#[test]
fn sorts_kernel_versions_numerically() {
    let mut v = vec!["vmlinuz-6.9.0", "vmlinuz-6.10.2", "vmlinuz-6.2.1", "vmlinuz-6.10.10"];
    v.sort_by(|a, b| natural_cmp(a, b));
    assert_eq!(v, ["vmlinuz-6.2.1", "vmlinuz-6.9.0", "vmlinuz-6.10.2", "vmlinuz-6.10.10"]);
}

#[test]
fn a_kernel_without_an_initramfs_boots_by_partition_uuid() {
    let f = Fixture::new();
    for e in fs::read_dir(f.mp().join("boot")).unwrap() {
        let p = e.unwrap().path();
        if p.file_name().unwrap().to_string_lossy().starts_with("initramfs-") {
            fs::remove_file(p).unwrap();
        }
    }
    let (res, r, _) = run(&f.request(Firmware::Uefi));
    res.unwrap();
    let conf = fs::read_to_string(f.mp().join("boot/limine.conf")).unwrap();
    assert!(!conf.contains("module_path"), "no initramfs to load: {conf}");
    // The kernel cannot resolve root=UUID= by itself, so it must be the partition's own id.
    assert!(conf.contains("cmdline: root=PARTUUID=1111-aaaa quiet rootflags=subvol=@ rw"), "{conf}");
    assert!(r.find("blkid").iter().any(|c| c.to_string().contains("PARTUUID")));
}

#[test]
fn an_initramfs_named_on_the_command_line_must_exist() {
    let f = Fixture::new();
    let mut req = f.request(Firmware::Uefi);
    req.initramfs = Some("initramfs-nope.img".into());
    let (res, _, _) = run(&req);
    assert!(matches!(res.unwrap_err(), StrapError::Step { .. }));
}

#[test]
fn the_installed_system_gets_its_own_hostname_not_the_live_systems() {
    let f = Fixture::new();
    // What rsync copied from the live system.
    fs::create_dir_all(f.mp().join("etc")).unwrap();
    fs::write(f.mp().join("etc/hostname"), "novara-live\n").unwrap();
    let mut req = f.request(Firmware::Uefi);
    req.hostname = "mybox".into();
    run(&req).0.unwrap();
    assert_eq!(fs::read_to_string(f.mp().join("etc/hostname")).unwrap(), "mybox\n");
    assert!(fs::read_to_string(f.mp().join("etc/forge/system.toml")).unwrap().contains("hostname = \"mybox\""));
}

#[test]
fn the_starter_declaration_points_at_the_projects_repository() {
    let f = Fixture::new();
    run(&f.request(Firmware::Uefi)).0.unwrap();
    let decl = fs::read_to_string(f.mp().join("etc/forge/system.toml")).unwrap();
    assert!(decl.contains(&format!("repos = [\"{}\"]", forge_schema::DEFAULT_REPO_URL)), "{decl}");
    assert!(forge_schema::SystemDecl::parse(&decl).unwrap().repos == [forge_schema::DEFAULT_REPO_URL], "and it parses");
}
