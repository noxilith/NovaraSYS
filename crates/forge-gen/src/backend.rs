use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

/// The filesystem operations generations need. Everything else (renames,
/// small files) goes through `std::fs`, which works on btrfs subvolumes too.
pub trait Backend {
    /// Snapshot subvolume `src` as the new subvolume `dst`.
    fn snapshot(&self, src: &Path, dst: &Path, readonly: bool) -> io::Result<()>;
    /// Delete the subvolume at `path`.
    fn delete(&self, path: &Path) -> io::Result<()>;
}

/// Real backend: shells out to `btrfs-progs`.
pub struct BtrfsCli;

fn run_btrfs(args: &[&std::ffi::OsStr]) -> io::Result<()> {
    let out = Command::new("btrfs").args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "btrfs {}: {}",
            args.first().and_then(|a| a.to_str()).unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

impl Backend for BtrfsCli {
    fn snapshot(&self, src: &Path, dst: &Path, readonly: bool) -> io::Result<()> {
        let mut args: Vec<&std::ffi::OsStr> = vec!["subvolume".as_ref(), "snapshot".as_ref()];
        if readonly {
            args.push("-r".as_ref());
        }
        args.push(src.as_os_str());
        args.push(dst.as_os_str());
        run_btrfs(&args)
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        run_btrfs(&["subvolume".as_ref(), "delete".as_ref(), path.as_os_str()])
    }
}

/// Testing backend: "snapshots" are plain directory copies. It needs no btrfs
/// and no privileges, so the generation logic can be exercised on any
/// filesystem. It is NOT safe or efficient for a real system.
pub struct CopyBackend;

fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else if ty.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

impl Backend for CopyBackend {
    fn snapshot(&self, src: &Path, dst: &Path, _readonly: bool) -> io::Result<()> {
        copy_dir(src, dst)
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}
