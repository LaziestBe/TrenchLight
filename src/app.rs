//! What a front-end needs, and nothing about how it draws.
//!
//! `mcl` was CLI-only, so every path, setting and download step lived inside
//! `main.rs`. The Trenchlight window needs the same steps with real progress
//! and without `println!`, so they are lifted here. `main.rs` keeps its own
//! `Ctx` for now; the two agree because `App::open` mirrors `Ctx::load`
//! deliberately — if one changes, change both.

use anyhow::{bail, Context, Result};
use indicatif::{MultiProgress, ProgressDrawTarget};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::download::Downloader;
use crate::instance::Instance;
use crate::java::{self, JavaInstall};
use crate::launch::Prepared;
use crate::paths::Paths;
use crate::settings::Settings;
use crate::version::{self, Features};
use crate::{auth, install, launch, manifest, skin, util};

/// Heap and window limits are enforced here, not in the window: a GUI that
/// skips its own validation must still not be able to ask Java for 8 MB.
const MIN_MEMORY_MB: u32 = 512;
const MAX_MEMORY_MB: u32 = 32_768;
const MAX_USER_CHARS: usize = 32;
const MAX_JVM_ARGS: usize = 32;
const MAX_JVM_ARG_CHARS: usize = 256;
const MIN_WINDOW: u32 = 320;
const MAX_WINDOW: u32 = 7_680;

