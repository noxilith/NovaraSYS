//! `forge apply`: converge the machine to the system declaration.
//!
//! Applying is two phases. [`plan_system`] reads the declaration and the
//! current state of the machine (under a root directory) and returns a
//! [`Plan`] without touching anything. [`apply`] then carries the plan out.
//! The CLI shows the plan as a preview and asks for confirmation in between.
//!
//! Everything is resolved under a `root` directory, so the whole thing can be
//! exercised against a scratch directory instead of `/`.

mod state;

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use std::collections::BTreeSet;

use forge_schema::SystemDecl;
use forge_store::{FetchHandle, Index, LocalRepo, Profile, RemoteRepo, Store, StoreError, StoreId, Trust, is_url};
pub use state::State;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("invalid {what} `{value}`")]
    InvalidName { what: &'static str, value: String },
    #[error("service `{0}` is declared but /etc/init.d/{0} does not exist")]
    ServiceMissing(String),
    #[error("reading {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("reading state file {path}: {message}")]
    BadState { path: PathBuf, message: String },
    #[error("packages are declared but no `repos` are configured in the declaration")]
    NoRepo,
    #[error("package store: {0}")]
    Store(String),
}

fn store_err(e: forge_store::StoreError) -> PlanError {
    PlanError::Store(e.to_string())
}

#[derive(Debug, thiserror::Error)]
#[error("applying `{summary}` ({path}): {source}")]
pub struct ApplyError {
    pub summary: String,
    pub path: PathBuf,
    pub source: io::Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Add,
    Modify,
    Remove,
}

impl Kind {
    fn mark(self) -> &'static str {
        match self {
            Kind::Add => "[+]",
            Kind::Modify => "[~]",
            Kind::Remove => "[-]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    WriteFile { path: PathBuf, content: String },
    RemoveFile(PathBuf),
    Symlink { link: PathBuf, target: PathBuf },
    RemoveLink(PathBuf),
    /// Verify an archive from a repository and install it into the store.
    /// `url` is set for a remote repository: the archive is downloaded first.
    Install { store_root: PathBuf, archive: PathBuf, url: Option<String>, fetch: FetchHandle, sha256: String, id: StoreId },
    /// Shown in the preview; the real work is the profile switch that follows.
    Note(PathBuf),
}

impl Op {
    fn path(&self) -> &Path {
        match self {
            Op::WriteFile { path, .. } | Op::RemoveFile(path) | Op::RemoveLink(path) | Op::Note(path) => path,
            Op::Symlink { link, .. } => link,
            Op::Install { archive, .. } => archive,
        }
    }
}

/// Make the system profile show exactly these packages (after they are installed).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileSwitch {
    store_root: PathBuf,
    profile_dir: PathBuf,
    ids: BTreeSet<StoreId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub section: &'static str,
    pub kind: Kind,
    pub summary: String,
    op: Op,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.kind.mark(), self.section, self.summary)
    }
}

/// Declared in the system declaration but not applied by this version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub section: &'static str,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub pending: Vec<Pending>,
    state_write: Option<(PathBuf, String)>,
    profile_switch: Option<ProfileSwitch>,
}

impl Plan {
    /// True when there is nothing to apply (pending items are not applied).
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.state_write.is_none() && self.profile_switch.is_none()
    }
}

/// How `forge apply` should behave.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Re-apply everything, even what already matches the declaration.
    pub rebuild: bool,
    /// How far to trust repository indexes. The default trusts nothing, so a
    /// repository is refused unless a keyring vouches for it (or the caller
    /// explicitly chooses `Trust::AllowUnsigned`).
    pub trust: Trust,
    /// How remote repositories are downloaded from (curl or BusyBox wget; tests substitute a fake).
    pub fetch: FetchHandle,
}

fn under(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.strip_prefix('/').unwrap_or(abs))
}

fn valid(s: &str, extra: &[char]) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

fn check(what: &'static str, value: &str, extra: &[char]) -> Result<(), PlanError> {
    if valid(value, extra) {
        Ok(())
    } else {
        Err(PlanError::InvalidName { what, value: value.into() })
    }
}

fn read_opt(path: &Path) -> Result<Option<String>, PlanError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(PlanError::Read { path: path.to_path_buf(), source }),
    }
}

struct Planner<'a> {
    root: &'a Path,
    opts: Options,
    changes: Vec<Change>,
}

