//! The Novara package store.
//!
//! Packages live once, immutably, in a shared store (`<store>/<name>-<version>-<hash>`).
//! A [`Profile`] is a set of symlinks into the store, with generations, so
//! different users (and the system) can see different sets of packages.
//!
//! This crate is the library; privilege handling (the store daemon) lives
//! elsewhere. Everything takes explicit paths, so it can be tested in a
//! temporary directory.

mod archive;
mod gc;
mod id;
mod manifest;
mod profile;
mod repo;
mod remote;
mod sign;
mod store;
#[cfg(test)]
mod testutil;

use std::io;
use std::path::PathBuf;

pub use archive::{PackOptions, Packed, pack_dir, read_manifest};
pub use gc::{GcReport, roots_from_snapshots};
pub use id::{StoreId, sha256_hex};
pub use manifest::{FileEntry, Kind, MANIFEST_PATH, Manifest};
pub use profile::{Profile, ProfileGen};
pub use remote::{Fetch, FetchHandle, RemoteRepo, Repository, SystemFetch, download_to_temp, is_url};
pub use repo::{Index, IndexEntry, Loaded, LocalRepo, Trust, resolve, version_cmp};
pub use sign::{Keyring, PublicKey, SecretKey, SigFile, load_public_key, load_secret_key, write_key_pair};
pub use store::{Store, remove_tree};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("invalid {what}: {value}")]
    Invalid { what: &'static str, value: String },
    #[error("unsafe archive: {0}")]
    UnsafeArchive(String),
    #[error("archive checksum mismatch: expected {expected}, got {actual}")]
    ArchiveHashMismatch { expected: String, actual: String },
    #[error("file `{path}` in the archive does not match its manifest checksum")]
    FileHashMismatch { path: String },
    #[error("`{0}` is not in the store")]
    NotInStore(StoreId),
    #[error("{path} is provided by both {a} and {b}")]
    Conflict { path: String, a: StoreId, b: StoreId },
    #[error("generation {0} does not exist")]
    NoSuchGeneration(u32),
    #[error("generation {0} is the current one and cannot be removed")]
    CurrentGeneration(u32),
    #[error("no package named `{0}` for this libc in the repository")]
    PackageNotFound(String),
    #[error("`{pkg}` needs {dep}, which is not in the repository")]
    DependencyMissing { pkg: String, dep: String },
    #[error("dependency cycle involving {0}")]
    Cycle(String),
    #[error("bad repository index {path}: {message}")]
    BadIndex { path: PathBuf, message: String },
    #[error("repository {0} has no signature (index.toml.sig), so its index cannot be trusted (--allow-unsigned overrides this, insecurely)")]
    Unsigned(PathBuf),
    #[error("repository {repo}: {reason}")]
    BadSignature { repo: PathBuf, reason: String },
    #[error("repository {repo} is signed by key {key_id}, which is not among the {known} trusted key(s)")]
    UnknownKey { repo: PathBuf, key_id: String, known: usize },
    #[error("repository {repo} went backwards: its index has serial {serial}, but serial {seen} was seen before (a rollback, or a different repository at the same path)")]
    Rollback { repo: PathBuf, serial: u64, seen: u64 },
    #[error("repository {0} is signed, so publishing needs the signing key (without it the signature would no longer match)")]
    SignedRepoNeedsKey(PathBuf),
    #[error("repository {0} has an index that is not signed; check it, then sign it with `forge repo sign` instead of signing it as a side effect of publishing")]
    UnsignedIndex(PathBuf),
    #[error("could not download {url}: {reason}")]
    Fetch { url: String, reason: String },
}

pub(crate) fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> StoreError {
    let context = context.into();
    move |source| StoreError::Io { context, source }
}
