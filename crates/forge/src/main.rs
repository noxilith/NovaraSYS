use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use forge_kernel::Http as _;
use forge_schema::{Recipe, SystemDecl, UserDecl};
use forge_strap::Firmware;

#[derive(Parser)]
#[command(name = "forge", version, about = "Novara's declarative system and package tool")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add or remove packages in a declaration
    Pkg {
        #[command(subcommand)]
        action: PkgAction,
    },
    /// Work with the package store: pack, publish, install, list, gc
    Store(StoreArgs),
    /// Create signing keys and manage which public keys are trusted
    Key(KeyArgs),
    /// Sign and verify package repositories
    Repo(RepoArgs),
    /// Build the live ISO (kernel, initramfs and squashfs system, booted by Limine)
    Iso(IsoArgs),
    /// Validate a declaration or recipe file
    Check {
        #[arg(value_enum)]
        kind: CheckKind,
        file: PathBuf,
    },
    /// Install Novara from the live system onto partitions you made (ERASES them)
    Strap(StrapArgs),
    /// Converge the machine to the system declaration
    Apply(ApplyArgs),
    /// Manage generations (read-only snapshots with their own boot entries)
    Gen(GenArgs),
    /// Switch init system (not implemented yet)
    SwapInit,
    /// Find, build and install kernels (every driver as a module, trimmed to this machine, or by hand)
    Kernel(KernelArgs),
    /// Build a recipe from source, in a sandbox, into a package archive
    Build(BuildArgs),
    /// Internal: the sandbox helper that `forge build` re-runs itself as
    #[command(name = "__sandbox", hide = true)]
    SandboxChild { config: PathBuf },
    /// Internal: check whether the sandbox can be created on this machine
    #[command(name = "__sandbox-probe", hide = true)]
    SandboxProbe,
}

#[derive(Clone, clap::ValueEnum)]
enum CheckKind {
    System,
    User,
    Recipe,
}

#[derive(Subcommand)]
enum PkgAction {
    /// Add a package to the declaration
    Add(PkgArgs),
    /// Remove a package from the declaration
    Remove(PkgArgs),
}

#[derive(Args)]
struct PkgArgs {
    name: String,
    /// Edit the system declaration (the default when you are root) instead of your user declaration
    #[arg(long)]
    system: bool,
    /// Edit this file instead of the default location
    #[arg(long)]
    file: Option<PathBuf>,
    /// Only edit the declaration; do not apply it
    #[arg(long)]
    no_apply: bool,
    /// Do not ask for confirmation when applying
    #[arg(long, short = 'y')]
    yes: bool,
    /// Work under this directory instead of / (for testing)
    #[arg(long, default_value = "/")]
    root: PathBuf,
    /// Directory of trusted public keys (default: <root>/etc/forge/keys)
    #[arg(long)]
    keys: Option<PathBuf>,
    /// Use repository indexes WITHOUT checking their signatures (insecure)
    #[arg(long)]
    allow_unsigned: bool,
}

#[derive(Args)]
struct ApplyArgs {
    /// Which declaration to apply (only `system` is implemented so far)
    #[arg(value_enum, default_value = "system")]
    category: Category,
    /// Declaration file (default: <root>/etc/forge/system.toml)
    #[arg(long)]
    file: Option<PathBuf>,
    /// Apply under this directory instead of / (for testing)
    #[arg(long, default_value = "/")]
    root: PathBuf,
    /// Re-apply everything, even what already matches
    #[arg(long)]
    rebuild: bool,
    /// Show the plan and exit without changing anything
    #[arg(long)]
    dry_run: bool,
    /// Do not ask for confirmation
    #[arg(long, short = 'y')]
    yes: bool,
    /// Do not create a generation afterwards (not recommended)
    #[arg(long)]
    no_generation: bool,
    /// Directory of trusted public keys that repository indexes must be signed by
    /// (default: <root>/etc/forge/keys)
    #[arg(long)]
    keys: Option<PathBuf>,
    /// Use repository indexes WITHOUT checking their signatures (insecure)
    #[arg(long)]
    allow_unsigned: bool,
}

#[derive(Args)]
struct StoreArgs {
    #[command(subcommand)]
    action: StoreAction,
    /// The package store directory
    #[arg(long, global = true, default_value = "/forge/store")]
    store: PathBuf,
}

#[derive(Subcommand)]
enum StoreAction {
    /// Pack a directory into a package archive
    Pack(PackArgs),
    /// Add a package archive to a repository directory and update its index
    Publish {
        archive: PathBuf,
        /// The repository directory (created if needed)
        #[arg(long)]
        repo: PathBuf,
        /// Sign the index with this secret key (required to extend a signed repository)
        #[arg(long)]
        sign: Option<PathBuf>,
    },
    /// Verify a package archive and install it into the store
    Install(InstallArgs),
    /// List packages in the store
    List,
    /// Delete store entries no profile or generation uses
    Gc(GcArgs),
}

#[derive(Args)]
struct PackArgs {
    /// Directory whose contents become the package
    dir: PathBuf,
    #[arg(long)]
    name: String,
    #[arg(long)]
    version: String,
    #[arg(long)]
    libc: LibcArg,
    /// Run-time dependency, as a store id (repeatable)
    #[arg(long = "dep")]
    deps: Vec<String>,
    /// Build-input hash (64 hex); default: a hash of the package content
    #[arg(long)]
    hash: Option<String>,
    /// Archive file to write, e.g. hello-1.0.tar.zst
    #[arg(long)]
    out: PathBuf,
}

#[derive(Args)]
struct InstallArgs {
    archive: PathBuf,
    /// Expected SHA-256 of the archive file
    #[arg(long, conflicts_with = "unverified")]
    sha256: Option<String>,
    /// Install without checking the archive's checksum (local testing only)
    #[arg(long)]
    unverified: bool,
}

#[derive(Args)]
struct GcArgs {
    /// Show what would be removed without removing it
    #[arg(long)]
    dry_run: bool,
    /// Profile directories that keep packages alive (default: the system
    /// profile and every user's profile found via /etc/passwd; needs root)
    #[arg(long = "profile")]
    profiles: Vec<PathBuf>,
    /// System generations directory (their `store_paths` keep packages alive)
    #[arg(long, default_value = "/.snapshots")]
    snapshots: PathBuf,
}

#[derive(Args)]
struct GenArgs {
    #[command(subcommand)]
    action: GenAction,
    #[command(flatten)]
    paths: GenPaths,
}

