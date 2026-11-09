//! `forge kernel`: find a kernel on kernel.org, download and verify it, configure it (every driver as
//! a module, trimmed to this machine, or by hand), build it into a staging directory, and install it.
//! Commands run through a [`Runner`] so the sequences are unit-tested.

mod build;
mod install;
mod release;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use build::{BuildSpec, Mode, Staged, build, preflight};
pub use install::{Installed, install_files, staged_release};
pub use release::{RELEASES_URL, Release, Selector, parse_releases, parse_sums, pick, sums_url, tarball_name, tarball_url, version_key};

/// The configuration script, shared with `recipes/linux` (the live ISO's kernel).
pub const CONFIG_SCRIPT: &str = include_str!("../../../recipes/linux/full-config.sh");

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("could not read kernel.org's release list: {0}")]
    Releases(String),
    #[error("no {0} release is listed on kernel.org")]
    NoRelease(&'static str),
    #[error("`{0}` is not a kernel version (use a plain one such as 7.3.3, not a release candidate)")]
    BadVersion(String),
    #[error("kernel.org's checksum list has no entry for {0}")]
    NoChecksum(String),
    #[error("`{cmd}` failed ({status})")]
    Command { cmd: String, status: String },
    #[error("could not run `{cmd}`: {source}")]
    Spawn { cmd: String, source: io::Error },
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("{0} exists already; remove it or choose another --work-dir")]
    Exists(PathBuf),
    #[error("missing build tools: {}", .0.join(", "))]
    MissingTools(Vec<String>),
    #[error("{0}")]
    Invalid(String),
    #[error("unpacking the kernel source: {0}")]
    Extract(String),
    #[error("{0}")]
    Fetch(String),
}

pub(crate) fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> KernelError {
    let context = context.into();
    move |source| KernelError::Io { context, source }
}

/// One external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub dir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Cmd { program: program.into(), args: args.iter().map(|s| s.to_string()).collect(), dir: None, env: vec![] }
    }
    pub fn dir(mut self, d: &Path) -> Self {
        self.dir = Some(d.to_path_buf());
        self
    }
    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
}

impl std::fmt::Display for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.program)?;
        for a in &self.args {
            write!(f, " {a}")?;
        }
        Ok(())
    }
}

/// Runs commands. `run` lets the command talk to the terminal (build output, `menuconfig`);
/// `capture` returns its standard output.
pub trait Runner {
    fn run(&mut self, cmd: &Cmd) -> Result<(), KernelError>;
    fn capture(&mut self, cmd: &Cmd) -> Result<String, KernelError>;
}

pub struct SystemRunner;

fn command(cmd: &Cmd) -> Command {
    let mut c = Command::new(&cmd.program);
    c.args(&cmd.args);
    if let Some(d) = &cmd.dir {
        c.current_dir(d);
    }
    for (k, v) in &cmd.env {
        c.env(k, v);
    }
    c
}

impl Runner for SystemRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<(), KernelError> {
        let status = command(cmd)
            .stdin(Stdio::inherit())
            .status()
            .map_err(|source| KernelError::Spawn { cmd: cmd.to_string(), source })?;
        if status.success() {
            Ok(())
        } else {
            Err(KernelError::Command { cmd: cmd.to_string(), status: status.to_string() })
        }
    }

    fn capture(&mut self, cmd: &Cmd) -> Result<String, KernelError> {
        let out = command(cmd)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|source| KernelError::Spawn { cmd: cmd.to_string(), source })?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(KernelError::Command { cmd: cmd.to_string(), status: out.status.to_string() })
        }
    }
}

/// Fetching small text files (the release list and checksums); tests substitute canned answers.
pub trait Http {
    fn get(&self, url: &str) -> Result<String, KernelError>;
}

pub struct Curl;

impl Http for Curl {
    fn get(&self, url: &str) -> Result<String, KernelError> {
        let out = Command::new("curl")
            .args(["-fsSL", "--max-time", "60", url])
            .stdin(Stdio::null())
            .output()
            .map_err(|source| KernelError::Spawn { cmd: format!("curl {url}"), source })?;
        if !out.status.success() {
            return Err(KernelError::Fetch(format!("downloading {url} failed ({})", out.status)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Resolve `sel`, then download its tarball into `cache`, checked against kernel.org's checksum list.
/// Returns the version and the verified tarball.
pub fn fetch(http: &dyn Http, cache: &forge_build::SourceCache, sel: &Selector) -> Result<(String, PathBuf), KernelError> {
    let version = match sel {
        Selector::Version(_) => pick(&[], sel)?,
        _ => pick(&parse_releases(&http.get(RELEASES_URL)?)?, sel)?,
    };
    let name = tarball_name(&version);
    let sums = http.get(&sums_url(&version))?;
    let sha256 = parse_sums(&sums, &name).ok_or_else(|| KernelError::NoChecksum(name.clone()))?;
    let source = forge_schema::Source { url: tarball_url(&version), sha256 };
    let path = cache.fetch(&source).map_err(|e| KernelError::Fetch(e.to_string()))?;
    Ok((version, path))
}

pub(crate) fn write(path: &Path, text: &str) -> Result<(), KernelError> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).map_err(io_ctx(format!("creating {}", d.display())))?;
    }
    let mut f = fs::File::create(path).map_err(io_ctx(format!("creating {}", path.display())))?;
    f.write_all(text.as_bytes()).map_err(io_ctx(format!("writing {}", path.display())))
}
