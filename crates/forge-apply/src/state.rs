use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::PlanError;

const STATE_PATH: &str = "var/lib/forge/state.toml";

/// What forge itself has set up, so it can undo exactly its own work and
/// nothing else (e.g. only disable services that forge enabled).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub managed_services: Vec<String>,
    /// Highest serial seen in each signed repository (keyed as written in the
    /// declaration), so an older index is recognized as a rollback.
    #[serde(default)]
    pub repo_serials: std::collections::BTreeMap<String, u64>,
}

impl State {
    fn path(root: &Path) -> PathBuf {
        root.join(STATE_PATH)
    }

    pub(crate) fn load(root: &Path) -> Result<Self, PlanError> {
        let path = Self::path(root);
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| PlanError::BadState { path, message: e.to_string() }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(PlanError::Read { path, source }),
        }
    }

    /// The state file to write, if it differs from what was loaded.
    pub(crate) fn render_if_changed(&self, root: &Path, old: &State) -> Option<(PathBuf, String)> {
        let path = Self::path(root);
        if self == old && (path.exists() || self == &State::default()) {
            return None;
        }
        let text = toml::to_string(self).expect("state serializes");
        Some((path, text))
    }
}
