use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

const STATE_DIR: &str = ".flatplay";
const STATE_FILE_NAME: &str = "state.json";

#[derive(Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct State {
    pub active_manifest: Option<PathBuf>,
    pub manifest_hash: Option<String>,
    pub dependencies_updated: bool,
    pub dependencies_built: bool,
    pub application_built: bool,
    #[serde(skip)]
    pub base_dir: PathBuf,
}

impl Default for State {
    fn default() -> Self {
        Self {
            active_manifest: None,
            manifest_hash: None,
            dependencies_updated: false,
            dependencies_built: false,
            application_built: false,
            base_dir: PathBuf::new(),
        }
    }
}

impl State {
    fn state_file_path(base_dir: &Path) -> PathBuf {
        base_dir.join(STATE_DIR).join(STATE_FILE_NAME)
    }

    pub fn load(base_dir: PathBuf) -> Result<Self> {
        let state_file = Self::state_file_path(&base_dir);
        if !state_file.exists() {
            return Ok(State {
                base_dir,
                ..Default::default()
            });
        }
        let content = fs::read_to_string(state_file)?;
        let mut state: State = serde_json::from_str(&content)?;
        state.base_dir = base_dir;
        Ok(state)
    }

    pub fn save(&self) -> Result<()> {
        let state_dir = self.base_dir.join(STATE_DIR);
        fs::create_dir_all(state_dir)?;
        let content = serde_json::to_string_pretty(self)?;
        fs::write(Self::state_file_path(&self.base_dir), content)?;
        Ok(())
    }

    /// Resets the state to its initial values.
    /// This is specifically only for build progress. Not general state.
    pub fn reset(&mut self) {
        self.dependencies_updated = false;
        self.dependencies_built = false;
        self.application_built = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_state_serialization() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_dir = temp_dir.path().to_path_buf();

        let mut state = State::load(base_dir.clone()).unwrap();
        assert_eq!(state.active_manifest, None);
        assert_eq!(state.manifest_hash, None);
        assert!(!state.dependencies_updated);
        assert!(!state.dependencies_built);
        assert!(!state.application_built);

        state.active_manifest = Some(PathBuf::from("/tmp/manifest.json"));
        state.manifest_hash = Some("abc123".to_string());
        state.dependencies_updated = true;

        state.save().unwrap();

        let loaded_state = State::load(base_dir).unwrap();
        assert_eq!(
            loaded_state.active_manifest,
            Some(PathBuf::from("/tmp/manifest.json"))
        );
        assert_eq!(loaded_state.manifest_hash, Some("abc123".to_string()));
        assert!(loaded_state.dependencies_updated);
        assert!(!loaded_state.dependencies_built);
    }

    #[test]
    fn test_state_reset() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut state = State::load(temp_dir.path().to_path_buf()).unwrap();

        state.dependencies_updated = true;
        state.dependencies_built = true;
        state.application_built = true;

        state.reset();

        assert!(!state.dependencies_updated);
        assert!(!state.dependencies_built);
        assert!(!state.application_built);
    }
}
