use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::paths::Paths;
use crate::util;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub name: String,
    /// Minecraft version id (may be a modded one from an existing install).
    pub version: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub java: Option<String>,
    #[serde(default)]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub jvm_args: Vec<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub last_played: Option<String>,
}

impl Instance {
    pub fn file(paths: &Paths, name: &str) -> PathBuf {
        paths.instance_dir(name).join("instance.json")
    }

    pub fn dir(paths: &Paths, name: &str) -> PathBuf {
        paths.instance_dir(name)
    }

    pub fn load(paths: &Paths, name: &str) -> Result<Self> {
        let file = Self::file(paths, name);
        if !file.exists() {
            bail!(
                "no instance named '{name}' (see `mcl list`, or create one with `mcl create {name} --version <version>`)"
            );
        }
        let text = fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let dir = paths.instance_dir(&self.name);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("instance.json"),
            serde_json::to_string_pretty(self)?,
        )?;
        Ok(())
    }

    pub fn create(
        paths: &Paths,
        name: &str,
        version: &str,
        user: Option<String>,
        memory_mb: Option<u32>,
        java: Option<PathBuf>,
    ) -> Result<Self> {
        let mut instance = Self::create_from(paths, name, Some(version), None)?;
        instance.user = user;
        instance.memory_mb = memory_mb;
        instance.java = java.map(|p| p.display().to_string());
        instance.save(paths)?;
        Ok(instance)
    }

    /// Create an instance, optionally as a copy of an existing one.
    ///
    /// A copy takes the setup — mods, config, resource packs, worlds, `options.txt` —
    /// and leaves the history behind: logs, crash reports and the source's own
    /// `instance.json`. Copying the whole store would be gigabytes; copying only the
    /// config would lose the point of "start from this one".
    pub fn create_from(
        paths: &Paths,
        name: &str,
        version: Option<&str>,
        copy_from: Option<&str>,
    ) -> Result<Self> {
        validate_name(name)?;
        if Self::file(paths, name).exists() {
            bail!("an instance named '{name}' already exists");
        }

        let source = match copy_from.map(str::trim).filter(|s| !s.is_empty()) {
            Some(from) if from != name => Some(Self::load(paths, from)?),
            _ => None,
        };

        let version = version
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| source.as_ref().map(|s| s.version.clone()))
            .context("a new instance needs a Minecraft version")?;

        let instance = Instance {
            name: name.to_string(),
            version,
            user: source.as_ref().and_then(|s| s.user.clone()),
            java: source.as_ref().and_then(|s| s.java.clone()),
            memory_mb: source.as_ref().and_then(|s| s.memory_mb),
            jvm_args: source
                .as_ref()
                .map(|s| s.jvm_args.clone())
                .unwrap_or_default(),
            width: source.as_ref().and_then(|s| s.width),
            height: source.as_ref().and_then(|s| s.height),
            created: Some(util::now_iso()),
            last_played: None,
        };

        let dir = paths.instance_dir(name);
        if let Some(source) = &source {
            let from = paths.instance_dir(&source.name);
            if let Err(error) = copy_tree(&from, &dir) {
                let _ = fs::remove_dir_all(&dir);
                return Err(error).with_context(|| format!("copy {}", from.display()));
            }
        }
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        instance.save(paths)?;

        // A game directory with the usual folders so mods and worlds have a home.
        for sub in [
            "mods",
            "config",
            "resourcepacks",
            "shaderpacks",
            "saves",
            "screenshots",
            "logs",
        ] {
            fs::create_dir_all(dir.join(sub))?;
        }
        Ok(instance)
    }

    pub fn list(paths: &Paths) -> Result<Vec<Instance>> {
        let dir = paths.instances_dir();
        let mut instances = Vec::new();
        if !dir.exists() {
            return Ok(instances);
        }
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(instance) = Self::load(paths, &name) {
                instances.push(instance);
            }
        }
        instances.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(instances)
    }

    pub fn delete(paths: &Paths, name: &str) -> Result<()> {
        let dir = paths.instance_dir(name);
        if !dir.exists() {
            bail!("no instance named '{name}'");
        }
        fs::remove_dir_all(&dir).with_context(|| format!("delete {}", dir.display()))?;
        Ok(())
    }

    /// Delete without the risk: the folder is moved into `<root>\trash\` instead
    /// of being destroyed, so a mis-click costs disk space, not a world.
    pub fn trash(paths: &Paths, name: &str) -> Result<PathBuf> {
        let dir = paths.instance_dir(name);
        if !dir.is_dir() {
            bail!("no instance named '{name}'");
        }
        let bin = paths.root.join("trash");
        fs::create_dir_all(&bin).with_context(|| format!("create {}", bin.display()))?;
        let stamp = util::now_iso().replace([' ', ':'], "-");
        let mut target = bin.join(format!("{name} {stamp}"));
        let mut n = 2;
        while target.exists() {
            target = bin.join(format!("{name} {stamp} ({n})"));
            n += 1;
        }
        fs::rename(&dir, &target)
            .with_context(|| format!("move {} to {}", dir.display(), target.display()))?;
        Ok(target)
    }

    pub fn user(&self, fallback: &str) -> String {
        self.user
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| fallback.to_string())
    }

    pub fn memory_mb(&self, fallback: u32) -> u32 {
        self.memory_mb.unwrap_or(fallback).clamp(512, 32_768)
    }
}

