//! Configuring and building a kernel into a staging directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{CONFIG_SCRIPT, Cmd, KernelError, Runner, io_ctx, write};

/// How the configuration is made.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Defconfig base plus every driver as a module: the live ISO's kernel (works on hardware not yet seen,
    /// at the cost of ~300 MB of modules).
    Full,
    /// Start from a known-good configuration (normally the running kernel's) and drop every module that
    /// is not loaded right now. `keep` names modules to keep anyway (hardware not plugged in at the moment).
    Auto { base_config: PathBuf, loaded_modules: String, keep: Vec<String> },
    /// Start from `base_config`, then you choose in `make menuconfig`.
    Menu { base_config: PathBuf },
    /// Use this configuration as given (only checked).
    File(PathBuf),
}

pub struct BuildSpec {
    pub version: String,
    pub tarball: PathBuf,
    /// Scratch directory; the source is unpacked here (it must not already hold this version).
    pub work: PathBuf,
    /// Where the finished kernel is staged (`boot/` and `usr/lib/modules/`), ready for `install`.
    pub out: PathBuf,
    pub mode: Mode,
    pub jobs: usize,
    /// The C compiler command (gcc 15 needs `-std=gnu11` for the EFI stub of current kernels).
    pub cc: String,
}

/// A built kernel.
#[derive(Debug, Clone)]
pub struct Staged {
    pub release: String,
    pub dir: PathBuf,
}

fn find_in_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
}

fn header_present(name: &str) -> bool {
    ["/usr/include", "/usr/local/include"].iter().any(|d| Path::new(d).join(name).is_file())
}

/// What a kernel build needs from the machine. These are the tools the Novara repositories do not
/// carry yet (bc, perl, libelf), so on a fresh install a build may have to wait for them.
pub fn preflight() -> Result<(), KernelError> {
    let mut missing: Vec<String> = ["make", "gcc", "ld", "bc", "flex", "bison", "perl", "xz", "tar", "sh", "awk", "sed"]
        .iter()
        .filter(|t| !find_in_path(t))
        .map(|t| t.to_string())
        .collect();
    if !header_present("libelf.h") {
        missing.push("libelf (libelf.h)".into());
    }
    if missing.is_empty() { Ok(()) } else { Err(KernelError::MissingTools(missing)) }
}

fn kmake(spec: &BuildSpec, tree: &Path, args: &[&str]) -> Cmd {
    let mut c = Cmd::new("make", &["ARCH=x86_64"]).dir(tree);
    c = c.arg(format!("CC={}", spec.cc));
    for a in args {
        c = c.arg(*a);
    }
    c
}

/// The `lsmod`-style file `make localmodconfig` reads: a header, then one module per line.
pub fn lsmod_file(loaded: &str, keep: &[String]) -> String {
    let mut out = String::from("Module                  Size  Used by\n");
    for line in loaded.lines() {
        let mut f = line.split_whitespace();
        if let (Some(name), Some(size)) = (f.next(), f.next()) {
            out.push_str(&format!("{name} {size} 0\n"));
        }
    }
    for k in keep {
        out.push_str(&format!("{k} 0 0\n"));
    }
    out
}

