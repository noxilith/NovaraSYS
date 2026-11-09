//! Building a kernel with the system's own packages, in the same sandbox every package build uses:
//! no compiler, `bc`, `perl` or `libelf` has to be installed first. The tools are fetched from the
//! repositories (verified like any package), a throwaway recipe describes the build, and the result is
//! the usual staged kernel (`boot/`, `usr/lib/modules/`).

use std::fs;
use std::path::{Path, PathBuf};

use forge_build::{BuildOptions, Tools};
use forge_schema::Libc;
use forge_store::{FetchHandle, Store, Trust};

use crate::build::{Mode, Staged, lsmod_file};
use crate::install::staged_release;
use crate::{CONFIG_SCRIPT, BUILD_SCRIPT, KernelError, io_ctx, write};

/// What the kernel build needs from the store (musl systems; a glibc toolchain is not packaged yet).
pub const BUILD_PACKAGES: &[&str] = &[
    "busybox", "make", "binutils", "gcc", "musl", "linux-headers", "bc", "perl", "libelf", "flex", "bison", "m4", "zlib",
    "pkgconf", "bash", "openssl", "kmod",
];

pub struct StoreBuildSpec {
    pub version: String,
    /// The verified source tarball, already in `source_cache` under its checksum.
    pub tarball_sha256: String,
    pub source_cache: PathBuf,
    pub work: PathBuf,
    pub mode: Mode,
    pub jobs: usize,
    pub libc: Libc,
    /// Directories or `https://` base URLs the build tools come from.
    pub repos: Vec<String>,
    pub trust: Trust,
    pub fetch: FetchHandle,
    /// The package store the tools are installed into.
    pub store: PathBuf,
    /// The `forge` executable (re-run as the sandbox helper).
    pub helper: PathBuf,
    pub keep_work: bool,
}

fn configure_step(mode: &Mode) -> Result<String, KernelError> {
    let cc = r#"make ARCH=x86_64 CC="gcc -std=gnu11""#;
    Ok(match mode {
        Mode::Full => "sh \"$RECIPE_DIR/full-config.sh\" full".to_string(),
        Mode::Auto { .. } => format!(
            "cp \"$RECIPE_DIR/base-config\" .config\n{cc} olddefconfig\n{cc} LSMOD=\"$RECIPE_DIR/lsmod.txt\" localmodconfig\nsh \"$RECIPE_DIR/full-config.sh\" essential\n"
        ),
        Mode::File(_) => format!("cp \"$RECIPE_DIR/base-config\" .config\n{cc} olddefconfig\nsh \"$RECIPE_DIR/full-config.sh\" check\n"),
        Mode::Menu { .. } => {
            return Err(KernelError::Invalid(
                "--menuconfig needs an interactive terminal, which the sandbox does not give it yet: use --host-tools (and the build tools installed on this machine)"
                    .into(),
            ))
        }
    })
}

/// The throwaway recipe directory for this build.
pub fn write_recipe(dir: &Path, spec: &StoreBuildSpec) -> Result<(), KernelError> {
    fs::create_dir_all(dir).map_err(io_ctx(format!("creating {}", dir.display())))?;
    let libc = spec.libc.as_str();
    let deps: Vec<String> = BUILD_PACKAGES.iter().map(|p| format!("\"{p}\"")).collect();
    let recipe = format!(
        "name = \"linux-local\"\nversion = \"{v}\"\ndescription = \"A kernel built on this machine\"\nlibc = [\"{libc}\"]\n\n\
         [depends]\nbuild = [{deps}]\n\n\
         [[source]]\nurl = \"{url}\"\nsha256 = \"{sha}\"\n\n\
         [build]\nconfigure = '''\n{configure}'''\ncompile = \"sh \\\"$RECIPE_DIR/kernel-build.sh\\\" compile\"\ninstall = \"sh \\\"$RECIPE_DIR/kernel-build.sh\\\" install\"\n",
        v = spec.version,
        deps = deps.join(", "),
        url = crate::tarball_url(&spec.version),
        sha = spec.tarball_sha256,
        configure = configure_step(&spec.mode)?,
    );
    write(&dir.join("recipe.toml"), &recipe)?;
    write(&dir.join("full-config.sh"), CONFIG_SCRIPT)?;
    write(&dir.join("kernel-build.sh"), BUILD_SCRIPT)?;
    match &spec.mode {
        Mode::Auto { base_config, loaded_modules, keep } => {
            fs::copy(base_config, dir.join("base-config")).map_err(io_ctx(format!("copying {}", base_config.display())))?;
            write(&dir.join("lsmod.txt"), &lsmod_file(loaded_modules, keep))?;
        }
        Mode::File(f) | Mode::Menu { base_config: f } => {
            fs::copy(f, dir.join("base-config")).map_err(io_ctx(format!("copying {}", f.display())))?;
        }
        Mode::Full => {}
    }
    Ok(())
}

