//! The build driver: recipe in, package archive out.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use forge_schema::{Libc, Recipe};
use forge_store::{Index, LocalRepo, PackOptions, Profile, Store, StoreId, Trust, pack_dir, remove_tree, resolve};

use crate::fetch::{SourceCache, file_name_of};
use crate::sandbox::{self, Bind, SETUP_FAILED, SandboxConfig};
use crate::{BuildError, extract, hash, io_ctx};

/// Where the build's tools (compiler, make, shell) come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tools {
    /// The host's `/usr`, `/bin`, `/lib`... mounted read-only. **Not
    /// reproducible**; only for bootstrapping the first packages, before any
    /// toolchain exists in the store.
    Host,
    /// The recipe's `depends.build` packages (and their dependencies) from the
    /// store, installed from the repositories if missing. Reproducible.
    Store,
}

impl Tools {
    fn name(self) -> &'static str {
        match self {
            Tools::Host => "host",
            Tools::Store => "store",
        }
    }
}

pub struct BuildOptions {
    /// Directory containing `recipe.toml` (and any patches).
    pub recipe_dir: PathBuf,
    pub libc: Libc,
    /// Where the finished archive is written.
    pub out_dir: PathBuf,
    /// Parent of the temporary per-build directory.
    pub work_dir: PathBuf,
    pub source_cache: PathBuf,
    /// Repositories used to resolve dependencies.
    pub repos: Vec<PathBuf>,
    /// How far to trust those repositories' indexes. A build downloads its own
    /// tools from them, so an unverified index could hand it a poisoned compiler.
    pub trust: Trust,
    pub tools: Tools,
    /// The package store used for `Tools::Store`.
    pub store: PathBuf,
    pub jobs: u32,
    /// The `forge` executable, re-run as the sandbox helper.
    pub helper: PathBuf,
    /// Keep the temporary build directory (for debugging a failed build).
    pub keep_work: bool,
}

pub struct Built {
    pub id: StoreId,
    pub archive: PathBuf,
    pub archive_sha256: String,
    /// The build-input hash that names the package.
    pub hash: String,
    pub warnings: Vec<String>,
    /// Set if `keep_work` was requested.
    pub work_dir: Option<PathBuf>,
}

fn load_indexes(repos: &[PathBuf], trust: &Trust) -> Result<Vec<(LocalRepo, Index)>, BuildError> {
    repos
        .iter()
        .map(|dir| {
            let repo = LocalRepo::new(dir);
            let index = repo.load(trust).map_err(|e| BuildError::Dependencies(e.to_string()))?.index;
            Ok((repo, index))
        })
        .collect()
}

fn merged(indexes: &[(LocalRepo, Index)]) -> Index {
    let mut all = Index::default();
    for (_, index) in indexes {
        for e in &index.packages {
            if e.id().ok().is_some_and(|id| all.by_id(&id).is_none()) {
                all.insert(e.clone());
            }
        }
    }
    all
}

/// The direct run-time dependencies of the recipe, as store ids.
fn run_dep_ids(recipe: &Recipe, libc: Libc, indexes: &[(LocalRepo, Index)]) -> Result<Vec<String>, BuildError> {
    let index = merged(indexes);
    recipe
        .depends
        .run
        .iter()
        .map(|name| {
            index
                .latest(name, libc)
                .and_then(|e| e.id().ok())
                .map(|id| id.to_string())
                .ok_or_else(|| {
                    BuildError::Dependencies(format!(
                        "run dependency `{name}` was not found for {}; pass a repository that has it with --repo",
                        libc.as_str()
                    ))
                })
        })
        .collect()
}

/// Resolve `depends.build` to store entries, installing missing ones.
fn install_build_deps(
    recipe: &Recipe,
    libc: Libc,
    indexes: &[(LocalRepo, Index)],
    store: &Store,
) -> Result<BTreeSet<StoreId>, BuildError> {
    if recipe.depends.build.is_empty() {
        return Ok(BTreeSet::new());
    }
    let dep_err = |e: forge_store::StoreError| BuildError::Dependencies(e.to_string());
    let index = merged(indexes);
    let mut ids = BTreeSet::new();
    for entry in resolve(&index, &recipe.depends.build, libc).map_err(dep_err)? {
        let id = entry.id().map_err(dep_err)?;
        if !store.has(&id) {
            let (repo, e) = indexes
                .iter()
                .find_map(|(r, i)| i.by_id(&id).map(|e| (r, e)))
                .expect("resolved from these indexes");
            let file = repo.open_archive(e).map_err(dep_err)?;
            store.install(file, Some(&e.archive_sha256)).map_err(dep_err)?;
        }
        ids.insert(id);
    }
    Ok(ids)
}