pub fn build(spec: &BuildSpec, runner: &mut dyn Runner) -> Result<Staged, KernelError> {
    let unpack = spec.work.join(format!("src-{}", spec.version));
    if unpack.exists() {
        return Err(KernelError::Exists(unpack));
    }
    let tree = forge_build::extract(&spec.tarball, &unpack).map_err(|e| KernelError::Extract(e.to_string()))?;
    let script = spec.work.join("full-config.sh");
    write(&script, CONFIG_SCRIPT)?;
    let script_s = script.display().to_string();
    let scratch = spec.work.join("scratch");
    fs::create_dir_all(&scratch).map_err(io_ctx(format!("creating {}", scratch.display())))?;
    let scratch_s = scratch.display().to_string();
    let run_script = |mode: &str| -> Cmd {
        Cmd::new("sh", &[]).dir(&tree).arg(script_s.clone()).arg(mode)
            .env("KCFG_TMP", &scratch_s)
            .env("KCFG_CC", &spec.cc)
    };

    // ---- configure
    match &spec.mode {
        Mode::Full => runner.run(&run_script("full"))?,
        Mode::Auto { base_config, loaded_modules, keep } => {
            fs::copy(base_config, tree.join(".config")).map_err(io_ctx(format!("copying {}", base_config.display())))?;
            runner.run(&kmake(spec, &tree, &["olddefconfig"]))?;
            let lsmod = spec.work.join("lsmod.txt");
            write(&lsmod, &lsmod_file(loaded_modules, keep))?;
            runner.run(&kmake(spec, &tree, &[&format!("LSMOD={}", lsmod.display()), "localmodconfig"]))?;
            // localmodconfig only trims modules; make sure nothing the system needs to boot went with them.
            runner.run(&run_script("essential"))?;
        }
        Mode::Menu { base_config } => {
            fs::copy(base_config, tree.join(".config")).map_err(io_ctx(format!("copying {}", base_config.display())))?;
            runner.run(&kmake(spec, &tree, &["olddefconfig"]))?;
            runner.run(&kmake(spec, &tree, &["menuconfig"]))?;
            runner.run(&run_script("check"))?;
        }
        Mode::File(path) => {
            fs::copy(path, tree.join(".config")).map_err(io_ctx(format!("copying {}", path.display())))?;
            runner.run(&kmake(spec, &tree, &["olddefconfig"]))?;
            runner.run(&run_script("check"))?;
        }
    }

    // ---- build
    let jobs = format!("-j{}", spec.jobs.max(1));
    let kcflags = "KCFLAGS=-Wno-error";
    runner.run(&kmake(spec, &tree, &[&jobs, kcflags, "bzImage"]))?;
    runner.run(&kmake(spec, &tree, &[&jobs, kcflags, "modules"]))?;
    let release = runner.capture(&kmake(spec, &tree, &["-s", "kernelrelease"]))?.trim().to_string();
    if release.is_empty() || !release.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+')) {
        return Err(KernelError::Invalid(format!("the build reported an odd kernel release: {release:?}")));
    }

    // ---- stage
    let usr = spec.out.join("usr");
    let boot = spec.out.join("boot");
    fs::create_dir_all(&boot).map_err(io_ctx(format!("creating {}", boot.display())))?;
    runner.run(&kmake(spec, &tree, &[
        &format!("INSTALL_MOD_PATH={}", usr.display()),
        "INSTALL_MOD_STRIP=1",
        "modules_install",
    ]))?;
    // The build and source links point into the build tree, which is going away.
    for l in ["build", "source"] {
        let _ = fs::remove_file(usr.join("lib/modules").join(&release).join(l));
    }
    fs::copy(tree.join("arch/x86/boot/bzImage"), boot.join(format!("vmlinuz-{release}")))
        .map_err(io_ctx("copying the kernel image"))?;
    fs::copy(tree.join(".config"), boot.join(format!("config-{release}"))).map_err(io_ctx("copying the configuration"))?;
    Ok(Staged { release, dir: spec.out.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[derive(Default)]
    struct Fake {
        cmds: Vec<Cmd>,
        tree: Option<PathBuf>,
    }

    impl Runner for Fake {
        fn run(&mut self, cmd: &Cmd) -> Result<(), KernelError> {
            self.cmds.push(cmd.clone());
            let a = |s: &str| cmd.args.iter().any(|x| x == s);
            if let (true, Some(dir)) = (a("bzImage"), cmd.dir.as_ref()) {
                self.tree = Some(dir.clone());
                fs::create_dir_all(dir.join("arch/x86/boot")).unwrap();
                fs::write(dir.join("arch/x86/boot/bzImage"), "KERNEL").unwrap();
                fs::write(dir.join(".config"), "CONFIG_X=y\n").unwrap();
            }
            if a("modules_install") {
                let p = cmd.args.iter().find_map(|x| x.strip_prefix("INSTALL_MOD_PATH=")).unwrap();
                let d = Path::new(p).join("lib/modules/9.9.9");
                fs::create_dir_all(&d).unwrap();
                fs::write(d.join("modules.dep"), "").unwrap();
                symlink("/nowhere/build", d.join("build")).unwrap();
            }
            Ok(())
        }
        fn capture(&mut self, cmd: &Cmd) -> Result<String, KernelError> {
            self.cmds.push(cmd.clone());
            Ok("9.9.9\n".into())
        }
    }

    fn tarball(dir: &Path) -> PathBuf {
        // A tiny "kernel source": linux-9.9.9/Makefile, gzip-free tar is enough for forge_build::extract.
        let p = dir.join("linux-9.9.9.tar");
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "linux-9.9.9/Makefile", &b"all:\n"[..]).unwrap();
        fs::write(&p, b.into_inner().unwrap()).unwrap();
        p
    }

    fn spec(t: &Path, mode: Mode) -> BuildSpec {
        BuildSpec {
            version: "9.9.9".into(),
            tarball: tarball(t),
            work: t.join("work"),
            out: t.join("out"),
            mode,
            jobs: 4,
            cc: "gcc -std=gnu11".into(),
        }
    }

    fn describe(f: &Fake) -> Vec<String> {
        f.cmds.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn full_mode_runs_the_shared_script_then_builds_and_stages() {
        let t = tempfile::tempdir().unwrap();
        let mut f = Fake::default();
        let staged = build(&spec(t.path(), Mode::Full), &mut f).unwrap();
        assert_eq!(staged.release, "9.9.9");
        let cmds = describe(&f);
        assert!(cmds[0].ends_with("full-config.sh full"), "{cmds:?}");
        assert_eq!(f.cmds[0].env.iter().find(|(k, _)| k == "KCFG_CC").unwrap().1, "gcc -std=gnu11");
        assert!(cmds.iter().any(|c| c.contains("-j4") && c.ends_with("bzImage")));
        assert!(cmds.iter().any(|c| c.contains("-j4") && c.ends_with("modules")));
        let pos = |s: &str| cmds.iter().position(|c| c.contains(s)).unwrap();
        assert!(pos("bzImage") < pos("modules_install"));
        // Staged for `install`: kernel, config, modules, and no dangling build link.
        let out = &staged.dir;
        assert_eq!(fs::read_to_string(out.join("boot/vmlinuz-9.9.9")).unwrap(), "KERNEL");
        assert!(out.join("boot/config-9.9.9").exists());
        assert!(out.join("usr/lib/modules/9.9.9/modules.dep").exists());
        assert!(fs::symlink_metadata(out.join("usr/lib/modules/9.9.9/build")).is_err());
    }

    #[test]
    fn auto_mode_trims_with_the_loaded_modules_and_then_restores_the_essentials() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("base-config");
        fs::write(&base, "CONFIG_BASE=y\n").unwrap();
        let mode = Mode::Auto {
            base_config: base,
            loaded_modules: "e1000e 311296 0 - Live 0x0\nsnd 1 2 - Live 0x1\n".into(),
            keep: vec!["usb_storage".into()],
        };
        let mut f = Fake::default();
        build(&spec(t.path(), mode), &mut f).unwrap();
        let cmds = describe(&f);
        let pos = |s: &str| cmds.iter().position(|c| c.contains(s)).unwrap();
        assert!(pos("olddefconfig") < pos("localmodconfig"));
        assert!(pos("localmodconfig") < pos("full-config.sh essential"), "boot-critical drivers are re-asserted after trimming");
        assert!(pos("full-config.sh essential") < pos("bzImage"));
        let lsmod = fs::read_to_string(t.path().join("work/lsmod.txt")).unwrap();
        assert!(lsmod.starts_with("Module"));
        assert!(lsmod.contains("e1000e 311296 0\n") && lsmod.contains("snd 1 0\n"));
        assert!(lsmod.contains("usb_storage 0 0\n"), "--also-keep modules are kept: {lsmod}");
    }

    #[test]
    fn menu_and_file_modes_are_only_checked_never_overridden() {
        for mode_name in ["menu", "file"] {
            let t = tempfile::tempdir().unwrap();
            let cfg = t.path().join("cfg");
            fs::write(&cfg, "CONFIG_X=y\n").unwrap();
            let mode = if mode_name == "menu" { Mode::Menu { base_config: cfg } } else { Mode::File(cfg) };
            let mut f = Fake::default();
            build(&spec(t.path(), mode), &mut f).unwrap();
            let cmds = describe(&f);
            assert!(cmds.iter().any(|c| c.ends_with("full-config.sh check")), "{mode_name}: {cmds:?}");
            assert!(!cmds.iter().any(|c| c.ends_with("full-config.sh essential")));
            assert_eq!(cmds.iter().any(|c| c.ends_with("menuconfig")), mode_name == "menu");
        }
    }

    #[test]
    fn an_existing_unpacked_tree_is_refused_so_nothing_is_overwritten() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("work/src-9.9.9")).unwrap();
        let err = build(&spec(t.path(), Mode::Full), &mut Fake::default()).unwrap_err();
        assert!(matches!(err, KernelError::Exists(_)));
    }

    #[test]
    fn the_shared_script_is_embedded() {
        assert!(CONFIG_SCRIPT.starts_with("#!/bin/sh"));
        for mode in ["full", "essential", "check"] {
            assert!(CONFIG_SCRIPT.contains(&format!("{mode})")), "mode {mode}");
        }
    }

    #[test]
    fn lsmod_file_keeps_names_and_sizes_only() {
        let s = lsmod_file("a 10 1 b,c Live 0xdead\n\nb 5 0 - Live 0x1\n", &["z".into()]);
        assert_eq!(s, "Module                  Size  Used by\na 10 0\nb 5 0\nz 0 0\n");
    }
}
