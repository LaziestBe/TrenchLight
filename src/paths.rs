use anyhow::Result;
use std::path::PathBuf;

pub const LAUNCHER_NAME: &str = "mcl";
pub const LAUNCHER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the launcher keeps its own state.
///
/// `root`  - launcher state: settings, instances
/// `shared` - the Minecraft data store: assets, libraries, versions
///            (can point at an existing `.minecraft`)
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub shared: PathBuf,
}

impl Paths {
    pub fn at(root: PathBuf) -> Self {
        let shared = root.join("shared");
        Paths { root, shared }
    }

    pub fn default_root() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mcl")
    }

    pub fn with_shared(mut self, shared: PathBuf) -> Self {
        self.shared = shared;
        self
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.instances_dir(),
            self.assets(),
            self.objects(),
            self.indexes(),
            self.log_configs(),
            self.libraries(),
            self.versions(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        Ok(())
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.root.join("instances")
    }

    pub fn instance_dir(&self, name: &str) -> PathBuf {
        self.instances_dir().join(name)
    }

    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    pub fn assets(&self) -> PathBuf {
        self.shared.join("assets")
    }

    pub fn indexes(&self) -> PathBuf {
        self.assets().join("indexes")
    }

    pub fn objects(&self) -> PathBuf {
        self.assets().join("objects")
    }

    pub fn log_configs(&self) -> PathBuf {
        self.assets().join("log_configs")
    }

    pub fn libraries(&self) -> PathBuf {
        self.shared.join("libraries")
    }

    pub fn versions(&self) -> PathBuf {
        self.shared.join("versions")
    }

    pub fn version_dir(&self, id: &str) -> PathBuf {
        self.versions().join(id)
    }

    pub fn version_json(&self, id: &str) -> PathBuf {
        self.version_dir(id).join(format!("{id}.json"))
    }

    pub fn version_jar(&self, id: &str) -> PathBuf {
        self.version_dir(id).join(format!("{id}.jar"))
    }

    pub fn natives_dir(&self, id: &str) -> PathBuf {
        self.version_dir(id).join("natives")
    }

    pub fn manifest_cache(&self) -> PathBuf {
        self.shared.join("version_manifest_v2.json")
    }
}
