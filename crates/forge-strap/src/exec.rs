use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use forge_gen::BootInfo;
use forge_schema::{Libc, SystemDecl};

use crate::{Firmware, Plan, Step, split_partition};

/// Paths in the live system that must not be copied to the installed one
/// (artifacts only the live environment uses).
pub const LIVE_ONLY_PATHS: &[&str] = &["/fastboot", "/.live", "/medium"];

/// Groups of the user the installer creates: administrator (`wheel`, named by sudoers), package store, devices.
pub const USER_GROUPS: &[&str] = &["wheel", "forge", "audio", "video", "input", "users"];

/// Directories that exist only as mount points / runtime state. They are not
/// copied; empty ones are created on the target instead.
const RUNTIME_DIRS: &[&str] = &["dev", "proc", "sys", "run", "tmp", "mnt"];

const ESP_TYPE_GUID: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

#[derive(Debug, thiserror::Error)]
pub enum StrapError {
    #[error("`{cmd}` failed ({status}): {stderr}")]
    Command { cmd: String, status: String, stderr: String },
    #[error("could not run `{cmd}`: {source}")]
    Spawn { cmd: String, source: io::Error },
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("{0} is not a block device")]
    NotBlock(String),
    #[error("{0} is mounted; unmount it first")]
    Mounted(String),
    #[error("no kernel found in the target's /boot ({0}); use --kernel (and --initramfs if the kernel needs one)")]
    KernelNotFound(String),
    #[error("cannot tell whether the system is glibc or musl; pass --libc")]
    UnknownLibc,
    #[error("invalid {what}: {value}")]
    Invalid { what: &'static str, value: String },
    #[error("Limine file not found: {0}")]
    LimineMissing(PathBuf),
    #[error("step \"{step}\" failed: {source}\nthe target disks are in an incomplete state")]
    Step { step: String, source: Box<StrapError> },
}

fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> StrapError {
    let context = context.into();
    move |source| StrapError::Io { context, source }
}

// ---- running commands ----------------------------------------------------

/// One external command. `stdin` is never printed (it can carry passwords).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    /// Let the command write straight to the terminal (progress output).
    pub show: bool,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Cmd {
            program: program.into(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            stdin: None,
            show: false,
        }
    }
    fn arg(mut self, a: impl AsRef<std::ffi::OsStr>) -> Self {
        self.args.push(a.as_ref().to_string_lossy().into_owned());
        self
    }
    fn stdin(mut self, s: String) -> Self {
        self.stdin = Some(s);
        self
    }
    fn show(mut self) -> Self {
        self.show = true;
        self
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program)?;
        for a in &self.args {
            write!(f, " {a}")?;
        }
        Ok(())
    }
}

