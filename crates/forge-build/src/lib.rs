//! The recipe builder: turn a `recipe.toml` into a package archive.
//!
//! A build fetches and checksums the sources, extracts them, then runs the
//! recipe's steps inside a sandbox (see [`sandbox`]) with no network and a
//! read-only view of the tools, and packs whatever the install step put in
//! `$DESTDIR` into an archive named after a hash of everything that went in.

mod build;
mod extract;
mod fetch;
mod hash;
pub mod sandbox;

use std::io;


pub use build::{BuildOptions, Built, Tools, build};
pub use extract::extract;
pub use fetch::SourceCache;
pub use hash::input_hash;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("recipe: {0}")]
    Recipe(String),
    #[error("{name} does not support {libc}")]
    UnsupportedLibc { name: String, libc: &'static str },
    #[error("source {url} has checksum {actual}, but the recipe says {expected}")]
    SourceHashMismatch { url: String, expected: String, actual: String },
    #[error("cannot download {url}: {message}")]
    Download { url: String, message: String },
    #[error("unsupported source url `{0}` (use https://, http://, file:// or an absolute path)")]
    UnsupportedUrl(String),
    #[error("cannot unpack {0}")]
    UnsupportedArchive(String),
    #[error("unsafe source archive: {0}")]
    UnsafeSource(String),
    #[error("sandbox: {0}")]
    Sandbox(String),
    #[error("the build failed ({status}); see the output above")]
    BuildFailed { status: String },
    #[error("the install step left $DESTDIR empty, so there is nothing to package")]
    EmptyOutput,
    #[error("dependencies: {0}")]
    Dependencies(String),
    #[error("{0}")]
    Package(String),
}

pub(crate) fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> BuildError {
    let context = context.into();
    move |source| BuildError::Io { context, source }
}
