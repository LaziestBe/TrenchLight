use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths::Paths;

#[derive(Debug, Clone)]
pub struct JavaInstall {
    pub path: PathBuf,
    pub major: u32,
    pub version: String,
    pub source: String,
}

impl JavaInstall {
    pub fn label(&self) -> String {
        format!("Java {} ({})", self.version, self.source)
    }
}

fn java_exe(dir: &Path) -> Option<PathBuf> {
    let candidate = if cfg!(windows) {
        dir.join("java.exe")
    } else {
        dir.join("java")
    };
    candidate.is_file().then_some(candidate)
}

fn probe(path: &Path) -> Option<(u32, String)> {
    let mut command = Command::new(path);
    command.arg("-XshowSettings:properties").arg("-version");
    // A GUI process has no console, so every console child it starts gets a
    // console window of its own: probing nine JDKs on startup flashed nine
    // black boxes before the launcher was even on screen. CREATE_NO_WINDOW
    // keeps them off the screen; the output is piped either way.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stderr);
    let mut major = None;
    let mut version = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("java.specification.version =") {
            let value = value.trim();
            // Java 8 reports "1.8"; everything newer reports "17", "21", "25".
            let mut parts = value.split('.');
            let first = parts.next().unwrap_or(value);
            major = parts
                .next()
                .filter(|_| first == "1")
                .unwrap_or(first)
                .parse::<u32>()
                .ok();
        }
        if let Some(value) = line.strip_prefix("java.version =") {
            version = Some(value.trim().to_string());
        }
    }
    let major = major?;
    let version = version.unwrap_or_else(|| major.to_string());
    Some((major, version))
}

fn push_candidate(
    found: &mut Vec<JavaInstall>,
    seen: &mut HashSet<PathBuf>,
    path: PathBuf,
    source: &str,
) {
    let Ok(canonical) = path.canonicalize() else {
        return;
    };
    if !seen.insert(canonical.clone()) {
        return;
    }
    if let Some((major, version)) = probe(&canonical) {
        found.push(JavaInstall {
            path,
            major,
            version,
            source: source.to_string(),
        });
    }
}

/// Mojang's runtime layout is
/// `runtime/<component>/<platform>/<component>/bin/java.exe`, so walk down a
/// few levels looking for a `bin` folder.
fn find_java_below(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    if let Some(exe) = java_exe(&dir.join("bin")) {
        out.push(exe);
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_java_below(&path, depth - 1, out);
        }
    }
}

fn collect_runtime_dirs(root: &Path, found: &mut Vec<JavaInstall>, seen: &mut HashSet<PathBuf>) {
    let Ok(components) = std::fs::read_dir(root) else {
        return;
    };
    for component in components.flatten() {
        if !component.path().is_dir() {
            continue;
        }
        let mut exes = Vec::new();
        find_java_below(&component.path(), 4, &mut exes);
        let label = format!(
            "minecraft/{}",
            component.file_name().to_string_lossy()
        );
        for exe in exes {
            push_candidate(found, seen, exe, &label);
        }
    }
}

fn collect_install_dirs(parent: &Path, found: &mut Vec<JavaInstall>, seen: &mut HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(exe) = java_exe(&path.join("bin")) {
            let label = format!("{}", path.display());
            push_candidate(found, seen, exe, &label);
            continue;
        }
        // e.g. C:\Program Files\Java\jdk-21.0.1\bin
        if let Ok(children) = std::fs::read_dir(&path) {
            for child in children.flatten() {
                let child = child.path();
                if let Some(exe) = java_exe(&child.join("bin")) {
                    let label = format!("{}", child.display());
                    push_candidate(found, seen, exe, &label);
                }
            }
        }
    }
}

/// Detects every JVM it can find, newest major version first.
pub fn detect(paths: &Paths) -> Vec<JavaInstall> {
    let mut found: Vec<JavaInstall> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    if let Ok(home) = std::env::var("JAVA_HOME") {
        if let Some(exe) = java_exe(&PathBuf::from(home).join("bin")) {
            push_candidate(&mut found, &mut seen, exe, "JAVA_HOME");
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if let Some(exe) = java_exe(&dir) {
                push_candidate(&mut found, &mut seen, exe, "PATH");
            }
        }
    }

    let mut runtime_roots = vec![paths.shared.join("runtime")];
    if let Some(appdata) = dirs::config_dir() {
        runtime_roots.push(appdata.join(".minecraft").join("runtime"));
    }
    if let Some(local) = dirs::data_local_dir() {
        runtime_roots.push(local.join(".minecraft").join("runtime"));
    }
    for root in runtime_roots {
        collect_runtime_dirs(&root, &mut found, &mut seen);
    }

    if cfg!(windows) {
        for base in ["C:\\Program Files", "C:\\Program Files (x86)"] {
            for vendor in [
                "Java",
                "Eclipse Adoptium",
                "Eclipse Foundation",
                "Zulu",
                "Microsoft",
                "Amazon Corretto",
                "BellSoft",
                "Semeru",
                "JetBrains",
                "Temurin",
            ] {
                collect_install_dirs(&Path::new(base).join(vendor), &mut found, &mut seen);
            }
        }
    }

    found.sort_by(|a, b| b.major.cmp(&a.major).then(a.version.cmp(&b.version)));
    found
}

/// Picks the JVM that best satisfies `required` major version.
pub fn pick<'a>(installs: &'a [JavaInstall], required: u32) -> Option<&'a JavaInstall> {
    installs
        .iter()
        .filter(|j| j.major == required)
        .min_by_key(|j| j.major)
        .or_else(|| {
            installs
                .iter()
                .filter(|j| j.major >= required)
                .min_by_key(|j| j.major)
        })
}

pub fn from_path(path: &Path) -> Result<JavaInstall> {
    let (major, version) = probe(path)
        .with_context(|| format!("{} is not a working java executable", path.display()))?;
    Ok(JavaInstall {
        path: path.to_path_buf(),
        major,
        version,
        source: "explicit".to_string(),
    })
}
