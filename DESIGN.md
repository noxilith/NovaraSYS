# NovaraSYS design

Independent Linux distribution. Declarative, generation-based, OpenRC by default with swappable init. Modeled on the install flow and ideas of HaliadeSYS, but built around one Rust tool, `forge`.

## Decisions

| Area | Decision |
|---|---|
| Base | Independent, built from source (not Arch/Debian derived) |
| libc | glibc **and** musl: two ISOs, two package repos, one recipe tree. libc is fixed at install time and never mixed |
| Init | OpenRC default; `forge swap-init` switches (creates a generation first) |
| Root fs | btrfs: `@` (live), `@snapshots` (generations) |
| Firmware | UEFI and BIOS |
| Bootloader | Limine, with a Generations submenu |
| Language | Rust for everything we write (see below); C components stay as upstream |
| Tooling | One Rust binary: `forge` |
| Declaration | TOML, edited round-trip with `toml_edit` (comments preserved) |
| Packages | Source recipes + binary cache; `--isolate` mode |
| Users | Each user has their own packages and dotfiles (per-user declaration) |

## Install flow (the Haliade way)

1. Boot the live ISO (Limine), OpenRC starts. Login `root` / `root`.
2. `loadkeys` if needed. `lsblk` to find the target disk.
3. Partition manually with `cfdisk`: a boot partition and a root partition. The boot partition is required for both UEFI and BIOS because Limine cannot read btrfs, so `/boot` is its own FAT32 partition (for UEFI it is the EFI System Partition).
4. `forge strap [--uefi|--bios] <root-dev> <boot-dev>`:
   - ask up front for the user name/password and a new root password (so typos are caught before any disk is touched), then require typing the root device path to confirm (`--yes` skips this for scripts);
   - refuse devices that are not block devices or are mounted;
   - format boot (FAT32) and root (btrfs), create `@`, `@snapshots`, `@home` and `@store`, mount them;
   - `rsync -aHX` the live system to the target (works offline), skipping runtime directories and live-only paths; `/boot` is copied separately with FAT-safe flags (no ownership/ACLs/xattrs);
   - write `/etc/fstab` (all four subvolumes plus `/boot`) and a starter `/etc/forge/system.toml` (hostname, libc detected from the copied system, the new user) unless one was copied;
   - install Limine: UEFI copies `BOOTX64.EFI` to `EFI/BOOT/` (works even without a firmware entry) and tries `efibootmgr` (failure is only a warning; `--no-efi-entry` skips it); BIOS copies `limine-bios.sys` and runs `limine bios-install`;
   - write `limine.conf` booting the newest `vmlinuz-*` from the new root;
   - create the user (in the `forge` group) and set passwords with `useradd --root` / `chpasswd --root`, passwords passed on stdin only, never on a command line;
   - sync, unmount, tell the user to reboot. On any failure the mounts are released and the error names the failed step.

The installed system is a clone of the live system, so anything done in the live session is kept.

## `forge` commands

| Command | Does |
|---|---|
| `forge strap ...` | Installer (above) |
| `forge pkg add/remove <name>` | Edit `packages` in the declaration, then converge |
| `forge pkg add <name> --isolate` | Install into a self-contained store with its full dependency closure |
| `forge apply [system\|user]` | Converge to the declaration: validate, plan per module, show `[+]` `[~]` `[-]` preview, y/N, apply (installs before removals), create a generation |
| `forge apply --rebuild` | Re-apply everything even if satisfied |
| `forge gen create/list/current/rollback/remove` | Generations (read-only btrfs snapshots + Limine entries) |
| `forge swap-init <init>` | Swap init system |
| `forge kernel --menuconfig\|--auto\|--full` | genkernel (below) |
| `forge build <recipe>` | Build a recipe from source in a sandbox |

## Declaration

- System: `/etc/forge/system.toml`. Covers hostname, timezone, locale, hwclock, keymap, console font, bootloader, init and services, env vars, users, kernel modules, fstab, `libc`, `packages`. Needs root to apply.
- Per user: `~/.config/forge/user.toml`. Covers that user's packages, dotfiles/config files, scripts, media, and run-commands. Does not need root (warn if run as root).
- `forge pkg add/remove` always edits the declaration first, so the declaration is the single source of truth and nothing drifts.

## Per-user installs

Goal: customize one user, create another, and the new one is like a fresh install. Example: declare `kitty` in the system declaration and every user gets it; one user declares `niri`, another `hyprland`, and neither sees the other's commands.

- **System declaration** (`/etc/forge/system.toml`): base system and system-wide packages. Everything here is on every user's `PATH`. Dotfiles are never shared.
- **User declaration** (`~/.config/forge/user.toml`): that user's own packages, dotfiles, scripts and run-commands. Packages here are visible only to that user.
- **Store:** all packages live in one shared immutable, content-addressed store (`/forge/store/<name>-<version>-<hash>`). Two users with the same package share the files; different versions coexist.
- **Profiles:** the system profile (`/forge/profiles/system`) is on every user's `PATH`. Each user also has a personal profile (`~/.local/state/forge/profiles/`), a symlink set into the store, layered on top of the system profile. Anything not in a user's profile (their own niri/hyprland, commands, desktop entries, man pages) does not appear for other users.
- **Fresh users:** creating a user creates an empty personal profile and a default `user.toml`, so they start from the system declaration alone.
- **No root for user installs:** `forge pkg add` as a normal user edits `user.toml` and asks the store daemon (below) to realize the package.
- **User generations:** each profile switch is a user-level generation (a symlink-set swap), so a user can roll back their own apps without touching the system.
- `--isolate` extends this: the package and its whole dependency closure are kept in their own store entry, exposed only through the profile's `bin`.

### Store daemon

- A small root daemon, started by OpenRC, listening on a unix socket. Only members of the `forge` group may use it.
- It accepts only "realize recipe X at hash H". It never accepts arbitrary paths.
- It verifies the binary-cache signature, or builds from the recipe in a sandbox, then writes the result into the store read-only.
- It is security-critical code: keep it small, with no recipe logic beyond verify/build/write.
- `forge gc` removes only store entries that no system generation, user generation or profile refers to.

## Generations

- Subvolumes: `@` (live root, rolled back by generations), `@.next` (staged rollback), `@snapshots/gen-N` (read-only, with a `meta.toml`: description, time, kernel, cmdline, **required store paths**), `@home` (user data, not rolled back with the system), `@store` (the package store: append-only, not rolled back).
- The store is outside `@` on purpose: a system rollback must not revert it, or profiles in `@home` would point at packages that no longer exist. Each generation's `meta.toml` pins the store paths it needs, so `forge gc` never collects them while that generation exists.
- Created automatically after system apply, user apply and init swap. Can also be created by hand.
- Each generation boots **its own kernel** and its own `init=`, so a rollback never mixes modules or init. `create` refuses on a generation boot; `remove` refuses the current or booted generation.
- Rollback is two-stage because the booted subvolume can't be replaced while mounted (copy to `@.next`, reboot, promote, clean up).

## genkernel (`forge kernel`)

Downloads a kernel from kernel.org (verify signature and checksum), then:

- `--menuconfig`: you pick the drivers.
- `--auto`: detect hardware (`lsmod`, `/sys`, `lspci`) and build a minimal localmodconfig-style kernel.
- `--full`: broad driver support, same config as the live ISO.

Installs kernel + initramfs, updates Limine entries, keeps older kernels for generations. The live ISO ships a full-compatibility kernel.

## Packages and building

- Recipe: one TOML file per package at `recipes/<name>/recipe.toml`, in a git-hosted recipe tree. Optional `patches/` and `files/` (e.g. OpenRC scripts) sit beside it.
  ```toml
  name        = "htop"
  version     = "3.3.0"
  description = "Interactive process viewer"
  license     = "GPL-2.0"
  libc        = ["glibc", "musl"]          # libcs this recipe supports

  [[source]]
  url    = "https://github.com/htop-dev/htop/archive/3.3.0.tar.gz"
  sha256 = "..."

  [depends]
  build = ["ncurses", "autoconf", "automake", "make", "gcc"]
  run   = ["ncurses"]

  [build]                                   # plain sh, run in order; no network
  prepare   = "./autogen.sh"
  configure = "./configure --prefix=/usr --sysconfdir=/etc"
  compile   = "make -j$JOBS"
  install   = "make DESTDIR=$DESTDIR install"

  [libc_override.musl]                             # per-libc overrides, merged over the above
  patches = ["musl-fix.patch"]
  configure_extra = "--disable-unicode"
  ```
  - Build variables: `$SRC`, `$DESTDIR`, `$JOBS`, `$LIBC`, `$TARGET`.
  - **No install scripts.** Instead of `postinst` shell, recipes declare effects: `[provides] users`, `groups`, `services` (OpenRC), `kernel_modules`. `forge apply` carries them out, so they show up in the preview and in generations.
  - Split packages via `[[output]]` (e.g. `htop`, `htop-doc`); a package's file manifest is recorded per output.
  - The **store hash** is computed from the recipe, its sources' checksums, the hashes of all build dependencies, the libc, and the build flags. Same inputs, same hash, so a cache hit is exact.
