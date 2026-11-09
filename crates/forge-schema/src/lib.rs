//! Shared schemas for Novara: the system/user declaration and package recipes.
//!
//! Every other crate (CLI, installer, store daemon) depends on these types so
//! they cannot drift apart.

mod recipe;
mod system;

pub use recipe::{Build, Depends, Effective, LibcOverride, Provides, Recipe, Source};
pub use system::{DEFAULT_REPO_URL, InitSection, Libc, SystemDecl, UserAccount, UserDecl};

/// Error parsing a declaration or recipe.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("invalid TOML")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}