/// Runs commands. The real one spawns processes; tests record them.
pub trait Runner {
    /// Run `cmd` and return its stdout.
    fn run(&mut self, cmd: &Cmd) -> Result<String, StrapError>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<String, StrapError> {
        let shown = cmd.to_string();
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args);
        c.stdin(if cmd.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        if cmd.show {
            c.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            c.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        let mut child = c.spawn().map_err(|source| StrapError::Spawn { cmd: shown.clone(), source })?;
        if let (Some(input), Some(mut pipe)) = (&cmd.stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes()).map_err(io_ctx(format!("writing to `{shown}`")))?;
        }
        let out = child.wait_with_output().map_err(io_ctx(format!("waiting for `{shown}`")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(StrapError::Command {
                cmd: shown,
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }
}

/// Progress and warnings go through this so the CLI controls the output.
pub trait Ui {
    fn step(&mut self, n: usize, total: usize, text: &str);
    fn warn(&mut self, text: &str);
}

// ---- request -------------------------------------------------------------

pub struct NewUser {
    pub name: String,
    pub password: String,
}

/// Accounts to set up on the installed system, collected before anything is
/// written so a typo cannot waste a half-finished install.
#[derive(Default)]
pub struct Accounts {
    pub user: Option<NewUser>,
    pub root_password: Option<String>,
}

fn valid_username(n: &str) -> bool {
    let mut chars = n.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && n.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn valid_password(p: &str) -> bool {
    !p.is_empty() && !p.contains(['\n', '\r', '\0'])
}

impl Accounts {
    pub fn validate(&self) -> Result<(), StrapError> {
        if let Some(u) = &self.user {
            if !valid_username(&u.name) {
                return Err(StrapError::Invalid { what: "user name", value: u.name.clone() });
            }
            if !valid_password(&u.password) {
                return Err(StrapError::Invalid { what: "password", value: "(hidden)".into() });
            }
        }
        if self.root_password.as_deref().is_some_and(|p| !valid_password(p)) {
            return Err(StrapError::Invalid { what: "root password", value: "(hidden)".into() });
        }
        Ok(())
    }
}

pub struct Request {
    pub plan: Plan,
    /// Directory to copy the system from (`/` on the live system).
    pub source: PathBuf,
    /// Where the target root is mounted during the install.
    pub mount_point: PathBuf,
    /// Temporary mount of the top-level btrfs volume (to create subvolumes).
    pub top_mount: PathBuf,
    /// Directory with `BOOTX64.EFI` and `limine-bios.sys`.
    pub limine_dir: PathBuf,
    pub hostname: String,
    pub libc: Option<Libc>,
    pub kernel: Option<String>,
    pub initramfs: Option<String>,
    /// Extra kernel command line, appended after `root=UUID=...`.
    pub extra_cmdline: String,
    /// Add a firmware (NVRAM) boot entry for UEFI installs.
    pub efi_entry: bool,
    pub accounts: Accounts,
}

// ---- preflight -----------------------------------------------------------

/// Refuse to continue if a target device is missing, not a block device, or
/// mounted. `mounts` is the content of `/proc/self/mounts`.
pub fn preflight(plan: &Plan, mounts: &str, is_block: &dyn Fn(&str) -> bool) -> Result<(), StrapError> {
    for dev in [plan.root_dev.as_str(), plan.boot_dev.as_str()] {
        if !is_block(dev) {
            return Err(StrapError::NotBlock(dev.into()));
        }
        if mounts.lines().any(|l| l.split_whitespace().next() == Some(dev)) {
            return Err(StrapError::Mounted(dev.into()));
        }
    }
    Ok(())
}

// ---- helpers -------------------------------------------------------------

/// Compare version-like names numerically ("6.10" > "6.9").
fn natural_cmp(a: &str, b: &str) -> Ordering {
    fn tokens(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let d = c.is_ascii_digit();
            match out.last_mut() {
                Some((is_d, t)) if *is_d == d => t.push(c),
                _ => out.push((d, c.to_string())),
            }
        }
        out
    }
    let (ta, tb) = (tokens(a), tokens(b));
    for (x, y) in ta.iter().zip(&tb) {
        let ord = if x.0 && y.0 {
            let (nx, ny) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
            nx.len().cmp(&ny.len()).then_with(|| nx.cmp(ny))
        } else {
            x.1.cmp(&y.1)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ta.len().cmp(&tb.len())
}

fn find_boot_files(boot: &Path, kernel: Option<&str>, initramfs: Option<&str>) -> Result<BootInfo, StrapError> {
    let not_found = || StrapError::KernelNotFound(boot.display().to_string());
    let kernel = match kernel {
        Some(k) => k.to_string(),
        None => {
            let mut found: Vec<String> = fs::read_dir(boot)
                .map_err(io_ctx(format!("reading {}", boot.display())))?
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("vmlinuz-"))
                .collect();
            found.sort_by(|a, b| natural_cmp(a, b));
            found.pop().ok_or_else(not_found)?
        }
    };
    if !boot.join(&kernel).is_file() {
        return Err(not_found());
    }
    // The initramfs is optional: without one the kernel mounts the root itself.
    let initramfs = match initramfs {
        Some(i) => {
            if !boot.join(i).is_file() {
                return Err(not_found());
            }
            i.to_string()
        }
        None => {
            let default = format!("initramfs-{}.img", kernel.strip_prefix("vmlinuz-").unwrap_or(&kernel));
            if boot.join(&default).is_file() { default } else { String::new() }
        }
    };
    Ok(BootInfo { kernel, initramfs, cmdline: String::new() })
}

fn detect_libc(root: &Path) -> Option<Libc> {
    for dir in ["lib", "usr/lib", "lib64", "usr/lib64"] {
        let Ok(rd) = fs::read_dir(root.join(dir)) else { continue };
        for name in rd.filter_map(Result::ok).filter_map(|e| e.file_name().into_string().ok()) {
            if name.starts_with("ld-musl-") {
                return Some(Libc::Musl);
            }
            if name.starts_with("ld-linux") {
                return Some(Libc::Glibc);
            }
        }
    }
    None
}

fn valid_hostname(h: &str) -> bool {
    !h.is_empty() && h.len() <= 63 && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn write_file(path: &Path, content: &str) -> Result<(), StrapError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_ctx(format!("creating {}", dir.display())))?;
    }
    fs::write(path, content).map_err(io_ctx(format!("writing {}", path.display())))
}

// ---- execution -----------------------------------------------------------

struct Exec<'a> {
    req: &'a Request,
    run: &'a mut dyn Runner,
    ui: &'a mut dyn Ui,
    mounts: Vec<PathBuf>,
    root_uuid: Option<String>,
    boot_uuid: Option<String>,
    boot_info: Option<BootInfo>,
}

impl Exec<'_> {
    fn cmd(&mut self, cmd: &Cmd) -> Result<String, StrapError> {
        self.run.run(cmd)
    }

    fn mount(&mut self, source: &str, target: &Path, opts: Option<&str>) -> Result<(), StrapError> {
        fs::create_dir_all(target).map_err(io_ctx(format!("creating {}", target.display())))?;
        let mut c = Cmd::new("mount", &[]);
        if let Some(o) = opts {
            c = c.arg("-o").arg(o);
        }
        self.cmd(&c.arg(source).arg(target))?;
        self.mounts.push(target.to_path_buf());
        Ok(())
    }

    fn unmount_all(&mut self) -> Result<(), StrapError> {
        while let Some(p) = self.mounts.pop() {
            self.cmd(&Cmd::new("umount", &[]).arg(&p))?;
        }
        Ok(())
    }

    fn uuid(&mut self, dev: &str) -> Result<String, StrapError> {
        let out = self.cmd(&Cmd::new("blkid", &["-s", "UUID", "-o", "value", dev]))?;
        let u = out.trim();
        if u.is_empty() || !u.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(StrapError::Invalid { what: "filesystem UUID", value: u.into() });
        }
        Ok(u.to_string())
    }