/// Where generations live. The defaults are for a real Novara system.
#[derive(Args, Clone)]
struct GenPaths {
    /// The running root subvolume to snapshot
    #[arg(long, global = true, default_value = "/")]
    live: PathBuf,
    /// Snapshots directory (the @snapshots subvolume)
    #[arg(long, global = true, default_value = "/.snapshots")]
    snapshots: PathBuf,
    /// Mount of the top-level btrfs volume (subvolid=5); needed by rollback
    #[arg(long, global = true)]
    top: Option<PathBuf>,
    /// limine.conf to (re)write
    #[arg(long, global = true, default_value = "/boot/limine.conf")]
    limine_conf: PathBuf,
    /// Directory holding the kernel and initramfs files
    #[arg(long, global = true, default_value = "/boot")]
    boot_dir: PathBuf,
    /// Kernel file name (default: vmlinuz-<running release>)
    #[arg(long, global = true)]
    kernel: Option<String>,
    /// Initramfs file name (default: initramfs-<running release>.img)
    #[arg(long, global = true)]
    initramfs: Option<String>,
    /// Use this kernel command line instead of /proc/cmdline
    #[arg(long, global = true)]
    cmdline: Option<String>,
    /// Testing only: "snapshot" by copying directories instead of using btrfs
    #[arg(long, global = true, hide = true)]
    copy_backend: bool,
}

impl GenPaths {
    fn defaults() -> Self {
        GenPaths {
            live: "/".into(),
            snapshots: "/.snapshots".into(),
            top: None,
            limine_conf: "/boot/limine.conf".into(),
            boot_dir: "/boot".into(),
            kernel: None,
            initramfs: None,
            cmdline: None,
            copy_backend: false,
        }
    }
}

#[derive(Subcommand)]
enum GenAction {
    /// Snapshot the live system as a new generation
    Create {
        /// What this generation is for
        #[arg(required = true, num_args = 1..)]
        description: Vec<String>,
    },
    /// List generations (* marks the current one)
    List,
    /// Show the current generation and the subvolume you booted
    Current,
    /// Remove one generation, or `all` (the current and booted ones are kept)
    Remove { target: String },
    /// Roll back to a generation (finishes after a reboot)
    Rollback { id: u32 },
    /// Delete the leftover @.next once a rollback is complete
    CleanupNext,
    /// Regenerate limine.conf (also completes a staged rollback)
    RewriteLimine,
}

#[derive(Clone, clap::ValueEnum)]
enum Category {
    System,
    User,
}

#[derive(Args)]
struct StrapArgs {
    #[arg(long, conflicts_with = "bios")]
    uefi: bool,
    #[arg(long)]
    bios: bool,
    /// Target root partition, e.g. /dev/sda2 (becomes btrfs; ERASED)
    root: String,
    /// Boot partition, e.g. /dev/sda1 (becomes FAT32; ERASED)
    boot: String,
    /// Directory to copy the system from
    #[arg(long, default_value = "/")]
    source: PathBuf,
    /// Where the target is mounted during the install
    #[arg(long, default_value = "/mnt/target")]
    mount_point: PathBuf,
    /// Directory holding BOOTX64.EFI and limine-bios.sys
    #[arg(long, default_value = "/usr/share/limine")]
    limine_dir: PathBuf,
    /// Hostname for the starter /etc/forge/system.toml
    #[arg(long, default_value = "novara")]
    hostname: String,
    /// glibc or musl (default: detected from the copied system)
    #[arg(long)]
    libc: Option<LibcArg>,
    /// Kernel file in /boot (default: the newest vmlinuz-*)
    #[arg(long)]
    kernel: Option<String>,
    /// Initramfs file in /boot (default: initramfs-<kernel release>.img)
    #[arg(long)]
    initramfs: Option<String>,
    /// Extra kernel command line, after root=UUID=...
    #[arg(long, default_value = "")]
    cmdline_extra: String,
    /// Do not ask to create a user or change the root password
    #[arg(long)]
    no_users: bool,
    /// Do not add a UEFI firmware boot entry (the removable-media path still works)
    #[arg(long)]
    no_efi_entry: bool,
    /// Print the plan and exit without changing anything
    #[arg(long)]
    dry_run: bool,
    /// Skip the typed confirmation (for scripts and test images)
    #[arg(long)]
    yes: bool,
}

#[derive(Clone, clap::ValueEnum)]
enum LibcArg {
    Glibc,
    Musl,
}

#[derive(Args)]
struct BuildArgs {
    /// Directory containing recipe.toml (and any patches)
    recipe: PathBuf,
    /// glibc or musl (needed if the recipe supports both)
    #[arg(long)]
    libc: Option<LibcArg>,
    /// Repository to resolve dependencies from (repeatable)
    #[arg(long = "repo")]
    repos: Vec<PathBuf>,
    /// Where the finished archive is written
    #[arg(long, default_value = "packages")]
    out: PathBuf,
    /// Where build tools come from: `store` (reproducible) or `host`
    /// (the host's /usr; for bootstrapping only, not reproducible)
    #[arg(long, value_enum, default_value = "store")]
    tools: ToolsArg,
    /// The package store used for `--tools store`
    #[arg(long, default_value = "/forge/store")]
    store: PathBuf,
    /// Cache of downloaded sources (default: ~/.cache/forge/sources)
    #[arg(long)]
    source_cache: Option<PathBuf>,
    /// Parent of the temporary build directory (default: ~/.cache/forge/build)
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Parallel jobs for the build ($JOBS); default: the number of CPUs
    #[arg(long)]
    jobs: Option<u32>,
    /// Keep the temporary build directory (to inspect a failed build)
    #[arg(long)]
    keep_work: bool,
    /// Also add the result to this repository directory
    #[arg(long)]
    publish: Option<PathBuf>,
    /// Sign the published index with this secret key (needed to publish into a signed repository)
    #[arg(long, requires = "publish")]
    sign: Option<PathBuf>,
    /// Directory of trusted public keys that repository indexes must be signed by
    /// (default: ~/.config/forge/keys)
    #[arg(long)]
    keys: Option<PathBuf>,
    /// Use repository indexes WITHOUT checking their signatures (insecure)
    #[arg(long)]
    allow_unsigned: bool,
}

#[derive(Clone, clap::ValueEnum)]
enum ToolsArg {
    Store,
    Host,
}

fn cache_dir(sub: &str) -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".cache"),
    };
    Ok(base.join("forge").join(sub))
}