- **Build sandbox:** `forge` creates Linux namespaces itself (user, mount, pid, net, uts; no external dependency, so it works during bootstrap).
  - Sources are fetched and checksummed by `forge` before the build, then the sandbox has **no network**.
  - Only the build dependencies' closure is mounted, read-only; the build tree and `$DESTDIR` are the only writable paths.
  - Reproducibility aids: fixed `SOURCE_DATE_EPOCH`, fixed build paths, normalized timestamps, stable archive ordering.
  - Output is packed as `.tar.zst` with a file manifest, named by store hash, signed with an ed25519 key. Users verify signatures before anything goes into the store.
  - The very first bootstrap (before `forge` exists) uses a plain chroot with root as scaffolding.
- Binary cache holds prebuilt archives per libc; users may build locally. The ISO ships a prebuilt base.
- Recipes are libc-parametrized from the start: `libc = "glibc" | "musl"`. Never hardcode glibc-specific paths (e.g. `ld-linux`) in `forge`.
- `forge` itself must build statically for both libcs.

## Deferred: foreign package files (`.deb`)

Subspaces (isolated roots of other distros) are **not** part of the design for now. A way to use apps that only ship a `.deb` is wanted later. Options to revisit then:

- a subspace-style isolated Debian root (bubblewrap) with the `.deb` installed inside and its commands exported to a profile (robust, works on both libcs, costs disk space);
- native extraction of self-contained `.deb` files into the store with `--isolate`, glibc ISO only (fast, but fragile: Debian dependency names, `postinst` scripts, glibc version, `/usr/lib/x86_64-linux-gnu` paths).

The store, profile and `--isolate` design should not make either option impossible.

## Implementation language: Rust

Everything we write ourselves is Rust by default (fast, lightweight, and the project owner's preferred language).

- **Cargo workspace:** `forge` (CLI) plus library crates for the declaration/recipe schemas, store and profiles, sandbox, generations, genkernel, the installer (`strap`) and the store daemon. A shared schema crate keeps the daemon, CLI and installer from drifting apart.
- **Live ISO tooling** (image builder, live-environment setup) is Rust too, instead of shell scripts.
- **Not rewritten (stay C):** Linux kernel, glibc/musl, OpenRC, Limine, btrfs-progs, rsync, cfdisk, the compiler toolchain. A Rust coreutils (`uutils`) can replace the basic commands later.
- **Bootstrap:** the first `forge` is cross-compiled with a Rust toolchain from the build host. Building `rustc` from source in Novara needs a prebuilt Rust to start from, so a `rust` recipe comes after Novara can build itself.
- **Static builds:** `forge`, the installer and the daemon build statically (musl target for the musl ISO; glibc carefully) so they work in the minimal live environment.

## Bootstrap order

1. Cross toolchain, once per libc.
2. Minimal base: kernel, userland (busybox or coreutils), OpenRC, btrfs-progs, Limine, rsync, cfdisk.
3. `forge` (Rust), then rebuild the base through it so every package has a recipe.
4. Live ISO + `forge strap`.
5. Test matrix: build and boot both ISOs (QEMU, UEFI and BIOS) on every change.

## Open questions

- Recipe details: how build dependencies are split from host tools when cross-building, virtual packages (e.g. "any libc"), and how `[provides]` effects are versioned.
- Layering order when a user's profile and the system profile contain different versions of the same package (current idea: the user's wins for that user).
- Which users get into the `forge` group by default (installer-created user: yes).
- Naming of components other than `forge` (live ISO, repos).

## Status

Scaffolded Cargo workspace (`crates/`):

- `forge-schema`: serde types and validation for `system.toml`, `user.toml` and `recipe.toml`.
- `forge-decl`: comment-preserving `packages` add/remove (`toml_edit`).
- `forge-strap`: `forge strap`. `plan` validates the request and returns the ordered steps; `execute` runs them through a `Runner` trait (`SystemRunner` spawns processes; tests use a recording fake), so the exact command sequence is unit-tested without a disk. Needs on the live system: `rsync`, `mkfs.btrfs`, `mkfs.vfat`, `btrfs`, `blkid`, `sfdisk`, and shadow-utils with `--root` support (`useradd`, `groupadd`, `chpasswd`); UEFI also `efibootmgr`, BIOS also `limine`. Limine's `BOOTX64.EFI` / `limine-bios.sys` are read from `--limine-dir` (default `/usr/share/limine`).
- `forge-apply`: `forge apply system`. Plans first (reads the machine, changes nothing), previews as `[+]` `[~]` `[-]`, asks y/N, then applies; installs/modifications before removals; `--rebuild` re-applies everything; `--dry-run`; `--root DIR` applies under a scratch directory instead of `/`. Applies hostname, timezone, locale, keymap (OpenRC), kernel modules (`/etc/modules-load.d/forge.conf`) and OpenRC services. Forge only disables services it enabled itself (tracked in `/var/lib/forge/state.toml`). Users and packages are shown as "not applied by this version" until user management and the store exist.
- `forge-gen`: generations. `forge gen create/list/current/remove [N|all]/rollback N/cleanup-next/rewrite-limine`. Read-only snapshots at `<snapshots>/gen-N` with `gen-N.toml` beside them (description, time, kernel, initramfs, command line), `.current`, and a `.rollback` marker. Each generation's Limine entry boots its own kernel, command line (so `init=` too) and subvolume, read-only. Two-stage rollback via `@.next` as designed (`@` is kept as `@.old-<time>`, never deleted); one step when you booted a generation. `create` refuses on a generation boot or while a rollback is staged; `remove` refuses the current/booted generation. Filesystem access goes through a `Backend` trait: `BtrfsCli` (real) and `CopyBackend` (directory copies, for tests only).
- `forge`: CLI. Working: `pkg add/remove`, `check`, `strap` (`--dry-run` prints the plan), `apply system` (creates a generation afterwards when applying to the real `/`; `--no-generation` to skip), `gen`. Stubs: `apply user`, `swap-init`, `kernel`.

