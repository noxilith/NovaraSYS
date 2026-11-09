//! Putting a staged kernel where the system boots from. Old kernels are never touched: every
//! generation pins the kernel it was made with, so removing one would break a rollback.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::{KernelError, io_ctx};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub release: String,
    /// The kernel's file name in the boot directory (`vmlinuz-<release>`).
    pub kernel_file: String,
}

/// The release of the kernel staged in `dir` (the one `boot/vmlinuz-*`).
pub fn staged_release(dir: &Path) -> Result<String, KernelError> {
    let boot = dir.join("boot");
    let mut found: Vec<String> = fs::read_dir(&boot)
        .map_err(io_ctx(format!("reading {} (is this a staged kernel directory?)", boot.display())))?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_prefix("vmlinuz-").map(String::from))
        .collect();
    found.sort();
    match found.as_slice() {
        [one] if !one.is_empty() && one.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+')) => {
            Ok(one.clone())
        }
        [] => Err(KernelError::Invalid(format!("no vmlinuz-* in {}", boot.display()))),
        _ => Err(KernelError::Invalid(format!("{} holds more than one kernel, or an odd name: {found:?}", boot.display()))),
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), KernelError> {
    fs::create_dir_all(to).map_err(io_ctx(format!("creating {}", to.display())))?;
    for e in fs::read_dir(from).map_err(io_ctx(format!("reading {}", from.display())))? {
        let e = e.map_err(io_ctx("reading a directory"))?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ft = e.file_type().map_err(io_ctx("reading a file type"))?;
        if ft.is_dir() {
            copy_tree(&src, &dst)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&src).map_err(io_ctx("reading a symlink"))?;
            symlink(target, &dst).map_err(io_ctx(format!("creating {}", dst.display())))?;
        } else {
            fs::copy(&src, &dst).map_err(io_ctx(format!("copying {}", src.display())))?;
        }
    }
    Ok(())
}