/// Copy an instance folder without its history: logs, crash reports and the
/// source's own instance file stay behind. Anything else is player content.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    const SKIP: [&str; 3] = ["logs", "crash-reports", "debug"];
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if SKIP.iter().any(|skip| skip.eq_ignore_ascii_case(&name_str))
            || name_str.eq_ignore_ascii_case("instance.json")
        {
            continue;
        }
        let path = entry.path();
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("instance name cannot be empty");
    }
    if name.len() > 64 {
        bail!("instance name is too long (max 64 characters)");
    }
    if name.starts_with('.') {
        bail!("instance name cannot start with a dot");
    }
    for bad in ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'] {
        if name.contains(bad) {
            bail!("instance name cannot contain '{bad}'");
        }
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name.trim()))
    {
        bail!("'{name}' is a reserved Windows name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("mcl-instance-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = Paths::at(root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn a_copy_takes_the_setup_and_leaves_the_history() {
        let paths = scratch("copy");
        let source = Instance::create(&paths, "source", "26.3", None, None, None).unwrap();

        let dir = Instance::dir(&paths, "source");
        fs::write(dir.join("mods").join("sodium.jar"), b"jar").unwrap();
        fs::create_dir_all(dir.join("saves").join("world")).unwrap();
        fs::write(dir.join("saves").join("world").join("level.dat"), b"dat").unwrap();
        fs::write(dir.join("options.txt"), b"fov:90").unwrap();
        fs::write(dir.join("logs").join("latest.log"), b"noise").unwrap();

        let copy = Instance::create_from(&paths, "copy", None, Some("source")).unwrap();
        let copy_dir = Instance::dir(&paths, "copy");

        assert_eq!(copy.version, source.version, "the version comes along");
        assert_eq!(copy.last_played, None, "a new instance has never been played");
        assert!(copy_dir.join("mods").join("sodium.jar").is_file());
        assert!(copy_dir.join("saves").join("world").join("level.dat").is_file());
        assert!(copy_dir.join("options.txt").is_file());
        assert!(!copy_dir.join("logs").join("latest.log").exists(), "logs stay behind");
        assert!(copy_dir.join("logs").is_dir(), "but the folder is still made");

        // The source keeps everything it had.
        assert!(dir.join("logs").join("latest.log").is_file());
        let _ = fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_name_cannot_be_reused_and_needs_a_version() {
        let paths = scratch("names");
        Instance::create(&paths, "one", "26.3", None, None, None).unwrap();

        let error = Instance::create_from(&paths, "one", Some("26.3"), None).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");

        let error = Instance::create_from(&paths, "two", None, None).unwrap_err();
        assert!(error.to_string().contains("version"), "{error}");
        let _ = fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn trash_moves_the_folder_instead_of_erasing_it() {
        let paths = scratch("trash");
        let made = Instance::create(&paths, "keep-me", "26.3", None, None, None).unwrap();
        fs::write(
            Instance::dir(&paths, &made.name).join("saves").join("a.dat"),
            b"world",
        )
        .unwrap();

        let moved = Instance::trash(&paths, "keep-me").unwrap();
        assert!(moved.starts_with(paths.root.join("trash")), "{}", moved.display());
        assert!(!Instance::dir(&paths, "keep-me").exists());
        assert!(moved.join("saves").join("a.dat").is_file());
        assert!(Instance::list(&paths).unwrap().is_empty());
        let _ = fs::remove_dir_all(&paths.root);
    }
}