pub struct App {
    /// The store this `App` was opened against (`--shared` already applied).
    pub paths: Paths,
    /// Settings are behind a lock because every method takes `&self`: the
    /// window holds one `Arc<App>` and reads settings from worker threads while
    /// a save may be landing.
    pub settings: RwLock<Settings>,
    /// `--shared` outranks settings.json, so a save in the window must not move
    /// the store when the process was started pointed at another one.
    shared_from_flag: bool,
    java: Vec<JavaInstall>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JavaView {
    pub major: u32,
    pub version: String,
    pub source: String,
    pub path: String,
}

/// The offline profile's skin, as the Account screen draws it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SkinView {
    /// The launcher's own copy of the PNG; empty when nothing was imported.
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// The PNG itself, base64: the page cannot read files and a skin is small.
    pub data: String,
    /// Installations where the pack is on disk *and* switched on.
    pub applied: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct InstanceView {
    pub name: String,
    pub version: String,
    pub user: String,
    pub memory_mb: u32,
    pub java: Option<String>,
    pub last_played: Option<String>,
    pub ago: String,
    pub mods: u32,
    pub worlds: u32,
    pub bytes: u64,
    pub size: String,
    pub missing: bool,
    pub installed: bool,
    pub dir: String,
    /// What the instance pins itself, so the window can show an override
    /// without asking a second time. `None` means "inherited from settings".
    pub jvm_args: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub user_set: Option<String>,
    pub memory_set: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SettingsView {
    /// Effective offline user (settings, else `"Player"`).
    pub user: String,
    /// The uuid the launch will hand to Java for `user`; version 3, offline.
    pub uuid: String,
    pub memory_mb: u32,
    pub jvm_args: Vec<String>,
    /// The data store a launch would use, resolved.
    pub shared: String,
    /// Launcher state, e.g. `%APPDATA%\mcl`.
    pub root: String,
    /// `settings.json` itself.
    pub config: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModRow {
    /// File name with whatever extension it really has: `x.jar` or
    /// `x.jar.disabled`.
    pub file: String,
    pub bytes: u64,
    pub size: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorldRow {
    pub name: String,
    pub last_played: String,
    pub bytes: u64,
    pub size: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstanceDetail {
    pub view: InstanceView,
    pub jvm_args: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Raw per-instance override; `None` when the instance inherits settings.
    pub user_set: Option<String>,
    pub memory_set: Option<u32>,
    pub mods: Vec<ModRow>,
    pub worlds: Vec<WorldRow>,
}

/// What the Settings screen sends. Every field is optional: absent means
/// "leave it alone", `Some("")` on `shared` means "go back to `root/shared`".
#[derive(Default, Deserialize)]
pub struct SettingsPatch {
    pub user: Option<String>,
    pub memory_mb: Option<u32>,
    pub jvm_args: Option<Vec<String>>,
    pub shared: Option<String>,
}

/// What the instance sheet sends. `Some(0)` on `memory_mb`/`width`/`height`
/// and `Some("")` on `user` clear an override; absent leaves it alone.
#[derive(Default, Deserialize)]
pub struct InstancePatch {
    pub user: Option<String>,
    pub memory_mb: Option<u32>,
    pub jvm_args: Option<Vec<String>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl App {
    /// Same resolution order as the CLI: `--root`, then settings, then
    /// `%APPDATA%\mcl`; `--shared` wins over `settings.shared_dir`.
    pub fn open(root: Option<PathBuf>, shared: Option<PathBuf>) -> Result<Self> {
        let root = root.unwrap_or_else(Paths::default_root);
        let base = Paths::at(root);
        let settings = Settings::load(&base.settings_file());
        let shared_from_flag = shared.is_some();
        let shared = shared.or_else(|| settings.shared());
        let mut paths = base;
        if let Some(shared) = shared {
            paths = paths.with_shared(shared);
        }
        paths.ensure()?;
        let java = java::detect(&paths);
        Ok(App {
            paths,
            settings: RwLock::new(settings),
            shared_from_flag,
            java,
        })
    }

    /// Settings as they are right now. A poisoned lock still holds the last
    /// good value, so it is read rather than propagated.
    fn read_settings(&self) -> std::sync::RwLockReadGuard<'_, Settings> {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The store the next launch will use. Public so a front end can put game
    /// files where the launch will look for them.
    pub fn store_paths(&self) -> Paths {
        self.effective_paths()
    }
    /// The store the next launch will use: `--shared` when one was given,
    /// otherwise whatever `settings.json` says now. `self.paths` stays the
    /// open-time snapshot, so a store moved in the window applies to launches
    /// immediately without re-opening the `App`.
    fn effective_paths(&self) -> Paths {
        if self.shared_from_flag {
            return self.paths.clone();
        }
        match self.read_settings().shared() {
            Some(shared) => self.paths.clone().with_shared(shared),
            None => self.paths.clone().with_shared(self.paths.root.join("shared")),
        }
    }

    pub fn user(&self) -> String {
        self.read_settings().user()
    }

    pub fn memory_mb(&self) -> u32 {
        self.read_settings().memory_mb()
    }

    pub fn shared_dir(&self) -> String {
        self.effective_paths().shared.display().to_string()
    }

    /// Everything the Settings screen draws, plus the uuid the launch will use.
    pub fn settings_view(&self) -> SettingsView {
        let (user, memory_mb, jvm_args) = {
            let settings = self.read_settings();
            (settings.user(), settings.memory_mb(), settings.jvm_args.clone())
        };
        SettingsView {
            uuid: auth::offline_uuid(&user),
            user,
            memory_mb,
            jvm_args,
            shared: self.shared_dir(),
            root: self.paths.root.display().to_string(),
            config: self.paths.settings_file().display().to_string(),
        }
    }

    /// Validate, then write, then remember: a rejected patch leaves both the
    /// file and the running window exactly as they were.
    pub fn save_settings(&self, patch: SettingsPatch) -> Result<()> {
        let file = self.paths.settings_file();
        let mut guard = self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next = guard.clone();

        if let Some(user) = &patch.user {
            next.default_user = Some(clean_user(user)?);
        }
        if let Some(memory_mb) = patch.memory_mb {
            next.memory_mb = Some(memory_mb.clamp(MIN_MEMORY_MB, MAX_MEMORY_MB));
        }
        if let Some(jvm_args) = &patch.jvm_args {
            next.jvm_args = clean_jvm_args(jvm_args)?;
        }
        if let Some(shared) = &patch.shared {
            let shared = shared.trim();
            if shared.is_empty() {
                next.shared_dir = None;
            } else {
                if !Path::new(shared).is_dir() {
                    bail!("{shared} is not a directory — the shared store must already exist");
                }
                next.shared_dir = Some(shared.to_string());
            }
        }

        next.save(&file)?;
        *guard = next;
        Ok(())
    }

    /// One screen's worth of instance data: the list row plus what the sheet
    /// edits, plus the mods and worlds the tabs list.
    pub fn instance_detail(&self, name: &str) -> Result<InstanceDetail> {
        let instance = Instance::load(&self.paths, name)?;
        let dir = Instance::dir(&self.paths, &instance.name);
        let (fallback_user, fallback_memory) = {
            let settings = self.read_settings();
            (settings.user(), settings.memory_mb())
        };
        let view = self.view_of(&instance, &self.versions(), &fallback_user, fallback_memory);
        Ok(InstanceDetail {
            jvm_args: instance.jvm_args.clone(),
            width: instance.width,
            height: instance.height,
            user_set: own_user(&instance),
            memory_set: instance.memory_mb,
            mods: mod_rows(&dir.join("mods")),
            worlds: world_rows(&dir.join("saves")),
            view,
        })
    }

    /// Save what the instance sheet changed. Only the fields in the patch are
    /// touched, so `created` and `last_played` survive.
    pub fn save_instance(&self, name: &str, patch: InstancePatch) -> Result<()> {
        crate::instance::validate_name(name)?;
        let mut instance = Instance::load(&self.paths, name)?;

        if let Some(user) = &patch.user {
            let user = user.trim();
            instance.user = if user.is_empty() {
                None
            } else {
                Some(clean_user(user)?)
            };
        }
        if let Some(memory_mb) = patch.memory_mb {
            instance.memory_mb = if memory_mb == 0 {
                None
            } else {
                Some(memory_mb.clamp(MIN_MEMORY_MB, MAX_MEMORY_MB))
            };
        }
        if let Some(jvm_args) = &patch.jvm_args {
            instance.jvm_args = clean_jvm_args(jvm_args)?;
        }
        if let Some(width) = patch.width {
            instance.width = if width == 0 {
                None
            } else {
                Some(width.clamp(MIN_WINDOW, MAX_WINDOW))
            };
        }
        if let Some(height) = patch.height {
            instance.height = if height == 0 {
                None
            } else {
                Some(height.clamp(MIN_WINDOW, MAX_WINDOW))
            };
        }

        instance
            .save(&self.paths)
            .with_context(|| format!("saving {name}"))?;
        Ok(())
    }

    /// Flip a mod between `x.jar` and `x.jar.disabled`, in place.
    ///
    /// `file` must be a bare name: the window sends back what `instance_detail`
    /// listed, and nothing else is allowed to reach the filesystem.
    pub fn set_mod_enabled(&self, name: &str, file: &str, enabled: bool) -> Result<()> {
        let instance = Instance::load(&self.paths, name)?;
        let file = clean_mod_file(file)?;
        let dir = Instance::dir(&self.paths, &instance.name).join("mods");
        let from = dir.join(&file);

        let disabled = file.to_ascii_lowercase().ends_with(".jar.disabled");
        let target_name = if enabled {
            if !disabled {
                return Ok(());
            }
            // `has_suffix` already proved the length, and the suffix is ASCII.
            file[..file.len() - ".disabled".len()].to_string()
        } else {
            if disabled {
                return Ok(());
            }
            format!("{file}.disabled")
        };

        let to = dir.join(&target_name);
        if to.exists() {
            bail!(
                "{} already exists — remove or rename it first",
                to.display()
            );
        }
        if !from.is_file() {
            bail!("no mod named '{file}' in {}", dir.display());
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("rename {} to {}", from.display(), to.display()))?;
        Ok(())
    }

    /// Import a version folder as a new instance, taking the mods and configs
    /// launcher-style installs keep beside the version.
    ///
    /// TLauncher and friends store each modpack's `mods`, `config`, `saves` and
    /// so on inside `versions/<pack>/`, with a fallback copy at the store root.
    /// The version folder wins; the root fills the gaps. The result is a
    /// self-contained isolated instance, and the version folder is untouched.
    pub fn import_instance(
        &self,
        version: &str,
        name: &str,
        with_mods: bool,
    ) -> Result<(String, Vec<(String, u64)>)> {
        let paths = &self.paths;
        let json = paths.version_json(version);
        if !json.exists() {
            bail!(
                "'{version}' is not in {}. Point the launcher at the store that has it.",
                paths.versions().display()
            );
        }
        let resolved = version::load_local(&json)?;
        if let Some(base) = resolved.inherits_from.as_deref() {
            if !paths.version_json(base).exists() {
                bail!("'{version}' inherits from '{base}', which is not installed");
            }
        }

        let instance = Instance::create(paths, name, version, None, None, None)?;
        let game_dir = Instance::dir(paths, &instance.name);
        let mut copied: Vec<(String, u64)> = Vec::new();
        if with_mods {
            let version_dir = paths.version_dir(version);
            for item in [
                "mods",
                "config",
                "defaultconfigs",
                "scripts",
                "kubejs",
                "resourcepacks",
                "shaderpacks",
                "saves",
                "options.txt",
                "optionsof.txt",
                "optionsshaders.txt",
                "servers.dat",
            ] {
                let source = [version_dir.join(item), paths.shared.join(item)]
                    .into_iter()
                    .find(|candidate| candidate.exists());
                let Some(source) = source else {
                    continue;
                };
                let dest = game_dir.join(item);
                let bytes = if source.is_dir() {
                    util::copy_dir(&source, &dest)?
                } else {
                    std::fs::copy(&source, &dest)
                        .with_context(|| format!("copy {}", source.display()))?;
                    std::fs::metadata(&dest).map(|meta| meta.len()).unwrap_or(0)
                };
                if bytes > 0 {
                    copied.push((item.to_string(), bytes));
                }
            }
        }
        Ok((instance.name, copied))
    }

    /// Move one world into `<root>\trash\worlds\<instance>\`, never erasing it.
    /// A mis-click then costs disk space, not a build.
    pub fn trash_world(&self, name: &str, world: &str) -> Result<String> {
        let (dir, world, instance_dir) = self.world_dir(name, world)?;
        let bin = self
            .paths
            .root
            .join("trash")
            .join("worlds")
            .join(instance_dir.file_name().unwrap_or_default());
        std::fs::create_dir_all(&bin).with_context(|| format!("create {}", bin.display()))?;
        let stamp = util::now_iso().replace([' ', ':'], "-");
        let mut target = bin.join(format!("{world} {stamp}"));
        let mut n = 2;
        while target.exists() {
            target = bin.join(format!("{world} {stamp} ({n})"));
            n += 1;
        }
        std::fs::rename(&dir, &target)
            .with_context(|| format!("move {} to {}", dir.display(), target.display()))?;
        Ok(target.display().to_string())
    }

    /// Copy one world into `<root>\backups\<instance>\`, outside `saves\` so
    /// the game does not list the copy as a second world.
    pub fn backup_world(&self, name: &str, world: &str) -> Result<(String, u64)> {
        let (dir, world, instance_dir) = self.world_dir(name, world)?;
        let bin = self
            .paths
            .root
            .join("backups")
            .join(instance_dir.file_name().unwrap_or_default());
        std::fs::create_dir_all(&bin).with_context(|| format!("create {}", bin.display()))?;
        let stamp = util::now_iso().replace([' ', ':'], "-");
        let mut target = bin.join(format!("{world} {stamp}"));
        let mut n = 2;
        while target.exists() {
            target = bin.join(format!("{world} {stamp} ({n})"));
            n += 1;
        }
        let bytes = util::copy_dir(&dir, &target)
            .with_context(|| format!("copy {} to {}", dir.display(), target.display()))?;
        Ok((target.display().to_string(), bytes))
    }

    /// Resolve `<instance>\saves\<world>`, refusing anything that is not a real
    /// world folder: the window sends back a name the detail listed.
    fn world_dir(&self, name: &str, world: &str) -> Result<(PathBuf, String, PathBuf)> {
        let instance = Instance::load(&self.paths, name)?;
        let world = clean_world(world)?;
        let instance_dir = Instance::dir(&self.paths, &instance.name);
        let dir = instance_dir.join("saves").join(&world);
        if !dir.join("level.dat").is_file() {
            bail!("no world named '{world}' in this installation");
        }
        Ok((dir, world, instance_dir))
    }

    /// One row per runtime *version*. The same JDK is often reachable twice
    /// (PATH and a Mojang runtime), and listing it twice reads as a bug.
    pub fn java_runtimes(&self) -> Vec<JavaView> {
        let mut out: Vec<JavaView> = Vec::new();
        for j in &self.java {
            if out
                .iter()
                .any(|v| v.major == j.major && v.version == j.version)
            {
                continue;
            }
            out.push(JavaView {
                major: j.major,
                version: j.version.clone(),
                source: j.source.clone(),
                path: j.path.display().to_string(),
            });
        }
        out
    }

    pub fn instances(&self) -> Result<Vec<InstanceView>> {
        let (fallback_user, fallback_memory) = {
            let settings = self.read_settings();
            (settings.user(), settings.memory_mb())
        };
        let installed = self.versions();
        let mut out = Vec::new();
        for instance in Instance::list(&self.paths)? {
            out.push(self.view_of(&instance, &installed, &fallback_user, fallback_memory));
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }

    fn view_of(
        &self,
        instance: &Instance,
        installed: &[String],
        fallback_user: &str,
        fallback_memory: u32,
    ) -> InstanceView {
        let dir = Instance::dir(&self.paths, &instance.name);
        let scan = scan_dir(&dir);
        InstanceView {
            user: instance.user(fallback_user),
            memory_mb: instance.memory_mb(fallback_memory),
            java: instance.java.clone(),
            ago: instance
                .last_played
                .as_deref()
                .map(human_age)
                .unwrap_or_else(|| "never".into()),
            last_played: instance.last_played.clone(),
            mods: scan.mods,
            worlds: scan.worlds,
            bytes: scan.bytes,
            size: human_bytes(scan.bytes),
            missing: !dir.is_dir(),
            installed: installed.iter().any(|v| v == &instance.version),
            dir: dir.display().to_string(),
            version: instance.version.clone(),
            name: instance.name.clone(),
            jvm_args: instance.jvm_args.clone(),
            width: instance.width,
            height: instance.height,
            user_set: own_user(instance),
            memory_set: instance.memory_mb,
        }
    }

    /// Everything up to the point where the game process exists: version
    /// resolve, Java pick, and any downloads this instance still needs.
    /// Blocking — call it from a worker thread.
    pub fn prepare_launch(&self, name: &str) -> Result<Prepared> {
        self.prepare_launch_with(name, &mut |_| {}, None)
    }

    /// Same, but reports each phase so a UI can show what is happening, and
    /// asks `stopped` between phases so a Cancel click does not have to wait
    /// for a download it no longer wants.
    pub fn prepare_launch_with(
        &self,
        name: &str,
        on: &mut dyn FnMut(&str),
        stopped: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Result<Prepared> {
        let halt = |on: &mut dyn FnMut(&str)| -> Result<()> {
            match stopped {
                Some(check) if check() => {
                    on("Cancelling");
                    bail!("launch cancelled")
                }
                _ => Ok(()),
            }
        };

        on("Reading the instance");
        halt(&mut *on)?;
        let paths = self.effective_paths();
        let instance = Instance::load(&paths, name)?;

        on("Resolving the version");
        halt(&mut *on)?;
        let dl = Downloader::new();
        let manifest = manifest::load(&paths, &dl, false)?;
        let resolved = version::resolve(&paths, &dl, &manifest, &instance.version)?;
        let features = Features::new();
        let plan = install::plan(&paths, &resolved, &features);

        on("Choosing Java");
        halt(&mut *on)?;
        let java = match instance.java.as_ref() {
            Some(path) => java::from_path(Path::new(path))?,
            None => java::pick(&self.java, resolved.java_major)
                .cloned()
                .with_context(|| {
                    format!(
                        "no Java {} or newer found — install one, or set it on the instance",
                        resolved.java_major
                    )
                })?,
        };

        on("Checking files and assets");
        halt(&mut *on)?;
        let mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        install::install(&paths, &dl, &resolved, &plan, &mp, stopped)?;

        on("Starting Java");
        halt(&mut *on)?;
        let (fallback_user, memory, mut extra_jvm) = {
            let settings = self.read_settings();
            (settings.user(), settings.memory_mb(), settings.jvm_args.clone())
        };
        // Global arguments first, then the instance's own: later flags win in
        // Java, so a per-instance override beats the shared list.
        extra_jvm.extend(instance.jvm_args.iter().cloned());
        let session = auth::offline(&instance.user(&fallback_user));
        launch::Launch {
            paths: &paths,
            instance: &instance,
            resolved: &resolved,
            plan: &plan,
            java: &java,
            session: &session,
            memory_mb: instance.memory_mb(memory),
            extra_jvm,
        }
        .prepare()
    }

    /// Make sure one Minecraft version is fully present in the data store.
    ///
    /// The Horizon front end needs real game files on disk before its preparer
    /// can convert them, and nobody should have to install Minecraft by hand
    /// first. Returns how many files were missing; 0 means it was already
    /// complete and nothing was downloaded.
    pub fn install_version(
        &self,
        id: &str,
        on: &mut dyn FnMut(&str),
        stopped: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Result<usize> {
        let halt = |on: &mut dyn FnMut(&str)| -> Result<()> {
            match stopped {
                Some(check) if check() => {
                    on("Cancelling");
                    bail!("install cancelled")
                }
                _ => Ok(()),
            }
        };

        let paths = self.effective_paths();
        on(&format!("Reading the Minecraft manifest for {id}"));
        halt(&mut *on)?;
        let dl = Downloader::new();
        let manifest = manifest::load(&paths, &dl, false)?;
        on(&format!("Resolving {id}"));
        halt(&mut *on)?;
        let resolved = version::resolve(&paths, &dl, &manifest, id)?;
        let features = Features::new();
        let plan = install::plan(&paths, &resolved, &features);

        let missing = install::missing_files(&paths, &plan);
        if missing.is_empty() {
            on(&format!("{id} is already in the data store"));
            return Ok(0);
        }
        let total = missing.len();
        on(&format!("Downloading {total} files for {id}"));
        halt(&mut *on)?;
        let mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        install::install(&paths, &dl, &resolved, &plan, &mp, stopped)?;
        halt(&mut *on)?;
        Ok(total)
    }

    /// Record that a dive happened. Errors are returned, not swallowed: a
    /// launcher that quietly loses play history is worse than one that says so.
    pub fn mark_played(&self, name: &str) -> Result<()> {
        let mut instance = Instance::load(&self.paths, name)
            .with_context(|| format!("recording the dive for {name}"))?;
        instance.last_played = Some(util::now_iso());
        instance
            .save(&self.paths)
            .with_context(|| format!("saving {name}"))?;
        Ok(())
    }

    /// Versions already in the shared store, newest first as far as the ids
    /// allow. A version that is not here still works — Play downloads it — so
    /// this is a suggestion list, not a limit.
    pub fn versions(&self) -> Vec<String> {
        let dir = self.effective_paths().versions();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| !name.is_empty() && !name.starts_with('.'))
            // A folder without `<id>.json` is a leftover, not a version that
            // can be launched; keep it out of the suggestions.
            .filter(|name| dir.join(name).join(format!("{name}.json")).is_file())
            .collect();
        out.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
        out
    }

    /// Make a new instance: its own folder, optionally seeded from another one.
    pub fn create_instance(
        &self,
        name: &str,
        version: &str,
        copy_from: Option<&str>,
    ) -> Result<String> {
        let instance = Instance::create_from(&self.paths, name.trim(), Some(version), copy_from)?;
        Ok(instance.name)
    }

    /// Delete an instance by moving it into `<root>\trash\`, never by erasing it.
    pub fn trash_instance(&self, name: &str) -> Result<String> {
        let moved = Instance::trash(&self.paths, name)?;
        Ok(moved.display().to_string())
    }

    /// The one copy of the skin the launcher owns, whatever the player name is
    /// now. Keeping it per name would lose the art on every rename.
    pub fn skin_file(&self) -> PathBuf {
        self.paths.root.join("skins").join("skin.png")
    }

    /// What the Account screen shows: the stored skin, or nothing.
    pub fn skin_view(&self) -> SkinView {
        let file = self.skin_file();
        let Ok(bytes) = std::fs::read(&file) else {
            return SkinView::default();
        };
        let (width, height) = skin::size(&bytes).unwrap_or((0, 0));
        let applied = Instance::list(&self.paths)
            .map(|list| {
                list.into_iter()
                    .filter(|instance| skin::is_live(&Instance::dir(&self.paths, &instance.name)))
                    .map(|instance| instance.name)
                    .collect()
            })
            .unwrap_or_default();
        SkinView {
            path: file.display().to_string(),
            width,
            height,
            bytes: bytes.len() as u64,
            data: skin::base64(&bytes),
            applied,
        }
    }

    /// Import a skin and put it on every installation. There is no upload to
    /// anything: the file is copied under the launcher root, and each
    /// installation gets a resource pack that replaces the default player
    /// texture, which is the only way an offline client can show one.
    pub fn set_skin(&self, source: &Path) -> Result<String> {
        let art = skin::read(source)?;
        let dir = self.paths.root.join("skins");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(self.skin_file(), &art.bytes)
            .with_context(|| format!("writing {}", self.skin_file().display()))?;

        let mut applied = Vec::new();
        let mut failed = Vec::new();
        for instance in Instance::list(&self.paths)? {
            let game_dir = Instance::dir(&self.paths, &instance.name);
            match skin::write_pack(&game_dir, &art.bytes) {
                Ok(()) => applied.push(instance.name),
                Err(_) => failed.push(instance.name),
            }
        }
        if applied.is_empty() {
            bail!(
                "the skin is saved but no installation took it{}",
                if failed.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", failed.join(", "))
                }
            );
        }
        let mut note = format!(
            "Skin {}x{} applied to {}.",
            art.width,
            art.height,
            applied.join(", ")
        );
        if !failed.is_empty() {
            note.push_str(&format!(" Could not write {}.", failed.join(", ")));
        }
        Ok(note)
    }

    /// Take the skin off every installation. The launcher's copy stays, so
    /// putting it back is a click rather than another file dialog.
    pub fn clear_skin(&self) -> Result<String> {
        let mut cleared = Vec::new();
        for instance in Instance::list(&self.paths)? {
            let game_dir = Instance::dir(&self.paths, &instance.name);
            if skin::remove_pack(&game_dir).is_ok() {
                cleared.push(instance.name);
            }
        }
        if cleared.is_empty() {
            bail!("no installation had the skin pack");
        }
        Ok(format!("Skin removed from {}.", cleared.join(", ")))
    }
}

/// Sort key for version ids: numbers compare as numbers, so `1.10` sits above
/// `1.9` and `26.3` above `1.21.11`, while snapshots sort on their text.
fn version_key(id: &str) -> (Vec<u64>, String) {
    let numbers: Vec<u64> = id
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    (numbers, id.to_string())
}

struct Scan {
    mods: u32,
    worlds: u32,
    bytes: u64,
}

/// Shallow on purpose: the instance folder holds saves, mods and logs, not the
/// 12 GB store. The walk stops at a fixed budget so a pathological folder can
/// never hang the window.
fn scan_dir(dir: &Path) -> Scan {
    Scan {
        mods: count_mods(&dir.join("mods")),
        worlds: count_worlds(&dir.join("saves")),
        bytes: tree_size(dir),
    }
}

/// Every file a mod can be: enabled (`.jar`) or installed but off
/// (`.jar.disabled`). The two counts agree with the sheet's list.
fn count_mods(dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(is_mod_name)
                .unwrap_or(false)
        })
        .count() as u32
}

/// A world is a folder with a `level.dat` in it; the other folders under
/// `saves` are half-made or leftovers and the sheet does not list them.
fn count_worlds(dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| e.path().join("level.dat").is_file())
        .count() as u32
}

/// Recursive size with a fixed budget, so a huge world can be measured without
/// the window looking hung.
fn tree_size(dir: &Path) -> u64 {
    let mut bytes = 0u64;
    let mut budget = 20_000usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        if budget == 0 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            budget = budget.saturating_sub(1);
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                bytes += meta.len();
            }
        }
    }
    bytes
}

fn mod_rows(dir: &Path) -> Vec<ModRow> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<ModRow> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|entry| {
            let file = entry.file_name().into_string().ok()?;
            let enabled = if has_suffix(&file, ".jar") {
                true
            } else if has_suffix(&file, ".jar.disabled") {
                false
            } else {
                return None;
            };
            let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            Some(ModRow {
                file,
                bytes,
                size: human_bytes(bytes),
                enabled,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a.file
            .to_lowercase()
            .cmp(&b.file.to_lowercase())
            .then_with(|| a.file.cmp(&b.file))
    });
    rows
}

fn world_rows(dir: &Path) -> Vec<WorldRow> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<WorldRow> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let level = entry.path().join("level.dat");
            let modified = std::fs::metadata(&level).ok()?.modified().ok()?;
            let bytes = tree_size(&entry.path());
            Some(WorldRow {
                name,
                // Same wall-clock frame `mark_played` writes, so both read
                // back through `human_age` identically.
                last_played: human_age(&util::iso_from_system_time(modified)),
                bytes,
                size: human_bytes(bytes),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// The instance's own user, or `None` when it inherits settings. An override
/// of `""` (or blanks) means the same as absent.
fn own_user(instance: &Instance) -> Option<String> {
    instance
        .user
        .clone()
        .filter(|user| !user.trim().is_empty())
}

fn clean_user(raw: &str) -> Result<String> {
    let user = raw.trim();
    if user.is_empty() {
        bail!("a user name is required");
    }
    if user.chars().count() > MAX_USER_CHARS {
        bail!("user name is too long (max {MAX_USER_CHARS} characters)");
    }
    if user.chars().any(char::is_control) {
        bail!("user name cannot contain control characters");
    }
    Ok(user.to_string())
}

/// Trimmed, blanks dropped, and bounded so a stray paste cannot build a
/// kilometre-long command line.
fn clean_jvm_args(raw: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for argument in raw {
        let argument = argument.trim();
        if argument.is_empty() {
            continue;
        }
        if out.len() >= MAX_JVM_ARGS {
            bail!("too many JVM arguments (max {MAX_JVM_ARGS})");
        }
        if argument.chars().count() > MAX_JVM_ARG_CHARS {
            bail!("a JVM argument is too long (max {MAX_JVM_ARG_CHARS} characters)");
        }
        out.push(argument.to_string());
    }
    Ok(out)
}

/// Only a bare file name that names a mod: no separators, no parent hops, no
/// drive, nothing the window could use to reach another folder.
fn clean_mod_file(file: &str) -> Result<String> {
    let name = file.trim();
    if name.is_empty() {
        bail!("a mod file name is required");
    }
    if name != file {
        bail!("'{file}' is not a bare file name");
    }
    if name.contains(['/', '\\', ':', '\0']) || name.contains("..") || name == "." {
        bail!("'{file}' is not a bare file name");
    }
    if !has_suffix(name, ".jar") && !has_suffix(name, ".jar.disabled") {
        bail!("'{file}' is neither .jar nor .jar.disabled");
    }
    Ok(name.to_string())
}

fn is_mod_name(name: &str) -> bool {
    has_suffix(name, ".jar") || has_suffix(name, ".jar.disabled")
}

/// A world is addressed by its folder name under `saves\`, so the only thing
/// accepted is one path segment: no separators, no `..`, no drive letter.
fn clean_world(world: &str) -> Result<String> {
    let name = world.trim();
    if name.is_empty() {
        bail!("a world name is required");
    }
    if name != world {
        bail!("'{world}' is not a bare folder name");
    }
    if name.contains(['/', '\\', ':', '\0']) || name.contains("..") || name == "." {
        bail!("'{world}' is not a bare folder name");
    }
    Ok(name.to_string())
}
fn has_suffix(name: &str, suffix: &str) -> bool {
    name.to_ascii_lowercase().ends_with(suffix)
}

pub fn human_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{} KB", (bytes / 1024).max(1))
    }
}

/// `"2026-11-18 18:23:36"` (local wall-clock, the format `util::now_iso` now
/// writes) to `"2 h ago"`.
///
/// The stamp is read in the frame it is written in: local time. Stamps written
/// before the local-time fix are UTC, and are read as local anyway (they are
/// not migrated), so an older stamp can read up to one zone offset wrong.
/// Anything that does not parse is handed back untouched.
pub fn human_age(stamp: &str) -> String {
    let Some(wall_secs) = parse_wall_clock(stamp) else {
        return stamp.to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(wall_secs + util::local_offset_secs());
    // The stamp is wall clock; take the offset back off to reach the epoch,
    // the frame `now` is already in.
    let secs = wall_secs - util::local_offset_secs();
    let delta = (now - secs).max(0);
    match delta {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", delta / 60),
        3600..=86_399 => format!("{} h ago", delta / 3600),
        86_400..=2_591_999 => format!("{} d ago", delta / 86_400),
        _ => stamp[..10].to_string(),
    }
}

/// Parses `"YYYY-MM-DD HH:MM:SS"` as a naive wall-clock reading and returns it
/// counted as if it were UTC, i.e. what the printed digits mean, not an
/// instant. `human_age` corrects the frame; nothing else should use this raw.
fn parse_wall_clock(stamp: &str) -> Option<i64> {
    let (date, time) = stamp.split_once(' ')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next()?.parse().ok()?;
    // days_from_civil (Howard Hinnant), the inverse of util::civil_from_days.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// State folder in the temp dir, the pattern `instance.rs` uses: no
    /// `tempfile` dependency, and the id keeps parallel test runs apart.
    fn scratch(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("mcl-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::at(root);
        paths.ensure().unwrap();
        paths
    }

    /// Built by hand, not with `App::open`: java detection is slow and has
    /// nothing to do with any of this.
    fn app(tag: &str) -> (App, Paths) {
        let paths = scratch(tag);
        let app = App {
            paths: paths.clone(),
            settings: RwLock::new(Settings::default()),
            shared_from_flag: false,
            java: Vec::new(),
        };
        (app, paths)
    }

    fn make_instance(app: &App, name: &str) -> PathBuf {
        Instance::create(&app.paths, name, "26.3", None, None, None).unwrap();
        Instance::dir(&app.paths, name)
    }

    #[test]
    fn settings_clamp_the_heap_to_what_java_can_take() {
        let (app, _paths) = app("heap");
        app.save_settings(SettingsPatch {
            memory_mb: Some(8),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.memory_mb(), 512, "8 MB becomes the floor, not a crash");

        app.save_settings(SettingsPatch {
            memory_mb: Some(999_999),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.memory_mb(), 32_768);
        assert_eq!(app.settings_view().memory_mb, 32_768);

        app.save_settings(SettingsPatch {
            memory_mb: Some(4096),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.memory_mb(), 4096, "a sane value is kept as sent");
    }

    #[test]
    fn settings_reject_a_bad_user_and_tidy_the_jvm_args() {
        let (app, paths) = app("user");
        for bad in ["   ", "\t", "Ana\nB", "Ana\rB"] {
            assert!(
                app.save_settings(SettingsPatch {
                    user: Some(bad.to_string()),
                    ..Default::default()
                })
                .is_err(),
                "{bad:?} was accepted as a user name"
            );
        }
        let long = "x".repeat(MAX_USER_CHARS + 1);
        assert!(app
            .save_settings(SettingsPatch {
                user: Some(long),
                ..Default::default()
            })
            .is_err());

        app.save_settings(SettingsPatch {
            user: Some("  Ana  ".into()),
            jvm_args: Some(vec![
                "  -Xss1M ".into(),
                "   ".into(),
                "-Dfml.ignoreInvalidMinecraftCertificates=true".into(),
            ]),
            ..Default::default()
        })
        .unwrap();

        let view = app.settings_view();
        assert_eq!(view.user, "Ana");
        assert_eq!(
            view.jvm_args,
            vec!["-Xss1M", "-Dfml.ignoreInvalidMinecraftCertificates=true"],
            "trimmed, and the blank entry is gone"
        );

        // What reached the disk is what the next process will read.
        let saved = Settings::load(&paths.settings_file());
        assert_eq!(saved.user(), "Ana");
        assert_eq!(saved.jvm_args.len(), 2);

        let too_many: Vec<String> = (0..=MAX_JVM_ARGS).map(|i| format!("-Dn={i}")).collect();
        assert!(app
            .save_settings(SettingsPatch {
                jvm_args: Some(too_many),
                ..Default::default()
            })
            .is_err());
        let too_long = "y".repeat(MAX_JVM_ARG_CHARS + 1);
        assert!(app
            .save_settings(SettingsPatch {
                jvm_args: Some(vec![too_long]),
                ..Default::default()
            })
            .is_err());
    }

    #[test]
    fn a_rejected_patch_writes_nothing() {
        let (app, paths) = app("atomic");
        app.save_settings(SettingsPatch {
            user: Some("Ana".into()),
            ..Default::default()
        })
        .unwrap();
        let before = std::fs::read_to_string(paths.settings_file()).unwrap();

        let error = app
            .save_settings(SettingsPatch {
                user: Some("Bob".into()),
                shared: Some(paths.root.join("no-such-store").display().to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("no-such-store"), "{error}");
        assert_eq!(
            std::fs::read_to_string(paths.settings_file()).unwrap(),
            before,
            "validation runs before the write"
        );
        assert_eq!(app.user(), "Ana", "memory was not updated either");
    }

    #[test]
    fn the_shared_store_must_already_be_a_directory() {
        let (app, paths) = app("shared");
        let missing = paths.root.join("not-there");
        let error = app
            .save_settings(SettingsPatch {
                shared: Some(missing.display().to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("not-there"), "{error}");

        let store = paths.root.join("store");
        std::fs::create_dir_all(&store).unwrap();
        app.save_settings(SettingsPatch {
            shared: Some(store.display().to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.settings_view().shared, store.display().to_string());
        assert_eq!(app.shared_dir(), store.display().to_string(), "launches follow it");
        assert_eq!(app.effective_paths().shared, store, "without a restart");

        // Empty clears the override; the store goes back to <root>\shared.
        app.save_settings(SettingsPatch {
            shared: Some(String::new()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            app.settings_view().shared,
            paths.root.join("shared").display().to_string()
        );
    }

    #[test]
    fn settings_view_names_real_paths() {
        let (app, paths) = app("paths");
        let view = app.settings_view();
        assert!(!view.root.is_empty());
        assert!(!view.shared.is_empty());
        assert!(!view.config.is_empty());
        assert!(view.config.ends_with("settings.json"), "{}", view.config);
        assert_eq!(view.root, paths.root.display().to_string());
        assert_eq!(view.user, "Player", "the default when nothing is set");
        assert_eq!(view.memory_mb, 2048);
        assert!(view.jvm_args.is_empty());
    }

    #[test]
    fn the_uuid_is_the_one_a_launch_would_use() {
        let (app, _paths) = app("uuid");
        app.save_settings(SettingsPatch {
            user: Some("Notch".into()),
            ..Default::default()
        })
        .unwrap();
        let notch = app.settings_view();
        assert_eq!(
            notch.uuid,
            auth::offline_uuid("Notch"),
            "auth.rs is the only uuid in the engine"
        );
        assert_eq!(
            notch.uuid,
            auth::offline(&notch.user).uuid,
            "settings screen and launch agree"
        );

        app.save_settings(SettingsPatch {
            user: Some("Steve".into()),
            ..Default::default()
        })
        .unwrap();
        let steve = app.settings_view();
        assert_ne!(steve.uuid, notch.uuid, "the uuid follows the user");
        assert_eq!(steve.uuid, auth::offline_uuid("Steve"));
    }

    #[test]
    fn instance_overrides_are_set_and_cleared() {
        let (app, _paths) = app("instance");
        make_instance(&app, "one");
        app.mark_played("one").unwrap();
        let before = Instance::load(&app.paths, "one").unwrap();
        assert!(before.last_played.is_some());

        app.save_instance(
            "one",
            InstancePatch {
                user: Some("Ana".into()),
                memory_mb: Some(4096),
                jvm_args: Some(vec!["-XX:+UseG1GC".into()]),
                width: Some(1280),
                height: Some(720),
            },
        )
        .unwrap();

        let detail = app.instance_detail("one").unwrap();
        assert_eq!(detail.view.user, "Ana");
        assert_eq!(detail.view.memory_mb, 4096);
        assert_eq!(detail.view.user_set.as_deref(), Some("Ana"));
        assert_eq!(detail.view.memory_set, Some(4096));
        assert_eq!(detail.view.jvm_args, vec!["-XX:+UseG1GC"]);
        assert_eq!(detail.jvm_args, vec!["-XX:+UseG1GC"]);
        assert_eq!(detail.width, Some(1280));
        assert_eq!(detail.height, Some(720));

        let saved = Instance::load(&app.paths, "one").unwrap();
        assert_eq!(saved.created, before.created, "created survives the sheet");
        assert_eq!(saved.last_played, before.last_played, "and so does history");

        // 0 and "" take an override back off; the row falls back to settings.
        app.save_instance(
            "one",
            InstancePatch {
                user: Some(String::new()),
                memory_mb: Some(0),
                jvm_args: Some(Vec::new()),
                width: Some(0),
                height: Some(0),
            },
        )
        .unwrap();

        let detail = app.instance_detail("one").unwrap();
        assert_eq!(detail.view.user_set, None);
        assert_eq!(detail.view.memory_set, None);
        assert_eq!(detail.width, None);
        assert_eq!(detail.height, None);
        assert!(detail.jvm_args.is_empty());
        assert_eq!(detail.view.user, app.user(), "inherited, not cleared");
        assert_eq!(detail.view.memory_mb, app.memory_mb());
    }

    #[test]
    fn instance_values_clamp_and_unknown_names_are_an_error() {
        let (app, _paths) = app("instance-clamp");
        make_instance(&app, "one");
        app.save_instance(
            "one",
            InstancePatch {
                memory_mb: Some(1),
                width: Some(1),
                height: Some(99_999),
                ..Default::default()
            },
        )
        .unwrap();
        let detail = app.instance_detail("one").unwrap();
        assert_eq!(detail.memory_set, Some(512));
        assert_eq!(detail.width, Some(320));
        assert_eq!(detail.height, Some(7_680));

        let error = app.instance_detail("nope").unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
        let error = app
            .save_instance("nope", InstancePatch::default())
            .unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
        assert!(app.set_mod_enabled("nope", "a.jar", false).is_err());
    }

    #[test]
    fn a_mod_toggles_between_jar_and_jar_disabled() {
        let (app, _paths) = app("mods");
        let dir = make_instance(&app, "one");
        std::fs::write(dir.join("mods").join("sodium.jar"), b"jar").unwrap();
        std::fs::write(dir.join("mods").join("notes.txt"), b"not a mod").unwrap();

        let detail = app.instance_detail("one").unwrap();
        assert_eq!(detail.mods.len(), 1, "only jars are listed");
        assert_eq!(detail.mods[0].file, "sodium.jar");
        assert!(detail.mods[0].enabled);
        assert_eq!(detail.mods[0].bytes, 3);
        assert!(!detail.mods[0].size.is_empty());
        assert_eq!(detail.view.mods, 1, "the list and the sheet agree");

        app.set_mod_enabled("one", "sodium.jar", false).unwrap();
        assert!(!dir.join("mods").join("sodium.jar").exists());
        assert!(dir.join("mods").join("sodium.jar.disabled").is_file());

        let detail = app.instance_detail("one").unwrap();
        assert_eq!(detail.mods[0].file, "sodium.jar.disabled");
        assert!(!detail.mods[0].enabled);

        // Already in the requested state: success, not an error.
        app.set_mod_enabled("one", "sodium.jar.disabled", false).unwrap();
        app.set_mod_enabled("one", "sodium.jar.disabled", true).unwrap();
        assert!(dir.join("mods").join("sodium.jar").is_file());
        assert!(!dir.join("mods").join("sodium.jar.disabled").exists());
        app.set_mod_enabled("one", "sodium.jar", true).unwrap();
    }

    #[test]
    fn a_mod_file_cannot_leave_the_mods_folder() {
        let (app, _paths) = app("mod-escape");
        make_instance(&app, "one");
        for bad in [
            r"..\evil.jar",
            "../evil.jar",
            "C:\\evil.jar",
            "sub/evil.jar",
            r"..\..\evil.jar",
            "evil.exe",
            "..",
            " .jar",
        ] {
            assert!(
                app.set_mod_enabled("one", bad, false).is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn toggling_refuses_to_overwrite_the_other_copy() {
        let (app, _paths) = app("mod-collide");
        let dir = make_instance(&app, "one");
        std::fs::write(dir.join("mods").join("sodium.jar"), b"on").unwrap();
        std::fs::write(dir.join("mods").join("sodium.jar.disabled"), b"off").unwrap();

        let error = app.set_mod_enabled("one", "sodium.jar", false).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        let error = app
            .set_mod_enabled("one", "sodium.jar.disabled", true)
            .unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");

        assert_eq!(
            std::fs::read(dir.join("mods").join("sodium.jar")).unwrap(),
            b"on",
            "neither copy was touched"
        );
        assert_eq!(
            std::fs::read(dir.join("mods").join("sodium.jar.disabled")).unwrap(),
            b"off"
        );
    }

    #[test]
    fn worlds_are_only_the_folders_with_a_level_dat() {
        let (app, _paths) = app("worlds");
        let dir = make_instance(&app, "one");
        let world = dir.join("saves").join("Dive One");
        std::fs::create_dir_all(world.join("region")).unwrap();
        std::fs::write(world.join("level.dat"), b"dat").unwrap();
        std::fs::write(world.join("region").join("r.0.0.mca"), b"terrain").unwrap();
        std::fs::create_dir_all(dir.join("saves").join("half-made")).unwrap();

        let detail = app.instance_detail("one").unwrap();
        assert_eq!(detail.worlds.len(), 1, "half-made has no level.dat");
        assert_eq!(detail.worlds[0].name, "Dive One");
        assert_eq!(detail.worlds[0].bytes, 10, "recursive: level.dat + region");
        assert!(!detail.worlds[0].size.is_empty());
        assert_eq!(
            detail.worlds[0].last_played, "just now",
            "the mtime read through human_age"
        );
        assert_eq!(detail.view.worlds, 1, "the list and the sheet agree");
    }

    #[test]
    fn a_world_is_backed_up_and_trashed_but_never_deleted() {
        let (app, paths) = app("worldmove");
        let dir = make_instance(&app, "one");
        let world = dir.join("saves").join("Dive Two");
        std::fs::create_dir_all(world.join("region")).unwrap();
        std::fs::write(world.join("level.dat"), b"dat").unwrap();
        std::fs::write(world.join("region").join("r.0.0.mca"), b"terrain").unwrap();

        let (backup, bytes) = app.backup_world("one", "Dive Two").unwrap();
        assert_eq!(bytes, 10);
        assert!(Path::new(&backup).join("level.dat").is_file(), "{backup}");
        assert!(world.is_dir(), "the backup left the world in place");
        assert!(Path::new(&backup).starts_with(paths.root.join("backups")));
        assert!(!Path::new(&backup).starts_with(dir.join("saves")), "outside saves, or the game lists it");
        assert_eq!(
            app.instance_detail("one").unwrap().worlds.len(),
            1,
            "the copy is not a second world"
        );

        let trashed = app.trash_world("one", "Dive Two").unwrap();
        assert!(!world.exists(), "the world really moved");
        assert!(Path::new(&trashed).join("level.dat").is_file(), "nothing was erased");
        assert!(Path::new(&trashed).starts_with(paths.root.join("trash").join("worlds").join("one")));
        assert!(app.instance_detail("one").unwrap().worlds.is_empty());
    }

    #[test]
    fn a_world_name_is_one_folder_name_and_must_exist() {
        let (app, _paths) = app("worldname");
        make_instance(&app, "one");
        for bad in ["", " ", "one/two", "one\\two", "..", ".", "C:", "a..b"] {
            assert!(clean_world(bad).is_err(), "{bad:?} was accepted");
        }
        assert_eq!(clean_world("Dive Two").unwrap(), "Dive Two");
        let error = app.backup_world("one", "Nowhere").unwrap_err();
        assert!(error.to_string().contains("no world named"), "{error}");
        assert!(app.trash_world("one", "../one").is_err());
    }

    #[test]
    fn import_needs_the_version_on_disk_and_creates_nothing_without_it() {
        let (app, paths) = app("import");
        let error = app.import_instance("26.3", "Pulled In", true).unwrap_err();
        assert!(error.to_string().contains("is not in"), "{error}");
        assert!(
            !paths.instance_dir("Pulled In").exists(),
            "a refused import left a folder behind"
        );
    }
}

