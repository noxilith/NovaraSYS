use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Libc, SchemaError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Depends {
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default)]
    pub run: Vec<String>,
}

/// Build phases: plain `sh`, run in order, no network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub prepare: Option<String>,
    pub configure: Option<String>,
    pub compile: Option<String>,
    pub install: Option<String>,
}

/// Declared effects instead of install scripts. `forge apply` carries them out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provides {
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub kernel_modules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: String,
    /// libcs this recipe supports.
    pub libc: Vec<Libc>,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub depends: Depends,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub provides: Provides,
    /// Per-libc overrides, merged over the base recipe (e.g. `[libc_override.musl]`).
    #[serde(default)]
    pub libc_override: BTreeMap<String, toml::Table>,
}

impl Recipe {
    pub fn parse(text: &str) -> Result<Self, SchemaError> {
        let r: Self = toml::from_str(text)?;
        r.validate()?;
        Ok(r)
    }

    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.name.is_empty() || self.version.is_empty() {
            return Err(SchemaError::Invalid("name and version are required".into()));
        }
        if self.libc.is_empty() {
            return Err(SchemaError::Invalid("recipe must support at least one libc".into()));
        }
        for s in &self.sources {
            if s.sha256.len() != 64 || !s.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(SchemaError::Invalid(format!(
                    "source {} needs a 64-character hex sha256",
                    s.url
                )));
            }
        }
        for key in self.libc_override.keys() {
            if key != "glibc" && key != "musl" {
                return Err(SchemaError::Invalid(format!("unknown libc override `{key}`")));
            }
        }
        // The name and version become part of the store id (`name-version-hash`),
        // so they must be usable there: a version may not contain `-`.
        let name_ok = self.name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && self.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
        let version_ok = self.version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~'));
        if !name_ok || !version_ok {
            return Err(SchemaError::Invalid(format!(
                "`{}` / `{}` is not a valid name and version (versions may not contain `-`)",
                self.name, self.version
            )));
        }
        for s in &self.sources {
            if s.url.is_empty() {
                return Err(SchemaError::Invalid("a source has an empty url".into()));
            }
        }
        // Every override must be well-formed, whichever libc is built.
        for libc in [Libc::Glibc, Libc::Musl] {
            self.effective(libc)?;
        }
        Ok(())
    }

    pub fn supports(&self, libc: Libc) -> bool {
        self.libc.contains(&libc)
    }

    /// The `[libc_override.<libc>]` table, parsed and checked.
    pub fn libc_override(&self, libc: Libc) -> Result<LibcOverride, SchemaError> {
        let key = libc.as_str();
        let Some(table) = self.libc_override.get(key) else {
            return Ok(LibcOverride::default());
        };
        let o: LibcOverride = toml::Value::Table(table.clone())
            .try_into()
            .map_err(|e| SchemaError::Invalid(format!("[libc_override.{key}]: {e}")))?;
        for (k, v) in &o.env {
            let name_ok = !k.is_empty()
                && !k.starts_with(|c: char| c.is_ascii_digit())
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !name_ok || v.contains('\0') {
                return Err(SchemaError::Invalid(format!("[libc_override.{key}.env]: bad variable `{k}`")));
            }
            if RESERVED_ENV.contains(&k.as_str()) {
                return Err(SchemaError::Invalid(format!(
                    "[libc_override.{key}.env]: `{k}` is set by forge and cannot be overridden"
                )));
            }
        }
        Ok(o)
    }

    /// The build steps and extra environment to use when building for `libc`:
    /// the base recipe with the libc's overrides applied (an override replaces
    /// a phase; `configure_extra` is appended to `configure`).
    pub fn effective(&self, libc: Libc) -> Result<Effective, SchemaError> {
        let o = self.libc_override(libc)?;
        let pick = |over: &Option<String>, base: &Option<String>| over.clone().or_else(|| base.clone());
        let mut build = Build {
            prepare: pick(&o.prepare, &self.build.prepare),
            configure: pick(&o.configure, &self.build.configure),
            compile: pick(&o.compile, &self.build.compile),
            install: pick(&o.install, &self.build.install),
        };
        if let Some(extra) = &o.configure_extra {
            let Some(configure) = &build.configure else {
                return Err(SchemaError::Invalid(format!(
                    "[libc_override.{}]: configure_extra needs a configure step",
                    libc.as_str()
                )));
            };
            build.configure = Some(format!("{configure} {extra}"));
        }
        Ok(Effective { build, env: o.env })
    }
}

