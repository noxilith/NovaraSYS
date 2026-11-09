//! Generations: read-only btrfs snapshots of the system, each with its own
//! metadata and its own Limine boot entry.
//!
//! Layout (names relative to the top-level btrfs volume):
//!
//! - `@`: the live, writable root
//! - `@.next`: staging area for a pending rollback (exists only between a
//!   rollback and its completion)
//! - `@snapshots/gen-N`: read-only snapshot, with `gen-N.toml` (a [`Meta`]) beside it
//!
//! `<snapshots>/.current` holds the id of the current generation and
//! `<snapshots>/.rollback` tracks a rollback in progress (`staged` or `promoted`).

mod backend;
mod limine;
mod meta;
mod time;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use backend::{Backend, BtrfsCli, CopyBackend};
pub use meta::{BootInfo, Meta, base_cmdline, booted_subvol, with_root_subvol};
pub use time::format_utc;

/// Render `limine.conf` (used by the installer for the very first boot menu).
pub fn render_limine(live: &BootInfo, live_subvol: &str, gens: &[Meta]) -> String {
    limine::render(live, live_subvol, gens)
}

#[derive(Debug, thiserror::Error)]
pub enum GenError {
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("no generation {0}")]
    NoSuchGeneration(u32),
    #[error("generation {id} is {why} and cannot be removed")]
    Protected { id: u32, why: &'static str },
    #[error("this needs the top-level btrfs volume; mount it (subvolid=5) and pass --top DIR")]
    NeedTop,
    #[error("you booted generation {0}; boot the live system (or use `rollback`) first")]
    BootedFromGeneration(u32),
    #[error("a rollback to a generation is staged; reboot to finish it before creating generations")]
    RollbackStaged,
    #[error("you are running on @.next; reboot into the promoted @ first")]
    BootedFromNext,
    #[error("there is no completed rollback to clean up")]
    NothingToCleanUp,
    #[error("generation {0} has no metadata file")]
    MissingMeta(u32),
    #[error("generation {id} has unreadable metadata: {message}")]
    BadMeta { id: u32, message: String },
    #[error("invalid boot information: {0}")]
    InvalidBoot(String),
}

fn io_err<'a>(context: impl Into<String> + 'a) -> impl FnOnce(io::Error) -> GenError + 'a {
    move |source| GenError::Io { context: context.into(), source }
}

/// Everything an operation needs to know about the machine.
pub struct Ctx<'a> {
    pub backend: &'a dyn Backend,
    /// The running root subvolume (normally `/`); what `create` snapshots.
    pub live: PathBuf,
    /// The snapshots directory (normally `/.snapshots`, i.e. `@snapshots`).
    pub snaps: PathBuf,
    /// Top-level volume mount, needed to replace `@` (rollback, cleanup).
    pub top: Option<PathBuf>,
    /// Where `limine.conf` is written.
    pub limine_conf: PathBuf,
    /// Subvolume the system booted from, e.g. `@` or `@snapshots/gen-3`.
    pub booted_subvol: Option<String>,
    /// Kernel and command line of the running system.
    pub boot: BootInfo,
    /// Current time, seconds since the Unix epoch.
    pub now: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rollback {
    None,
    Staged,
    Promoted,
}

impl Ctx<'_> {
    fn gen_path(&self, id: u32) -> PathBuf {
        self.snaps.join(format!("gen-{id}"))
    }
    fn meta_path(&self, id: u32) -> PathBuf {
        self.snaps.join(format!("gen-{id}.toml"))
    }
    fn top(&self) -> Result<&Path, GenError> {
        self.top.as_deref().ok_or(GenError::NeedTop)
    }
    fn booted_is(&self, name: &str) -> bool {
        self.booted_subvol.as_deref() == Some(name)
    }
    fn booted_generation(&self) -> Option<u32> {
        self.booted_subvol
            .as_deref()?
            .strip_prefix("@snapshots/gen-")?
            .parse()
            .ok()
    }

    fn read_marker(&self, name: &str) -> Result<Option<String>, GenError> {
        let p = self.snaps.join(name);
        match fs::read_to_string(&p) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(format!("reading {}", p.display()))(e)),
        }
    }

    fn rollback_state(&self) -> Result<Rollback, GenError> {
        Ok(match self.read_marker(".rollback")?.as_deref() {
            Some("staged") => Rollback::Staged,
            Some("promoted") => Rollback::Promoted,
            _ => Rollback::None,
        })
    }

    fn set_rollback_state(&self, s: Rollback) -> Result<(), GenError> {
        let p = self.snaps.join(".rollback");
        match s {
            Rollback::None => match fs::remove_file(&p) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    Err(io_err(format!("removing {}", p.display()))(e))
                }
                _ => Ok(()),
            },
            Rollback::Staged => write_atomic(&p, "staged\n"),
            Rollback::Promoted => write_atomic(&p, "promoted\n"),
        }
    }
}