/// Copy kernel, configuration and modules from `staged` into `boot_dir` and `root`. Fails if that
/// release is already installed, unless `force`.
pub fn install_files(staged: &Path, boot_dir: &Path, root: &Path, force: bool) -> Result<Installed, KernelError> {
    let release = staged_release(staged)?;
    let kernel_file = format!("vmlinuz-{release}");
    let config_file = format!("config-{release}");
    let mods_src = staged.join("usr/lib/modules").join(&release);
    if !mods_src.is_dir() {
        return Err(KernelError::Invalid(format!("{} has no modules for {release}", staged.display())));
    }
    let mods_dir = root.join("usr/lib/modules");
    let mods_dst = mods_dir.join(&release);
    let kernel_dst = boot_dir.join(&kernel_file);
    if !force {
        for p in [&kernel_dst, &mods_dst] {
            if p.exists() {
                return Err(KernelError::Invalid(format!("{} exists: {release} is already installed (use --force to replace it)", p.display())));
            }
        }
    }

    // Modules first: a kernel in /boot without its modules would be a worse state than the reverse.
    fs::create_dir_all(&mods_dir).map_err(io_ctx(format!("creating {}", mods_dir.display())))?;
    let tmp = mods_dir.join(format!(".{release}.new"));
    let _ = fs::remove_dir_all(&tmp);
    copy_tree(&mods_src, &tmp)?;
    if force {
        let _ = fs::remove_dir_all(&mods_dst);
    }
    fs::rename(&tmp, &mods_dst).map_err(io_ctx(format!("moving the modules into {}", mods_dst.display())))?;

    // modprobe looks in /lib/modules/<release>; on a system where /lib is separate, link it.
    let lib_modules: PathBuf = root.join("lib/modules");
    if fs::symlink_metadata(&lib_modules).is_err() && root.join("lib").is_dir() {
        let _ = symlink("../usr/lib/modules", &lib_modules);
    }

    fs::create_dir_all(boot_dir).map_err(io_ctx(format!("creating {}", boot_dir.display())))?;
    for name in [&config_file, &kernel_file] {
        let tmp = boot_dir.join(format!(".{name}.new"));
        fs::copy(staged.join("boot").join(name), &tmp).map_err(io_ctx(format!("copying {name}")))?;
        fs::rename(&tmp, boot_dir.join(name)).map_err(io_ctx(format!("installing {name}")))?;
    }
    Ok(Installed { release, kernel_file })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(t: &Path, release: &str) -> PathBuf {
        let s = t.join("staged");
        fs::create_dir_all(s.join("boot")).unwrap();
        fs::write(s.join("boot").join(format!("vmlinuz-{release}")), "KERNEL").unwrap();
        fs::write(s.join("boot").join(format!("config-{release}")), "CONFIG_X=y\n").unwrap();
        let m = s.join("usr/lib/modules").join(release).join("kernel/drivers");
        fs::create_dir_all(&m).unwrap();
        fs::write(m.join("a.ko"), "MOD").unwrap();
        fs::write(s.join("usr/lib/modules").join(release).join("modules.dep"), "").unwrap();
        s
    }

    #[test]
    fn installs_kernel_config_and_modules_and_links_lib_modules() {
        let t = tempfile::tempdir().unwrap();
        let s = staged(t.path(), "7.3.3");
        let (boot, root) = (t.path().join("boot"), t.path().join("root"));
        fs::create_dir_all(root.join("lib")).unwrap();
        let i = install_files(&s, &boot, &root, false).unwrap();
        assert_eq!(i, Installed { release: "7.3.3".into(), kernel_file: "vmlinuz-7.3.3".into() });
        assert_eq!(fs::read_to_string(boot.join("vmlinuz-7.3.3")).unwrap(), "KERNEL");
        assert!(boot.join("config-7.3.3").exists());
        assert!(root.join("usr/lib/modules/7.3.3/kernel/drivers/a.ko").exists());
        assert_eq!(fs::read_link(root.join("lib/modules")).unwrap(), Path::new("../usr/lib/modules"));
        assert!(fs::read_dir(&boot).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".new")), "no temp files left");
    }

    #[test]
    fn older_kernels_are_left_alone_and_an_installed_release_needs_force() {
        let t = tempfile::tempdir().unwrap();
        let (boot, root) = (t.path().join("boot"), t.path().join("root"));
        fs::create_dir_all(&boot).unwrap();
        fs::write(boot.join("vmlinuz-6.12.0"), "OLD").unwrap();
        fs::create_dir_all(root.join("usr/lib/modules/6.12.0")).unwrap();
        let s = staged(t.path(), "7.3.3");
        install_files(&s, &boot, &root, false).unwrap();
        assert_eq!(fs::read_to_string(boot.join("vmlinuz-6.12.0")).unwrap(), "OLD");
        assert!(root.join("usr/lib/modules/6.12.0").exists());

        let err = install_files(&s, &boot, &root, false).unwrap_err();
        assert!(err.to_string().contains("already installed"), "{err}");
        fs::write(s.join("boot/vmlinuz-7.3.3"), "NEWER").unwrap();
        install_files(&s, &boot, &root, true).unwrap();
        assert_eq!(fs::read_to_string(boot.join("vmlinuz-7.3.3")).unwrap(), "NEWER");
    }

    #[test]
    fn a_directory_that_is_not_one_staged_kernel_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let empty = t.path().join("e");
        fs::create_dir_all(empty.join("boot")).unwrap();
        assert!(staged_release(&empty).is_err());
        let s = staged(t.path(), "7.3.3");
        fs::write(s.join("boot/vmlinuz-7.4.0"), "").unwrap();
        assert!(staged_release(&s).is_err(), "two kernels is ambiguous");
        let bad = staged(&t.path().join("b"), "7.3.3");
        fs::write(bad.join("boot/vmlinuz-7.3.3"), "").unwrap();
        fs::remove_dir_all(bad.join("usr/lib/modules/7.3.3")).unwrap();
        assert!(install_files(&bad, &t.path().join("bb"), &t.path().join("br"), false).is_err(), "modules are required");
    }
}
