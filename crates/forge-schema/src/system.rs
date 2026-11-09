use serde::{Deserialize, Serialize};

use crate::SchemaError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Libc {
    Glibc,
    Musl,
}

impl Libc {
    pub fn as_str(self) -> &'static str {
        match self {
            Libc::Glibc => "glibc",
            Libc::Musl => "musl",
        }
    }

    /// The target triple recipes can pass to configure scripts.
    pub fn target(self) -> &'static str {
        match self {
            Libc::Glibc => "x86_64-linux-gnu",
            Libc::Musl => "x86_64-linux-musl",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitSection {
    /// Which init system is active (e.g. "openrc").
    #[serde(default = "default_init")]
    pub system: String,
    /// Services enabled under that init.
    #[serde(default)]
    pub services: Vec<String>,
}

impl Default for InitSection {
    fn default() -> Self {
        Self { system: default_init(), services: Vec::new() }
    }
}

fn default_init() -> String {
    "openrc".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAccount {
    pub name: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default = "default_shell")]
    pub shell: String,
}

fn default_shell() -> String {
    "/bin/sh".into()
}

/// `/etc/forge/system.toml`: the single source of truth for the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemDecl {
    pub hostname: String,
    pub libc: Libc,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default = "default_keymap")]
    pub keymap: String,
    #[serde(default)]
    pub init: InitSection,
    #[serde(default)]
    pub users: Vec<UserAccount>,
    #[serde(default)]
    pub kernel_modules: Vec<String>,
    /// System-wide packages, visible to every user.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Package repositories, as local directories (searched in order).
    #[serde(default)]
    pub repos: Vec<String>,
}

fn default_timezone() -> String {
    "UTC".into()
}
fn default_locale() -> String {
    "en_US.UTF-8".into()
}
fn default_keymap() -> String {
    "us".into()
}

impl SystemDecl {
    pub fn parse(text: &str) -> Result<Self, SchemaError> {
        let decl: Self = toml::from_str(text)?;
        decl.validate()?;
        Ok(decl)
    }

    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.hostname.is_empty() {
            return Err(SchemaError::Invalid("hostname must not be empty".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for p in &self.packages {
            if !seen.insert(p) {
                return Err(SchemaError::Invalid(format!("duplicate package `{p}`")));
            }
        }
        Ok(())
    }
}

/// `~/.config/forge/user.toml`: one user's own packages (visible only to them).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDecl {
    #[serde(default)]
    pub packages: Vec<String>,
}

impl UserDecl {
    pub fn parse(text: &str) -> Result<Self, SchemaError> {
        Ok(toml::from_str(text)?)
    }
}

/// Where the project's packages are published: a GitHub Release holding `index.toml`, `index.toml.sig`
/// and every archive as flat files. A new system's declaration starts with this repository, and the
/// keys that sign it ship in `/etc/forge/keys`.
pub const DEFAULT_REPO_URL: &str = "https://github.com/noxilith/NovaraSYS/releases/download/repo";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_system() {
        let d = SystemDecl::parse("hostname = \"novara\"\nlibc = \"musl\"\n").unwrap();
        assert_eq!(d.libc, Libc::Musl);
        assert_eq!(d.init.system, "openrc");
        assert_eq!(d.timezone, "UTC");
        assert!(d.packages.is_empty());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(SystemDecl::parse("hostname = \"x\"\nlibc = \"glibc\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn rejects_unknown_libc() {
        assert!(SystemDecl::parse("hostname = \"x\"\nlibc = \"uclibc\"\n").is_err());
    }

    #[test]
    fn rejects_duplicate_packages() {
        let t = "hostname = \"x\"\nlibc = \"glibc\"\npackages = [\"kitty\", \"kitty\"]\n";
        assert!(SystemDecl::parse(t).is_err());
    }

    #[test]
    fn parses_repos() {
        let d = SystemDecl::parse("hostname = \"x\"\nlibc = \"musl\"\nrepos = [\"/srv/repo\"]\n").unwrap();
        assert_eq!(d.repos, ["/srv/repo"]);
        assert!(SystemDecl::parse("hostname = \"x\"\nlibc = \"musl\"\n").unwrap().repos.is_empty());
    }

    #[test]
    fn user_decl_defaults_empty() {
        assert!(UserDecl::parse("").unwrap().packages.is_empty());
    }
}