fn write_atomic(path: &Path, content: &str) -> Result<(), GenError> {
    let ctx = || format!("writing {}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err(ctx()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".forge-tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, content).map_err(io_err(ctx()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err(ctx())(e)
    })
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

// ---- reading generations -------------------------------------------------

fn ids(ctx: &Ctx) -> Result<Vec<u32>, GenError> {
    let rd = match fs::read_dir(&ctx.snaps) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(format!("reading {}", ctx.snaps.display()))(e)),
    };
    let mut ids: Vec<u32> = rd
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_prefix("gen-")?.parse().ok())
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

fn read_meta(ctx: &Ctx, id: u32) -> Result<Meta, GenError> {
    let p = ctx.meta_path(id);
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(GenError::MissingMeta(id)),
        Err(e) => return Err(io_err(format!("reading {}", p.display()))(e)),
    };
    toml::from_str(&text).map_err(|e| GenError::BadMeta { id, message: e.to_string() })
}

/// All generations, oldest first.
pub fn list(ctx: &Ctx) -> Result<Vec<Meta>, GenError> {
    ids(ctx)?.into_iter().map(|id| read_meta(ctx, id)).collect()
}

/// The id of the current generation, if any.
pub fn current(ctx: &Ctx) -> Result<Option<u32>, GenError> {
    Ok(ctx.read_marker(".current")?.and_then(|s| s.parse().ok()))
}

// ---- writing generations -------------------------------------------------

/// Snapshot the live root as a new read-only generation and refresh the boot menu.
pub fn create(ctx: &Ctx, description: &str) -> Result<Meta, GenError> {
    ctx.boot.validate()?;
    if let Some(n) = ctx.booted_generation() {
        return Err(GenError::BootedFromGeneration(n));
    }
    promote(ctx)?;
    if ctx.rollback_state()? == Rollback::Staged {
        return Err(GenError::RollbackStaged);
    }

    let id = ids(ctx)?.last().map_or(1, |n| n + 1);
    let meta = Meta {
        id,
        description: description.trim().to_string(),
        created: ctx.now,
        kernel: ctx.boot.kernel.clone(),
        initramfs: ctx.boot.initramfs.clone(),
        cmdline: ctx.boot.cmdline.clone(),
        store_paths: Vec::new(),
    };

    fs::create_dir_all(&ctx.snaps).map_err(io_err(format!("creating {}", ctx.snaps.display())))?;
    let dst = ctx.gen_path(id);
    ctx.backend
        .snapshot(&ctx.live, &dst, true)
        .map_err(io_err(format!("snapshotting {} to {}", ctx.live.display(), dst.display())))?;

    let text = toml::to_string(&meta).expect("meta serializes");
    if let Err(e) = write_atomic(&ctx.meta_path(id), &text) {
        let _ = ctx.backend.delete(&dst);
        return Err(e);
    }
    write_atomic(&ctx.snaps.join(".current"), &format!("{id}\n"))?;
    rewrite_limine(ctx)?;
    Ok(meta)
}

/// What `remove` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub removed: Vec<u32>,
    /// Generations that were left alone, with the reason.
    pub skipped: Vec<(u32, &'static str)>,
}

fn protected(ctx: &Ctx, id: u32) -> Result<Option<&'static str>, GenError> {
    if current(ctx)? == Some(id) {
        return Ok(Some("the current generation"));
    }
    if ctx.booted_generation() == Some(id) {
        return Ok(Some("the generation you booted"));
    }
    Ok(None)
}

fn remove_one(ctx: &Ctx, id: u32) -> Result<(), GenError> {
    let p = ctx.gen_path(id);
    ctx.backend.delete(&p).map_err(io_err(format!("deleting {}", p.display())))?;
    match fs::remove_file(ctx.meta_path(id)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(io_err(format!("removing metadata of generation {id}"))(e))
        }
        _ => Ok(()),
    }
}

/// Remove one generation. The current and the booted generation are refused.
pub fn remove(ctx: &Ctx, id: u32) -> Result<Removed, GenError> {
    if !exists(&ctx.gen_path(id)) {
        return Err(GenError::NoSuchGeneration(id));
    }
    if let Some(why) = protected(ctx, id)? {
        return Err(GenError::Protected { id, why });
    }
    remove_one(ctx, id)?;
    rewrite_limine(ctx)?;
    Ok(Removed { removed: vec![id], skipped: vec![] })
}

/// Remove every generation except the current and the booted one.
pub fn remove_all(ctx: &Ctx) -> Result<Removed, GenError> {
    let mut out = Removed::default();
    for id in ids(ctx)? {
        match protected(ctx, id)? {
            Some(why) => out.skipped.push((id, why)),
            None => {
                remove_one(ctx, id)?;
                out.removed.push(id);
            }
        }
    }
    rewrite_limine(ctx)?;
    Ok(out)
}

