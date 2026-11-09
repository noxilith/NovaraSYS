//! The build sandbox, built from Linux namespaces with no external tools.
//!
//! `forge` re-runs itself as a helper (`forge __sandbox <config>`). The helper
//! is a fresh single-threaded process, which is what `unshare(2)` needs:
//!
//! 1. it unshares user, mount, PID, network, UTS and IPC namespaces and maps
//!    "root" inside onto the calling user (no real privilege is gained);
//! 2. it forks, so the child is PID 1 of the new PID namespace;
//! 3. the child builds a root filesystem on a fresh tmpfs from only the
//!    mounts the build is allowed to see (read-only unless marked writable),
//!    `pivot_root`s into it, and `exec`s `/bin/sh -c <script>`.
//!
//! The network namespace has only a down loopback interface, so a build
//! cannot reach the network, and the host's filesystem, processes and
//! hostname are not visible.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::Signal;
use nix::sys::statvfs::{FsFlags, statvfs};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, fork, getgid, getuid, pivot_root, sethostname};
use serde::{Deserialize, Serialize};

use crate::{BuildError, io_ctx};

/// Exit status the helper uses when it could not set the sandbox up (as
/// opposed to the build itself failing).
pub const SETUP_FAILED: i32 = 125;

const NAMESPACES: CloneFlags = CloneFlags::from_bits_truncate(
    CloneFlags::CLONE_NEWUSER.bits()
        | CloneFlags::CLONE_NEWNS.bits()
        | CloneFlags::CLONE_NEWPID.bits()
        | CloneFlags::CLONE_NEWNET.bits()
        | CloneFlags::CLONE_NEWUTS.bits()
        | CloneFlags::CLONE_NEWIPC.bits(),
);

/// Make host path `src` visible at `dst` inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bind {
    pub src: PathBuf,
    pub dst: PathBuf,
    /// Read-only unless this is set.
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// An empty host directory on which the new root filesystem is built.
    pub root_dir: PathBuf,
    pub binds: Vec<Bind>,
    /// `(link path, target)` created in the new root, e.g. `("/bin", "usr/bin")`.
    #[serde(default)]
    pub symlinks: Vec<(String, String)>,
    /// `(path, content)` small files created in the new root, e.g. `/etc/passwd`.
    #[serde(default)]
    pub files: Vec<(String, String)>,
    pub env: BTreeMap<String, String>,
    /// Working directory inside the sandbox.
    pub cwd: PathBuf,
    /// Run with `/bin/sh -c`.
    pub script: String,
    pub hostname: String,
}

// ---- parent side -----------------------------------------------------------

/// Run `config` in a sandbox by invoking `helper` (the `forge` binary). The
/// build's output goes straight to this process's stdout/stderr.
pub fn run(helper: &Path, config: &SandboxConfig, config_path: &Path) -> Result<ExitStatus, BuildError> {
    let text = toml::to_string(config).map_err(|e| BuildError::Sandbox(format!("cannot encode the configuration: {e}")))?;
    fs::write(config_path, text).map_err(io_ctx(format!("writing {}", config_path.display())))?;
    Command::new(helper)
        .arg("__sandbox")
        .arg(config_path)
        .status()
        .map_err(io_ctx(format!("running the sandbox helper {}", helper.display())))
}

/// Can this machine create the namespaces a build needs?
pub fn supported(helper: &Path) -> bool {
    Command::new(helper).arg("__sandbox-probe").status().is_ok_and(|s| s.success())
}

// ---- helper side -----------------------------------------------------------

type Res<T> = Result<T, String>;

fn sys<T>(what: &str, r: nix::Result<T>) -> Res<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn io_res<T>(what: impl AsRef<str>, r: io::Result<T>) -> Res<T> {
    r.map_err(|e| format!("{}: {e}", what.as_ref()))
}

/// Entry point of `forge __sandbox-probe`.
pub fn probe_main() -> i32 {
    match unshare(NAMESPACES) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("forge sandbox: cannot create namespaces: {e}");
            1
        }
    }
}