fn build_cmd(a: BuildArgs) -> Result<()> {
    let recipe_text = std::fs::read_to_string(a.recipe.join("recipe.toml"))
        .with_context(|| format!("reading {}/recipe.toml", a.recipe.display()))?;
    // Load the signing key first: a bad path or permissions should fail now, not
    // after the build.
    let signer = a.sign.as_deref().map(forge_store::load_secret_key).transpose()?;
    let recipe = Recipe::parse(&recipe_text).context("in the recipe")?;
    let libc = match (&a.libc, recipe.libc.as_slice()) {
        (Some(l), _) => to_libc(l),
        (None, [only]) => *only,
        (None, _) => bail!("{} supports several libcs; choose one with --libc glibc|musl", recipe.name),
    };
    let opts = forge_build::BuildOptions {
        recipe_dir: a.recipe,
        libc,
        out_dir: a.out,
        work_dir: match a.work_dir {
            Some(d) => d,
            None => cache_dir("build")?,
        },
        source_cache: match a.source_cache {
            Some(d) => d,
            None => cache_dir("sources")?,
        },
        repos: a.repos,
        trust: load_trust(a.keys, a.allow_unsigned, user_dir("keys")?)?,
        tools: match a.tools {
            ToolsArg::Store => forge_build::Tools::Store,
            ToolsArg::Host => forge_build::Tools::Host,
        },
        store: a.store,
        jobs: a.jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() as u32)),
        helper: std::env::current_exe().context("finding the forge executable")?,
        keep_work: a.keep_work,
    };
    if !forge_build::sandbox::supported(&opts.helper) {
        bail!(
            "cannot create the build sandbox: unprivileged user namespaces are unavailable \
             (check /proc/sys/user/max_user_namespaces)"
        );
    }
    println!("building {} {} for {}", recipe.name, recipe.version, libc.as_str());
    let built = forge_build::build(&opts)?;
    for w in &built.warnings {
        eprintln!("warning: {w}");
    }
    println!("built {}", built.id);
    println!("archive: {}", built.archive.display());
    println!("archive sha256: {}", built.archive_sha256);
    if let Some(dir) = &built.work_dir {
        println!("kept the build directory: {}", dir.display());
    }
    if let Some(repo) = a.publish {
        forge_store::LocalRepo::new(&repo).publish(&built.archive, signer.as_ref())?;
        println!("published to {}", repo.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    // The sandbox helper reports the build's own exit status, so it bypasses
    // the normal error handling below.
    let cli = Cli::parse();
    match &cli.command {
        Cmd::SandboxChild { config } => {
            return ExitCode::from(forge_build::sandbox::child_main(config).clamp(0, 255) as u8);
        }
        Cmd::SandboxProbe => return ExitCode::from(forge_build::sandbox::probe_main() as u8),
        _ => {}
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("forge: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Pkg { action } => pkg(action),
        Cmd::Store(a) => store_cmd(a),
        Cmd::Key(a) => key_cmd(a),
        Cmd::Repo(a) => repo_cmd(a),
        Cmd::Iso(a) => iso_cmd(a),
        Cmd::Check { kind, file } => check(kind, &file),
        Cmd::Strap(a) => strap(a),
        Cmd::Apply(a) => apply(a),
        Cmd::Gen(a) => gen_cmd(a),
        Cmd::SwapInit => todo_cmd("swap-init"),
        Cmd::Kernel(a) => kernel_cmd(a),
        Cmd::Build(a) => build_cmd(a),
        Cmd::SandboxChild { .. } | Cmd::SandboxProbe => unreachable!("handled in main"),
    }
}

fn todo_cmd(name: &str) -> Result<()> {
    bail!("`forge {name}` is not implemented yet")
}

fn decl_path(system: bool, file: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(f) = file {
        return Ok(f);
    }
    if system {
        return Ok(PathBuf::from("/etc/forge/system.toml"));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("forge/user.toml"))
}

fn pkg(action: PkgAction) -> Result<()> {
    let (args, adding) = match action {
        PkgAction::Add(a) => (a, true),
        PkgAction::Remove(a) => (a, false),
    };
    // Packages are installed for the whole system by default when you are root, and into your own
    // declaration otherwise (per-user installs are not implemented yet, so that only records the wish).
    let system = args.system || (args.file.is_none() && (is_root() || args.root != Path::new("/")));
    let path = match (&args.file, system) {
        (Some(f), _) => f.clone(),
        (None, true) => args.root.join("etc/forge/system.toml"),
        (None, false) => decl_path(false, None)?,
    };
    if system && args.root == Path::new("/") && !is_root() && args.file.is_none() {
        bail!("changing the system declaration needs root (try: sudo forge pkg {} {})", if adding { "add" } else { "remove" }, args.name);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !system => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let edited = if adding {
        forge_decl::add_package(&text, &args.name)?
    } else {
        forge_decl::remove_package(&text, &args.name)?
    };

    let verb = if adding { "already in" } else { "not in" };
    if !edited.changed {
        println!("{} is {verb} {}", args.name, path.display());
        return Ok(());
    }

    // For a system package, check that the edited declaration can actually be applied BEFORE writing it:
    // a typo or a package missing from the repositories must not leave a declaration that cannot apply.
    let opts = if system && !args.no_apply {
        let o = forge_apply::Options {
            rebuild: false,
            trust: load_trust(args.keys.clone(), args.allow_unsigned, args.root.join("etc/forge/keys"))?,
            fetch: Default::default(),
        };
        let decl = SystemDecl::parse(&edited.text).with_context(|| format!("in the edited {}", path.display()))?;
        forge_apply::plan_system(&decl, &args.root, o.clone())
            .with_context(|| format!("{} cannot be applied, so {} was not changed", args.name, path.display()))?;
        Some(o)
    } else {
        None
    };

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, edited.text).with_context(|| format!("writing {}", path.display()))?;
    println!("{} {} {}", if adding { "added" } else { "removed" }, args.name, path.display());

    if !system {
        println!("note: per-user installs are not implemented yet, so nothing was installed;");
        println!("      for a system-wide package run: sudo forge pkg {} {}", if adding { "add" } else { "remove" }, args.name);
        return Ok(());
    }
    if opts.is_none() {
        println!("not applied (--no-apply): run `forge apply` when you are ready");
        return Ok(());
    }
    apply(ApplyArgs {
        category: Category::System,
        file: Some(path),
        root: args.root,
        rebuild: false,
        dry_run: false,
        yes: args.yes,
        no_generation: false,
        keys: args.keys,
        allow_unsigned: args.allow_unsigned,
    })
}

fn check(kind: CheckKind, file: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    match kind {
        CheckKind::System => drop(SystemDecl::parse(&text)?),
        CheckKind::User => drop(UserDecl::parse(&text)?),
        CheckKind::Recipe => drop(Recipe::parse(&text)?),
    }
    println!("{}: ok", file.display());
    Ok(())
}

fn strap(a: StrapArgs) -> Result<()> {
    let firmware = if a.uefi {
        Firmware::Uefi
    } else if a.bios {
        Firmware::Bios
    } else if Path::new("/sys/firmware/efi").is_dir() {
        Firmware::Uefi
    } else {
        Firmware::Bios
    };
    let plan = forge_strap::plan(firmware, &a.root, Some(&a.boot))?;
    println!("Install plan ({:?}):", plan.firmware);
    for (i, step) in plan.steps.iter().enumerate() {
        println!("  {}. {step}", i + 1);
    }
    if a.dry_run {
        println!("dry run: nothing was changed");
        return Ok(());
    }
    if !is_root() {
        bail!("installing needs root");
    }

    use std::os::unix::fs::FileTypeExt;
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    forge_strap::preflight(&plan, &mounts, &|d| {
        std::fs::metadata(d).is_ok_and(|m| m.file_type().is_block_device())
    })?;

    let accounts = if a.no_users { forge_strap::Accounts::default() } else { collect_accounts()? };

    if !a.yes {
        println!();
        println!("This will ERASE {} and {}.", plan.root_dev, plan.boot_dev);
        let typed = prompt_line(&format!("Type {} to continue: ", plan.root_dev))?;
        if typed.trim() != plan.root_dev {
            println!("aborted: nothing was changed");
            return Ok(());
        }
    }

    let req = forge_strap::Request {
        top_mount: PathBuf::from(format!("{}-top", a.mount_point.display())),
        mount_point: a.mount_point,
        source: a.source,
        limine_dir: a.limine_dir,
        hostname: a.hostname,
        libc: a.libc.map(|l| match l {
            LibcArg::Glibc => forge_schema::Libc::Glibc,
            LibcArg::Musl => forge_schema::Libc::Musl,
        }),
        kernel: a.kernel,
        initramfs: a.initramfs,
        extra_cmdline: a.cmdline_extra,
        efi_entry: !a.no_efi_entry,
        accounts,
        plan,
    };
    forge_strap::execute(&req, &mut forge_strap::SystemRunner, &mut TermUi)?;
    println!();
    println!("Installed. Remove the live media and reboot.");
    println!("After the first boot, run `forge apply` to converge the system to /etc/forge/system.toml.");
    Ok(())
}

struct TermUi;

impl forge_strap::Ui for TermUi {
    fn step(&mut self, n: usize, total: usize, text: &str) {
        println!("[{n}/{total}] {text}");
    }
    fn warn(&mut self, text: &str) {
        eprintln!("warning: {text}");
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("no input (end of file)");
    }
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    Ok(match prompt_line(&format!("{question} {hint} "))?.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}

fn ask_password(who: &str) -> Result<String> {
    loop {
        let a = rpassword::prompt_password(format!("Password for {who}: "))?;
        let b = rpassword::prompt_password("Repeat password: ")?;
        if a != b {
            println!("passwords do not match, try again");
        } else if a.is_empty() {
            println!("the password must not be empty");
        } else {
            return Ok(a);
        }
    }
}

/// Ask for the accounts up front, so typos are caught before any disk is touched.
fn collect_accounts() -> Result<forge_strap::Accounts> {
    let mut acc = forge_strap::Accounts::default();
    println!();
    if yes_no("Create a user account?", true)? {
        loop {
            let name = prompt_line("Username: ")?;
            let candidate = forge_strap::Accounts {
                user: Some(forge_strap::NewUser { name: name.trim().to_string(), password: "x".into() }),
                root_password: None,
            };
            match candidate.validate() {
                Ok(()) => {
                    let name = name.trim().to_string();
                    let password = ask_password(&name)?;
                    acc.user = Some(forge_strap::NewUser { name, password });
                    break;
                }
                Err(_) => println!("use lowercase letters, digits, `-` and `_`, starting with a letter or `_`"),
            }
        }
    }
    if yes_no("Change the root password from the live default?", true)? {
        acc.root_password = Some(ask_password("root")?);
    }
    Ok(acc)
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:").map(|r| r.split_whitespace().next() == Some("0")))
        })
        .unwrap_or(false)
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

fn apply(a: ApplyArgs) -> Result<()> {
    if matches!(a.category, Category::User) {
        bail!("`forge apply user` is not implemented yet");
    }
    if a.root == Path::new("/") && !a.dry_run && !is_root() {
        bail!("applying to / needs root (use --dry-run to preview, or --root DIR to test)");
    }
    let path = a.file.unwrap_or_else(|| a.root.join("etc/forge/system.toml"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let decl = SystemDecl::parse(&text).with_context(|| format!("in {}", path.display()))?;

    let opts = forge_apply::Options {
        rebuild: a.rebuild,
        trust: load_trust(a.keys, a.allow_unsigned, a.root.join("etc/forge/keys"))?,
        fetch: Default::default(),
    };
    let plan = forge_apply::plan_system(&decl, &a.root, opts)?;

    for c in &plan.changes {
        println!("{c}");
    }
    if !plan.pending.is_empty() {
        println!("not applied by this version of forge:");
        for p in &plan.pending {
            println!("  [ ] {}: {}", p.section, p.summary);
        }
    }
    if plan.is_empty() {
        println!("nothing to do: the system already matches {}", path.display());
        return Ok(());
    }
    if a.dry_run {
        println!("dry run: nothing was changed");
        return Ok(());
    }
    if !a.yes && !confirm("apply these changes?")? {
        println!("aborted: nothing was changed");
        return Ok(());
    }
    forge_apply::apply(&plan)?;
    println!("applied {} change(s)", plan.changes.len());
    if a.no_generation {
        println!("note: no generation was created (--no-generation)");
    } else if a.root != Path::new("/") {
        println!("note: no generation was created (--root is not the real system)");
    } else {
        let g = with_gen_ctx(&GenPaths::defaults(), true, |ctx| {
            Ok(forge_gen::create(ctx, "forge apply system")?)
        })
        .context("the changes were applied, but creating the generation failed")?;
        println!("created generation {}", g.id);
    }
    Ok(())
}

fn with_gen_ctx<R>(
    p: &GenPaths,
    need_boot: bool,
    f: impl FnOnce(&forge_gen::Ctx) -> Result<R>,
) -> Result<R> {
    let raw = match &p.cmdline {
        Some(c) => c.clone(),
        None => std::fs::read_to_string("/proc/cmdline").unwrap_or_default(),
    };
    let boot = if need_boot {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let kernel = p.kernel.clone().unwrap_or_else(|| format!("vmlinuz-{release}"));
        // The initramfs is optional (a kernel with its root drivers built in needs none).
        let initramfs = match &p.initramfs {
            Some(i) => i.clone(),
            None => {
                let default = format!("initramfs-{release}.img");
                if p.boot_dir.join(&default).exists() { default } else { String::new() }
            }
        };
        for f in [&kernel, &initramfs] {
            if !f.is_empty() && !p.boot_dir.join(f).exists() {
                bail!(
                    "{} not found in {} (use --kernel / --initramfs / --boot-dir)",
                    f,
                    p.boot_dir.display()
                );
            }
        }
        forge_gen::BootInfo { kernel, initramfs, cmdline: forge_gen::base_cmdline(&raw) }
    } else {
        forge_gen::BootInfo { kernel: "-".into(), initramfs: "-".into(), cmdline: String::new() }
    };
    let backend: Box<dyn forge_gen::Backend> = if p.copy_backend {
        Box::new(forge_gen::CopyBackend)
    } else {
        Box::new(forge_gen::BtrfsCli)
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ctx = forge_gen::Ctx {
        backend: backend.as_ref(),
        live: p.live.clone(),
        snaps: p.snapshots.clone(),
        top: p.top.clone(),
        limine_conf: p.limine_conf.clone(),
        booted_subvol: forge_gen::booted_subvol(&raw),
        boot,
        now,
    };
    f(&ctx)
}

fn gen_cmd(a: GenArgs) -> Result<()> {
    let read_only = matches!(a.action, GenAction::List | GenAction::Current);
    if !read_only && !a.paths.copy_backend && !is_root() {
        bail!("changing generations needs root");
    }
    with_gen_ctx(&a.paths, !read_only, |ctx| match a.action {
        GenAction::Create { description } => {
            let g = forge_gen::create(ctx, &description.join(" "))?;
            println!("created generation {}: {}", g.id, g.description);
            Ok(())
        }
        GenAction::List => {
            let cur = forge_gen::current(ctx)?;
            let gens = forge_gen::list(ctx)?;
            if gens.is_empty() {
                println!("no generations");
            }
            for g in gens {
                let mark = if cur == Some(g.id) { '*' } else { ' ' };
                println!(
                    "{mark} {:>3}  {}  {}  [{}]",
                    g.id,
                    forge_gen::format_utc(g.created),
                    g.description,
                    g.kernel
                );
            }
            Ok(())
        }
        GenAction::Current => {
            match forge_gen::current(ctx)? {
                Some(n) => println!("current generation: {n}"),
                None => println!("current generation: none"),
            }
            match &ctx.booted_subvol {
                Some(s) => println!("booted subvolume:   {s}"),
                None => println!("booted subvolume:   unknown"),
            }
            Ok(())
        }
        GenAction::Remove { target } => {
            let r = if target == "all" {
                forge_gen::remove_all(ctx)?
            } else {
                let id: u32 = target.parse().context("expected a generation number or `all`")?;
                forge_gen::remove(ctx, id)?
            };
            for id in &r.removed {
                println!("removed generation {id}");
            }
            for (id, why) in &r.skipped {
                println!("kept generation {id}: it is {why}");
            }
            Ok(())
        }
        GenAction::Rollback { id } => {
            match forge_gen::rollback(ctx, id)? {
                forge_gen::RollbackOutcome::Staged => println!(
                    "rollback to generation {id} is staged; reboot to finish it, then run \
                     `forge gen rewrite-limine` and, after one more reboot, `forge gen cleanup-next`"
                ),
                forge_gen::RollbackOutcome::Replaced => {
                    println!("@ was replaced with generation {id}; reboot into the normal entry")
                }
            }
            Ok(())
        }
        GenAction::CleanupNext => {
            forge_gen::cleanup_next(ctx)?;
            println!("removed @.next");
            Ok(())
        }
        GenAction::RewriteLimine => {
            let p = forge_gen::promote(ctx)?;
            forge_gen::rewrite_limine(ctx)?;
            println!("wrote {}", ctx.limine_conf.display());
            if p == forge_gen::Promotion::WaitingForReboot {
                println!("a rollback is staged: reboot into it to finish");
            }
            Ok(())
        }
    })
}

fn to_libc(l: &LibcArg) -> forge_schema::Libc {
    match l {
        LibcArg::Glibc => forge_schema::Libc::Glibc,
        LibcArg::Musl => forge_schema::Libc::Musl,
    }
}

/// Every profile directory that must keep its packages alive: the system
/// profile and each account's profile, found through /etc/passwd so that no
/// home directory is missed.
fn default_profile_dirs() -> Result<Vec<PathBuf>> {
    let mut dirs = vec![PathBuf::from("/forge/profiles/system")];
    let passwd = std::fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?;
    for line in passwd.lines() {
        if let Some(home) = line.split(':').nth(5).filter(|h| h.starts_with('/') && *h != "/") {
            let dir = PathBuf::from(home).join(".local/state/forge/profile");
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    Ok(dirs)
}

fn store_cmd(a: StoreArgs) -> Result<()> {
    let store = forge_store::Store::new(&a.store);
    match a.action {
        StoreAction::Pack(p) => {
            let packed = forge_store::pack_dir(
                &p.dir,
                &forge_store::PackOptions {
                    name: p.name,
                    version: p.version,
                    libc: to_libc(&p.libc),
                    hash: p.hash,
                    deps: p.deps,
                },
                &p.out,
            )?;
            println!("packed {} -> {}", packed.id, p.out.display());
            println!("archive sha256: {}", packed.archive_sha256);
        }
        StoreAction::Publish { archive, repo, sign } => {
            let signer = sign.as_deref().map(forge_store::load_secret_key).transpose()?;
            let e = forge_store::LocalRepo::new(&repo).publish(&archive, signer.as_ref())?;
            println!("published {} to {}", e.id()?, repo.display());
        }
        StoreAction::Install(i) => {
            if i.sha256.is_none() && !i.unverified {
                bail!("give --sha256 <hash> to verify the archive, or --unverified to skip the check");
            }
            let file = std::fs::File::open(&i.archive)
                .with_context(|| format!("opening {}", i.archive.display()))?;
            let id = store.install(file, i.sha256.as_deref())?;
            println!("installed {id}");
        }
        StoreAction::List => {
            let ids = store.list()?;
            if ids.is_empty() {
                println!("the store is empty");
            }
            for id in ids {
                println!("{id}");
            }
        }
        StoreAction::Gc(g) => {
            if g.profiles.is_empty() && !is_root() {
                bail!("gc must read every user's profile, which needs root (or name the roots with --profile)");
            }
            let dirs = if g.profiles.is_empty() { default_profile_dirs()? } else { g.profiles };
            let mut roots = std::collections::BTreeSet::new();
            for d in &dirs {
                // A profile that cannot be read is an error, never "empty":
                // otherwise its packages would look unused and be deleted.
                roots.extend(
                    forge_store::Profile::new(d)
                        .all_packages()
                        .with_context(|| format!("reading profile {}", d.display()))?,
                );
            }
            roots.extend(forge_store::roots_from_snapshots(&g.snapshots)?);
            let report = store.gc(&roots, g.dry_run)?;
            let verb = if g.dry_run { "would remove" } else { "removed" };
            for id in &report.removed {
                println!("{verb} {id}");
            }
            for p in &report.stale_tmp {
                println!("{verb} leftover {}", p.display());
            }
            println!("kept {} package(s), {verb} {}", report.kept.len(), report.removed.len());
        }
    }
    Ok(())
}

// ---- signing: keys, trust and repositories -----------------------------------

/// `~/.config/forge/<sub>` (or under `$XDG_CONFIG_HOME`).
fn user_dir(sub: &str) -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("forge").join(sub))
}

/// The policy for repository indexes: signed by a key in `keys` (or `default_dir`),
/// unless `allow_unsigned` explicitly turns the check off.
fn load_trust(keys: Option<PathBuf>, allow_unsigned: bool, default_dir: PathBuf) -> Result<forge_store::Trust> {
    if allow_unsigned {
        eprintln!("warning: --allow-unsigned: repository indexes are NOT being verified");
        return Ok(forge_store::Trust::AllowUnsigned);
    }
    let dir = keys.unwrap_or(default_dir);
    let ring = forge_store::Keyring::load_dir(&dir)
        .with_context(|| format!("reading the trusted keys in {}", dir.display()))?;
    if ring.is_empty() {
        eprintln!(
            "note: no trusted keys in {}; trust one with `forge key trust <file.pub> --dir {}`",
            dir.display(),
            dir.display()
        );
    }
    Ok(forge_store::Trust::Require(ring))
}

#[derive(Args)]
struct KeyArgs {
    #[command(subcommand)]
    action: KeyAction,
}

#[derive(Subcommand)]
enum KeyAction {
    /// Create a signing key pair (NAME.key is secret, NAME.pub is for sharing)
    Generate {
        name: String,
        /// Where to write the pair (default: ~/.config/forge/signing)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// List the trusted public keys in a directory
    List {
        /// The keyring (default: ~/.config/forge/keys)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Trust a public key by adding it to a keyring
    Trust {
        /// The NAME.pub file to trust
        file: PathBuf,
        /// The keyring (default: ~/.config/forge/keys; the system's is /etc/forge/keys)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

fn key_cmd(a: KeyArgs) -> Result<()> {
    match a.action {
        KeyAction::Generate { name, dir } => {
            let dir = match dir {
                Some(d) => d,
                None => user_dir("signing")?,
            };
            let key = forge_store::SecretKey::generate()?;
            let (secret, public) = forge_store::write_key_pair(&dir, &name, &key)?;
            println!("key id:     {}", key.public().id());
            println!("secret key: {}  (keep this private; it signs repositories)", secret.display());
            println!("public key: {}  (share this; others trust it with `forge key trust`)", public.display());
        }
        KeyAction::List { dir } => {
            let dir = match dir {
                Some(d) => d,
                None => user_dir("keys")?,
            };
            let ring = forge_store::Keyring::load_dir(&dir)?;
            if ring.is_empty() {
                println!("no trusted keys in {}", dir.display());
            }
            for id in ring.ids() {
                println!("{id}");
            }
        }
        KeyAction::Trust { file, dir } => {
            let dir = match dir {
                Some(d) => d,
                None => user_dir("keys")?,
            };
            let key = forge_store::load_public_key(&file)?;
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let dest = dir.join(format!("{}.pub", &key.id()[..16]));
            let text = std::fs::read_to_string(&file)?;
            match std::fs::read_to_string(&dest) {
                Ok(existing) if existing == text => println!("already trusted"),
                Ok(_) => bail!("{} already exists with different content", dest.display()),
                Err(_) => {
                    std::fs::write(&dest, text).with_context(|| format!("writing {}", dest.display()))?;
                    println!("trusted");
                }
            }
            println!("key id: {}", key.id());
            println!("keyring: {}", dir.display());
        }
    }
    Ok(())
}

#[derive(Args)]
struct RepoArgs {
    #[command(subcommand)]
    action: RepoAction,
}

#[derive(Subcommand)]
enum RepoAction {
    /// Sign a repository's index (this vouches for whatever it lists: check it first)
    Sign {
        repo: PathBuf,
        /// The secret key file to sign with
        #[arg(long)]
        key: PathBuf,
    },
    /// Build one clean repository from several: the newest version of every package, signed by KEY
    Assemble {
        /// The new repository (must not exist yet)
        dest: PathBuf,
        /// A repository to take packages from (repeatable; on a tie the earlier one wins)
        #[arg(long = "from", required = true)]
        from: Vec<PathBuf>,
        /// The secret key to sign the new repository with
        #[arg(long)]
        key: PathBuf,
        /// Leave out packages whose name contains this (default: -stage, the bootstrap steps)
        #[arg(long, default_value = "-stage")]
        skip: Vec<String>,
    },
    /// Check a repository's signature against the trusted keys
    Verify {
        repo: PathBuf,
        /// The keyring (default: ~/.config/forge/keys)
        #[arg(long)]
        keys: Option<PathBuf>,
    },
}

fn repo_cmd(a: RepoArgs) -> Result<()> {
    match a.action {
        RepoAction::Sign { repo, key } => {
            let signer = forge_store::load_secret_key(&key)?;
            let repo_handle = forge_store::LocalRepo::new(&repo);
            repo_handle.sign(&signer)?;
            println!("signed {} with key {}", repo.display(), signer.public().id());
        }
        RepoAction::Assemble { dest, from, key, skip } => {
            if dest.exists() {
                bail!("{} already exists; remove it or choose another name", dest.display());
            }
            let signer = forge_store::load_secret_key(&key)?;
            // The newest version of each (name, libc) across the sources; earlier sources win ties.
            let mut best: std::collections::BTreeMap<(String, String), (forge_store::IndexEntry, PathBuf)> = Default::default();
            for src in &from {
                let repo = forge_store::LocalRepo::new(src);
                let index = repo.index_unverified().with_context(|| format!("reading the index of {}", src.display()))?;
                for e in index.packages {
                    if skip.iter().any(|s| e.name.contains(s.as_str())) {
                        continue;
                    }
                    let k = (e.name.clone(), format!("{:?}", e.libc));
                    let newer = match best.get(&k) {
                        None => true,
                        Some((cur, _)) => forge_store::version_cmp(&e.version, &cur.version) == std::cmp::Ordering::Greater,
                    };
                    if newer {
                        let path = repo.archive_path(&e);
                        best.insert(k, (e, path));
                    }
                }
            }
            if best.is_empty() {
                bail!("no packages found in the given repositories");
            }
            let target = forge_store::LocalRepo::new(&dest);
            for (e, path) in best.values() {
                target.publish(path, Some(&signer)).with_context(|| format!("adding {} {}", e.name, e.version))?;
                println!("  {} {}", e.name, e.version);
            }
            // Run-time dependencies are exact builds: every one must have come along.
            let index = target.load(&forge_store::Trust::AllowUnsigned)?.index;
            let mut missing = Vec::new();
            for e in &index.packages {
                for d in &e.deps {
                    if forge_store::StoreId::parse(d).and_then(|id| index.by_id(&id).map(|_| ())).is_none() {
                        missing.push(format!("{} {} needs {d}", e.name, e.version));
                    }
                }
            }
            if !missing.is_empty() {
                bail!("the assembled repository is incomplete:\n  {}", missing.join("\n  "));
            }
            println!("assembled {} package(s) into {} (serial {}), signed with key {}",
                index.packages.len(), dest.display(), index.serial, signer.public().id());
        }
        RepoAction::Verify { repo, keys } => {
            let trust = load_trust(keys, false, user_dir("keys")?)?;
            let loaded = forge_store::LocalRepo::new(&repo).load(&trust)?;
            println!(
                "{}: signed by {}, serial {}, {} package(s)",
                repo.display(),
                loaded.signed_by.as_deref().unwrap_or("?"),
                loaded.index.serial,
                loaded.index.packages.len()
            );
        }
    }
    Ok(())
}

// ---- the live ISO ---------------------------------------------------------------

#[derive(Args)]
struct IsoArgs {
    /// Package repository to take the system from (repeatable)
    #[arg(long, required = true)]
    repo: Vec<PathBuf>,
    /// Packages in the live system; their dependencies come along
    #[arg(long = "package", default_values_t = ["linux", "busybox", "musl", "openrc", "sudo", "forge", "rsync", "util-linux", "ncurses", "btrfs-progs", "shadow", "limine", "efibootmgr", "wpa_supplicant", "wireless-regdb", "linux-firmware-wifi"].map(String::from))]
    packages: Vec<String>,
    #[arg(long, default_value = "musl")]
    libc: String,
    /// The ISO file to write
    #[arg(long, default_value = "novara-live.iso")]
    out: PathBuf,
    /// Scratch directory (must not exist; kept afterwards for inspection)
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Boot this kernel instead of the newest /boot/vmlinuz-* in the system
    #[arg(long)]
    kernel: Option<PathBuf>,
    /// Where Limine's files are
    #[arg(long, default_value = "/usr/share/limine")]
    limine_dir: PathBuf,
    /// Volume label (capital letters, digits and underscores)
    #[arg(long, default_value = "NOVARA_LIVE")]
    label: String,
    #[arg(long, default_value = "novara-live")]
    hostname: String,
    /// Kernel command line
    #[arg(long, default_value = "console=ttyS0,115200 console=tty0")]
    cmdline: String,
    /// The live root password
    #[arg(long, default_value = "root")]
    root_password: String,
    /// Directory of `*.pub` keys the installed system will trust for package repositories
    /// (copied to /etc/forge/keys; the project's release key is in distro/keys)
    #[arg(long, value_name = "DIR")]
    trusted_keys: Option<PathBuf>,
    /// Directory of trusted public keys (default: ~/.config/forge/keys)
    #[arg(long)]
    keys: Option<PathBuf>,
    /// Do not verify repository signatures (insecure)
    #[arg(long)]
    allow_unsigned: bool,
}

fn iso_cmd(a: IsoArgs) -> Result<()> {
    let libc = match a.libc.as_str() {
        "musl" => forge_schema::Libc::Musl,
        "glibc" => forge_schema::Libc::Glibc,
        other => bail!("--libc must be musl or glibc, not {other}"),
    };
    let work = a.work_dir.clone().unwrap_or_else(|| PathBuf::from(format!("{}.work", a.out.display())));
    if work.exists() {
        bail!("{} already exists; remove it or pass another --work-dir", work.display());
    }
    if a.out.exists() {
        bail!("{} already exists; remove it first", a.out.display());
    }
    let trust = load_trust(a.keys, a.allow_unsigned, user_dir("keys")?)?;

    eprintln!("fetching {} (and dependencies) from {} repositor{}", a.packages.join(", "), a.repo.len(), if a.repo.len() == 1 { "y" } else { "ies" });
    let trees = forge_iso::fetch(&a.repo, &trust, &a.packages, libc, &work.join("store"))?;
    let rootfs = work.join("rootfs");
    forge_iso::merge(&trees, &rootfs)?;

    let kernel = match a.kernel {
        Some(k) => k,
        None => forge_iso::kernel_in(&rootfs)
            .context("the system has no kernel in /boot (add the `linux` package, or pass --kernel)")?,
    };
    eprintln!("kernel: {}", kernel.display());
    let mut trusted_keys = Vec::new();
    if let Some(dir) = &a.trusted_keys {
        for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "pub") {
                forge_store::load_public_key(&p).with_context(|| format!("{} is not a usable public key", p.display()))?;
                trusted_keys.push(p);
            }
        }
        if trusted_keys.is_empty() {
            bail!("no *.pub files in {}", dir.display());
        }
        trusted_keys.sort();
    }
    let spec = forge_iso::IsoSpec {
        rootfs,
        kernel,
        limine_dir: a.limine_dir,
        work: work.join("build"),
        out: a.out.clone(),
        label: a.label,
        hostname: a.hostname,
        cmdline: a.cmdline,
        root_password: a.root_password,
        trusted_keys,
    };
    forge_iso::build(&spec, &mut forge_iso::SystemRunner)?;
    let size = std::fs::metadata(&a.out)?.len();
    println!("wrote {} ({} MiB)", a.out.display(), size / (1024 * 1024));
    println!("scratch files are in {} (safe to delete)", work.display());
    Ok(())
}

// ---- kernels ---------------------------------------------------------------------------------------

#[derive(Args)]
struct KernelArgs {
    #[command(subcommand)]
    action: KernelAction,
}

#[derive(Subcommand)]
enum KernelAction {
    /// List the kernels kernel.org offers now
    List,
    /// Download, verify, configure and build a kernel into a staging directory
    Build(KernelBuildArgs),
    /// Install a staged kernel: modules, boot files, a generation to roll back to, and Limine
    Install(KernelInstallArgs),
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Series {
    /// The newest stable release (new hardware support soonest, supported for a few months)
    Stable,
    /// The newest long-term release (fewer changes, supported for years)
    Longterm,
}

#[derive(Args)]
struct KernelBuildArgs {
    /// An exact version, e.g. 7.3.3
    #[arg(long, conflicts_with = "series")]
    version: Option<String>,
    /// Which kernel.org series to take the newest of
    #[arg(long, value_enum, default_value = "stable")]
    series: Series,
    /// Every driver the kernel has, as a module: the live ISO's kernel (about 300 MB of modules)
    #[arg(long, group = "mode")]
    full: bool,
    /// Keep only the modules loaded right now (the default). Plug in the devices you use first, or name them with --also-keep
    #[arg(long, group = "mode")]
    auto: bool,
    /// Choose in `make menuconfig`, starting from --base-config
    #[arg(long, group = "mode")]
    menuconfig: bool,
    /// Use this configuration as it is (it is only checked)
    #[arg(long, group = "mode", value_name = "FILE")]
    config: Option<PathBuf>,
    /// Starting point for --auto and --menuconfig (default: the running kernel's configuration)
    #[arg(long)]
    base_config: Option<PathBuf>,
    /// With --auto: read the loaded modules from this file (a copy of /proc/modules saved on the machine the
    /// kernel is FOR) instead of from this machine
    #[arg(long, value_name = "FILE")]
    loaded_modules: Option<PathBuf>,
    /// With --auto: keep this module even if it is not loaded now (repeatable)
    #[arg(long = "also-keep", value_name = "MODULE")]
    also_keep: Vec<String>,
    #[arg(long)]
    jobs: Option<usize>,
    /// Scratch directory (default ~/.cache/forge/kernel)
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Where to stage the result (default <work-dir>/stage-<version>)
    #[arg(long)]
    out: Option<PathBuf>,
    /// Downloaded sources are kept here (default ~/.cache/forge/sources)
    #[arg(long)]
    source_cache: Option<PathBuf>,
    /// The C compiler command
    #[arg(long, default_value = "gcc -std=gnu11")]
    cc: String,
    /// Install it when the build is done (needs root)
    #[arg(long)]
    install: bool,
    /// Do not check for the build tools first
    #[arg(long)]
    skip_checks: bool,
}

#[derive(Args)]
struct KernelInstallArgs {
    /// The staging directory `forge kernel build` printed
    staged: PathBuf,
    #[arg(long, default_value = "/boot")]
    boot_dir: PathBuf,
    /// The system root the modules go under
    #[arg(long, default_value = "/")]
    root: PathBuf,
    #[arg(long, default_value = "/boot/limine.conf")]
    limine_conf: PathBuf,
    #[arg(long, default_value = "/.snapshots")]
    snapshots: PathBuf,
    /// Do not snapshot a generation first (you could not roll back to the old kernel's system)
    #[arg(long)]
    no_generation: bool,
    /// Copy the files but leave limine.conf alone: the new kernel is not offered at boot
    #[arg(long)]
    no_limine: bool,
    /// Replace this release if it is already installed
    #[arg(long)]
    force: bool,
}

fn cache_home(sub: &str) -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".cache"),
    };
    Ok(base.join("forge").join(sub))
}

fn kernel_cmd(a: KernelArgs) -> Result<()> {
    match a.action {
        KernelAction::List => {
            let text = forge_kernel::Curl.get(forge_kernel::RELEASES_URL)?;
            for r in forge_kernel::parse_releases(&text)? {
                if r.moniker != "linux-next" {
                    println!("{:<10} {}", r.moniker, r.version);
                }
            }
            Ok(())
        }
        KernelAction::Build(b) => kernel_build(b),
        KernelAction::Install(i) => kernel_install(i),
    }
}

/// The running kernel's configuration: `/boot/config-<release>`, or `/proc/config.gz`.
fn running_config(work: &Path) -> Result<PathBuf> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let in_boot = PathBuf::from(format!("/boot/config-{}", release.trim()));
    if in_boot.is_file() {
        return Ok(in_boot);
    }
    if Path::new("/proc/config.gz").is_file() {
        let dst = work.join("running-config");
        std::fs::create_dir_all(work)?;
        let out = std::process::Command::new("gzip").args(["-dc", "/proc/config.gz"]).output().context("running gzip")?;
        if out.status.success() {
            std::fs::write(&dst, out.stdout)?;
            return Ok(dst);
        }
    }
    bail!("cannot find the running kernel's configuration (no /boot/config-{} and no /proc/config.gz); pass --base-config", release.trim())
}

fn kernel_build(b: KernelBuildArgs) -> Result<()> {
    if !b.skip_checks {
        forge_kernel::preflight()?;
    }
    let work = match b.work_dir {
        Some(w) => w,
        None => cache_home("kernel")?,
    };
    let cache = forge_build::SourceCache::new(match b.source_cache {
        Some(c) => c,
        None => cache_home("sources")?,
    });
    let selector = match (&b.version, b.series) {
        (Some(v), _) => forge_kernel::Selector::Version(v.clone()),
        (None, Series::Stable) => forge_kernel::Selector::Stable,
        (None, Series::Longterm) => forge_kernel::Selector::Longterm,
    };
    eprintln!("finding the kernel...");
    let (version, tarball) = forge_kernel::fetch(&forge_kernel::Curl, &cache, &selector)?;
    eprintln!("kernel {version}: {} (checked against kernel.org's checksum list)", tarball.display());

    let mode = if b.full {
        forge_kernel::Mode::Full
    } else if let Some(file) = b.config {
        forge_kernel::Mode::File(file)
    } else if b.menuconfig {
        forge_kernel::Mode::Menu { base_config: match b.base_config { Some(c) => c, None => running_config(&work)? } }
    } else {
        let loaded = match &b.loaded_modules {
            Some(f) => std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?,
            None => std::fs::read_to_string("/proc/modules").context("reading /proc/modules")?,
        };
        if loaded.trim().is_empty() {
            bail!("no modules are loaded, so there is nothing to base a trimmed kernel on; use --full or --menuconfig");
        }
        forge_kernel::Mode::Auto {
            base_config: match b.base_config { Some(c) => c, None => running_config(&work)? },
            loaded_modules: loaded,
            keep: b.also_keep,
        }
    };
    let out = b.out.unwrap_or_else(|| work.join(format!("stage-{version}")));
    if out.exists() && std::fs::read_dir(&out).map(|mut d| d.next().is_some()).unwrap_or(false) {
        bail!("{} is not empty; remove it or pass another --out", out.display());
    }
    let jobs = b.jobs.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    let spec = forge_kernel::BuildSpec { version, tarball, work, out, mode, jobs, cc: b.cc };
    let staged = forge_kernel::build(&spec, &mut forge_kernel::SystemRunner)?;
    println!("built kernel {} in {}", staged.release, staged.dir.display());
    if b.install {
        return kernel_install(KernelInstallArgs {
            staged: staged.dir,
            boot_dir: "/boot".into(),
            root: "/".into(),
            limine_conf: "/boot/limine.conf".into(),
            snapshots: "/.snapshots".into(),
            no_generation: false,
            no_limine: false,
            force: false,
        });
    }
    println!("install it with: sudo forge kernel install {}", staged.dir.display());
    Ok(())
}

fn kernel_install(i: KernelInstallArgs) -> Result<()> {
    if !is_root() {
        bail!("installing a kernel needs root");
    }
    let release = forge_kernel::staged_release(&i.staged)?;
    let paths = |kernel: Option<String>, initramfs: Option<String>| GenPaths {
        snapshots: i.snapshots.clone(),
        limine_conf: i.limine_conf.clone(),
        boot_dir: i.boot_dir.clone(),
        kernel,
        initramfs,
        ..GenPaths::defaults()
    };

    // A generation of the system as it is now, with the kernel it boots now: the way back.
    if !i.no_generation && !i.no_limine {
        with_gen_ctx(&paths(None, None), true, |ctx| {
            let g = forge_gen::create(ctx, &format!("before kernel {release}"))?;
            println!("created generation {} to roll back to", g.id);
            Ok(())
        })
        .context("could not snapshot a generation first (not a Novara btrfs system? use --no-generation to go on without one)")?;
    }

    let installed = forge_kernel::install_files(&i.staged, &i.boot_dir, &i.root, i.force)?;
    println!("installed {} and its modules", installed.kernel_file);

    if i.no_limine {
        println!("limine.conf was left alone: the new kernel is not offered at boot yet");
        return Ok(());
    }
    // No initramfs: these kernels carry the drivers needed to reach the root.
    with_gen_ctx(&paths(Some(installed.kernel_file.clone()), Some(String::new())), true, |ctx| {
        forge_gen::rewrite_limine(ctx)?;
        Ok(())
    })?;
    println!("limine.conf now boots {}; older kernels stay available under Generations", installed.kernel_file);
    println!("reboot to use kernel {}", installed.release);
    Ok(())
}