impl Planner<'_> {
    fn file(&mut self, section: &'static str, abs: &str, content: String) -> Result<(), PlanError> {
        let path = under(self.root, abs);
        let (kind, note) = match read_opt(&path)? {
            None => (Kind::Add, ""),
            Some(cur) if cur != content => (Kind::Modify, ""),
            Some(_) if self.opts.rebuild => (Kind::Modify, " (re-apply)"),
            Some(_) => return Ok(()),
        };
        let first = content.lines().find(|l| !l.starts_with('#')).unwrap_or("").to_string();
        self.changes.push(Change {
            section,
            kind,
            summary: format!("{abs} = {first}{note}"),
            op: Op::WriteFile { path, content },
        });
        Ok(())
    }

    fn link(&mut self, section: &'static str, abs_link: &str, target: &str) {
        let link = under(self.root, abs_link);
        let want = PathBuf::from(target);
        let (kind, note) = match fs::read_link(&link) {
            Ok(cur) if cur == want => {
                if !self.opts.rebuild {
                    return;
                }
                (Kind::Modify, " (re-apply)")
            }
            Ok(_) => (Kind::Modify, ""),
            Err(_) if link.symlink_metadata().is_ok() => (Kind::Modify, ""),
            Err(_) => (Kind::Add, ""),
        };
        self.changes.push(Change {
            section,
            kind,
            summary: format!("{abs_link} -> {target}{note}"),
            op: Op::Symlink { link, target: want },
        });
    }

    fn unlink(&mut self, section: &'static str, abs_link: &str) {
        let link = under(self.root, abs_link);
        if link.symlink_metadata().is_ok() {
            self.changes.push(Change {
                section,
                kind: Kind::Remove,
                summary: abs_link.to_string(),
                op: Op::RemoveLink(link),
            });
        }
    }
}

const MODULES_FILE: &str = "/etc/modules-load.d/forge.conf";
const MANAGED_HEADER: &str = "# managed by forge; edit /etc/forge/system.toml instead\n";

/// Work out what has to change to match `decl`. Reads the machine, writes nothing.
pub fn plan_system(decl: &SystemDecl, root: &Path, opts: Options) -> Result<Plan, PlanError> {
    check("hostname", &decl.hostname, &['-', '.'])?;
    check("timezone", &decl.timezone, &['/', '_', '+', '-'])?;
    if decl.timezone.contains("..") || decl.timezone.starts_with('/') {
        return Err(PlanError::InvalidName { what: "timezone", value: decl.timezone.clone() });
    }
    check("locale", &decl.locale, &['.', '_', '@', '-'])?;
    check("keymap", &decl.keymap, &['-', '_'])?;
    for m in &decl.kernel_modules {
        check("kernel module", m, &['_', '-'])?;
    }
    for s in &decl.init.services {
        check("service", s, &['_', '-', '.'])?;
    }

    let openrc = decl.init.system == "openrc";
    let old_state = State::load(root)?;
    let mut p = Planner { root, opts, changes: Vec::new() };

    p.file("hostname", "/etc/hostname", format!("{}\n", decl.hostname))?;
    p.link(
        "timezone",
        "/etc/localtime",
        &format!("/usr/share/zoneinfo/{}", decl.timezone),
    );
    p.file("locale", "/etc/locale.conf", format!("LANG={}\n", decl.locale))?;
    p.file("environment", "/etc/profile.d/forge.sh", profile_env())?;

    let mut pending = Vec::new();

    if openrc {
        p.file("keymap", "/etc/conf.d/keymaps", format!("keymap=\"{}\"\n", decl.keymap))?;
    } else {
        pending.push(Pending {
            section: "keymap",
            summary: format!("keymap `{}`: only OpenRC is supported so far", decl.keymap),
        });
    }

    let modules_current = read_opt(&under(root, MODULES_FILE))?;
    if decl.kernel_modules.is_empty() {
        if modules_current.is_some() {
            p.changes.push(Change {
                section: "kernel modules",
                kind: Kind::Remove,
                summary: MODULES_FILE.to_string(),
                op: Op::RemoveFile(under(root, MODULES_FILE)),
            });
        }
    } else {
        let mut content = String::from(MANAGED_HEADER);
        for m in &decl.kernel_modules {
            content.push_str(m);
            content.push('\n');
        }
        p.file("kernel modules", MODULES_FILE, content)?;
    }

    let mut declared: Vec<String> = decl.init.services.clone();
    declared.sort();
    declared.dedup();
    let mut new_state = old_state.clone();

    if openrc {
        for s in &declared {
            if !under(root, &format!("/etc/init.d/{s}")).exists() {
                return Err(PlanError::ServiceMissing(s.clone()));
            }
            p.link(
                "services",
                &format!("/etc/runlevels/default/{s}"),
                &format!("/etc/init.d/{s}"),
            );
        }
        // Only services forge itself enabled earlier may be disabled again;
        // anything else on the machine is left alone.
        for s in old_state.managed_services.iter().filter(|s| !declared.contains(s)) {
            p.unlink("services", &format!("/etc/runlevels/default/{s}"));
        }
        new_state.managed_services = declared;
    } else {
        for s in &declared {
            pending.push(Pending {
                section: "services",
                summary: format!("service `{s}`: only OpenRC is supported so far"),
            });
        }
    }

    for u in &decl.users {
        pending.push(Pending {
            section: "users",
            summary: format!("user `{}`: user management is not implemented yet", u.name),
        });
    }
    let profile_switch = plan_packages(decl, root, &mut p, &old_state.repo_serials, &mut new_state.repo_serials)?;

    // Installs and modifications first, removals last, so a swap never leaves a gap.
    let mut changes = p.changes;
    changes.sort_by_key(|c| c.kind == Kind::Remove);

    let state_write = new_state.render_if_changed(root, &old_state);
    Ok(Plan { changes, pending, state_write, profile_switch })
}