/// Entry point of `forge __sandbox <config>`. Returns the exit status to use.
pub fn child_main(config_path: &Path) -> i32 {
    let cfg: SandboxConfig = match fs::read_to_string(config_path)
        .map_err(|e| e.to_string())
        .and_then(|t| toml::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("forge sandbox: bad configuration {}: {e}", config_path.display());
            return SETUP_FAILED;
        }
    };
    match enter(&cfg) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("forge sandbox: {e}");
            SETUP_FAILED
        }
    }
}

fn enter(cfg: &SandboxConfig) -> Res<i32> {
    let (uid, gid) = (getuid(), getgid());
    sys("unshare", unshare(NAMESPACES)).map_err(|e| {
        format!("{e} (this needs unprivileged user namespaces; check /proc/sys/user/max_user_namespaces)")
    })?;
    // Map "root" inside onto the calling user. `setgroups` must be denied
    // before an unprivileged process may write its gid map.
    io_res("writing setgroups", fs::write("/proc/self/setgroups", "deny\n"))?;
    io_res("writing uid_map", fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")))?;
    io_res("writing gid_map", fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")))?;

    // The PID namespace applies to children, so fork: the child is its PID 1.
    // SAFETY: this process is single-threaded, so fork is sound.
    match sys("fork", unsafe { fork() })? {
        ForkResult::Child => {
            let code = match setup_root(cfg).and_then(|()| exec_script(cfg)) {
                Ok(never) => match never {},
                Err(e) => {
                    eprintln!("forge sandbox: {e}");
                    SETUP_FAILED
                }
            };
            std::process::exit(code);
        }
        ForkResult::Parent { child } => loop {
            match sys("waitpid", waitpid(child, None))? {
                WaitStatus::Exited(_, code) => return Ok(code),
                WaitStatus::Signaled(_, sig, _) => return Ok(128 + sig as i32),
                _ => {}
            }
        },
    }
}

fn depth(p: &Path) -> usize {
    p.components().count()
}

/// Where `path` (absolute inside the sandbox) lives on the host while the
/// new root is being assembled.
fn in_root(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

fn mount_none(src: Option<&str>, target: &Path, fstype: Option<&str>, flags: MsFlags, data: Option<&str>) -> Res<()> {
    sys(&format!("mounting on {}", target.display()), mount(src, target, fstype, flags, data))
}

/// Make a bind mount read-only. The kernel refuses unless the flags the
/// source mount already has (nosuid, nodev, ...) are repeated.
fn remount_readonly(target: &Path) -> Res<()> {
    let have = sys(&format!("statvfs {}", target.display()), statvfs(target))?.flags();
    let mut flags = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY;
    for (fs_flag, ms_flag) in [
        (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
        (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
        (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
        (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
        (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        (FsFlags::ST_RELATIME, MsFlags::MS_RELATIME),
    ] {
        if have.contains(fs_flag) {
            flags |= ms_flag;
        }
    }
    mount_none(None, target, None, flags, None)
}

/// Decode the octal escapes (`\040` for a space...) mountinfo uses in paths.
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b.get(i + 1..i + 4).filter(|d| b[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        if let Some(d) = octal {
            out.push(d.iter().fold(0u8, |acc, c| acc.wrapping_mul(8).wrapping_add(c - b'0')));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every mount point at or below `target`, from `/proc/self/mountinfo` text.
fn mounts_under(mountinfo: &str, target: &Path) -> Vec<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|p| PathBuf::from(unescape_mountinfo(p)))
        .filter(|p| p.starts_with(target))
        .collect()
}

/// A recursive bind brings along the source's submounts, so making the bind
/// read-only means making each of them read-only too (the kernel only
/// remounts one mount at a time).
fn remount_tree_readonly(target: &Path) -> Res<()> {
    let info = io_res("reading /proc/self/mountinfo", fs::read_to_string("/proc/self/mountinfo"))?;
    for m in mounts_under(&info, target) {
        remount_readonly(&m)?;
    }
    Ok(())
}

fn make_mount_point(src: &Path, target: &Path) -> Res<()> {
    if src.is_dir() {
        io_res(format!("creating {}", target.display()), fs::create_dir_all(target))
    } else {
        if let Some(parent) = target.parent() {
            io_res(format!("creating {}", parent.display()), fs::create_dir_all(parent))?;
        }
        io_res(format!("creating {}", target.display()), File::create(target).map(drop))
    }
}

fn setup_root(cfg: &SandboxConfig) -> Res<()> {
    sys("PR_SET_PDEATHSIG", nix::sys::prctl::set_pdeathsig(Signal::SIGKILL))?;
    let root = &cfg.root_dir;

    // Nothing we mount may propagate back to the host.
    mount_none(None, Path::new("/"), None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)?;
    mount_none(Some("tmpfs"), root, Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=755"))?;

    // /proc for the new PID namespace, a minimal /dev, and a scratch /tmp.
    // These come BEFORE the binds: a bind whose destination lies below one of
    // them (a store or repository under /tmp, say) must be mounted on top of it,
    // not hidden underneath it.
    let proc_dir = root.join("proc");
    io_res("creating /proc", fs::create_dir_all(&proc_dir))?;
    mount_none(Some("proc"), &proc_dir, Some("proc"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC, None)?;

    let dev = root.join("dev");
    io_res("creating /dev", fs::create_dir_all(&dev))?;
    mount_none(Some("tmpfs"), &dev, Some("tmpfs"), MsFlags::MS_NOSUID, Some("mode=755"))?;
    for node in ["null", "zero", "full", "random", "urandom", "tty"] {
        let host = PathBuf::from("/dev").join(node);
        if host.exists() {
            let target = dev.join(node);
            io_res(format!("creating /dev/{node}"), File::create(&target).map(drop))?;
            sys(
                &format!("binding /dev/{node}"),
                mount(Some(&host), &target, None::<&str>, MsFlags::MS_BIND, None::<&str>),
            )?;
        }
    }
    for (link, target) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
        io_res(format!("linking /dev/{link}"), symlink(target, dev.join(link)))?;
    }
    let shm = dev.join("shm");
    io_res("creating /dev/shm", fs::create_dir_all(&shm))?;
    mount_none(Some("tmpfs"), &shm, Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=1777"))?;

    let tmp = root.join("tmp");
    io_res("creating /tmp", fs::create_dir_all(&tmp))?;
    mount_none(Some("tmpfs"), &tmp, Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=1777"))?;

    // Parents before children, so a nested mount lands in its parent's mount.
    let mut binds = cfg.binds.clone();
    binds.sort_by_key(|b| depth(&b.dst));
    for b in &binds {
        let target = in_root(root, &b.dst);
        make_mount_point(&b.src, &target)?;
        // Recursive: inside a user namespace the kernel refuses a plain bind
        // of a directory that has other mounts below it.
        let flags = if b.src.is_dir() { MsFlags::MS_BIND | MsFlags::MS_REC } else { MsFlags::MS_BIND };
        sys(
            &format!("binding {}", b.src.display()),
            mount(Some(&b.src), &target, None::<&str>, flags, None::<&str>),
        )?;
        if !b.writable {
            remount_tree_readonly(&target)?;
        }
    }
    for (link, target) in &cfg.symlinks {
        let at = in_root(root, Path::new(link));
        if let Some(parent) = at.parent() {
            io_res(format!("creating {}", parent.display()), fs::create_dir_all(parent))?;
        }
        io_res(format!("linking {link}"), symlink(target, &at))?;
    }
    for (path, content) in &cfg.files {
        let at = in_root(root, Path::new(path));
        if let Some(parent) = at.parent() {
            io_res(format!("creating {}", parent.display()), fs::create_dir_all(parent))?;
        }
        io_res(format!("writing {path}"), fs::write(&at, content))?;
    }

    // Switch to the new root and drop every reference to the old one.
    let old = root.join(".oldroot");
    io_res("creating .oldroot", fs::create_dir_all(&old))?;
    sys("pivot_root", pivot_root(root, &old))?;
    sys("chdir /", chdir("/"))?;
    sys("unmounting the old root", umount2("/.oldroot", MntFlags::MNT_DETACH))?;
    io_res("removing .oldroot", fs::remove_dir("/.oldroot"))?;

    // The skeleton of the root (/usr, /etc, ...) must not be writable: a build
    // that forgets $DESTDIR should fail, not "install" into a directory that
    // vanishes. The writable places (/build, /tmp, /dev/shm) are separate mounts.
    remount_readonly(Path::new("/"))?;

    sys("sethostname", sethostname(&cfg.hostname))?;
    sys(&format!("chdir {}", cfg.cwd.display()), chdir(&cfg.cwd))
}

fn exec_script(cfg: &SandboxConfig) -> Res<std::convert::Infallible> {
    // `exec` only returns on failure.
    let err = Command::new("/bin/sh").arg("-c").arg(&cfg.script).env_clear().envs(&cfg.env).exec();
    Err(format!("cannot run /bin/sh in the sandbox (are the tools mounted?): {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SandboxConfig {
        SandboxConfig {
            root_dir: "/tmp/root".into(),
            binds: vec![
                Bind { src: "/usr".into(), dst: "/usr".into(), writable: false },
                Bind { src: "/tmp/w".into(), dst: "/build".into(), writable: true },
            ],
            symlinks: vec![("/bin".into(), "usr/bin".into())],
            files: vec![("/etc/passwd".into(), "root:x:0:0::/:/bin/sh\n".into())],
            env: BTreeMap::from([("A".into(), "1".into())]),
            cwd: "/build".into(),
            script: "echo \"hi 'there'\"\nexit 3\n".into(),
            hostname: "b".into(),
        }
    }

    #[test]
    fn configuration_round_trips_through_toml_including_awkward_scripts() {
        let c = sample();
        let text = toml::to_string(&c).unwrap();
        assert_eq!(toml::from_str::<SandboxConfig>(&text).unwrap(), c);
    }

    #[test]
    fn finds_submounts_in_mountinfo_and_decodes_escapes() {
        let info = "\
24 1 0:21 / /tmp/root rw - tmpfs tmpfs rw
25 24 0:22 / /tmp/root/usr rw - btrfs /dev/x rw
26 25 0:23 / /tmp/root/usr/var/lib/with\\040space rw - tmpfs t rw
27 24 0:24 / /tmp/root-other rw - tmpfs t rw
28 1 0:25 / /elsewhere rw - tmpfs t rw
";
        let found = mounts_under(info, Path::new("/tmp/root/usr"));
        assert_eq!(found, [PathBuf::from("/tmp/root/usr"), PathBuf::from("/tmp/root/usr/var/lib/with space")]);
        assert!(mounts_under(info, Path::new("/nothing")).is_empty());
        // `/tmp/root-other` shares a string prefix with `/tmp/root` but is not below it.
        assert_eq!(mounts_under(info, Path::new("/tmp/root")).len(), 3);
        assert_eq!(unescape_mountinfo("a\\011b\\134c"), "a\tb\\c");
        assert_eq!(unescape_mountinfo("plain"), "plain");
    }

    #[test]
    fn mounts_are_ordered_parents_first() {
        let mut dsts = ["/a/b/c", "/a", "/a/b", "/z"].map(PathBuf::from).to_vec();
        dsts.sort_by_key(|p| depth(p));
        assert_eq!(dsts, ["/a", "/z", "/a/b", "/a/b/c"].map(PathBuf::from));
    }

    #[test]
    fn paths_are_placed_under_the_new_root() {
        let root = Path::new("/work/root");
        assert_eq!(in_root(root, Path::new("/usr/bin")), Path::new("/work/root/usr/bin"));
        assert_eq!(in_root(root, Path::new("etc")), Path::new("/work/root/etc"));
    }
}