/// Variables forge itself sets for every build.
const RESERVED_ENV: &[&str] = &[
    "SRC", "DESTDIR", "JOBS", "LIBC", "TARGET", "PATH", "HOME", "SOURCE_DATE_EPOCH", "RECIPE_DIR", "SOURCES",
    "TZ", "LC_ALL", "PWD",
];

/// What a `[libc_override.<libc>]` table may contain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibcOverride {
    pub prepare: Option<String>,
    pub configure: Option<String>,
    pub compile: Option<String>,
    pub install: Option<String>,
    /// Appended to the `configure` command.
    pub configure_extra: Option<String>,
    /// Extra environment variables for the build steps.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A recipe's build steps and environment, resolved for one libc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub build: Build,
    pub env: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn sample() -> String {
        format!(
            r#"
name = "htop"
version = "3.3.0"
libc = ["glibc", "musl"]

[[source]]
url = "https://example.com/htop.tar.gz"
sha256 = "{HASH}"

[depends]
build = ["ncurses"]
run = ["ncurses"]

[build]
configure = "./configure --prefix=/usr"
compile = "make -j$JOBS"
install = "make DESTDIR=$DESTDIR install"
"#
        )
    }

    #[test]
    fn parses_recipe() {
        let r = Recipe::parse(&sample()).unwrap();
        assert_eq!(r.name, "htop");
        assert!(r.supports(Libc::Musl));
        assert_eq!(r.sources.len(), 1);
        assert_eq!(r.depends.run, ["ncurses"]);
    }

    #[test]
    fn rejects_bad_checksum() {
        let t = sample().replace(HASH, "nothex");
        assert!(Recipe::parse(&t).is_err());
    }

    #[test]
    fn rejects_empty_libc_list() {
        let t = sample().replace(r#"libc = ["glibc", "musl"]"#, "libc = []");
        assert!(Recipe::parse(&t).is_err());
    }

    #[test]
    fn effective_build_applies_the_libc_override() {
        let t = format!(
            "{}\n[libc_override.musl]\nconfigure_extra = \"--disable-nls\"\ninstall = \"make install-musl\"\n[libc_override.musl.env]\nCFLAGS = \"-Os\"\n",
            sample()
        );
        let r = Recipe::parse(&t).unwrap();
        let musl = r.effective(Libc::Musl).unwrap();
        assert_eq!(musl.build.configure.as_deref(), Some("./configure --prefix=/usr --disable-nls"));
        assert_eq!(musl.build.install.as_deref(), Some("make install-musl"), "override replaces the phase");
        assert_eq!(musl.build.compile.as_deref(), Some("make -j$JOBS"), "other phases are kept");
        assert_eq!(musl.env["CFLAGS"], "-Os");

        let glibc = r.effective(Libc::Glibc).unwrap();
        assert_eq!(glibc.build.configure.as_deref(), Some("./configure --prefix=/usr"));
        assert!(glibc.env.is_empty());
    }

    #[test]
    fn rejects_bad_overrides() {
        for bad in [
            "[libc_override.musl]\nbogus = 1\n",
            "[libc_override.musl]\nconfigure_extra = \"x\"\n",
            "[libc_override.musl.env]\nSRC = \"/x\"\n",
            "[libc_override.musl.env]\nPATH = \"/x\"\n",
            "[libc_override.musl.env]\n\"A B\" = \"x\"\n",
        ] {
            // `sample()` already has a [build] table, so only add what differs.
            let base = sample().replace("[build]\nconfigure = \"./configure --prefix=/usr\"\n", "[build]\n");
            let t = format!("{base}\n{bad}");
            assert!(Recipe::parse(&t).is_err(), "should be rejected: {bad}");
        }
    }

    #[test]
    fn rejects_names_and_versions_that_cannot_be_store_ids() {
        for (n, v) in [("a b", "1"), ("a", "1-2"), ("-a", "1"), ("a/b", "1"), ("a", "1 2")] {
            let t = sample().replace("name = \"htop\"", &format!("name = \"{n}\"")).replace("3.3.0", v);
            assert!(Recipe::parse(&t).is_err(), "{n:?} {v:?}");
        }
    }

    #[test]
    fn rejects_unknown_override() {
        let t = format!("{}\n[libc_override.uclibc]\nx = 1\n", sample());
        assert!(Recipe::parse(&t).is_err());
    }
}
