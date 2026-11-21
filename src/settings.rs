use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// Reuse an existing Minecraft data store instead of `root/shared`.
    /// Point this at `%APPDATA%\.minecraft` to share assets and libraries.
    #[serde(default)]
    pub shared_dir: Option<String>,
    #[serde(default)]
    pub default_user: Option<String>,
    #[serde(default)]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub jvm_args: Vec<String>,
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn shared(&self) -> Option<PathBuf> {
        self.shared_dir.as_ref().map(PathBuf::from)
    }

    pub fn memory_mb(&self) -> u32 {
        self.memory_mb.unwrap_or(2048).clamp(512, 32_768)
    }

    pub fn user(&self) -> String {
        self.default_user
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| "Player".to_string())
    }
}
