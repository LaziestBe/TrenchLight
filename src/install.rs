use anyhow::{anyhow, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::download::Downloader;
use crate::paths::Paths;
use crate::util;
use crate::version::{self, Artifact, Resolved};

/// One file that has to exist before the game can start.
#[derive(Debug, Clone)]
pub struct LibFile {
    pub artifact: Artifact,
    pub relative: String,
    /// Natives jars are unpacked into the natives directory instead of
    /// being put on the classpath.
    pub extract: bool,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub lib_files: Vec<LibFile>,
    pub classpath: Vec<PathBuf>,
    pub natives: PathBuf,
    pub client_jar: PathBuf,
    pub log_config: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Installed {
    pub files_downloaded: usize,
    pub libraries: usize,
    pub assets_present: usize,
    pub assets_total: usize,
    pub assets_downloaded: usize,
}

#[derive(Deserialize)]
struct AssetIndexFile {
    #[serde(default)]
    objects: HashMap<String, AssetObject>,
}

#[derive(Deserialize)]
struct AssetObject {
    hash: String,
    #[serde(default)]
    size: u64,
}

pub fn style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg:16} [{bar:32.cyan/blue}] {pos}/{len}")
        .unwrap()
        .progress_chars("=>-")
}

fn artifacts_for(library: &version::Library) -> Vec<(Artifact, bool)> {
    let mut out = Vec::new();

    if let Some(downloads) = &library.downloads {
        if let Some(artifact) = &downloads.artifact {
            out.push((artifact.clone(), false));
        }
        if let Some(natives) = &library.natives {
            if let Some(classifier) = natives.get(version::os_name()) {
                if let Some(classified) = downloads
                    .classifiers
                    .as_ref()
                    .and_then(|map| map.get(classifier))
                {
                    out.push((classified.clone(), true));
                }
            }
        }
        return out;
    }

    // Forge/NeoForge style: maven coordinates plus a repository base url.
    if let Some(relative) = version::maven_path(&library.name) {
        let base = library
            .url
            .clone()
            .unwrap_or_else(|| "https://libraries.minecraft.net/".to_string());
        let base = if base.ends_with('/') {
            base
        } else {
            format!("{base}/")
        };
        out.push((
            Artifact {
                path: Some(relative.clone()),
                url: Some(format!("{base}{relative}")),
                sha1: None,
                size: None,
            },
            false,
        ));
    }
    out
}

/// Works out every path the game will need, without touching the network.
pub fn plan(paths: &Paths, resolved: &Resolved, features: &version::Features) -> Plan {
    let natives = paths.natives_dir(&resolved.base_id);
    let mut lib_files: Vec<LibFile> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for library in &resolved.libraries {
        if !version::rules_allow(library.rules.as_ref(), features) {
            continue;
        }
        for (artifact, extract) in artifacts_for(library) {
            let relative = artifact.relative_path();
            if !seen.insert(relative.clone()) {
                continue;
            }
            lib_files.push(LibFile {
                artifact,
                relative,
                extract,
            });
        }
    }

    let classpath = lib_files
        .iter()
        .filter(|file| !file.extract)
        .map(|file| paths.libraries().join(&file.relative))
        .collect();

    Plan {
        lib_files,
        classpath,
        natives,
        client_jar: paths.version_jar(&resolved.base_id),
        log_config: resolved
            .logging
            .as_ref()
            .map(|log| paths.log_configs().join(&log.file.id)),
    }
}

