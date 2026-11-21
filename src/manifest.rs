use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::download::Downloader;
use crate::paths::Paths;

pub const MANIFEST_URL: &str = "https://launchermeta.mojang.com/mc/game/version_manifest_v2.json";
const CACHE_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Deserialize)]
pub struct VersionManifest {
    pub latest: Latest,
    pub versions: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Latest {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(rename = "releaseTime", default)]
    pub release_time: String,
}

impl VersionManifest {
    pub fn find(&self, id: &str) -> Option<&ManifestEntry> {
        self.versions.iter().find(|v| v.id == id)
    }

    pub fn find_ci(&self, needle: &str) -> Vec<&ManifestEntry> {
        let needle = needle.to_lowercase();
        self.versions
            .iter()
            .filter(|v| v.id.to_lowercase().contains(&needle))
            .collect()
    }
}

fn cache_is_fresh(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age < CACHE_MAX_AGE)
        .unwrap_or(false)
}

/// Loads the version manifest, using the on-disk cache when it is fresh
/// and falling back to the cache when the network is unavailable.
pub fn load(paths: &Paths, dl: &Downloader, refresh: bool) -> Result<VersionManifest> {
    let cache = paths.manifest_cache();

    if !refresh && cache_is_fresh(&cache) {
        if let Ok(text) = fs::read_to_string(&cache) {
            if let Ok(manifest) = serde_json::from_str::<VersionManifest>(&text) {
                return Ok(manifest);
            }
        }
    }

    match dl.get_text(MANIFEST_URL) {
        Ok(text) => {
            let manifest: VersionManifest =
                serde_json::from_str(&text).context("parse version manifest")?;
            if let Some(parent) = cache.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&cache, &text);
            Ok(manifest)
        }
        Err(error) => {
            if let Ok(text) = fs::read_to_string(&cache) {
                let manifest: VersionManifest =
                    serde_json::from_str(&text).context("parse cached version manifest")?;
                eprintln!("warning: using a cached version manifest ({error})");
                return Ok(manifest);
            }
            Err(error).context("download version manifest")
        }
    }
}