type Mounts = (Vec<Bind>, Vec<(String, String)>);

/// The parts of the host that tools live in. `/usr` is not bound whole: it can
/// hold odd mounts (state directories of services) that have no business in a
/// build, so only the standard subdirectories are exposed.
const HOST_TOOL_DIRS: &[&str] = &[
    "/bin", "/sbin", "/lib", "/lib64", "/usr/bin", "/usr/sbin", "/usr/lib", "/usr/lib32", "/usr/lib64",
    "/usr/libexec", "/usr/include", "/usr/share", "/usr/local",
];

fn host_tool_mounts() -> Mounts {
    let (mut binds, mut links) = (Vec::new(), Vec::new());
    for dir in HOST_TOOL_DIRS {
        let p = Path::new(dir);
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => {
                if let Ok(t) = fs::read_link(p) {
                    links.push((dir.to_string(), t.to_string_lossy().into_owned()));
                }
            }
            Ok(m) if m.is_dir() => binds.push(Bind { src: p.to_path_buf(), dst: p.to_path_buf(), writable: false }),
            _ => {}
        }
    }
    // The host's dynamic loader finds libraries through its cache; without it
    // only the default directories are searched, and host tools whose libraries
    // live elsewhere fail to start. The cache holds absolute paths that exist
    // inside the sandbox (the directories above are bound at the same paths).
    let cache = Path::new("/etc/ld.so.cache");
    if cache.is_file() {
        binds.push(Bind { src: cache.to_path_buf(), dst: cache.to_path_buf(), writable: false });
    }
    (binds, links)
}

