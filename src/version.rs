use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::download::Downloader;
use crate::manifest::VersionManifest;
use crate::paths::Paths;

pub type Features = HashMap<String, bool>;

/// Mojang and third-party launchers disagree about whether some numbers are
/// integers or floats (`"majorVersion": 25.0` really does occur in Forge jsons),
/// so accept both shapes.
fn de_u32_lenient<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::Number(number) => number
            .as_u64()
            .map(|v| v as u32)
            .or_else(|| number.as_f64().map(|v| v as u32))
            .unwrap_or(0),
        Value::String(text) => text.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

fn de_opt_u64_lenient<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        Value::Number(number) => number.as_u64().or_else(|| number.as_f64().map(|v| v as u64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }))
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct VersionJson {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "inheritsFrom", default)]
    pub inherits_from: Option<String>,
    #[serde(rename = "mainClass", default)]
    pub main_class: Option<String>,
    #[serde(rename = "minecraftArguments", default)]
    pub minecraft_arguments: Option<String>,
    #[serde(default)]
    pub arguments: Option<Arguments>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(rename = "assetIndex", default)]
    pub asset_index: Option<AssetIndex>,
    #[serde(rename = "javaVersion", default)]
    pub java_version: Option<JavaVersion>,
    #[serde(default)]
    pub logging: Option<HashMap<String, Logging>>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub downloads: Option<Downloads>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Arguments {
    #[serde(default)]
    pub game: Option<Vec<Value>>,
    #[serde(default)]
    pub jvm: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibDownloads>,
    #[serde(default)]
    pub natives: Option<HashMap<String, String>>,
    #[serde(default)]
    pub rules: Option<Vec<Rule>>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LibDownloads {
    #[serde(default)]
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Artifact {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64_lenient")]
    pub size: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
}

impl Artifact {
    /// Path of the file relative to the shared data store.
    pub fn relative_path(&self) -> String {
        if let Some(path) = &self.path {
            return path.clone();
        }
        let url = self.url.clone().unwrap_or_default();
        let name = url.rsplit('/').next().unwrap_or("library.jar");
        format!("extra/{name}")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsRule>,
    #[serde(default)]
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OsRule {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndex {
    pub id: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64_lenient")]
    pub size: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "totalSize", default, deserialize_with = "de_opt_u64_lenient")]
    pub total_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JavaVersion {
    #[serde(default)]
    pub component: Option<String>,
    #[serde(rename = "majorVersion", default, deserialize_with = "de_u32_lenient")]
    pub major_version: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Logging {
    pub argument: String,
    pub file: LoggingFile,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingFile {
    pub id: String,
    pub sha1: String,
    #[serde(default, deserialize_with = "de_opt_u64_lenient")]
    pub size: Option<u64>,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Downloads {
    #[serde(default)]
    pub client: Option<Artifact>,
}

pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

pub fn os_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "x86",
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        other => other,
    }
}

pub fn rules_allow(rules: Option<&Vec<Rule>>, features: &Features) -> bool {
    let Some(rules) = rules else {
        return true;
    };
    if rules.is_empty() {
        return true;
    }
    // No rules means "allowed"; with rules, the last matching rule wins.
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule, features) {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

fn rule_matches(rule: &Rule, features: &Features) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            if name != os_name() {
                return false;
            }
        }
        if let Some(arch) = &os.arch {
            if arch != os_arch() {
                return false;
            }
        }
    }
    if let Some(required) = &rule.features {
        for (key, value) in required {
            if features.get(key).copied().unwrap_or(false) != *value {
                return false;
            }
        }
    }
    true
}

pub fn maven_path(name: &str) -> Option<String> {
    let mut parts = name.split(':');
    let group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    let classifier = parts.next();
    let extension = parts.next().unwrap_or("jar");
    let file = match classifier {
        Some(classifier) => format!("{artifact}-{version}-{classifier}.{extension}"),
        None => format!("{artifact}-{version}.{extension}"),
    };
    Some(format!(
        "{}/{}/{}/{}",
        group.replace('.', "/"),
        artifact,
        version,
        file
    ))
}

/// A version with all `inheritsFrom` parents merged in.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub id: String,
    /// Version that owns the client jar (the last one in the chain that has one).
    pub base_id: String,
    pub main_class: String,
    pub game_args: Vec<Value>,
    pub jvm_args: Vec<Value>,
    pub libraries: Vec<Library>,
    pub asset_index: AssetIndex,
    pub client: Option<Artifact>,
    pub java_major: u32,
    pub logging: Option<Logging>,
    pub kind: String,
    pub chain: Vec<String>,
}

pub fn resolve(
    paths: &Paths,
    dl: &Downloader,
    manifest: &VersionManifest,
    id: &str,
) -> Result<Resolved> {
    // Walk the inheritsFrom chain, child first, then flip it to root-first.
    let mut chain: Vec<VersionJson> = Vec::new();
    let mut walk_ids: Vec<String> = Vec::new();
    let mut cursor = id.to_string();
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 16 {
            bail!("inheritsFrom chain for '{id}' is too deep");
        }
        let json = load_or_fetch(paths, dl, manifest, &cursor)?;
        let parent = json.inherits_from.clone().filter(|p| !p.is_empty());
        walk_ids.push(cursor.clone());
        chain.push(json);
        match parent {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    chain.reverse();
    walk_ids.reverse();

    let mut main_class: Option<String> = None;
    let mut game_args: Vec<Value> = Vec::new();
    let mut jvm_args: Vec<Value> = Vec::new();
    let mut legacy_args: Option<String> = None;
    let mut libraries: Vec<Library> = Vec::new();
    let mut asset_index: Option<AssetIndex> = None;
    let mut client: Option<Artifact> = None;
    let mut java_major: Option<u32> = None;
    let mut logging: Option<Logging> = None;
    let mut kind: Option<String> = None;
    let mut base_id = id.to_string();
    let mut chain_ids: Vec<String> = Vec::new();

    for (index_in_chain, json) in chain.iter().enumerate() {
        chain_ids.push(
            walk_ids
                .get(index_in_chain)
                .cloned()
                .unwrap_or_else(|| json.id.clone()),
        );

        if let Some(value) = &json.main_class {
            main_class = Some(value.clone());
        }
        if let Some(arguments) = &json.arguments {
            if let Some(game) = &arguments.game {
                game_args.extend(game.iter().cloned());
            }
            if let Some(jvm) = &arguments.jvm {
                jvm_args.extend(jvm.iter().cloned());
            }
        }
        if let Some(legacy) = &json.minecraft_arguments {
            legacy_args = Some(legacy.clone());
        }
        libraries.extend(json.libraries.iter().cloned());
        if let Some(index) = &json.asset_index {
            asset_index = Some(index.clone());
        }
        if let Some(downloads) = &json.downloads {
            if let Some(value) = &downloads.client {
                client = Some(value.clone());
                base_id = json.id.clone();
            }
        }
        if let Some(java) = &json.java_version {
            java_major = Some(java.major_version);
        }
        if let Some(log) = &json.logging {
            if let Some(entry) = log.get("client") {
                logging = Some(entry.clone());
            }
        }
        if let Some(value) = &json.kind {
            kind = Some(value.clone());
        }
    }

    // Prefer the modern `arguments.game`; fall back to the legacy string.
    if game_args.is_empty() {
        if let Some(legacy) = legacy_args {
            game_args = legacy
                .split_whitespace()
                .map(|part| Value::String(part.to_string()))
                .collect();
        }
    }

    // Later entries win for duplicate coordinates.
    let mut seen: HashSet<String> = HashSet::new();
    let mut deduped: Vec<Library> = Vec::with_capacity(libraries.len());
    for library in libraries.into_iter().rev() {
        if seen.insert(library.name.clone()) {
            deduped.push(library);
        }
    }
    deduped.reverse();

    let asset_index = asset_index.ok_or_else(|| anyhow!("'{id}' has no asset index"))?;

    Ok(Resolved {
        id: id.to_string(),
        base_id,
        main_class: main_class.unwrap_or_else(|| "net.minecraft.client.main.Main".to_string()),
        game_args,
        jvm_args,
        libraries: deduped,
        asset_index,
        client,
        java_major: java_major.unwrap_or(8),
        logging,
        kind: kind.unwrap_or_else(|| "release".to_string()),
        chain: chain_ids,
    })
}

fn load_or_fetch(
    paths: &Paths,
    dl: &Downloader,
    manifest: &VersionManifest,
    id: &str,
) -> Result<VersionJson> {
    let path = paths.version_json(id);
    if path.exists() {
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        return serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()));
    }

    let entry = manifest.find(id).ok_or_else(|| {
        anyhow!("version '{id}' is not installed in {} and is not in the Mojang manifest", paths.versions().display())
    })?;
    let text = dl
        .get_text(&entry.url)
        .with_context(|| format!("download version json for {id}"))?;
    let json: VersionJson = serde_json::from_str(&text)
        .with_context(|| format!("parse version json for {id}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Keep the raw text so nothing Mojang sends is lost.
    fs::write(&path, text)?;
    Ok(json)
}

pub fn load_local(path: &Path) -> Result<VersionJson> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(action: &str, os: Option<&str>) -> Rule {
        Rule {
            action: action.to_string(),
            os: os.map(|name| OsRule {
                name: Some(name.to_string()),
                arch: None,
            }),
            features: None,
        }
    }

    #[test]
    fn no_rules_allows() {
        assert!(rules_allow(None, &Features::new()));
        assert!(rules_allow(Some(&Vec::new()), &Features::new()));
    }

    #[test]
    fn os_rules_are_evaluated_against_this_platform() {
        let features = Features::new();
        let windows = vec![rule("allow", Some("windows"))];
        assert_eq!(rules_allow(Some(&windows), &features), os_name() == "windows");

        let linux = vec![rule("allow", Some("linux"))];
        assert_eq!(rules_allow(Some(&linux), &features), os_name() == "linux");

        let denied = vec![rule("allow", None), rule("deny", Some(os_name()))];
        assert!(!rules_allow(Some(&denied), &features));
    }

    #[test]
    fn feature_rules_default_to_false() {
        let rule = Rule {
            action: "allow".to_string(),
            os: None,
            features: Some(HashMap::from([("has_custom_resolution".to_string(), true)])),
        };
        assert!(!rules_allow(Some(&vec![rule.clone()]), &Features::new()));
        let on = Features::from([("has_custom_resolution".to_string(), true)]);
        assert!(rules_allow(Some(&vec![rule]), &on));
    }

    #[test]
    fn maven_coordinates_become_paths() {
        assert_eq!(
            maven_path("com.mojang:brigadier:1.0.18").unwrap(),
            "com/mojang/brigadier/1.0.18/brigadier-1.0.18.jar"
        );
        assert_eq!(
            maven_path("org.lwjgl:lwjgl:3.2.1:natives-windows").unwrap(),
            "org/lwjgl/lwjgl/3.2.1/lwjgl-3.2.1-natives-windows.jar"
        );
        assert!(maven_path("nonsense").is_none());
    }
}
