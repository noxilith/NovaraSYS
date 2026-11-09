//! `forge iso`: build the live ISO.
//!
//! A bootable image with Limine (BIOS and UEFI) holding a kernel, an initramfs and the
//! live system as a squashfs. The initramfs finds the medium, mounts the squashfs and
//! puts a RAM-backed overlay on top, so the live system is writable but nothing
//! persists. Commands run through a [`Runner`] so the sequence is unit-tested.

mod cpio;
mod live;
mod rootfs;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use live::{SQUASHFS_PATH, init_script, limine_conf};
pub use rootfs::{fetch, merge};

#[derive(Debug, thiserror::Error)]
pub enum IsoError {
    #[error("`{cmd}` failed ({status}): {stderr}")]
    Command { cmd: String, status: String, stderr: String },
    #[error("could not run `{cmd}`: {source}")]
    Spawn { cmd: String, source: io::Error },
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("invalid {what}: {value}")]
    Invalid { what: &'static str, value: String },
    #[error("two packages both provide /{}", .0.display())]
    Conflict(PathBuf),
    #[error("{0} not found")]
    Missing(PathBuf),
    #[error(transparent)]
    Store(#[from] forge_store::StoreError),
}

pub(crate) fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> IsoError {
    let context = context.into();
    move |source| IsoError::Io { context, source }
}

/// Runs external commands; tests record them instead.
pub trait Runner {
    /// Run `program` and return its stdout. `stdin` is never echoed (it can be a password).
    fn run(&mut self, program: &str, args: &[String], stdin: Option<&str>) -> Result<String, IsoError>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&mut self, program: &str, args: &[String], stdin: Option<&str>) -> Result<String, IsoError> {
        let shown = format!("{program} {}", args.join(" "));
        let mut c = Command::new(program);
        c.args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().map_err(|source| IsoError::Spawn { cmd: shown.clone(), source })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes()).map_err(io_ctx(format!("writing to `{program}`")))?;
        }
        let out = child.wait_with_output().map_err(io_ctx(format!("waiting for `{program}`")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(IsoError::Command {
                cmd: shown,
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }
}

pub struct IsoSpec {
    /// The merged package tree (modified: the live `/etc` is written into it).
    pub rootfs: PathBuf,
    /// The kernel image to boot.
    pub kernel: PathBuf,
    /// Where Limine's files are (`limine-bios-cd.bin`, `limine-uefi-cd.bin`, `BOOTX64.EFI`).
    pub limine_dir: PathBuf,
    /// Scratch directory (created; must not exist or be empty).
    pub work: PathBuf,
    pub out: PathBuf,
    /// Volume label of the ISO.
    pub label: String,
    pub hostname: String,
    pub cmdline: String,
    /// The live root password.
    pub root_password: String,
    /// Public key files (`*.pub`) the system will trust for package repositories: they are installed into
    /// `/etc/forge/keys`, which is where `forge apply` and `forge pkg` look.
    pub trusted_keys: Vec<PathBuf>,
}

const LIMINE_FILES: &[&str] = &["limine-bios-cd.bin", "limine-uefi-cd.bin", "limine-bios.sys", "BOOTX64.EFI"];

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Check that everything the build needs is there, before anything is written.
pub fn check_inputs(spec: &IsoSpec) -> Result<(), IsoError> {
    let busybox = spec.rootfs.join("bin/busybox");
    for p in [&spec.kernel, &busybox] {
        if !p.is_file() {
            return Err(IsoError::Missing(p.clone()));
        }
    }
    for f in LIMINE_FILES {
        let p = spec.limine_dir.join(f);
        if !p.is_file() {
            return Err(IsoError::Missing(p));
        }
    }
    let label_ok = !spec.label.is_empty()
        && spec.label.len() <= 32
        && spec.label.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !label_ok {
        return Err(IsoError::Invalid { what: "volume label", value: spec.label.clone() });
    }
    if spec.cmdline.contains('\n') {
        return Err(IsoError::Invalid { what: "kernel command line", value: spec.cmdline.clone() });
    }
    Ok(())
}

pub fn build(spec: &IsoSpec, runner: &mut dyn Runner) -> Result<(), IsoError> {
    check_inputs(spec)?;
    if spec.work.exists() && fs::read_dir(&spec.work).map_err(io_ctx("reading the work directory"))?.next().is_some() {
        return Err(IsoError::Invalid { what: "work directory", value: format!("{} is not empty", spec.work.display()) });
    }
    let iso_root = spec.work.join("iso");
    let boot = iso_root.join("boot");
    let limine_out = boot.join("limine");
    for d in [&limine_out, &iso_root.join("live"), &iso_root.join("EFI/BOOT")] {
        fs::create_dir_all(d).map_err(io_ctx(format!("creating {}", d.display())))?;
    }

    // The live system.
    let hash = runner.run("openssl", &args(&["passwd", "-6", "-salt", "novaralive", "-stdin"]), Some(&format!("{}\n", spec.root_password)))?;
    live::write_live_root(&spec.rootfs, &spec.hostname, hash.trim())?;
    let keys_dir = spec.rootfs.join("etc/forge/keys");
    fs::create_dir_all(&keys_dir).map_err(io_ctx("creating /etc/forge/keys"))?;
    for key in &spec.trusted_keys {
        let name = key.file_name().ok_or_else(|| IsoError::Invalid { what: "trusted key", value: key.display().to_string() })?;
        fs::copy(key, keys_dir.join(name)).map_err(io_ctx(format!("copying {}", key.display())))?;
    }
    runner.run(
        "mksquashfs",
        &[
            spec.rootfs.display().to_string(),
            iso_root.join(SQUASHFS_PATH).display().to_string(),
            "-comp".into(), "zstd".into(), "-noappend".into(), "-all-root".into(),
            "-all-time".into(), "0".into(), "-mkfs-time".into(), "0".into(), "-quiet".into(),
        ],
        None,
    )?;

    // The initramfs.
    let rd = spec.work.join("initramfs");
    live::write_initramfs_tree(&rd, &spec.rootfs.join("bin/busybox"), &spec.label)?;
    let img = fs::File::create(boot.join("initramfs.img")).map_err(io_ctx("creating the initramfs"))?;
    let mut gz = flate2::GzBuilder::new().mtime(0).write(img, flate2::Compression::best());
    cpio::write_tree(&mut gz, &rd, &[("console", 0o020600, (5, 1)), ("null", 0o020666, (1, 3))]).map_err(io_ctx("writing the initramfs"))?;
    gz.finish().map_err(io_ctx("compressing the initramfs"))?;

    // Boot files.
    fs::copy(&spec.kernel, boot.join("vmlinuz")).map_err(io_ctx("copying the kernel"))?;
    for f in ["limine-bios-cd.bin", "limine-uefi-cd.bin", "limine-bios.sys"] {
        fs::copy(spec.limine_dir.join(f), limine_out.join(f)).map_err(io_ctx(format!("copying {f}")))?;
    }
    fs::copy(spec.limine_dir.join("BOOTX64.EFI"), iso_root.join("EFI/BOOT/BOOTX64.EFI")).map_err(io_ctx("copying BOOTX64.EFI"))?;
    fs::write(limine_out.join("limine.conf"), limine_conf(&spec.cmdline)).map_err(io_ctx("writing limine.conf"))?;

    // The image, then Limine's BIOS stage into its boot sector.
    runner.run(
        "xorriso",
        &[
            "-as".into(), "mkisofs".into(), "-R".into(), "-r".into(), "-J".into(),
            "-V".into(), spec.label.clone(),
            "-b".into(), "boot/limine/limine-bios-cd.bin".into(),
            "-no-emul-boot".into(), "-boot-load-size".into(), "4".into(), "-boot-info-table".into(),
            "-hfsplus".into(), "-apm-block-size".into(), "2048".into(),
            "--efi-boot".into(), "boot/limine/limine-uefi-cd.bin".into(),
            "-efi-boot-part".into(), "--efi-boot-image".into(), "--protective-msdos-label".into(),
            iso_root.display().to_string(),
            "-o".into(), spec.out.display().to_string(),
        ],
        None,
    )?;
    runner.run("limine", &["bios-install".into(), spec.out.display().to_string()], None)?;
    Ok(())
}

/// Where a kernel built by the `linux` recipe puts its image, inside a store path.
pub fn kernel_in(tree: &Path) -> Option<PathBuf> {
    let boot = tree.join("boot");
    let mut found: Vec<_> = fs::read_dir(boot).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("vmlinuz"))).collect();
    found.sort();
    found.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        calls: Vec<(String, Vec<String>, Option<String>)>,
    }
    impl Runner for Fake {
        fn run(&mut self, program: &str, args: &[String], stdin: Option<&str>) -> Result<String, IsoError> {
            self.calls.push((program.into(), args.to_vec(), stdin.map(String::from)));
            Ok("$6$novaralive$HASH\n".into())
        }
    }

    fn fixture(t: &Path) -> IsoSpec {
        let root = t.join("root");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/busybox"), "ELF").unwrap();
        let lim = t.join("limine");
        fs::create_dir_all(&lim).unwrap();
        for f in LIMINE_FILES {
            fs::write(lim.join(f), f).unwrap();
        }
        fs::write(t.join("vmlinuz"), "KERNEL").unwrap();
        IsoSpec {
            rootfs: root,
            kernel: t.join("vmlinuz"),
            limine_dir: lim,
            work: t.join("work"),
            out: t.join("out.iso"),
            label: "NOVARA_LIVE".into(),
            hostname: "novara-live".into(),
            cmdline: "console=ttyS0".into(),
            root_password: "root".into(),
            trusted_keys: vec![],
        }
    }

    #[test]
    fn builds_in_order_and_lays_out_the_image() {
        let t = tempfile::tempdir().unwrap();
        let spec = fixture(t.path());
        let mut r = Fake::default();
        build(&spec, &mut r).unwrap();
        let order: Vec<_> = r.calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(order, ["openssl", "mksquashfs", "xorriso", "limine"]);
        // The password goes in on stdin, never on the command line.
        assert_eq!(r.calls[0].2.as_deref(), Some("root\n"));
        assert!(!r.calls[0].1.iter().any(|a| a == "root"));
        let iso = t.path().join("work/iso");
        assert_eq!(fs::read_to_string(iso.join("boot/vmlinuz")).unwrap(), "KERNEL");
        assert!(iso.join("boot/initramfs.img").metadata().unwrap().len() > 0);
        assert!(iso.join("EFI/BOOT/BOOTX64.EFI").exists());
        let conf = fs::read_to_string(iso.join("boot/limine/limine.conf")).unwrap();
        assert!(conf.contains("console=ttyS0"));
        // The system got its live /etc, with the hash openssl produced.
        let shadow = fs::read_to_string(spec.rootfs.join("etc/shadow")).unwrap();
        assert!(shadow.starts_with("root:$6$novaralive$HASH:"));
        // Limine is installed into the finished image.
        assert_eq!(r.calls[3].1, [spec.out.display().to_string()].iter().fold(vec!["bios-install".to_string()], |mut v, s| { v.push(s.clone()); v }));
    }

    #[test]
    fn the_initramfs_is_gzip_cpio_with_init() {
        let t = tempfile::tempdir().unwrap();
        let spec = fixture(t.path());
        build(&spec, &mut Fake::default()).unwrap();
        let bytes = fs::read(t.path().join("work/iso/boot/initramfs.img")).unwrap();
        assert_eq!(&bytes[..2], [0x1f, 0x8b]);
        let mut raw = vec![];
        io::copy(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut raw).unwrap();
        assert_eq!(&raw[..6], b"070701");
        assert!(raw.windows(5).any(|w| w == b"init\0"));
        assert!(raw.windows(10).any(|w| w == b"TRAILER!!!"));
    }

    #[test]
    fn missing_inputs_are_reported_before_anything_runs() {
        let t = tempfile::tempdir().unwrap();
        let mut spec = fixture(t.path());
        fs::remove_file(spec.limine_dir.join("limine-uefi-cd.bin")).unwrap();
        let mut r = Fake::default();
        assert!(matches!(build(&spec, &mut r), Err(IsoError::Missing(_))));
        assert!(r.calls.is_empty());
        let t2 = tempfile::tempdir().unwrap();
        spec = fixture(t2.path());
        spec.label = "not valid".into();
        assert!(matches!(build(&spec, &mut r), Err(IsoError::Invalid { .. })));
        spec = fixture(t2.path());
        fs::remove_file(spec.rootfs.join("bin/busybox")).unwrap();
        assert!(matches!(build(&spec, &mut r), Err(IsoError::Missing(_))));
    }

    #[test]
    fn a_used_work_directory_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let spec = fixture(t.path());
        fs::create_dir_all(&spec.work).unwrap();
        fs::write(spec.work.join("old"), "").unwrap();
        assert!(matches!(build(&spec, &mut Fake::default()), Err(IsoError::Invalid { .. })));
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn trusted_keys_are_installed_where_forge_looks_for_them() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/busybox"), "ELF").unwrap();
        let lim = t.path().join("limine");
        fs::create_dir_all(&lim).unwrap();
        for f in LIMINE_FILES {
            fs::write(lim.join(f), f).unwrap();
        }
        fs::write(t.path().join("vmlinuz"), "K").unwrap();
        fs::write(t.path().join("project.pub"), "version = 1\npublic_key = \"ab\"\n").unwrap();
        struct Quiet;
        impl Runner for Quiet {
            fn run(&mut self, _: &str, _: &[String], _: Option<&str>) -> Result<String, IsoError> {
                Ok("$6$x$y".into())
            }
        }
        let spec = IsoSpec {
            rootfs: root.clone(),
            kernel: t.path().join("vmlinuz"),
            limine_dir: lim,
            work: t.path().join("work"),
            out: t.path().join("o.iso"),
            label: "NOVARA_LIVE".into(),
            hostname: "h".into(),
            cmdline: "c".into(),
            root_password: "root".into(),
            trusted_keys: vec![t.path().join("project.pub")],
        };
        build(&spec, &mut Quiet).unwrap();
        let k = fs::read_to_string(root.join("etc/forge/keys/project.pub")).unwrap();
        assert!(k.contains("public_key"), "{k}");
    }
}