fn store_tool_mounts(store: &Store, ids: &BTreeSet<StoreId>, tools_dir: &Path) -> Result<Mounts, BuildError> {
    let dep_err = |e: forge_store::StoreError| BuildError::Dependencies(e.to_string());
    let profile = Profile::new(tools_dir);
    profile.switch(store, ids, 0).map_err(dep_err)?;
    let generation = fs::canonicalize(profile.current_link()).map_err(io_ctx("locating the tools profile"))?;

    let (mut binds, mut links) = (Vec::new(), Vec::new());
    for entry in fs::read_dir(&generation).map_err(io_ctx(format!("reading {}", generation.display())))? {
        let entry = entry.map_err(io_ctx("reading the tools profile"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let ty = entry.file_type().map_err(io_ctx("reading the tools profile"))?;
        if ty.is_symlink() {
            let t = fs::read_link(entry.path()).map_err(io_ctx("reading a tools link"))?;
            links.push((format!("/{name}"), t.to_string_lossy().into_owned()));
        } else if ty.is_dir() {
            binds.push(Bind { src: entry.path(), dst: Path::new("/").join(&name), writable: false });
        }
    }
    // The profile's symlinks point at absolute store paths, so the store has
    // to be visible at the same path.
    binds.push(Bind { src: store.root().to_path_buf(), dst: store.root().to_path_buf(), writable: false });
    Ok((binds, links))
}

fn abs(p: &Path) -> Result<PathBuf, BuildError> {
    fs::canonicalize(p).map_err(io_ctx(format!("resolving {}", p.display())))
}

/// Make the fetched sources available (read-only) under one directory by name.
fn stage_sources(files: &[PathBuf], dir: &Path) -> Result<(), BuildError> {
    for f in files {
        let name = f.file_name().expect("cache file has a name").to_string_lossy().into_owned();
        // Cache names are `<sha256>-<name>`; the recipe sees just `<name>`.
        let short = name.split_once('-').map_or(name.as_str(), |(_, rest)| rest).to_string();
        let target = dir.join(&short);
        if target.exists() {
            return Err(BuildError::Recipe(format!("two sources are both named `{short}`")));
        }
        if fs::hard_link(f, &target).is_err() {
            fs::copy(f, &target).map_err(io_ctx(format!("staging {}", f.display())))?;
        }
    }
    Ok(())
}

fn script() -> String {
    // Each phase runs in its own subshell from $SRC, so a `cd` or variable in
    // one phase never leaks into the next.
    "set -eu\numask 022\n\
     for phase in prepare configure compile install; do\n\
     \x20 if [ -f \"/build/phases/$phase.sh\" ]; then\n\
     \x20   echo \"==> forge: $phase\"\n\
     \x20   (cd \"$SRC\" && sh -eu \"/build/phases/$phase.sh\")\n\
     \x20 fi\n\
     done\n"
        .to_string()
}

/// Build the recipe in `opts.recipe_dir` and write the package archive to
/// `opts.out_dir`.
pub fn build(opts: &BuildOptions) -> Result<Built, BuildError> {
    let recipe_path = opts.recipe_dir.join("recipe.toml");
    let text = fs::read_to_string(&recipe_path).map_err(io_ctx(format!("reading {}", recipe_path.display())))?;
    let recipe = Recipe::parse(&text).map_err(|e| BuildError::Recipe(format!("{}: {e}", recipe_path.display())))?;
    if !recipe.supports(opts.libc) {
        return Err(BuildError::UnsupportedLibc { name: recipe.name.clone(), libc: opts.libc.as_str() });
    }
    let effective = recipe.effective(opts.libc).map_err(|e| BuildError::Recipe(e.to_string()))?;

    let mut warnings = Vec::new();
    let indexes = load_indexes(&opts.repos, &opts.trust)?;
    let run_deps = run_dep_ids(&recipe, opts.libc, &indexes)?;

    let store = if opts.tools == Tools::Store {
        fs::create_dir_all(&opts.store).map_err(io_ctx(format!("creating {}", opts.store.display())))?;
        Some(Store::new(abs(&opts.store)?))
    } else {
        None
    };
    let build_dep_ids: BTreeSet<StoreId> = match &store {
        Some(s) => install_build_deps(&recipe, opts.libc, &indexes, s)?,
        None => {
            if !recipe.depends.build.is_empty() {
                warnings.push(format!(
                    "build dependencies ({}) are not enforced with host tools; the build uses whatever the host provides",
                    recipe.depends.build.join(", ")
                ));
            }
            BTreeSet::new()
        }
    };
    if opts.tools == Tools::Host {
        warnings.push("building with host tools: the result is not reproducible".to_string());
    }

    let build_dep_strs: Vec<String> = build_dep_ids.iter().map(ToString::to_string).collect();
    let input_hash = hash::input_hash(
        &recipe,
        &opts.recipe_dir,
        opts.libc,
        &effective,
        opts.tools.name(),
        &build_dep_strs,
        &run_deps,
    )?;

    let work = opts.work_dir.join(format!("{}-{}-{}", recipe.name, recipe.version, std::process::id()));
    let _ = remove_tree(&work);
    fs::create_dir_all(&work).map_err(io_ctx(format!("creating {}", work.display())))?;

    let result = run_build(opts, &recipe, &effective, &work, store.as_ref(), &build_dep_ids, &run_deps, &input_hash);
    let kept = if opts.keep_work {
        Some(work.clone())
    } else {
        let _ = remove_tree(&work);
        None
    };
    let (id, archive, archive_sha256) = result?;
    Ok(Built { id, archive, archive_sha256, hash: input_hash, warnings, work_dir: kept })
}

#[allow(clippy::too_many_arguments)]
fn run_build(
    opts: &BuildOptions,
    recipe: &Recipe,
    effective: &forge_schema::Effective,
    work: &Path,
    store: Option<&Store>,
    build_deps: &BTreeSet<StoreId>,
    run_deps: &[String],
    input_hash: &str,
) -> Result<(StoreId, PathBuf, String), BuildError> {
    let build_dir = work.join("build");
    for d in ["extract", "dest", "home", "phases"] {
        fs::create_dir_all(build_dir.join(d)).map_err(io_ctx(format!("creating {}", build_dir.join(d).display())))?;
    }
    for d in ["sources", "root", "tools"] {
        fs::create_dir_all(work.join(d)).map_err(io_ctx(format!("creating {}", work.join(d).display())))?;
    }

    // Sources: fetch and verify, unpack the first, make all of them readable.
    let cache = SourceCache::new(&opts.source_cache);
    let mut fetched = Vec::new();
    for s in &recipe.sources {
        file_name_of(&s.url).map_err(|e| BuildError::Recipe(e.to_string()))?;
        fetched.push(cache.fetch(s)?);
    }
    stage_sources(&fetched, &work.join("sources"))?;
    let src_root = match fetched.first() {
        Some(first) => extract(first, &build_dir.join("extract"))?,
        None => build_dir.join("extract"),
    };
    let src_in_sandbox = Path::new("/build").join(src_root.strip_prefix(&build_dir).expect("inside the build directory"));

    for (name, text) in [
        ("prepare", &effective.build.prepare),
        ("configure", &effective.build.configure),
        ("compile", &effective.build.compile),
        ("install", &effective.build.install),
    ] {
        if let Some(t) = text {
            let p = build_dir.join("phases").join(format!("{name}.sh"));
            fs::write(&p, format!("{t}\n")).map_err(io_ctx(format!("writing {}", p.display())))?;
        }
    }

    let (tool_binds, tool_links) = match store {
        Some(s) => store_tool_mounts(s, build_deps, &work.join("tools"))?,
        None => host_tool_mounts(),
    };
    let mut binds = vec![
        Bind { src: abs(&build_dir)?, dst: "/build".into(), writable: true },
        Bind { src: abs(&opts.recipe_dir)?, dst: "/recipe".into(), writable: false },
        Bind { src: abs(&work.join("sources"))?, dst: "/sources".into(), writable: false },
    ];
    binds.extend(tool_binds);

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.extend(effective.env.clone());
    for (k, v) in [
        ("PATH", "/usr/bin:/usr/sbin:/bin:/sbin".to_string()),
        ("HOME", "/build/home".to_string()),
        ("SRC", src_in_sandbox.to_string_lossy().into_owned()),
        ("DESTDIR", "/build/dest".to_string()),
        ("JOBS", opts.jobs.max(1).to_string()),
        ("LIBC", opts.libc.as_str().to_string()),
        ("TARGET", opts.libc.target().to_string()),
        // A fixed date, so tools that embed timestamps give the same output every time.
        ("SOURCE_DATE_EPOCH", "315532800".to_string()),
        ("RECIPE_DIR", "/recipe".to_string()),
        ("SOURCES", "/sources".to_string()),
        ("TZ", "UTC".to_string()),
        ("LC_ALL", "C".to_string()),
    ] {
        env.insert(k.to_string(), v);
    }

    let config = SandboxConfig {
        root_dir: abs(&work.join("root"))?,
        binds,
        symlinks: tool_links,
        files: vec![
            ("/etc/passwd".into(), "root:x:0:0:root:/build/home:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/false\n".into()),
            ("/etc/group".into(), "root:x:0:\nnobody:x:65534:\n".into()),
            ("/etc/hostname".into(), "novara-build\n".into()),
        ],
        env,
        cwd: "/build".into(),
        script: script(),
        hostname: "novara-build".into(),
    };
    let status = sandbox::run(&opts.helper, &config, &work.join("sandbox.toml"))?;
    match status.code() {
        Some(0) => {}
        Some(c) if c == SETUP_FAILED => {
            return Err(BuildError::Sandbox("could not set up the sandbox (see the message above)".into()));
        }
        _ => return Err(BuildError::BuildFailed { status: status.to_string() }),
    }

    // Package whatever the install step put in $DESTDIR.
    let dest = build_dir.join("dest");
    let empty = fs::read_dir(&dest).map_err(io_ctx(format!("reading {}", dest.display())))?.next().is_none();
    if empty {
        return Err(BuildError::EmptyOutput);
    }
    fs::create_dir_all(&opts.out_dir).map_err(io_ctx(format!("creating {}", opts.out_dir.display())))?;
    let tmp_out = opts.out_dir.join(format!(".building-{}.tar.zst", std::process::id()));
    let packed = pack_dir(
        &dest,
        &PackOptions {
            name: recipe.name.clone(),
            version: recipe.version.clone(),
            libc: opts.libc,
            hash: Some(input_hash.to_string()),
            deps: run_deps.to_vec(),
        },
        &tmp_out,
    )
    .map_err(|e| {
        let _ = fs::remove_file(&tmp_out);
        BuildError::Package(e.to_string())
    })?;
    let final_path = opts.out_dir.join(format!("{}.tar.zst", packed.id));
    fs::rename(&tmp_out, &final_path).map_err(io_ctx(format!("saving {}", final_path.display())))?;
    Ok((packed.id, final_path, packed.archive_sha256))
}
