use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::auth::Session;
use crate::install::Plan;
use crate::instance::Instance;
use crate::java::JavaInstall;
use crate::paths::{Paths, LAUNCHER_NAME, LAUNCHER_VERSION};
use crate::version::{Features, Resolved};

pub struct Launch<'a> {
    pub paths: &'a Paths,
    pub instance: &'a Instance,
    pub resolved: &'a Resolved,
    pub plan: &'a Plan,
    pub java: &'a JavaInstall,
    pub session: &'a Session,
    pub memory_mb: u32,
    pub extra_jvm: Vec<String>,
}

pub struct Prepared {
    pub command: Command,
    pub display: String,
    pub game_dir: PathBuf,
}

fn substitute(value: &str, variables: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                if let Some(replacement) = variables.get(key) {
                    out.push_str(replacement);
                } else {
                    out.push_str(&rest[start..start + 2 + end + 1]);
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn expand(
    args: &[serde_json::Value],
    features: &Features,
    variables: &HashMap<&str, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for arg in args {
        match arg {
            serde_json::Value::String(text) => out.push(substitute(text, variables)),
            serde_json::Value::Object(object) => {
                let rules = object
                    .get("rules")
                    .and_then(|value| serde_json::from_value::<Vec<crate::version::Rule>>(value.clone()).ok());
                if !crate::version::rules_allow(rules.as_ref(), features) {
                    continue;
                }
                // Mojang uses "value"; some third-party jsons use "values".
                let value = object.get("value").or_else(|| object.get("values"));
                match value {
                    Some(serde_json::Value::String(text)) => {
                        out.push(substitute(text, variables))
                    }
                    Some(serde_json::Value::Array(items)) => {
                        for item in items {
                            if let Some(text) = item.as_str() {
                                out.push(substitute(text, variables));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

fn quote(argument: &str) -> String {
    if argument.is_empty() || argument.contains(' ') || argument.contains('\t') {
        format!("\"{argument}\"")
    } else {
        argument.to_string()
    }
}

impl<'a> Launch<'a> {
    pub fn prepare(&self) -> Result<Prepared> {
        let game_dir = self.paths.instance_dir(&self.instance.name);
        let assets_root = self.paths.assets();

        let mut features: Features = HashMap::new();
        features.insert("is_demo_user".to_string(), false);
        features.insert(
            "has_custom_resolution".to_string(),
            self.instance.width.is_some() && self.instance.height.is_some(),
        );
        features.insert("has_quick_plays_support".to_string(), false);
        features.insert("is_quick_play_singleplayer".to_string(), false);
        features.insert("is_quick_play_multiplayer".to_string(), false);
        features.insert("is_quick_play_realms".to_string(), false);

        let separator = if cfg!(windows) { ";" } else { ":" };
        let classpath = self
            .plan
            .classpath
            .iter()
            .chain(std::iter::once(&self.plan.client_jar))
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(separator);

        let log_config = self
            .plan
            .log_config
            .as_ref()
            .filter(|path| path.exists())
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        let mut variables: HashMap<&str, String> = HashMap::new();
        variables.insert("auth_player_name", self.session.username.clone());
        variables.insert("version_name", self.resolved.id.clone());
        variables.insert("game_directory", game_dir.display().to_string());
        variables.insert("assets_root", assets_root.display().to_string());
        variables.insert(
            "assets_index_name",
            self.resolved.asset_index.id.clone(),
        );
        variables.insert("auth_uuid", self.session.uuid.clone());
        variables.insert("auth_access_token", self.session.access_token.clone());
        variables.insert("auth_session", format!("token:{}:{}", self.session.access_token, self.session.uuid));
        variables.insert("user_type", self.session.user_type.clone());
        variables.insert("version_type", self.resolved.kind.clone());
        variables.insert(
            "natives_directory",
            self.plan.natives.display().to_string(),
        );
        variables.insert("library_directory", self.paths.libraries().display().to_string());
        variables.insert("classpath_separator", separator.to_string());
        variables.insert("launcher_name", LAUNCHER_NAME.to_string());
        variables.insert("launcher_version", LAUNCHER_VERSION.to_string());
        variables.insert("classpath", classpath);
        variables.insert("clientid", self.session.client_id.clone());
        variables.insert("auth_xuid", self.session.xuid.clone());
        variables.insert("path", log_config);
        variables.insert(
            "resolution_width",
            self.instance.width.unwrap_or(854).to_string(),
        );
        variables.insert(
            "resolution_height",
            self.instance.height.unwrap_or(480).to_string(),
        );
        variables.insert("quickPlayPath", String::new());
        variables.insert("quickPlaySingleplayer", String::new());
        variables.insert("quickPlayMultiplayer", String::new());
        variables.insert("quickPlayRealms", String::new());
        variables.insert("game_assets", assets_root.display().to_string());
        variables.insert("user_properties", "{}".to_string());

        let mut jvm: Vec<String> = Vec::new();
        jvm.push(format!("-Xms{}M", (self.memory_mb / 2).max(512)));
        jvm.push(format!("-Xmx{}M", self.memory_mb));
        jvm.extend(self.extra_jvm.iter().cloned());

        // The launcher, not the version json, owns the log4j configuration.
        if let Some(logging) = &self.resolved.logging {
            if self
                .plan
                .log_config
                .as_ref()
                .map(|path| path.exists())
                .unwrap_or(false)
            {
                jvm.push(substitute(&logging.argument, &variables));
            }
        }

        if self.resolved.jvm_args.is_empty() {
            // Ancient versions ship no jvm arguments at all.
            jvm.push(format!(
                "-Djava.library.path={}",
                self.plan.natives.display()
            ));
            jvm.push("-cp".to_string());
            jvm.push(variables["classpath"].clone());
        } else {
            jvm.extend(expand(&self.resolved.jvm_args, &features, &variables));
        }

        let game = expand(&self.resolved.game_args, &features, &variables);
        if game.is_empty() {
            bail!("version '{}' produced no game arguments", self.resolved.id);
        }

        let mut command = Command::new(&self.java.path);
        command.current_dir(&game_dir);
        for argument in &jvm {
            command.arg(argument);
        }
        command.arg(&self.resolved.main_class);
        for argument in &game {
            command.arg(argument);
        }

        let mut display = String::new();
        display.push_str(&quote(&self.java.path.display().to_string()));
        for argument in jvm.iter().chain(std::iter::once(&self.resolved.main_class)).chain(game.iter()) {
            display.push(' ');
            display.push_str(&quote(argument));
        }

        Ok(Prepared {
            command,
            display,
            game_dir,
        })
    }
}

/// Attaches the child to the launcher's console so logs are visible.
pub fn inherit_console(command: &mut Command) {
    // Default behaviour already inherits stdio.
    let _ = command;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs
            .iter()
            .map(|(key, value)| (*key, value.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_known_variables_and_keeps_unknown_ones() {
        let table = vars(&[("auth_player_name", "Steve")]);
        assert_eq!(
            substitute("--username ${auth_player_name}", &table),
            "--username Steve"
        );
        assert_eq!(substitute("${nope}", &table), "${nope}");
        assert_eq!(substitute("plain", &table), "plain");
        assert_eq!(substitute("${", &table), "${");
    }

    #[test]
    fn expands_value_values_and_rules() {
        let table = vars(&[("x", "1")]);
        let args: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                "--a",
                {"rules": [], "values": ["--b", "${x}"]},
                {"rules": [{"action": "allow", "os": {"name": "definitely-not-this-os"}}], "value": "--never"},
                {"rules": [{"action": "allow"}], "value": ["--c", "${x}"]}
            ]"#,
        )
        .unwrap();

        let out = expand(&args, &Features::new(), &table);
        assert_eq!(out, vec!["--a", "--b", "1", "--c", "1"]);
    }
}