Testing: the unit tests cover generation logic with the copy backend. The real `btrfs` calls are only exercised by `scripts/test-gen-btrfs.sh` (loopback btrfs image, needs root; passed on the author's machine on 2026-11-09). The default paths (`/.snapshots`, `/boot/limine.conf`) are the real system's; use the path flags to test elsewhere.

Known limits: Limine must be able to read the boot partition's kernel files, so `/boot` keeps versioned kernels (`vmlinuz-<release>`, `initramfs-<release>.img`); other `rootflags` options are dropped from generation command lines; `store_paths` in generation metadata is not filled in until the store exists.

Notes: per-libc recipe overrides use `[libc_override.<libc>]` (not `[libc.<libc>]`) because `libc` is already the supported-libc array. `.cargo/config.toml` pins the linker to `gcc` because this build host has no `cc`.

Strap testing: unit tests cover the command sequence, fstab/limine.conf/system.toml contents, account handling and failure cleanup. `scripts/test-strap-loop.sh` runs a real install of a tiny fake system onto a loopback image (needs root; never formats anything but the temporary image; passes `--no-efi-entry` so firmware boot entries are untouched) and then checks the result and that `forge gen` works on it. Passed on the author's machine on 2026-11-09 (including that strap itself marks the boot partition as an EFI System Partition). It does not check that the installed system actually **boots**; that needs a VM and a real live ISO. The BIOS path (`limine bios-install`) is only covered by the command-sequence unit test.

## Package store (implemented: `forge-store`, `forge store`, `forge apply` packages)

- **Store:** `/forge/store/<name>-<version>-<first 32 hex of the build hash>`, immutable (files `0444`/`0555`, directories `0555`, setuid/setgid never preserved). Versions may not contain `-`, so ids parse unambiguously.
- **Archive:** zstd-compressed tar (pure Rust: `tar` + `ruzstd`; `Fastest` level only for now). First entry is `.forge/manifest.toml` (name, version, libc, build hash, run-time dependency ids, and every file with its SHA-256 / symlink target). Packing is deterministic.
- **Installing is verified and atomic.** The archive is unpacked into a private `.tmp-*` directory without tar's own extraction: every entry must be listed in the manifest with the right type and content hash, is created exclusively (no overwrite, never through a symlink), and parents must be real directories. The archive's own SHA-256 is checked too. Only then is the directory renamed into place, so a bad archive never appears in the store. Installing something already present is a no-op.
- **Profiles:** `<dir>/gen-N/` is a tree of symlinks into the store (plus `gen-N.toml`), and `current` is a symlink to the active generation. A switch builds the whole new generation first, then flips `current` atomically; two packages providing the same file is an error that leaves the old generation active. Rolling back is `set_current`. System profile: `/forge/profiles/system`; user profile (planned): `~/.local/state/forge/profile`.
- **`forge apply system`** resolves `packages` against `repos` (local directories) to the newest version for the declared libc plus dependencies (cycles, missing dependencies and wrong-libc dependencies are errors), installs what is missing, and switches the system profile. It also writes `/etc/profile.d/forge.sh` to put the profile on `PATH`.
- **Garbage collection:** `forge store gc [--dry-run]` keeps every package in every profile generation (so rollbacks keep working), every package listed in a system generation's `store_paths`, and everything reachable from those through dependencies. It refuses to run if any profile can't be read, finds user profiles through `/etc/passwd`, never deletes on a damaged manifest, and only removes `.tmp-*` leftovers older than an hour.
- **Repository:** `index.toml` plus archives; `forge store pack` / `publish` build one by hand.

Not done yet (deliberately):

- **Authenticity:** the index is **not signed**; a repository must be trusted as a whole. Archive checksums are verified, but they come from that same index. Signing (ed25519) is next.
- **Network:** only local-directory repositories; no HTTP fetching.
- **Daemon:** installs need write access to the store, so only root can install today. The store daemon (for per-user installs) is not written; user profiles and `apply user` are not wired up.
- **Builds:** nothing builds packages from recipes yet (the sandbox and the recipe builder), so packages come from `forge store pack`.
- **System generations** do not record `store_paths` yet, so rolling back the root filesystem does not yet protect the packages it used from gc (profile generations do).
- **File conflicts** between packages are found when the profile is switched, not during planning, so a failed apply can leave newly installed (but unlinked) packages in the store; `forge store gc` cleans them up.
- Archives are packed through a temporary tar file; there is no cap on unpacked size.

## Recipe builder (implemented: `forge-build`, `forge build`)

`forge build <recipe-dir> [--libc musl|glibc] [--tools store|host] [--repo DIR]... [--out DIR]` turns a `recipe.toml` into a package archive.

**Pipeline.** Parse and validate the recipe, resolve run-time dependencies (exact store ids go into the manifest), fetch and checksum sources, unpack the first source into `$SRC`, run the steps in the sandbox, pack `$DESTDIR`, name the archive after the build-input hash.

- **Sources** are pinned by SHA-256 and kept in a content-addressed cache (`<sha256>-<name>`); a cache entry that no longer matches is discarded, never trusted. Local paths and `file://` are copied; `http(s)://` is downloaded with `curl` (an interim choice: no TLS stack is linked into `forge`; redirects may not leave http/https; the download is untrusted until its checksum matches, so the transport only needs to work, not to be trusted). Unpacking: `.tar`, `.gz`/`.tgz`, `.bz2`/`.tbz2`, `.xz`/`.txz`, `.zst` (all pure Rust); entries escaping the destination are rejected. No `.zip`.
- **Steps** (`prepare`, `configure`, `compile`, `install`) run as plain `sh -eu`, each in its own subshell started in `$SRC`, so `cd`/variables never leak between steps. `-u` means a possibly-unset variable needs `${VAR:-}`. `[libc_override.<libc>]` can replace a step, append `configure_extra`, and set `env`.
- **Environment** is built from nothing (the host's never leaks in): `SRC`, `DESTDIR=/build/dest`, `JOBS`, `LIBC`, `TARGET` (`x86_64-linux-musl|gnu`), `RECIPE_DIR=/recipe`, `SOURCES=/sources`, `HOME=/build/home`, `PATH=/usr/bin:/usr/sbin:/bin:/sbin`, `SOURCE_DATE_EPOCH=315532800`, `TZ=UTC`, `LC_ALL=C`. Recipes cannot override these.
- **Input hash** covers name, version, libc, tools mode, source URLs and checksums, every step's text, env, **every file in the recipe directory** (so patches count), and the build and run dependency ids. Dependency order does not matter. Building the same inputs twice gives a byte-identical archive (tested).

**Sandbox** (no external tools). `forge` re-runs itself as a hidden helper (`forge __sandbox <config>`; `__sandbox-probe` checks support). The helper unshares user, mount, PID, network, UTS and IPC namespaces, maps "root" inside to the calling user (no real privilege), forks so the child is PID 1, builds a root on a fresh tmpfs from only the declared mounts (read-only unless writable), mounts `/proc`, a minimal `/dev` and a scratch `/tmp`, `pivot_root`s, makes the root itself read-only, and `exec`s `/bin/sh -c`. Result, all verified by tests that fail if broken: no network (no default route, connections fail), host files and processes invisible, `/usr` `/etc` `/bin` read-only (so a recipe that forgets `$DESTDIR` **fails** instead of installing into a directory that vanishes), only `/build`, `/tmp` and `/dev/shm` writable.

**Tools.** `--tools host` mounts the host's standard directories (`/bin /sbin /lib /lib64`, and under `/usr`: `bin sbin lib lib32 lib64 libexec include share local` — an allowlist, not all of `/usr`) and is flagged **not reproducible**; it is for bootstrapping the first packages. `--tools store` builds a profile from the recipe's `depends.build` (installing missing ones from `--repo` into `--store`) and mounts it plus the store read-only.

**Found by testing, worth remembering**
- Inside a user namespace the kernel refuses a plain bind of a directory that has other mounts below it (`/usr` and `/var` do on the author's machine: `EINVAL`). The sandbox now binds recursively and then makes *every* submount read-only by reading `/proc/self/mountinfo`.
- Binding all of `/usr` would also pull in odd host mounts (e.g. a service's state directory), hence the allowlist.
- A new network namespace contains `lo` and a `sit0` tunnel device the kernel creates itself; neither has connectivity.

**Not done / limits**
- `--tools store` is now proven end to end with the stage-0 base (see below). A top-level symlink inside a package (e.g. `bin -> usr/bin`) still does not resolve across packages in a profile.
- Fetching needs `curl` on the build machine; no `.zip` sources; no cap on unpacked source size.
- No timeout and no CPU/memory/disk limits on a build; no seccomp filter. The sandbox protects against accidents and ordinary misbehaving builds; it is not hardened against a build that exploits the kernel. Builds run as a fake root (uid 0 mapped to the caller).
- Requires unprivileged user namespaces (`/proc/sys/user/max_user_namespaces`); `forge build` says so if they are unavailable.
- Build dependencies are only enforced with `--tools store`.

## Stage 0 base (implemented: `recipes/*-stage0`, `scripts/bootstrap-base.sh`)

`scripts/bootstrap-base.sh` (`LIBCS="musl glibc"` for both) builds the first packages with the **host's compiler** and checks them. About 1.5 minutes for both libcs on the author's machine.

| Package | What it is |
|---|---|
| `linux-headers-stage0` 0.stage0 | the kernel API headers, **copied from the build host** (impure); `scsi/` is skipped because on a glibc host it holds glibc's own headers, which musl also ships |
| `musl-stage0` 1.2.5 | the C library (musl only) |
| `busybox-stage0` 1.36.1_r1 | one static binary: `sh` (ash) and ~400 applets, minus `strings` (binutils provides the real one; two packages may not supply the same file) |
| `make-stage0` 4.4.1 | GNU make, static |

**What the script proves.** Every binary is static and linked against the right libc (checked by libc-specific strings, not trusted from the build); the shell and make run; and then a fixture recipe (`scripts/fixtures/store-tools`) is built with `--tools store` using **only BusyBox and make from the store**, in a sandbox where the host's libc and gcc are absent. So later stages no longer need `--tools host`. The same check exists as a test (`FORGE_TEST_STAGE0_REPO=repo/stage0-<stamp> cargo test -p forge --test build store_tools`), skipped unless a repository is supplied.

**Design.** Nothing in the store can compile yet, so `busybox` and `make` build **their own copy of musl first** inside the build (`[libc_override.musl]`), plus its `musl-gcc` wrapper, and compile against it; the glibc variants are plain static links with the host gcc. The standalone `musl` package is built for later stages but nothing consumes it yet. Each run writes fresh directories (`packages/stage0-<stamp>`, `repo/stage0-<stamp>`) and refuses to reuse one; logs are kept in `packages/stage0-<stamp>/logs/`.

**Sources.** Downloaded on 2026-11-09 from musl.libc.org, busybox.net and ftp.gnu.org and pinned by SHA-256 in each recipe. The BusyBox hash matches the checksum busybox.net publishes; the **musl and make hashes are pinned from the first download (trust on first use)**, since no independent checksum was available to compare. PGP signatures were not checked.

**Reproducibility, measured.** Packaging the same inputs in two separate runs gave byte-identical archives and identical ids for all seven packages. But the id does not include the host compiler's version, so a different host `gcc` could produce different bytes under the same id. That is why `--tools host` stays flagged as impure.

**Problems found while building it** (each now has a fix or a guard):
- `forge` exports `DESTDIR` to every step, and `make` turns environment variables into make variables, so a helper `make install` (the stage-0 musl) installed into the package's own output. Recipes that build helpers must pass `DESTDIR=` explicitly.
- The host is GCC 16, whose default link sequence adds `-latomic_asneeded` from the system library directory, which a musl build must not search. The recipes override `*link_gcc_c_sequence` in `musl-gcc.specs`; gcc rejects a specs file unless the last section ends with exactly the expected blank line.
- BusyBox's `make install` **silently rebuilt everything with the host gcc** because it was not given the same `CC`, producing a glibc binary inside a package labelled musl. The build log showed musl-gcc being used and the build succeeded; only inspecting the binary revealed it. The recipe now passes `CC` and fails the build if glibc strings are found, and the script re-checks every binary.
- GNU make 4.4.1 does not build with C23 (the default of gcc 15+): `-std=gnu17`.
- The sandbox mounted its scratch `/tmp` after the bind mounts, hiding anything bound below `/tmp` (a store or repository kept there). Special mounts (`/proc`, `/dev`, `/tmp`) now come first; a regression test fails if the order regresses (checked by temporarily reintroducing the bug).
- Shell: with `set -o pipefail`, `strings … | grep -q` can fail a correct check (grep exits at the first match, `strings` dies of SIGPIPE).

**Limits / next.**
- Stage 0 is **impure**: it needs a compiler on the host and copies the host's kernel headers. Stage 1 (below) adds a compiler to the store; it is itself still built by the host gcc.
- BusyBox uses its default configuration minus `tc` (which does not build against current kernel headers) and `strings` (see above). Applets that a full package in the base provides should be dropped the same way; the profile builder refuses overlapping files, which is how the `strings` and `scsi/` clashes were found.
- The repository index is still unsigned.

## Stage 1: bootstrap binutils and gcc (implemented: `recipes/binutils-stage1`, `recipes/gcc-stage1`, `scripts/bootstrap-toolchain.sh`)

`BASE_REPO=repo/stage0-<stamp> scripts/bootstrap-toolchain.sh` builds **binutils-stage1** (2.43.1) and **gcc-stage1** (14.2.0, C and C++, with a static libstdc++) for the target `x86_64-linux-musl`, then proves Novara can compile with them. About 11 minutes on 8 cores (binutils 1 minute, gcc about 10, `JOBS=6`); `KEEP_WORK=1` keeps the multi-GB build directories for debugging.

**What it proves.** A fixture (`scripts/fixtures/toolchain-check`) is built with `--tools store` using *only* store packages (BusyBox, make, binutils, gcc, musl, linux-headers). In that sandbox the store's gcc compiled and ran C and C++ programs, dynamic and static, including STL containers and a thrown-and-caught exception: musl's strerror text confirms the libc, the dynamic loader is `/lib/ld-musl-x86_64.so.1`, static binaries have no interpreter, and the host's libc and gcc are absent. The script also checks that every compiler binary and tool is fully static.

**Design.**
- The compilers are **static** (linked with the host's glibc), so they start in a sandbox that contains no libc. They target musl and are configured `--with-sysroot=/`, with unprefixed names (`gcc`, `cc`, `g++`, `ld`, `as`, ...) beside the prefixed ones, so a build sees an ordinary native toolchain. `gcc` runs against `/usr/include` and `/usr/lib` from the `musl` and `linux-headers` packages, which are its run-time dependencies, so installing gcc brings its own sysroot.
- The `gcc` recipe is one build in four stages, because nothing else can supply tools: (1) cross binutils (build-time only), (2) a musl sysroot and kernel headers (build-time only), (3) gmp, mpfr and mpc unpacked inside the gcc tree, (4) gcc. The standalone `binutils` package is a separate recipe.
- Disabled because they are slow, need shared libraries, or do not work on musl: LTO and plugins, sanitizers, OpenMP, libitm, libquadmath, libssp, libvtv, shared libraries. libatomic is built.
- Sources (downloaded 2026-11-09 from ftp.gnu.org: binutils 26.9 MB, gcc 88.0 MB, gmp, mpfr, mpc): hashes are **pinned from the first download (trust on first use)**. ftp.gnu.org publishes only PGP signatures, which were not checked. (gcc.gnu.org publishes a checksum for gcc, which was not used because it is a different site from the one approved for download.)

**Reproducibility, measured.** Two independent runs gave byte-identical archives and identical ids for both packages. This needed a fix: gcc's inline cross-binutils lacked `--enable-deterministic-archives`, so 7 of the 1004 files (all `.a` archives, e.g. libgcc.a and libstdc++.a) differed only in an embedded timestamp; the 304 objects inside libgcc.a were identical. As before, the id does not include the host compiler's version.

**Problems found while building it** (each now has a fix or a guard):
- `libtool` interprets a single-dash `-static` and still produces dynamic programs; `--static` (two dashes) is needed. The first binutils build was dynamic and only ran because the host has glibc. Both recipes now fail the build if any tool has a program interpreter.
- `tar` run as the sandbox's fake root tries to `chown` to the tarball's recorded uid, which is unmapped, and fails: use `--no-same-owner`.
- Host tools such as `file` and `pkg-config` could not find libraries that live outside the loader's default directories. The sandbox now binds the host's `/etc/ld.so.cache` read-only in host-tools mode.
- gcc installs 64-bit target libraries in `lib64` by default, and searches there; musl uses `lib`. Fixed the way musl distributions do: `m64=../lib64` becomes `m64=../lib` in `gcc/config/i386/t-linux64` before configuring (with a guard that the edit applied).
- Two packages providing the same file: BusyBox's `strings` applet clashed with binutils, and `linux-headers` clashed with musl on `scsi/{scsi,scsi_ioctl,sg}.h` (a glibc host's `/usr/include/scsi` is glibc's, not the kernel's; my gcc recipe's inline sysroot also copied it over musl's). The profile builder refused to merge them, which is how both were found. BusyBox drops the applet and `scsi/` is no longer copied.
- Preflight order mistakes in my own scripts: a repository that did not exist yet, and an over-strict expectation of `gcc --version` output.

**Limits / next.**
- **musl only.** The toolchain targets musl. A glibc toolchain needs glibc built from source as a sysroot and is not done, so the glibc ISO still has no compiler in the store.
- **Still impure.** These bootstrap packages were compiled by the host's gcc 16, which is why they are named `-stage1`: they exist only to build the real toolchain (stage 2, below), and their `-stage1` names let a stage-2 build use them as tools while depending on the stage-2 packages at run time.
- Compilers are glibc-static binaries, hence large (gcc archive 66 MB, binutils 21 MB). No shared libstdc++ or libgcc_s.
- The repository index is still unsigned.

## Self-hosting: the base and the toolchain rebuild themselves (implemented: `recipes/` (plain names), `scripts/bootstrap-selfhost.sh`, `scripts/compare-trees.sh`)

`BASE_REPO=repo/stage0-<stamp> STAGE1_REPO=repo/stage1-<stamp> scripts/bootstrap-selfhost.sh` takes about 27 minutes on 8 cores. It needs no downloads once the sources (including the kernel) are cached. Everything it builds runs in a sandbox with **no host tools** (`--tools store`).

**The six real packages.** `linux-headers` (generated from the kernel 6.12 source), `musl`, `binutils`, `busybox`, `make` and `gcc`, all native on musl, static, with the plain names. The host-built ones are `*-stage0` (musl, linux-headers, busybox, make) and `*-stage1` (binutils, gcc); they exist only to bootstrap.

**Generations.** The recipes in `recipes/` describe the real packages. The script derives each generation by renaming (`name`, `build` and `run` lines only), so generations never clash by name:

| Generation | Built by | Names |
|---|---|---|
| stage2 | the stage-0/1 bootstrap packages | `*-stage2` |
| stage3 | stage2 | `*-stage3` |
| real | stage3 | plain |

Distinct names are required, not cosmetic: dependencies are resolved by name, a build's tool profile includes its tools' run-time dependencies, and two packages providing the same file cannot be merged. A stage-2 `gcc` must use the stage-1 compiler as a tool while depending on stage-2 packages at run time; with one name for both that cannot be expressed.

**Why three generations.** A static binary contains libc code. Stage2 links the stage-0 musl, which the *host's* gcc compiled; stage3 links the musl that Novara's own compiler built. So stage3 is the first generation with no host-compiler output in it, and the fixed point appears one generation later: stage3 and the real packages must be identical.

**Results** (all six packages, file by file, nothing different or missing): linux-headers 2001 files, musl 234, binutils 172, busybox 401, make 1, gcc 1012. The real packages also compile and run C and C++ (dynamic and static, with exceptions) in a store that holds *only* those six: the repository passed to that build contains nothing else, and the script fails if a `-stage` package is in the store.

Stage2 differs from stage3, as predicted, and only where libc code is linked in: `libc.so` (it links `libgcc.a`, which differs between the stage-1 cross build and a native one), busybox, make, the 16 differing binutils programs and the 18 differing gcc programs. Static libraries and the (text) kernel headers are identical.

**Kernel headers.** Generated from `linux-6.12.tar.xz` (141.1 MB, downloaded 2026-11-09 from cdn.kernel.org); its SHA-256 matches the one kernel.org publishes (in a PGP-signed list; the signature itself was not verified). `make headers_install` ends with an `rsync` the sandbox does not have, so the recipe runs `make headers` (which produces the same tree) and copies it. Generating them takes about a minute, including unpacking the tarball with the pure-Rust xz decoder.

**Debug info must not be recorded** (a lesson from comparing stage2 and stage3 of gcc alone). GCC finds its headers relative to the real location of its executable, which in a store contains the package id, different at every generation; with `-g` those paths are recorded in object files, and `cc1`'s embedded `executable_checksum` (an MD5 over its objects) then differs although the code is identical. A controlled test showed objects without `-g` are byte-identical across compilers of different generations. The gcc and make recipes therefore build with `-O2` and no `-g` (the packages are stripped anyway); the compiler ships without debug symbols.

**Problems found along the way** (each now has a fix or a guard):
- `make headers_install` needs `rsync` (see above).
- A comparison piped into `tail` once printed "IDENTICAL" with files different: a pipe hides the exit status. The comparison script's status is checked directly.
- My own scripts had three bugs the first runs exposed: a fixture rewrite that also changed the recipe's `libc` line, a log path containing a slash, and a repository passed before it existed.

**Limits / next.**
- **The root of trust is still the host's gcc**, through `gcc-stage1`: its output is what the chain is built from. The fixed point shows the toolchain reproduces itself independent of what compiled it, not that the original compiler was trustworthy. Stage-0/1 are also impure in other ways (stage-0 `linux-headers` copies the host's headers).
- **musl only.** A glibc toolchain needs glibc built from source and is not done; the glibc ISO still has no compiler in the store.
- No shared libraries (static libstdc++ only), no LTO, plugins or sanitizers, and no debug symbols in the compiler.
- Source hashes are pinned from the first download, except BusyBox and the kernel, which match checksums the upstream sites publish (their PGP signatures were not verified). The repository index is still unsigned.

## Signed repositories (implemented: `forge-store` `sign.rs`/`repo.rs`, `forge key`, `forge repo`)

A repository index says which archives exist and what they hash to, so whoever can
write the index controls what gets installed. Indexes are therefore signed, and
unsigned or untrusted ones are refused.

- **Format.** `index.toml.sig` is a detached ed25519 signature over the exact bytes of
  `index.toml`, prefixed with the domain string `forge-index-v1\n`. The index is read
  once, verified, and parsed from those same bytes (no check/use gap). Verification
  uses `verify_strict` (rejects weak keys and malleable signatures). A key id is the
  sha256 of the public key.
- **Trust.** A keyring is a directory of `*.pub` files: `/etc/forge/keys` for
  `forge apply`, `~/.config/forge/keys` for `forge build`; `--keys DIR` overrides.
  Fail closed: unsigned, unknown-key and bad-signature repositories are errors.
  `--allow-unsigned` turns the check off, with a warning.
- **Rollback.** The index carries a signed `serial`, bumped on every publish or sign.
  `forge apply` remembers the highest serial seen per repository in `state.toml` and
  refuses a lower one.
- **Keys.** `forge key generate NAME` writes `NAME.key` (mode 0600, never overwritten;
  loading refuses group/world-readable keys) and `NAME.pub`. `forge key trust` adds a
  public key to a keyring. `forge store publish --sign KEY` and `forge build --publish
  --sign KEY` sign; an unsigned existing repository is only signed by an explicit
  `forge repo sign`, and a tampered signed index is never signed over.
  `forge repo verify` checks a repository.
- **Scripts.** The bootstrap scripts use a project-local development key in `keys/`
  (git-ignored, created on first use) and trust only it.

Limits, stated plainly:
- The keyring is the root of trust and must arrive over a trusted channel (e.g. baked
  into the ISO). `forge strap` does not install trusted keys yet.
- No key expiry, revocation list or threshold signing: remove the `.pub` to revoke.
- No freshness protection beyond the serial: an attacker can serve an old index to a
  machine that has never seen a newer one, or freeze a machine at its current serial.
- Signing covers the index, which pins every archive by hash; archives are not
  separately signed.
- The chain's root of trust is still the host gcc, via `gcc-stage1`.

## The live ISO (implemented: `forge-iso`, `forge iso`, `recipes/linux`, `scripts/build-iso.sh`, `scripts/test-iso-qemu.sh`)

`BASE_REPO=repo/selfhost-r2 scripts/build-iso.sh` builds `novara-live.iso` (35 MiB with the default packages).

- **Contents.** `forge iso --repo R... [--package NAME...]` resolves the packages (default
  `linux busybox musl`) and their dependencies in signed repositories (same trust rules as
  `forge build`/`apply`), installs them into a scratch store (every archive hash-checked),
  merges the trees (a file in two packages is an error), writes the live `/etc`
  (root/root, getty on tty1 and ttyS0, BusyBox init), and builds:
  - `live/rootfs.squashfs` (zstd, all files owned by root, timestamps zeroed),
  - `boot/initramfs.img` (gzip cpio written by `forge-iso` itself: BusyBox, `/init`,
    `/dev/console` and `/dev/null` as archive entries),
  - `boot/vmlinuz`, Limine's BIOS and UEFI files, `boot/limine/limine.conf`.
  `xorriso` makes a hybrid image (El Torito BIOS + UEFI, GPT/MBR, so it also works written to
  a USB stick) and `limine bios-install` finishes it.
- **Boot path.** Limine → kernel + initramfs → `/init` scans block devices for an iso9660
  filesystem containing `live/rootfs.squashfs` (retries for 30 s, then drops to a shell),
  mounts it, stacks a tmpfs overlay (writable, nothing persists), moves the medium to
  `/medium`, and `switch_root`s to BusyBox `init`.
- **Kernel** (`recipes/linux`, 6.12). x86_64 defconfig + `kvm_guest.config`; everything needed to
  reach the root (SATA/AHCI, NVMe, USB storage and host controllers, MMC, virtio, iso9660,
  squashfs, overlayfs, loop, btrfs, vfat, EFI framebuffer, serial console) is **built in**, so the
  initramfs needs no modules. The kernel is also in the squashfs, in `/boot`, which is where
  `forge strap` looks for the kernel to install. **Impure**: built with `--tools host` (host gcc,
  flex, bison, bc, perl, libelf, OpenSSL headers), like stage 0. gcc 15 needs `CC="gcc -std=gnu11"`
  (C23 breaks the 6.12 EFI stub) and `-Wno-error` (new warnings in old ACPI headers). About 10 minutes.
- **Needs on the host** to build the ISO: `xorriso`, `mksquashfs` (with zstd), `openssl`, `limine`.

Not done yet (the live system cannot install a system until these exist):
- **No packages for `forge strap`'s tools**: `rsync`, `mkfs.btrfs`/`btrfs`, `cfdisk`/`sfdisk`/`blkid`
  (util-linux), `limine`, `efibootmgr`, shadow-utils (`useradd`, `chpasswd`). The live system has
  BusyBox, musl and the kernel only.
- **`forge` itself is not in the ISO**: it is built for the host (glibc, dynamic). It needs a musl
  static build, which means the Rust toolchain as a package or a musl target.
- **No OpenRC** (BusyBox init stands in), no `forge kernel` (the kernel is a fixed recipe), the
  default kernel is not a "full compatibility" one (defconfig only), and no installed trusted keys
  on the ISO (see signing).
- **Never booted.** There is no QEMU on the author's machine, so the image was checked structurally
  only (boot records, partition table, squashfs and initramfs contents, 15 unit tests). Run
  `scripts/test-iso-qemu.sh novara-live.iso` (BIOS; set `OVMF=` for UEFI) on a machine with QEMU: it
  boots the ISO on the serial console, waits for the login prompt, logs in as root/root and runs a command.

## Strap tools on the live system (implemented: `recipes/{zlib,zstd,lzo,ncurses,pkgconf,util-linux,rsync,btrfs-progs,shadow,limine,efivar,efibootmgr}`, `scripts/build-tools.sh`, `scripts/package-forge.sh`)

`BASE_REPO=repo/selfhost-r2 scripts/build-tools.sh` builds everything `forge strap` runs, with the
self-hosted toolchain only (`--tools store`), into the signed repository `repo/tools`;
`scripts/package-forge.sh` adds forge itself. The ISO (`scripts/build-iso.sh`, 46 MiB) now holds
`linux busybox musl forge rsync util-linux ncurses btrfs-progs shadow limine efibootmgr`.

- **Static everywhere.** Libraries are static-only packages (`zlib`, `zstd`, `lzo`, `ncurses`, `efivar`,
  and util-linux's `libuuid`/`libblkid`), used only as build dependencies; every tool is linked
  statically, so the live image needs no shared libraries. `readelf` checks in each recipe's install step
  refuse a dynamic result.
- **What each gives.** util-linux: `cfdisk`, `fdisk`, `sfdisk`, `blkid`, `wipefs`. btrfs-progs:
  `mkfs.btrfs`, `btrfs`, `btrfstune` (the `.static` targets; no udev, no ext4 conversion). shadow:
  `useradd`, `groupadd`, `usermod`, `chpasswd`, `passwd` (no PAM; `login`, `su`, `newgrp` are dropped, so
  BusyBox stays the login program; ships `/etc/login.defs`). limine: the `limine` tool built from source plus
  the release's prebuilt boot files in `/usr/share/limine` (where strap looks). efibootmgr: the binary only
  (it is optional for strap: failure is a warning).
- **BusyBox applets yield.** The image builder lets a real package's file replace a BusyBox applet link
  (and never the other way round), so `/usr/sbin/sfdisk` etc. win without changing the BusyBox recipe
  (which would restart the self-hosting chain). Two real files at one path are still an error.
- **ncurses ships no programs** (`clear`, `tput`, `tic`... clash with BusyBox and nothing runs them):
  libraries, headers and the terminfo database `cfdisk` needs.
- **forge itself is impure**: built with the host's Rust toolchain, `-C target-feature=+crt-static`
  (static glibc, runs on a musl system), packed as an ordinary package. A musl build needs the Rust
  toolchain as a package first.
- **Verified** by running each tool inside the assembled live root (a rootless `unshare -Ur chroot`):
  versions and usage of all of them, and `groupadd`/`useradd`/`chpasswd --root` against a scratch root
  producing `$6$` (SHA-512) hashes. `forge strap` itself has not been run on this root: that needs root
  and a disk (`scripts/test-strap-loop.sh` covers strap's commands against a loopback image on the host),
  and the ISO has not been booted.

Things found on the way, worth remembering:
- Source tarballs may list a file twice (btrfs-progs 6.11 does). The extractor now lets a later regular
  file replace an earlier one it wrote itself, as GNU tar does, and nothing else.
- shadow with only `SHA_CRYPT_MIN_ROUNDS` set asks for `rounds=999999999`, which musl refuses, so every
  `chpasswd` fails with "crypt method not supported by libcrypt?". The shipped `login.defs` sets neither.
- util-linux 2.40 has no `--enable-cfdisk`/`--enable-sfdisk`: they come from `--enable-fdisks`.
- btrfs-progs' static build requires a real `pkg-config` (hence `pkgconf`), and efivar's Makefile needs
  `TOPDIR` when run from `src/`.

Still missing: OpenRC, a trusted-key keyring on the ISO, the Rust toolchain as a package, and a booted test.

## Installing the live system for real (`scripts/test-strap-live.sh`)

`sudo scripts/test-strap-live.sh [novara-live.iso]` extracts the live root from the ISO, makes a 2 GB loopback
disk, and runs the packaged `forge strap --uefi` inside a chroot of that root (private mount namespace),
so rsync, mkfs.btrfs, limine and the rest are the packaged ones. It then checks subvolumes, fstab,
`limine.conf`, kernel, EFI loader, init and forge on the result and, if QEMU and `OVMF=` are available,
boots the installed disk to a login prompt. **Passed on the author's machine on 2026-11-09 up to the boot step** (the real strap, with the packaged tools, produced the expected subvolumes, fstab, `limine.conf` and EFI System Partition); the QEMU boot was skipped (no QEMU), so the installed system has still never been booted.

Writing it showed what an installed system needs that the live one did not, now fixed:
- **No initramfs.** The Novara kernel has its root drivers built in, but a kernel cannot resolve
  `root=UUID=` without an initramfs. The initramfs is now optional end to end (`BootInfo.initramfs`
  empty = none: no `module_path` line, in strap and in generations), and then strap writes
  `root=PARTUUID=<root partition>` (read with `blkid`). An initramfs named with `--initramfs` must exist.
- **rsync without ACLs.** The rsync package has no ACL support, so strap uses `-aHX`, not `-aHAX`
  (a fresh squashfs root has no ACLs to keep).
- **`/medium` is not copied.** It is where the live system mounts the ISO.
- **rcS mounts what the initramfs used to.** `/proc`, `/sys`, `/dev/pts`, `/dev/shm`, `/run`, `/tmp` and
  fstab (`mount -a`, with `/etc/mtab` → `/proc/self/mounts`), each only if not already mounted, so the same
  script serves the live and the installed system.

## First boot on real hardware: black screen (fixed)

The first live USB booted to Limine, then showed a black screen. Cause: the kernel had `DRM_SIMPLEDRM`
but no framebuffer console (`FB`, `DRM_FBDEV_EMULATION` and `FRAMEBUFFER_CONSOLE` were off): `scripts/config`
had set `FRAMEBUFFER_CONSOLE`, and `olddefconfig` silently dropped it because its dependency was not yet
enabled. On UEFI there is no VGA text mode, so nothing drew text. Fixes (`linux` 6.12_r1):
- `DRM_FBDEV_EMULATION` first, then `FRAMEBUFFER_CONSOLE` and fonts in a second pass;
- the recipe now **fails the build** if any option the live system needs did not survive `olddefconfig`;
- the live command line is `console=ttyS0,115200 console=tty0` (the last `console=` is `/dev/console`, which
  is where `/init` writes, so it must be the screen).
Graphics: Intel (`i915`) is built in; AMD and NVIDIA have no native driver in this kernel (no initramfs to load
modules from), so they run on the firmware's framebuffer via simpledrm: slow, no acceleration, but a console.

## OpenRC as init (implemented: `recipes/{libcap,python,ninja,meson,openrc}`, `crates/forge-iso` live.rs)

The live system and installs made from it now boot with **OpenRC 0.64.1** (`openrc-init` as PID 1, not the
sysvinit mode), replacing BusyBox init. **Written and checked by running OpenRC inside the live root
(`openrc --version`, `rc-update show`); never booted** — see the rescue entry below.

- **Build chain.** Current OpenRC builds only with meson, so the store gained `python` 3.12.16 (trimmed; no
  ssl/sqlite/ctypes), `ninja` 1.13.2 (static), `meson` 1.12.1 (pure Python, a wrapper in `/usr/bin/meson`) and
  `libcap` 2.78 (library only, static and PIC: the programs need bash). `zlib` is now PIC so Python can
  link it. OpenRC is configured without PAM/SELinux/audit, `-Dsysvinit=false`, branding "NovaraSYS". It
  installs its programs in `/sbin` and `/bin`, configuration in `/etc`, helpers in `/usr/libexec/rc`.
- **Runlevels** (written by the ISO builder, replacing the package's defaults, which include fsck, hwclock
  and swap): sysinit `devfs dmesg sysfs`; boot `procfs root hostname localmount bootmisc loopback`; default
  `agetty.tty1 agetty.ttyS0 local`; shutdown `killprocs mount-ro`. They are plain symlinks in
  `/etc/runlevels/<level>/`, exactly what `forge apply` already creates for declared services.
  `hostname` reads `/etc/hostname`, which strap and `forge apply` both write.
- **Terminals** are `agetty` services (`agetty.tty1`, `agetty.ttyS0` linked to the stock template, settings
  in `/etc/conf.d/`); `util-linux` now builds `agetty` and links it at `/sbin/agetty`, where the template
  looks. `login` stays BusyBox's.
- **Shutdown.** `/sbin/reboot` and `halt` are small scripts calling `openrc-shutdown`, and `shutdown [-h|-P|-p|-H|-r] [now|+min]` powers off (reboots with `-r`); there is no `poweroff` (BusyBox's link is removed):
  BusyBox's would signal PID 1 the BusyBox way.
- **Fallback.** Without the `openrc` package in the image the builder falls back to BusyBox init and the
  old `rcS`.
- **Rescue.** The live Limine menu has a second entry, "NovaraSYS live (rescue shell, no init system)", which
  adds `init=/bin/sh`; `/init` in the initramfs now honours `init=` from the kernel command line. On an
  installed system (no initramfs) press `e` at the Limine menu and add `init=/bin/sh`.
- **Known gaps.** `localmount` runs BusyBox's `mount -at`, which may not understand every option OpenRC passes
  (`-O no_netdev`): extra mounts could fail while `/` and `/boot` are unaffected. No `hwclock`/`seedrng`/`sysctl`/
  `modules` services yet. A busybox `start-stop-daemon` is replaced by OpenRC's.

## Networking, wired (implemented: `dhcp` OpenRC service, `linux` 6.12_r2); Wi-Fi not done

- **DHCP.** `/etc/init.d/dhcp` (in the `default` runlevel, provides `net`) runs BusyBox `udhcpc` on every
  physical wired interface (interfaces with a `device` link that are not wireless; override with `interfaces=`
  in `/etc/conf.d/dhcp`). `/usr/share/udhcpc/default.script` applies the lease: address, default routes
  (one per router, increasing metric) and `/etc/resolv.conf` (search domain and nameservers). Run it with
  `rc-service dhcp restart`; disable with `rc-update del dhcp default` (`forge apply` manages the same links).
- **Kernel drivers** (6.12_r2, all built in, since there is no module loading): Intel e1000/e1000e/**igb/igc**,
  Realtek r8169/8139, Broadcom **tg3 (TIGON3)**, Atheros **alx/atl1c**, virtio, and USB Ethernet (**r8152**, CDC
  Ethernet/NCM, AX8817x/AX88179). The kernel recipe's survival check caught a wrong symbol name (`TG3` is
  `TIGON3`) before it produced a kernel without the driver.
- **Tested**: the lease script's exact commands and resulting `resolv.conf` against a recording stub of `ip`; the
  service's syntax. **Not tested**: real DHCP (this build machine's sandbox has no veth/dummy/tun, and BusyBox
  `ip` cannot create interfaces) and any real NIC.
- **Wi-Fi, not done, and bigger than it looks**: the kernel has the 802.11 core but no Wi-Fi driver; drivers
  need firmware blobs (linux-firmware, hundreds of MB), probably modules (no autoloading exists yet), and a
  userland to join networks (wpa_supplicant or iwd, with libnl/OpenSSL or ell). USB tethering from a phone
  works already through the CDC/RNDIS-style Ethernet drivers above only for CDC Ethernet/NCM phones.

## Wi-Fi (implemented, untested on hardware: `linux` 6.12_r3, `wpa_supplicant`, `linux-firmware-wifi`, `wireless-regdb`)

Goal: work on whatever Wi-Fi hardware the kernel has a driver for, not on one chip. The ISO is now 372 MiB
(the firmware is 671 MB unpacked), and an installed system carries the same.

- **Drivers.** 136 kernel modules: every Wi-Fi vendor the 6.12 tree has (Intel iwlwifi, Atheros ath5k/9k/10k/11k/12k,
  Broadcom brcmsmac/brcmfmac, Realtek rtl8xxxu/rtlwifi/rtw88/rtw89, MediaTek mt76 family, Ralink rt2x00, Marvell
  mwifiex/libertas/mwl8k, RSI, ZyDAS...). They are **modules**, not built in: a built-in driver probes before the
  root filesystem (and `/lib/firmware`) exists and cannot load its firmware. The recipe prints the Wi-Fi options
  that do not exist in this kernel (MT76x0U/x0E/x2E/x2U) and fails the build if the main ones are missing.
  `/lib/modules` links to `/usr/lib/modules`, where BusyBox `modprobe` looks.
- **Autoloading.** The `coldplug` OpenRC service (boot runlevel) reads the `modalias` of every device in
  `/sys/devices` and runs `modprobe` on each (BusyBox resolves aliases through `modules.alias`), then loads the
  modules named in `/etc/modules-load.d/*.conf` (what `forge apply` writes). OpenRC's own `modules` service is not
  used: it passes `modprobe` options BusyBox lacks. Only *coldplug*: a device plugged in after boot is not
  matched to a driver until `rc-service coldplug restart`.
- **Firmware** (`linux-firmware-wifi`): from the linux-firmware snapshot, the directories of every wireless vendor
  plus Realtek Ethernet; graphics, sound, DVB, storage are left out. `wireless-regdb` supplies
  `regulatory.db(.p7s)` so the kernel knows each country's allowed channels (without it only the conservative
  "world" rules apply).
- **Joining a network.** `wpa_supplicant` 2.12 (static, nl80211) with its own TLS: **WPA2 personal, WPA3 transition
  mode and WPA-Enterprise (PEAP/TTLS/MSCHAPv2/GTC/TLS) work; WPA3-only (SAE) and OWE do not**, because the built-in
  crypto lacks what they need (OpenSSL needs Perl to build; wolfSSL is the other option). The `network` service
  replaces `dhcp`: a watcher process rescans `/sys/class/net` every 3 seconds and, for each physical interface,
  brings it up, starts `wpa_supplicant` on wireless ones and `udhcpc` on all (a Wi-Fi interface appears a second or
  two *after* its driver loads, so a one-shot start would miss it). It unblocks soft-blocked radios.
  The user-facing command is `wifi scan | connect SSID [passphrase] | status | disconnect`; `connect` appends a
  network block (from `wpa_passphrase`) to `/etc/wpa_supplicant/wpa_supplicant.conf` and reconfigures.
- **New packages on the way**: `m4`, `bison` and `flex` (libnl's build needs them), `libnl` (static).
- **Not tested**: any real Wi-Fi hardware, and the `network`/`coldplug` services booting (this build machine has
  no network devices and cannot boot the ISO). The scripts pass `sh -n` in the live root; `wpa_passphrase` and
  `wpa_supplicant` run there.
- **Not done**: Bluetooth, WPA3-only, hotplug beyond coldplug, a TUI/`forge net` front end, Wi-Fi in the declarative
  system.toml, DNS beyond what DHCP provides.

## Full-compatibility kernel (implemented: `linux` 6.12_r4; the `forge kernel --full` of the design, as a recipe)

The live ISO's kernel is meant to run on hardware nobody has tried yet, so it has **5,363 modules** (316 MB
stripped; the ISO is 431 MiB). It is the stable defconfig base, with the boot-critical drivers built in as
before, plus every driver `allmodconfig` offers as a module:
- The recipe generates an allmodconfig out of tree, takes its `=m` symbols (minus tests, samples, debug aids
  and `IKHEADERS`, which needs `cpio`), switches on the menu gates that hide drivers on defconfig
  (`*_TOPLEVEL`, `NETFILTER_ADVANCED`, `MEDIA_*_SUPPORT`, `USB4`), and repeats until the count stops growing.
  It deliberately does not use allmodconfig itself: that turns on KCOV, UBSAN, KCSAN, LOCKDEP and GCOV, which
  make a kernel unusably slow.
- **Left out on purpose: the big GPU drivers (amdgpu, radeon, nouveau, xe).** Without their firmware (hundreds
  of MB) they blank the screen on probe, because they take the display from the firmware framebuffer before
  loading it. The live system uses simpledrm and the built-in i915; a graphical install needs those drivers
  and their firmware added.
- Still built in (no modules can load before the root is up): storage, USB host, filesystems for the medium,
  common Ethernet, input, the framebuffer console.
- `coldplug` loads the modules for the hardware found (BusyBox's small `modprobe` resolves PCI/USB/ACPI aliases
  through `modules.alias`; checked against real aliases: Intel iwlwifi, Realtek rtw89 with its dependency
  chain). `make -k` does not rescue a failed object (modpost needs them all), so every module that fails
  to build with gcc 15 must be excluded by hand; only IKHEADERS did.
- Build time, 8 cores: roughly 35 minutes.
- An installed system gets the same 316 MB of modules; the design's `forge kernel --auto` (localmodconfig from
  the machine's own hardware) is what will shrink that, and is not written.

## Kernel 7.3 instead of 6.12 (the Realtek 2.5 Gb Ethernet that 6.12 could not drive)

The first real-hardware test of wired networking showed no Ethernet interface on a board with a Realtek
`10EC:8125` controller, although `r8169` was built in. Kernel 6.12's `r8169` knows only the RTL8125A and
RTL8125B revisions; **RTL8125D arrived in 6.13, RTL8125BP in 6.14 and RTL8125K in 7.0**, and recent boards carry
those. (The firmware package already had files for them; the driver did not recognise the chips.) A long-term
kernel that old will keep missing new chips, so `recipes/linux` now builds the current stable **7.3.3**
(checksum compared with kernel.org's `sha256sums.asc`; same configuration, which passed all the survival checks
unchanged; 5,659 modules; 438 MiB ISO). The repository is still named `repo/kernel-6.12`. The user-space
`linux-headers` package stays at 6.12 on purpose: the kernel's user-space ABI is backward compatible.
**The cause was inferred from the kernel source and the PCI ID, not from a `dmesg` line; not yet verified on that
board.** A stable kernel is not a long-term one: the design question of 6.18 LTS versus staying current remains.

## `forge kernel` (implemented: `crates/forge-kernel`, `forge kernel list|build|install`)

The genkernel of the design, minus `initramfs` (the Novara kernels carry the drivers needed to reach the root).

- **`forge kernel list`**: kernel.org's current releases (mainline, stable, long-term).
- **`forge kernel build`** picks a kernel (`--version 7.3.3`, or `--series stable|longterm`; release candidates are
  refused), downloads the tarball and checks it against kernel.org's `sha256sums.asc` (the PGP signature on that
  file is **not** verified: there is no gpg here, so this catches corruption and a tampered mirror, not a
  compromised kernel.org), unpacks it, configures it and builds it into a staging directory
  (`<work>/stage-<version>`: `boot/vmlinuz-REL`, `boot/config-REL`, `usr/lib/modules/REL`). Configuration modes:
  - `--auto` (default): start from the running kernel's configuration and drop every module that is not loaded now
    (`make localmodconfig` on `/proc/modules`, or on a copy saved on the target machine with `--loaded-modules FILE`),
    then re-assert what a Novara system needs built in. Plug in the
    devices you use first, or name modules with `--also-keep`. Checked on the author's machine: 5,484 → 202 modules, 52 MB
    against 316 MB.
  - `--full`: every driver as a module: the live ISO's kernel. `recipes/linux/full-config.sh` is the one script behind
    both this and the `linux` recipe, and `forge-kernel` embeds it, so they cannot drift apart.
  - `--menuconfig` (starting from `--base-config`) and `--config FILE`: your choice. These are only *checked* for the
    essentials afterwards, never overridden; a configuration that lost a boot-critical driver fails the build.
- **`forge kernel install DIR`** (root): snapshots a generation first (`before kernel REL`, so you can roll back to the
  system and kernel you had), copies modules (moved into place, with `/lib/modules` linked) and kernel/config, and
  rewrites `limine.conf` to boot the new kernel with no initramfs. Older kernels are never removed: each generation
  pins its own. `--no-generation`, `--no-limine`, `--force` and path options exist. `build --install` does both.
- **Needs on the machine**: make, gcc, ld, bc, flex, bison, perl, xz, tar and libelf headers (it checks first).
  **bc, perl and libelf are not Novara packages yet**, so a freshly installed Novara system cannot build kernels
  until they are. (The toolchain, flex, bison and m4 are.)
- **Tested**: 14 unit tests (release choice, URLs, checksum parsing, command order per mode, staging, install,
  refusal cases), 5 CLI tests, and a real `build --auto` here. **Not tested**: `install` end to end (needs root and a
  Novara btrfs system), `--menuconfig`, and `--series longterm` on a real download.

## Package repositories on GitHub; `forge pkg add` works (implemented, published and tested against GitHub)

Packages are not installed on the system at install time: the system fetches them from the project's
repository, `https://github.com/noxilith/NovaraSYS`.

- **Layout.** Git holds the project: recipes, scripts, `crates/`, `DESIGN.md` and the release **public** key in
  `distro/keys/`. The binary packages live in a **GitHub Release** called `repo` (git refuses files over 100 MB;
  the firmware archive is 402 MB, the kernel 145 MB; a release asset may be 2 GB). That release is a flat directory:
  `index.toml`, `index.toml.sig` and every archive. Systems use it as
  `https://github.com/noxilith/NovaraSYS/releases/download/repo` (`forge_schema::DEFAULT_REPO_URL`), which a
  starter `system.toml` from `forge strap` already lists under `repos`.
- **Trust.** Everything is checked after download, so the transport needs no authentication: the index against the
  keys in `/etc/forge/keys` (the ISO ships the release key there), each archive against the hash in that index, and
  the index's signed serial against the highest one seen (a replayed old index is refused). That is why BusyBox's
  `wget`, which does not verify certificates, is an acceptable fallback where there is no `curl` (the live system
  has none; checked: it reaches GitHub over HTTPS and follows the redirect to the asset store). A man in the middle
  can only withhold packages.
- **Release key.** `keys/release/novara-release.key` (git-ignored, mode 0600; key id
  `a0178076c79b3c16…`), public half `distro/keys/novara-release.pub`. **Back the secret key up, and never commit
  it:** whoever holds it can publish packages every Novara system trusts, and losing it means shipping a new key
  with a new ISO. There is no revocation or expiry yet.
- **Publishing.** `forge repo assemble DEST --from REPO... --key KEY` builds one clean repository from the build
  outputs (newest version of each real package, no `-stage` bootstrap packages, every run-time dependency present),
  signed with the release key; `scripts/publish-release.sh` runs it, checks the result verifies against
  `distro/keys`, shows what it will upload and asks, then uploads archives first and index plus signature last
  (so no client sees an index naming a missing file). Needs the `gh` CLI. Today's release: 32 packages, 661 MB.
- **`forge pkg add|remove NAME`** edits the declaration, then applies it: as root it edits `/etc/forge/system.toml`.
  It first plans the edited declaration, so a package that does not exist, an untrusted repository or an
  unreachable network fail **before** the file is written; then it shows the plan, asks, installs into the store
  (`/forge/store`), links the system profile and makes a generation. `--no-apply` only edits. `pkg add --root DIR`
  works against a scratch root. A user (not root) edits `~/.config/forge/user.toml`, which `forge apply user`
  does not apply yet.
- **`PATH`.** `/etc/profile` puts `/forge/profiles/system/current/{usr/sbin,usr/bin,sbin,bin}` first, so declared
  packages win over the base system, and sources `/etc/profile.d/*.sh` (where `forge apply` also writes it).
- **Tested.** 4 unit tests of the remote transport and verification, 4 of `forge apply` against a remote
  repository (index and signature only at plan time; the archive only when applying; tampered index refused;
  wrong archive installs nothing; serial remembered under the URL), 6 CLI tests of `pkg add/remove`, and by hand
  an end-to-end `pkg add rsync` and `pkg add python` over HTTP (BusyBox `httpd`) into a scratch root, trusting only
  the public key. **Published 2026-11-09** (34 files, 661 MB, serial 32, release `repo` of noxilith/NovaraSYS), and
  installed from GitHub into a scratch root both with `curl` (rsync) and with only BusyBox `wget` (util-linux): it
  connects, follows the redirect to the asset store, verifies and installs. **Not tested**: on a booted installed
  system, and the live system's TLS handshake on other networks.
- **Limits.** One release holds one version of each package; there is no per-package history to roll a repository
  back to (the client's generations are the way back). `pkg add` cannot yet pull in build tools for `forge kernel`
  automatically.

## Administrators, groups and `sudo` (implemented, published; never run as a non-root user)

A user created by `forge strap` had only the `forge` group, and the system had no `sudo` at all (I had been writing
`sudo forge ...` in instructions that could not work on an installed system). Fixed:

- **Groups.** The base system now has the usual groups with fixed ids (root, daemon, tty, disk, wheel 10, cdrom,
  audio 18, video 27, input 97, users 100, nogroup). The installer's user joins `wheel` (administrator),
  `forge` (package store), `audio`, `video`, `input` and `users`; strap creates each group first with `groupadd -f`,
  and the starter `system.toml` lists them. `audio`/`video`/`input` give nothing yet: nothing assigns those groups to
  device nodes (no udev or mdev rules in the base), so for now they only exist.
- **`sudo` (1.9.17p2), static, no PAM** (it checks `/etc/shadow` itself), the sudoers plugin linked in. `/etc/sudoers` is
  written by the image builder: `root` and `%wheel` may do anything, mode 0440; `secure_path` includes the forge profile.
- **setuid lives in the base, never in the store.** The store strips setuid/setgid when it unpacks (a package must not be
  able to ship a root-owned privileged binary), so the `sudo` package ships without the bit. The image builder makes
  exactly `usr/bin/sudo` and `usr/bin/passwd` setuid (`SETUID_FILES`) on the copies the image holds, refuses a symlink
  in that list, and touches nothing else. A package installed later cannot add to the list. Consequence: sudo is part of
  the base image; updating the package does not update the base copy. `su` is deliberately not setuid (`sudo -i` instead).
- **Tested**: group, sudoers and setuid logic by unit tests; in the finished image `sudo` and `passwd` are `-rwsr-xr-x root`
  and `sudoers` is `-r--r----- root` (read from the squashfs); `sudo -V` runs there. **Not tested**: sudo as an ordinary
  user (needs a real boot; a rootless chroot cannot switch user ids, so `visudo -c` could not run either), and the
  strap group changes on a real disk.
- **On a system installed before this**: no sudo exists. Log in as root, then `usermod -aG wheel,audio,video,input,users NAME`;
  for sudo reinstall from the new ISO.