const STORE_DIR: &str = "/forge/store";
const SYSTEM_PROFILE_DIR: &str = "/forge/profiles/system";

/// Put the system profile on `PATH` for login shells.
fn profile_env() -> String {
    let base = format!("{SYSTEM_PROFILE_DIR}/current");
    format!(
        "# managed by forge: puts the system profile on PATH\n\
         for d in {base}/usr/bin {base}/usr/sbin {base}/bin {base}/sbin; do\n\
         \x20   case \":$PATH:\" in *\":$d:\"*) ;; *) PATH=\"$d:$PATH\" ;; esac\n\
         done\n\
         export PATH\n"
    )
}

/// Resolve the declared packages against the repositories and plan the
/// installs plus the profile switch. Nothing is written here.
fn plan_packages(
    decl: &SystemDecl,
    root: &Path,
    p: &mut Planner,
    seen_serials: &std::collections::BTreeMap<String, u64>,
    new_serials: &mut std::collections::BTreeMap<String, u64>,
) -> Result<Option<ProfileSwitch>, PlanError> {
    let store = Store::new(under(root, STORE_DIR));
    let profile = Profile::new(under(root, SYSTEM_PROFILE_DIR));
    let current = profile.current_packages().map_err(store_err)?;

    // id -> (archive path, archive sha256) for packages that must be fetched.
    let mut target: BTreeSet<StoreId> = BTreeSet::new();
    // Where each missing archive comes from: a file, or a URL to download first.
    let mut sources: std::collections::BTreeMap<StoreId, (PathBuf, Option<String>, String)> = Default::default();

    if !decl.packages.is_empty() {
        if decl.repos.is_empty() {
            return Err(PlanError::NoRepo);
        }
        let mut repos: Vec<(Repo, Index)> = Vec::new();
        for r in &decl.repos {
            let (repo, label) = if is_url(r) {
                (Repo::Remote(RemoteRepo::new(r.as_str())), PathBuf::from(r))
            } else {
                (Repo::Local(LocalRepo::new(under(root, r))), under(root, r))
            };
            let loaded = repo.load(&p.opts.fetch, &p.opts.trust).map_err(store_err)?;
            // A signed index must not be older than one seen before: that would be an
            // attacker replaying a genuine but outdated repository (with the old,
            // vulnerable packages). Unsigned indexes carry no trustworthy serial.
            if loaded.signed_by.is_some() {
                let seen = seen_serials.get(r).copied().unwrap_or(0);
                if loaded.index.serial < seen {
                    return Err(store_err(StoreError::Rollback {
                        repo: label.clone(),
                        serial: loaded.index.serial,
                        seen,
                    }));
                }
                new_serials.insert(r.clone(), seen.max(loaded.index.serial));
            }
            repos.push((repo, loaded.index));
        }
        // Earlier repositories win when several offer the same build.
        let mut merged = Index::default();
        for (_, index) in &repos {
            for e in &index.packages {
                if merged.by_id(&e.id().map_err(store_err)?).is_none() {
                    merged.insert(e.clone());
                }
            }
        }
        for e in forge_store::resolve(&merged, &decl.packages, decl.libc).map_err(store_err)? {
            let id = e.id().map_err(store_err)?;
            if !store.has(&id) {
                let (repo, entry) = repos
                    .iter()
                    .find_map(|(r, i)| i.by_id(&id).map(|e| (r, e)))
                    .expect("resolved from these indexes");
                let (path, url) = repo.archive_ref(entry).map_err(store_err)?;
                sources.insert(id.clone(), (path, url, entry.archive_sha256.clone()));
            }
            target.insert(id);
        }
    }

    for id in &target {
        match sources.remove(id) {
            Some((archive, url, sha256)) => p.changes.push(Change {
                section: "packages",
                kind: if current.contains(id) { Kind::Modify } else { Kind::Add },
                summary: format!("{id} (install from {})", archive.display()),
                op: Op::Install { store_root: store.root().to_path_buf(), archive, url, fetch: p.opts.fetch.clone(), sha256, id: id.clone() },
            }),
            None if !current.contains(id) => p.changes.push(Change {
                section: "packages",
                kind: Kind::Add,
                summary: format!("{id} (already in the store; link into the profile)"),
                op: Op::Note(store.root().to_path_buf()),
            }),
            None => {}
        }
    }
    for id in current.difference(&target) {
        p.changes.push(Change {
            section: "packages",
            kind: Kind::Remove,
            summary: format!("{id} (unlink from the profile; `forge store gc` frees the space)"),
            op: Op::Note(store.root().to_path_buf()),
        });
    }

    Ok((target != current).then(|| ProfileSwitch {
        store_root: store.root().to_path_buf(),
        profile_dir: profile.dir().to_path_buf(),
        ids: target,
    }))
}

fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".forge-tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

fn ignore_missing(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn run(op: &Op) -> io::Result<()> {
    match op {
        Op::WriteFile { path, content } => write_atomic(path, content),
        Op::RemoveFile(p) | Op::RemoveLink(p) => ignore_missing(fs::remove_file(p)),
        Op::Symlink { link, target } => {
            if let Some(dir) = link.parent() {
                fs::create_dir_all(dir)?;
            }
            ignore_missing(fs::remove_file(link))?;
            symlink(target, link)
        }
        Op::Install { store_root, archive, url, fetch, sha256, id } => {
            // A remote archive is downloaded next to the store first; either way the store verifies
            // it against the hash from the (signed) index before anything is kept.
            let downloaded = match url {
                Some(u) => Some(
                    forge_store::download_to_temp(fetch.0.as_ref(), u, &store_root.join(".downloads"))
                        .map_err(|e| io::Error::other(e.to_string()))?,
                ),
                None => None,
            };
            let result = (|| {
                let file = fs::File::open(downloaded.as_deref().unwrap_or(archive))?;
                Store::new(store_root).install(file, Some(sha256)).map_err(|e| io::Error::other(e.to_string()))
            })();
            if let Some(tmp) = &downloaded {
                let _ = fs::remove_file(tmp);
            }
            let got = result?;
            if &got != id {
                return Err(io::Error::other(format!("the archive contained {got}, but the index promised {id}")));
            }
            Ok(())
        }
        Op::Note(_) => Ok(()),
    }
}

/// Carry out a plan, stopping at the first failure. Changes are applied in plan
/// order, and the bookkeeping state is written last, only if everything worked.
pub fn apply(plan: &Plan) -> Result<(), ApplyError> {
    for c in &plan.changes {
        run(&c.op).map_err(|source| ApplyError {
            summary: c.summary.clone(),
            path: c.op.path().to_path_buf(),
            source,
        })?;
    }
    if let Some(sw) = &plan.profile_switch {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Profile::new(&sw.profile_dir)
            .switch(&Store::new(&sw.store_root), &sw.ids, now)
            .map_err(|e| ApplyError {
                summary: "switch the system profile".into(),
                path: sw.profile_dir.clone(),
                source: io::Error::other(e.to_string()),
            })?;
    }
    if let Some((path, content)) = &plan.state_write {
        write_atomic(path, content).map_err(|source| ApplyError {
            summary: "forge state".into(),
            path: path.clone(),
            source,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// A repository named in the declaration: a directory, or an HTTP(S) base URL.
enum Repo {
    Local(LocalRepo),
    Remote(RemoteRepo),
}

impl Repo {
    fn load(&self, fetch: &FetchHandle, trust: &Trust) -> Result<forge_store::Loaded, StoreError> {
        match self {
            Repo::Local(r) => r.load(trust),
            Repo::Remote(r) => r.load(fetch.0.as_ref(), trust),
        }
    }

    /// Where an archive comes from: its path (for a remote one, the URL as a path, only for display) and the
    /// URL to download it from, if remote.
    fn archive_ref(&self, e: &forge_store::IndexEntry) -> Result<(PathBuf, Option<String>), StoreError> {
        match self {
            Repo::Local(r) => Ok((r.archive_path(e), None)),
            Repo::Remote(r) => {
                let url = r.archive_url(e)?;
                Ok((PathBuf::from(&url), Some(url)))
            }
        }
    }
}