// ---- boot menu -----------------------------------------------------------

/// Regenerate `limine.conf`. Also completes a staged rollback if we are
/// running on `@.next`.
pub fn rewrite_limine(ctx: &Ctx) -> Result<(), GenError> {
    ctx.boot.validate()?;
    promote(ctx)?;
    let live_subvol = if ctx.rollback_state()? == Rollback::Staged { "@.next" } else { "@" };
    let gens = list(ctx)?;
    let conf = limine::render(&ctx.boot, live_subvol, &gens);
    write_atomic(&ctx.limine_conf, &conf)
}

// ---- rollback ------------------------------------------------------------

/// Where a rollback stands after `rollback`.
#[derive(Debug, PartialEq, Eq)]
pub enum RollbackOutcome {
    /// Staged in `@.next`; reboot, then the next `create`/`rewrite-limine` promotes it.
    Staged,
    /// You booted a generation, so `@` was replaced directly; reboot into it.
    Replaced,
}

fn old_name(ctx: &Ctx) -> String {
    format!("@.old-{}", ctx.now)
}

/// Replace `@` with a writable copy of `source`, keeping the old `@` as
/// `@.old-<time>`. Undoes the rename if the copy fails, so `@` never goes missing.
fn replace_live(ctx: &Ctx, top: &Path, source: &Path) -> Result<(), GenError> {
    let live = top.join("@");
    let old = top.join(old_name(ctx));
    let had_live = exists(&live);
    if had_live {
        fs::rename(&live, &old).map_err(io_err(format!("keeping {} as {}", live.display(), old.display())))?;
    }
    if let Err(e) = ctx.backend.snapshot(source, &live, false) {
        if had_live {
            let _ = fs::rename(&old, &live);
        }
        return Err(io_err(format!("copying {} to {}", source.display(), live.display()))(e));
    }
    Ok(())
}

/// Roll back to generation `id`.
pub fn rollback(ctx: &Ctx, id: u32) -> Result<RollbackOutcome, GenError> {
    let top = ctx.top()?.to_path_buf();
    let src = ctx.gen_path(id);
    if !exists(&src) {
        return Err(GenError::NoSuchGeneration(id));
    }
    read_meta(ctx, id)?;

    if ctx.booted_generation().is_some() {
        // We are not running on `@`, so it can be replaced in one step.
        replace_live(ctx, &top, &src)?;
        ctx.set_rollback_state(Rollback::None)?;
        write_atomic(&ctx.snaps.join(".current"), &format!("{id}\n"))?;
        rewrite_limine(ctx)?;
        return Ok(RollbackOutcome::Replaced);
    }
    if ctx.booted_is("@.next") {
        return Err(GenError::BootedFromNext);
    }

    let next = top.join("@.next");
    if exists(&next) {
        ctx.backend.delete(&next).map_err(io_err(format!("removing stale {}", next.display())))?;
    }
    ctx.backend
        .snapshot(&src, &next, false)
        .map_err(io_err(format!("copying {} to {}", src.display(), next.display())))?;
    ctx.set_rollback_state(Rollback::Staged)?;
    write_atomic(&ctx.snaps.join(".current"), &format!("{id}\n"))?;
    rewrite_limine(ctx)?;
    Ok(RollbackOutcome::Staged)
}

/// What `promote` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Promotion {
    NothingStaged,
    /// A rollback is staged but we have not rebooted into `@.next` yet.
    WaitingForReboot,
    Promoted,
}

/// If a rollback is staged and we are running on `@.next`, make `@` a copy of it.
pub fn promote(ctx: &Ctx) -> Result<Promotion, GenError> {
    if ctx.rollback_state()? != Rollback::Staged {
        return Ok(Promotion::NothingStaged);
    }
    if !ctx.booted_is("@.next") {
        return Ok(Promotion::WaitingForReboot);
    }
    let top = ctx.top()?.to_path_buf();
    replace_live(ctx, &top, &top.join("@.next"))?;
    ctx.set_rollback_state(Rollback::Promoted)?;
    Ok(Promotion::Promoted)
}

/// After rebooting into the promoted `@`, drop the leftover `@.next`.
pub fn cleanup_next(ctx: &Ctx) -> Result<(), GenError> {
    if ctx.rollback_state()? != Rollback::Promoted {
        return Err(GenError::NothingToCleanUp);
    }
    if ctx.booted_is("@.next") {
        return Err(GenError::BootedFromNext);
    }
    let next = ctx.top()?.join("@.next");
    if exists(&next) {
        ctx.backend.delete(&next).map_err(io_err(format!("deleting {}", next.display())))?;
    }
    ctx.set_rollback_state(Rollback::None)?;
    rewrite_limine(ctx)
}

#[cfg(test)]
mod tests;
