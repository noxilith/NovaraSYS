# NovaraSYS

An independent Linux distribution, written in Rust where the project writes anything: not derived from Arch or Debian,
OpenRC by default, glibc and musl, declarative, with generations you can roll back to.

**Status: early and experimental.** It boots a live ISO and installs to disk on the author's hardware; most of it is
new and little of it has met other machines. Read `DESIGN.md` for what is done, what was tested and what was not.

## How it is meant to work

1. Boot the live ISO (Limine, BIOS and UEFI), log in (`root` / `root`), partition the disk with `cfdisk`.
2. `forge strap <root-partition> <boot-partition>` installs to a btrfs layout (`@ @snapshots @home @store`) and
   writes a starter `/etc/forge/system.toml`.
3. The system is declared in `/etc/forge/system.toml`. `forge pkg add NAME` edits it and applies it; `forge apply`
   converges the machine to it. Every change can be rolled back through generations (`forge gen`).
4. Packages come from this repository's `repo` release as signed binary archives, checked against the key in
   `distro/keys/` before anything is installed. `forge kernel` builds and installs kernels.

## Layout

| Path | What |
|---|---|
| `crates/` | `forge` (the one binary) and its libraries: declaration, apply, generations, strap, store, build, kernel, ISO |
| `recipes/` | Package recipes (source, build steps, dependencies), including the kernel |
| `scripts/` | Bootstrapping the toolchain from nothing, building the ISO and the packages, tests that need root |
| `distro/keys/` | The public key that signs the package repository (the secret key is never in git) |
| `DESIGN.md` | Design, status, and an honest account of what is not verified |

## Building

```
cargo build
BASE_REPO=repo/selfhost-r2 scripts/build-iso.sh     # needs the packages built first, see DESIGN.md
```

Building the base packages from source takes hours and gigabytes (`scripts/bootstrap-*.sh`, `scripts/build-tools.sh`);
the kernel needs a host with make, gcc, bc, flex, bison, perl and libelf.

## Licence

The workspace declares `MIT OR Apache-2.0`. Package recipes build third-party software under its own licence.