    /// The partition's own UUID (GPT) or disk-signature id (MBR): the only `root=` form a kernel
    /// without an initramfs understands.
    fn partuuid(&mut self, dev: &str) -> Result<String, StrapError> {
        let out = self.cmd(&Cmd::new("blkid", &["-s", "PARTUUID", "-o", "value", dev]))?;
        let u = out.trim();
        if u.is_empty() || !u.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(StrapError::Invalid { what: "partition UUID", value: u.into() });
        }
        Ok(u.to_string())
    }

    fn mp(&self) -> &Path {
        &self.req.mount_point
    }

    fn step(&mut self, step: &Step) -> Result<(), StrapError> {
        let plan = &self.req.plan;
        let (root_dev, boot_dev) = (plan.root_dev.clone(), plan.boot_dev.clone());
        match step {
            Step::FormatBoot { dev, .. } => {
                self.cmd(&Cmd::new("mkfs.vfat", &["-F", "32", "-n", "NOVARA"]).arg(dev))?;
                if plan.firmware == Firmware::Uefi
                    && let Some((disk, part)) = split_partition(dev) {
                        let c = Cmd::new("sfdisk", &["--part-type"]).arg(&disk).arg(part.to_string()).arg(ESP_TYPE_GUID);
                        if let Err(e) = self.cmd(&c) {
                            self.ui.warn(&format!("could not mark {dev} as an EFI System Partition: {e}"));
                        }
                    }
            }
            Step::FormatRoot { dev, .. } => {
                self.cmd(&Cmd::new("mkfs.btrfs", &["-f", "-L", "novara"]).arg(dev))?;
            }
            Step::CreateSubvolumes(subvols) => {
                let top = self.req.top_mount.clone();
                self.mount(&root_dev, &top, None)?;
                for sv in subvols {
                    self.cmd(&Cmd::new("btrfs", &["subvolume", "create"]).arg(top.join(sv)))?;
                }
                self.unmount_all()?;
            }
            Step::MountTarget => {
                let mp = self.mp().to_path_buf();
                self.mount(&root_dev, &mp, Some("subvol=@,noatime"))?;
                for (sv, dir) in [("@snapshots", ".snapshots"), ("@home", "home"), ("@store", "forge/store")] {
                    self.mount(&root_dev, &mp.join(dir), Some(&format!("subvol={sv},noatime")))?;
                }
                self.mount(&boot_dev, &mp.join("boot"), None)?;
            }
            Step::CopyRootfs => self.copy_rootfs()?,
            Step::ConfigureTarget => self.configure_target()?,
            Step::InstallLimine(fw) => self.install_limine(*fw)?,
            Step::WriteLimineConf => self.write_limine_conf()?,
            Step::CreateUsers => self.create_users()?,
            Step::Finish => {
                self.cmd(&Cmd::new("sync", &[]))?;
                self.unmount_all()?;
            }
        }
        Ok(())
    }

    fn copy_rootfs(&mut self) -> Result<(), StrapError> {
        let mp = self.mp().to_path_buf();
        let mut c = Cmd::new("rsync", &["-aHX", "--numeric-ids", "--info=progress2"]);
        for d in RUNTIME_DIRS {
            c = c.arg(format!("--exclude=/{d}"));
        }
        // /boot is its own FAT32 partition, which cannot hold ownership, ACLs or
        // xattrs, so it is copied separately below with flags FAT accepts.
        for p in ["/lost+found", "/.snapshots", "/boot"].iter().chain(LIVE_ONLY_PATHS) {
            c = c.arg(format!("--exclude={p}"));
        }
        let source = format!("{}/", self.req.source.display().to_string().trim_end_matches('/'));
        self.cmd(&c.arg(&source).arg(format!("{}/", mp.display())).show())?;

        if self.req.source.join("boot").is_dir() {
            let boot = Cmd::new(
                "rsync",
                &["-rtD", "--no-perms", "--no-owner", "--no-group", "--modify-window=2", "--info=progress2"],
            )
            .arg(format!("{source}boot/"))
            .arg(format!("{}/boot/", mp.display()))
            .show();
            self.cmd(&boot)?;
        }

        for d in RUNTIME_DIRS {
            let p = mp.join(d);
            fs::create_dir_all(&p).map_err(io_ctx(format!("creating {}", p.display())))?;
        }
        let tmp = mp.join("tmp");
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o1777))
            .map_err(io_ctx(format!("setting permissions on {}", tmp.display())))
    }

    fn configure_target(&mut self) -> Result<(), StrapError> {
        let mp = self.mp().to_path_buf();
        let root_uuid = self.uuid(&self.req.plan.root_dev.clone())?;
        let boot_uuid = self.uuid(&self.req.plan.boot_dev.clone())?;

        let mut fstab = String::from("# /etc/fstab: written by forge strap\n");
        for (sv, dir) in [("@", "/"), ("@snapshots", "/.snapshots"), ("@home", "/home"), ("@store", "/forge/store")] {
            fstab.push_str(&format!("UUID={root_uuid} {dir} btrfs subvol={sv},noatime 0 0\n"));
        }
        fstab.push_str(&format!("UUID={boot_uuid} /boot vfat umask=0077 0 2\n"));
        write_file(&mp.join("etc/fstab"), &fstab)?;

        // The live system's name must not follow the install: rcS sets the hostname from this file
        // at every boot (`forge apply` writes it too, but the first boot comes before any apply).
        if !valid_hostname(&self.req.hostname) {
            return Err(StrapError::Invalid { what: "hostname", value: self.req.hostname.clone() });
        }
        write_file(&mp.join("etc/hostname"), &format!("{}\n", self.req.hostname))?;

        let decl_path = mp.join("etc/forge/system.toml");
        if !decl_path.exists() {
            if !valid_hostname(&self.req.hostname) {
                return Err(StrapError::Invalid { what: "hostname", value: self.req.hostname.clone() });
            }
            let libc = self
                .req
                .libc
                .or_else(|| detect_libc(&mp))
                .ok_or(StrapError::UnknownLibc)?;
            let libc = if libc == Libc::Musl { "musl" } else { "glibc" };
            let mut decl = format!(
                "hostname = \"{}\"\nlibc = \"{libc}\"\nrepos = [\"{}\"]\npackages = []\n",
                self.req.hostname,
                forge_schema::DEFAULT_REPO_URL
            );
            if let Some(u) = &self.req.accounts.user {
                let groups: Vec<String> = USER_GROUPS.iter().map(|g| format!("\"{g}\"")).collect();
                decl.push_str(&format!("\n[[users]]\nname = \"{}\"\ngroups = [{}]\n", u.name, groups.join(", ")));
            }
            SystemDecl::parse(&decl).map_err(|e| StrapError::Invalid {
                what: "generated system.toml",
                value: e.to_string(),
            })?;
            write_file(&decl_path, &decl)?;
        }
        self.root_uuid = Some(root_uuid);
        self.boot_uuid = Some(boot_uuid);
        Ok(())
    }

    fn install_limine(&mut self, fw: Firmware) -> Result<(), StrapError> {
        let boot = self.mp().join("boot");
        let limine = |name: &str| -> Result<PathBuf, StrapError> {
            let p = self.req.limine_dir.join(name);
            if p.is_file() { Ok(p) } else { Err(StrapError::LimineMissing(p)) }
        };
        match fw {
            Firmware::Uefi => {
                let efi = limine("BOOTX64.EFI")?;
                let dst = boot.join("EFI/BOOT/BOOTX64.EFI");
                fs::create_dir_all(dst.parent().expect("has parent")).map_err(io_ctx("creating EFI/BOOT"))?;
                // The fallback path works even if the firmware entry below fails.
                fs::copy(&efi, &dst).map_err(io_ctx(format!("copying {}", efi.display())))?;
                if self.req.efi_entry
                    && let Some((disk, part)) = split_partition(&self.req.plan.boot_dev) {
                        let c = Cmd::new("efibootmgr", &["--create", "--label", "Novara", "--disk"])
                            .arg(&disk)
                            .arg("--part")
                            .arg(part.to_string())
                            .arg("--loader")
                            .arg("\\EFI\\BOOT\\BOOTX64.EFI");
                        if let Err(e) = self.cmd(&c) {
                            self.ui.warn(&format!(
                                "could not add a firmware boot entry ({e}); the disk should still boot via the removable-media path"
                            ));
                        }
                    }
            }
            Firmware::Bios => {
                let sys = limine("limine-bios.sys")?;
                fs::copy(&sys, boot.join("limine-bios.sys")).map_err(io_ctx(format!("copying {}", sys.display())))?;
                let (disk, _) = split_partition(&self.req.plan.boot_dev)
                    .ok_or_else(|| StrapError::Invalid { what: "boot device", value: self.req.plan.boot_dev.clone() })?;
                self.cmd(&Cmd::new("limine", &["bios-install"]).arg(&disk))?;
            }
        }
        Ok(())
    }

    fn write_limine_conf(&mut self) -> Result<(), StrapError> {
        let boot = self.mp().join("boot");
        let mut info = find_boot_files(&boot, self.req.kernel.as_deref(), self.req.initramfs.as_deref())?;
        let root_uuid = self.root_uuid.clone().expect("ConfigureTarget runs first");
        let mut cmdline = if info.initramfs.is_empty() {
            let part = self.partuuid(&self.req.plan.root_dev.clone())?;
            format!("root=PARTUUID={part}")
        } else {
            format!("root=UUID={root_uuid}")
        };
        let extra = self.req.extra_cmdline.trim();
        if !extra.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(&forge_gen::base_cmdline(extra));
        }
        info.cmdline = cmdline;
        info.validate().map_err(|e| StrapError::Invalid { what: "boot entry", value: e.to_string() })?;
        write_file(&boot.join("limine.conf"), &forge_gen::render_limine(&info, "@", &[]))?;
        self.boot_info = Some(info);
        Ok(())
    }

    fn create_users(&mut self) -> Result<(), StrapError> {
        let mp = self.mp().to_path_buf();
        let acc = &self.req.accounts;
        let (user, root_pw) = (
            acc.user.as_ref().map(|u| (u.name.clone(), u.password.clone())),
            acc.root_password.clone(),
        );
        if let Some((name, pw)) = user {
            // The installer's user is an administrator (`wheel`, which sudoers names), may use the package store
            // (`forge`), and joins the usual hardware groups. Creating them first with -f is harmless if they exist.
            for g in USER_GROUPS {
                self.cmd(&Cmd::new("groupadd", &["--root"]).arg(&mp).arg("-f").arg(*g))?;
            }
            let groups = USER_GROUPS.join(",");
            self.cmd(
                &Cmd::new("useradd", &["--root"]).arg(&mp).args(["-m", "-G", groups.as_str(), "-s", "/bin/sh"]).arg(&name),
            )?;
            self.cmd(&Cmd::new("chpasswd", &["--root"]).arg(&mp).stdin(format!("{name}:{pw}\n")))?;
        }
        if let Some(pw) = root_pw {
            self.cmd(&Cmd::new("chpasswd", &["--root"]).arg(&mp).stdin(format!("root:{pw}\n")))?;
        }
        Ok(())
    }
}

impl Cmd {
    fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        for a in it {
            self = self.arg(a);
        }
        self
    }
}

/// Carry out the install. On failure, mounted target filesystems are
/// unmounted (best effort) and the error says which step failed.
pub fn execute(req: &Request, run: &mut dyn Runner, ui: &mut dyn Ui) -> Result<(), StrapError> {
    req.accounts.validate()?;
    let mut ex = Exec {
        req,
        run,
        ui,
        mounts: Vec::new(),
        root_uuid: None,
        boot_uuid: None,
        boot_info: None,
    };
    let total = req.plan.steps.len();
    for (i, step) in req.plan.steps.iter().enumerate() {
        ex.ui.step(i + 1, total, &step.to_string());
        if let Err(e) = ex.step(step) {
            while let Some(p) = ex.mounts.pop() {
                let _ = ex.run.run(&Cmd::new("umount", &[]).arg(&p));
            }
            return Err(StrapError::Step { step: step.to_string(), source: Box::new(e) });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