pub fn install(
    paths: &Paths,
    dl: &Downloader,
    resolved: &Resolved,
    plan: &Plan,
    mp: &MultiProgress,
    stopped: Option<&(dyn Fn() -> bool + Sync)>,
) -> Result<Installed> {
    let halt = || stopped.map(|check| check()).unwrap_or(false);
    let mut report = Installed::default();

    // 1. client jar
    if let Some(client) = &resolved.client {
        let bar = mp.add(ProgressBar::new(1));
        bar.set_style(style());
        bar.set_message("client jar");
        let url = client
            .url
            .clone()
            .ok_or_else(|| anyhow!("client download has no url"))?;
        if dl.download(
            &url,
            &plan.client_jar,
            client.sha1.as_deref(),
            client.size,
        )? {
            report.files_downloaded += 1;
        }
        bar.inc(1);
        bar.finish_and_clear();
    }

    // 2. libraries
    let files = &plan.lib_files;
    report.libraries = files.len();
    let bar = mp.add(ProgressBar::new(files.len() as u64));
    bar.set_style(style());
    bar.set_message("libraries");
    let fetched = AtomicUsize::new(0);
    util::parallel_for(files.len(), 6, |index| {
        if stopped.map(|check| check()).unwrap_or(false) {
            return Ok(());
        }
        let file = &files[index];
        let dest = paths.libraries().join(&file.relative);
        let Some(url) = file.artifact.url.clone() else {
            bar.inc(1);
            return Ok(());
        };
        if dl.download(
            &url,
            &dest,
            file.artifact.sha1.as_deref(),
            file.artifact.size,
        )? {
            fetched.fetch_add(1, Ordering::Relaxed);
        }
        bar.inc(1);
        Ok(())
    })?;
    report.files_downloaded += fetched.load(Ordering::Relaxed);
    bar.finish_and_clear();

    // 3. legacy natives extraction (sequential: one directory, many jars)
    let natives: Vec<&LibFile> = files.iter().filter(|file| file.extract).collect();
    if !natives.is_empty() {
        fs::create_dir_all(&plan.natives)?;
        let bar = mp.add(ProgressBar::new(natives.len() as u64));
        bar.set_style(style());
        bar.set_message("natives");
        for file in natives {
            if halt() {
                break;
            }
            let jar = paths.libraries().join(&file.relative);
            if jar.exists() {
                extract_natives(&jar, &plan.natives)?;
            }
            bar.inc(1);
        }
        bar.finish_and_clear();
    }

    // 4. asset index + objects
    let (present, total, assets_downloaded) = install_assets(paths, dl, resolved, mp, stopped)?;
    report.assets_present = present;
    report.assets_total = total;
    report.assets_downloaded = assets_downloaded;

    // 5. log4j config
    if let (Some(log), Some(dest)) = (&resolved.logging, &plan.log_config) {
        dl.download(&log.file.url, dest, Some(&log.file.sha1), log.file.size)?;
    }

    Ok(report)
}

fn install_assets(
    paths: &Paths,
    dl: &Downloader,
    resolved: &Resolved,
    mp: &MultiProgress,
    stopped: Option<&(dyn Fn() -> bool + Sync)>,
) -> Result<(usize, usize, usize)> {
    let index = &resolved.asset_index;
    let index_path = paths.indexes().join(format!("{}.json", index.id));

    if !index_path.exists() {
        let bar = mp.add(ProgressBar::new(1));
        bar.set_style(style());
        bar.set_message("asset index");
        let url = index
            .url
            .clone()
            .ok_or_else(|| anyhow!("asset index '{}' has no url", index.id))?;
        dl.download(&url, &index_path, index.sha1.as_deref(), index.size)?;
        bar.inc(1);
        bar.finish_and_clear();
    }

    let text = fs::read_to_string(&index_path)
        .with_context(|| format!("read {}", index_path.display()))?;
    let parsed: AssetIndexFile =
        serde_json::from_str(&text).with_context(|| format!("parse asset index {}", index.id))?;

    let objects = paths.objects();
    let mut missing: Vec<(String, u64)> = Vec::new();
    for object in parsed.objects.values() {
        let hash = &object.hash;
        if hash.len() < 2 {
            continue;
        }
        let dest = objects.join(&hash[0..2]).join(hash);
        if !dest.exists() {
            missing.push((hash.clone(), object.size));
        }
    }

    let total = parsed.objects.len();
    let downloaded = missing.len();

    if !missing.is_empty() {
        let bar = mp.add(ProgressBar::new(missing.len() as u64));
        bar.set_style(style());
        bar.set_message("assets");
        util::parallel_for(missing.len(), 8, |i| {
            if stopped.map(|check| check()).unwrap_or(false) {
                bar.inc(1);
                return Ok(());
            }
            let (hash, size) = &missing[i];
            if hash.len() < 2 {
                bar.inc(1);
                return Ok(());
            }
            let dest = objects.join(&hash[0..2]).join(hash);
            let url = format!(
                "https://resources.download.minecraft.net/{}/{}",
                &hash[0..2],
                hash
            );
            dl.download(&url, &dest, Some(hash), Some(*size))?;
            bar.inc(1);
            Ok(())
        })?;
        bar.finish_and_clear();
    }

    // Everything that was missing has been fetched by now.
    Ok((total, total, downloaded))
}

fn extract_natives(jar: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(jar).with_context(|| format!("open {}", jar.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("read {} as a zip", jar.display()))?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            continue;
        };
        let text = name.to_string_lossy().replace('\\', "/");
        if text.starts_with("META-INF/") || text.ends_with(".git") || text.ends_with(".sha1") {
            continue;
        }
        let out = dest.join(&name);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut buffer = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buffer)?;
        let mut file = fs::File::create(&out)
            .with_context(|| format!("create {}", out.display()))?;
        file.write_all(&buffer)?;
    }
    Ok(())
}

/// A short summary of what is still missing, used by `mcl info`.
pub fn missing_files(paths: &Paths, plan: &Plan) -> Vec<String> {
    let mut missing = Vec::new();
    if !plan.client_jar.exists() {
        missing.push(plan.client_jar.display().to_string());
    }
    for file in &plan.lib_files {
        let dest = paths.libraries().join(&file.relative);
        if !dest.exists() {
            missing.push(dest.display().to_string());
        }
    }
    missing
}