/// Build, then unpack the resulting package into a private store and return it as a staged kernel.
pub fn build_in_store(spec: &StoreBuildSpec) -> Result<Staged, KernelError> {
    if spec.libc != Libc::Musl {
        return Err(KernelError::Invalid("building in the store needs the musl toolchain; glibc has none packaged yet".into()));
    }
    let recipe_dir = spec.work.join(format!("recipe-{}", spec.version));
    if recipe_dir.exists() {
        return Err(KernelError::Exists(recipe_dir));
    }
    write_recipe(&recipe_dir, spec)?;

    let out_dir = spec.work.join(format!("package-{}", spec.version));
    let opts = BuildOptions {
        recipe_dir,
        libc: spec.libc,
        out_dir: out_dir.clone(),
        work_dir: spec.work.join("build"),
        source_cache: spec.source_cache.clone(),
        repos: spec.repos.iter().map(PathBuf::from).collect(),
        trust: spec.trust.clone(),
        fetch: spec.fetch.clone(),
        tools: Tools::Store,
        store: spec.store.clone(),
        jobs: spec.jobs.max(1) as u32,
        helper: spec.helper.clone(),
        keep_work: spec.keep_work,
    };
    let built = forge_build::build(&opts).map_err(|e| KernelError::Fetch(format!("the kernel build failed: {e}")))?;

    // The package is a verified tree of boot/ and usr/lib/modules/: unpack it into a store of its own.
    let kstore = Store::new(spec.work.join("kernel-store"));
    let file = fs::File::open(&built.archive).map_err(io_ctx(format!("opening {}", built.archive.display())))?;
    let id = kstore.install(file, Some(&built.archive_sha256)).map_err(|e| KernelError::Invalid(e.to_string()))?;
    let dir = kstore.path(&id);
    let release = staged_release(&dir)?;
    Ok(Staged { release, dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(t: &Path, mode: Mode) -> StoreBuildSpec {
        StoreBuildSpec {
            version: "7.3.3".into(),
            tarball_sha256: "ab".repeat(32),
            source_cache: t.join("cache"),
            work: t.join("work"),
            mode,
            jobs: 4,
            libc: Libc::Musl,
            repos: vec!["https://example.test/repo".into()],
            trust: Trust::AllowUnsigned,
            fetch: FetchHandle::default(),
            store: t.join("store"),
            helper: "/usr/bin/forge".into(),
            keep_work: false,
        }
    }

    #[test]
    fn the_throwaway_recipe_is_valid_names_the_tools_and_the_verified_tarball() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        write_recipe(&dir, &spec(t.path(), Mode::Full)).unwrap();
        let text = fs::read_to_string(dir.join("recipe.toml")).unwrap();
        let recipe = forge_schema::Recipe::parse(&text).expect("the generated recipe parses");
        assert_eq!(recipe.version, "7.3.3");
        for pkg in ["gcc", "bc", "perl", "libelf", "kmod", "openssl", "bash"] {
            assert!(recipe.depends.build.iter().any(|d| d == pkg), "{pkg}");
        }
        assert_eq!(recipe.sources[0].url, "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.3.tar.xz");
        assert_eq!(recipe.sources[0].sha256, "ab".repeat(32));
        assert!(text.contains("full-config.sh\\\" full") || text.contains("full-config.sh\" full"), "{text}");
        assert!(dir.join("kernel-build.sh").exists() && dir.join("full-config.sh").exists());
    }

    #[test]
    fn auto_mode_ships_the_base_config_and_the_module_list_next_to_the_recipe() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("cfg");
        fs::write(&base, "CONFIG_X=y\n").unwrap();
        let mode = Mode::Auto { base_config: base, loaded_modules: "e1000e 1 0 - Live 0x0\n".into(), keep: vec!["usb_storage".into()] };
        let dir = t.path().join("r");
        write_recipe(&dir, &spec(t.path(), mode)).unwrap();
        let text = fs::read_to_string(dir.join("recipe.toml")).unwrap();
        assert!(text.contains("localmodconfig") && text.contains("full-config.sh\" essential"), "{text}");
        assert!(forge_schema::Recipe::parse(&text).is_ok());
        assert_eq!(fs::read_to_string(dir.join("base-config")).unwrap(), "CONFIG_X=y\n");
        let lsmod = fs::read_to_string(dir.join("lsmod.txt")).unwrap();
        assert!(lsmod.contains("e1000e 1 0\n") && lsmod.contains("usb_storage 0 0\n"), "{lsmod}");
    }

    #[test]
    fn menuconfig_is_refused_here_with_a_pointer_to_the_host_mode() {
        let t = tempfile::tempdir().unwrap();
        let e = write_recipe(&t.path().join("r"), &spec(t.path(), Mode::Menu { base_config: t.path().join("x") })).unwrap_err();
        assert!(e.to_string().contains("--host-tools"), "{e}");
    }

    #[test]
    fn only_musl_is_supported_and_an_existing_recipe_directory_is_not_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let mut s = spec(t.path(), Mode::Full);
        s.libc = Libc::Glibc;
        assert!(build_in_store(&s).is_err());
        s.libc = Libc::Musl;
        fs::create_dir_all(t.path().join("work/recipe-7.3.3")).unwrap();
        assert!(matches!(build_in_store(&s), Err(KernelError::Exists(_))));
    }
}
