use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use indicatif::MultiProgress;
use std::path::{Path, PathBuf};

use mcl::{auth, install, java, launch, manifest, util, version};

use mcl::download::Downloader;
use mcl::instance::Instance;
use mcl::paths::Paths;
use mcl::settings::Settings;
use mcl::version::Features;

#[derive(Parser)]
#[command(
    name = "mcl",
    version,
    about = "Minecraft Java launcher with isolated instances and offline accounts",
    long_about = None
)]
struct Cli {
    /// Launcher state directory (default: %APPDATA%\mcl)
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Minecraft data store to use instead of <root>/shared.
    /// Point it at %APPDATA%\.minecraft to reuse assets and libraries.
    #[arg(long, global = true, value_name = "DIR")]
    shared: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List Minecraft versions that can be installed
    Versions {
        /// Include snapshots, old betas and alphas
        #[arg(long)]
        all: bool,
        /// Show only snapshots
        #[arg(long)]
        snapshots: bool,
        /// Maximum rows to print
        #[arg(long, default_value_t = 25)]
        limit: usize,
        /// Re-download the version manifest
        #[arg(long)]
        refresh: bool,
    },
    /// Download a version: client jar, libraries and assets
    Install {
        /// Version id, e.g. 1.21.10 or 26.3
        version: String,
    },
    /// List versions already present in the data store
    Installed,
    /// Create an isolated instance
    Create {
        name: String,
        #[arg(short, long)]
        version: String,
        /// Offline player name used by default
        #[arg(short, long)]
        user: Option<String>,
        /// Heap size in MB
        #[arg(long)]
        memory: Option<u32>,
        /// Path to a specific java.exe
        #[arg(long)]
        java: Option<PathBuf>,
    },
    /// List instances
    List,
    /// Show an instance, including the exact java command
    Info {
        name: String,
        /// Print the full command line
        #[arg(long)]
        command: bool,
    },
    /// Launch an instance
    Play {
        name: String,
        /// Offline player name for this run
        #[arg(short, long)]
        user: Option<String>,
        /// Do not wait for the game to exit
        #[arg(long)]
        detach: bool,
    },
    /// Delete an instance (keeps the shared data store)
    Delete {
        name: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// Show detected Java installations
    Java {
        /// Show every JVM, not just the ones Minecraft can use
        #[arg(long)]
        all: bool,
    },
    /// Show or change launcher settings
    Config {
        /// Reuse an existing Minecraft data store, e.g. %APPDATA%\.minecraft
        #[arg(long, value_name = "DIR")]
        shared: Option<PathBuf>,
        /// Default offline player name
        #[arg(long)]
        user: Option<String>,
        /// Default heap size in MB
        #[arg(long)]
        memory: Option<u32>,
        /// Go back to <root>/shared
        #[arg(long)]
        reset_shared: bool,
    },
    /// Turn an existing version folder into an instance
    Import {
        /// Version id found in <shared>/versions
        version: String,
        /// Instance name (defaults to a cleaned-up version id)
        #[arg(long)]
        name: Option<String>,
        /// Offline player name for this instance
        #[arg(short, long)]
        user: Option<String>,
        /// Also copy that pack's mods, configs and worlds into the instance,
        /// taken from the version folder first and the data store root second
        #[arg(long)]
        with_mods: bool,
    },
}

struct Ctx {
    paths: Paths,
    settings: Settings,
}

impl Ctx {
    fn load(root: Option<PathBuf>, shared: Option<PathBuf>) -> Result<Self> {
        let root = root.unwrap_or_else(Paths::default_root);
        let base = Paths::at(root);
        let settings = Settings::load(&base.settings_file());
        let shared = shared.or_else(|| settings.shared());
        let mut paths = base;
        if let Some(shared) = shared {
            paths = paths.with_shared(shared);
        }
        paths.ensure()?;
        Ok(Ctx { paths, settings })
    }

    fn downloader(&self) -> Downloader {
        Downloader::new()
    }

    fn user(&self) -> String {
        self.settings.user()
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("\nerror: {error:#}");
        std::process::exit(1);
    }
}

/// Start `trenchlight.exe` when it sits next to this exe; otherwise print the
/// help. A missing GUI is not an error — a lone CLI copy still explains itself.
fn open_gui_or_help(root: Option<PathBuf>) -> Result<()> {
    let gui = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("trenchlight.exe")))
        .filter(|path| path.is_file());
    if let Some(gui) = gui {
        let mut cmd = std::process::Command::new(gui);
        if let Some(root) = root {
            cmd.env("MCL_ROOT", root);
        }
        cmd.spawn().context("starting Trenchlight")?;
        return Ok(());
    }
    Cli::command().print_help()?;
    println!();
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // No subcommand means someone double-clicked the exe. Opening the GUI is
    // the only useful thing to do; printing help into a console that closes
    // with the process is what made it look broken.
    let Some(command) = cli.command else {
        return open_gui_or_help(cli.root);
    };
    let ctx = Ctx::load(cli.root, cli.shared)?;

    match command {
        Command::Versions {
            all,
            snapshots,
            limit,
            refresh,
        } => cmd_versions(&ctx, all, snapshots, limit, refresh),
        Command::Install { version } => cmd_install(&ctx, &version),
        Command::Installed => cmd_installed(&ctx),
        Command::Create {
            name,
            version,
            user,
            memory,
            java,
        } => cmd_create(&ctx, &name, &version, user, memory, java),
        Command::List => cmd_list(&ctx),
        Command::Info { name, command } => cmd_info(&ctx, &name, command),
        Command::Play {
            name,
            user,
            detach,
        } => cmd_play(&ctx, &name, user, detach),
        Command::Delete { name, yes } => cmd_delete(&ctx, &name, yes),
        Command::Java { all } => cmd_java(&ctx, all),
        Command::Config {
            shared,
            user,
            memory,
            reset_shared,
        } => cmd_config(&ctx, shared, user, memory, reset_shared),
        Command::Import {
            version,
            name,
            user,
            with_mods,
        } => cmd_import(&ctx, &version, name, user, with_mods),
    }
}

fn cmd_versions(ctx: &Ctx, all: bool, snapshots: bool, limit: usize, refresh: bool) -> Result<()> {
    let dl = ctx.downloader();
    let manifest = manifest::load(&ctx.paths, &dl, refresh)?;

    println!(
        "latest release {}  |  latest snapshot {}  |  data store {}",
        manifest.latest.release,
        manifest.latest.snapshot,
        ctx.paths.shared.display()
    );
    println!();

    let mut shown = 0usize;
    let mut matched = 0usize;
    for entry in &manifest.versions {
        if !all && entry.kind != "release" {
            continue;
        }
        if snapshots && entry.kind != "snapshot" {
            continue;
        }
        matched += 1;
        if shown >= limit {
            continue;
        }
        println!(
            "  {:<24} {:<10} {}",
            entry.id,
            entry.kind,
            entry.release_time.get(0..10).unwrap_or("")
        );
        shown += 1;
    }

    if matched > shown {
        println!(
            "\n  ... {} more (use --limit {}, or --all for every version)",
            matched - shown,
            matched
        );
    }
    Ok(())
}

fn resolve_version(ctx: &Ctx, id: &str) -> Result<(Downloader, version::Resolved)> {
    let dl = ctx.downloader();
    let manifest = manifest::load(&ctx.paths, &dl, false)?;
    let resolved = version::resolve(&ctx.paths, &dl, &manifest, id)?;
    Ok((dl, resolved))
}

fn cmd_install(ctx: &Ctx, version_id: &str) -> Result<()> {
    let (dl, resolved) = resolve_version(ctx, version_id)?;
    let features = Features::new();
    let plan = install::plan(&ctx.paths, &resolved, &features);

    println!(
        "installing {} (needs Java {}, {} library files)",
        resolved.id,
        resolved.java_major,
        plan.lib_files.len()
    );

    let mp = MultiProgress::new();
    let report = install::install(&ctx.paths, &dl, &resolved, &plan, &mp, None)?;

    println!();
    println!("  version     {}", resolved.id);
    if resolved.base_id != resolved.id {
        println!("  base        {}", resolved.base_id);
    }
    println!("  libraries   {}", report.libraries);
    println!(
        "  assets      {}/{} ({} downloaded)",
        report.assets_present, report.assets_total, report.assets_downloaded
    );
    println!(
        "  downloaded  {} files, {} assets",
        report.files_downloaded, report.assets_downloaded
    );
    Ok(())
}

fn cmd_installed(ctx: &Ctx) -> Result<()> {
    let versions = ctx.paths.versions();
    if !versions.exists() {
        println!("nothing installed in {}", versions.display());
        return Ok(());
    }
    let mut entries: Vec<String> = std::fs::read_dir(&versions)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join(format!("{}.json", entry.file_name().to_string_lossy())).exists())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    for entry in entries {
        println!("  {entry}");
    }
    Ok(())
}

fn cmd_create(
    ctx: &Ctx,
    name: &str,
    version_id: &str,
    user: Option<String>,
    memory: Option<u32>,
    java: Option<PathBuf>,
) -> Result<()> {
    let instance = Instance::create(&ctx.paths, name, version_id, user, memory, java)?;
    println!("created instance '{}' ({})", instance.name, instance.version);
    println!("  {}", Instance::dir(&ctx.paths, name).display());
    println!("  next: mcl play {}", instance.name);
    Ok(())
}

fn cmd_list(ctx: &Ctx) -> Result<()> {
    let instances = Instance::list(&ctx.paths)?;
    if instances.is_empty() {
        println!("no instances yet. create one with:");
        println!("  mcl create <name> --version 26.3 --user <player>");
        return Ok(());
    }
    println!(
        "  {:<24} {:<22} {:<14} {:<18}",
        "NAME", "VERSION", "PLAYER", "LAST PLAYED"
    );
    for instance in instances {
        println!(
            "  {:<24} {:<22} {:<14} {:<18}",
            instance.name,
            instance.version,
            instance.user.unwrap_or_else(|| "-".to_string()),
            instance.last_played.unwrap_or_else(|| "never".to_string())
        );
    }
    Ok(())
}

fn cmd_info(ctx: &Ctx, name: &str, show_command: bool) -> Result<()> {
    let instance = Instance::load(&ctx.paths, name)?;
    let (_, resolved) = resolve_version(ctx, &instance.version)?;
    let features = Features::new();
    let plan = install::plan(&ctx.paths, &resolved, &features);

    let missing = install::missing_files(&ctx.paths, &plan);

    println!("  instance    {}", instance.name);
    println!("  version     {} ({})", resolved.id, resolved.kind);
    if resolved.base_id != resolved.id {
        println!("  base        {}", resolved.base_id);
    }
    println!("  game dir    {}", Instance::dir(&ctx.paths, name).display());
    println!("  data store  {}", ctx.paths.shared.display());
    println!(
        "  libraries   {} files{}",
        plan.lib_files.len(),
        if missing.is_empty() {
            String::new()
        } else {
            format!(" ({} missing)", missing.len())
        }
    );
    let index_path = ctx
        .paths
        .indexes()
        .join(format!("{}.json", resolved.asset_index.id));
    println!(
        "  assets      index {} ({})",
        resolved.asset_index.id,
        if index_path.exists() {
            "installed"
        } else {
            "not downloaded yet"
        }
    );
    println!("  memory      {} MB", instance.memory_mb(ctx.settings.memory_mb()));
    println!("  player      {}", instance.user(&ctx.settings.user()));
    println!("  created     {}", instance.created.as_deref().unwrap_or("-"));

    if show_command {
        let java = match instance.java.as_ref() {
            Some(path) => java::from_path(Path::new(path))?,
            None => {
                let installs = java::detect(&ctx.paths);
                java::pick(&installs, resolved.java_major)
                    .cloned()
                    .with_context(|| {
                        format!("no Java {} or newer found", resolved.java_major)
                    })?
            }
        };
        let session = auth::offline(&instance.user(&ctx.settings.user()));
        let launcher = launch::Launch {
            paths: &ctx.paths,
            instance: &instance,
            resolved: &resolved,
            plan: &plan,
            java: &java,
            session: &session,
            memory_mb: instance.memory_mb(ctx.settings.memory_mb()),
            extra_jvm: ctx.settings.jvm_args.clone(),
        };
        let prepared = launcher.prepare()?;
        println!();
        println!("{}", prepared.display);
    }
    Ok(())
}

fn cmd_play(ctx: &Ctx, name: &str, user: Option<String>, detach: bool) -> Result<()> {
    let mut instance = Instance::load(&ctx.paths, name)?;
    let (dl, resolved) = resolve_version(ctx, &instance.version)?;
    let features = Features::new();
    let plan = install::plan(&ctx.paths, &resolved, &features);

    let java = match instance.java.as_ref() {
        Some(path) => java::from_path(Path::new(path))?,
        None => {
            let installs = java::detect(&ctx.paths);
            java::pick(&installs, resolved.java_major)
                .cloned()
                .with_context(|| {
                    format!(
                        "no Java {} or newer found. Run `mcl java` to see what exists",
                        resolved.java_major
                    )
                })?
        }
    };

    let mp = MultiProgress::new();
    let report = install::install(&ctx.paths, &dl, &resolved, &plan, &mp, None)?;

    let username = user
        .or_else(|| instance.user.clone())
        .unwrap_or_else(|| ctx.user());
    let session = auth::offline(&username);
    let memory = instance.memory_mb(ctx.settings.memory_mb());

    let launcher = launch::Launch {
        paths: &ctx.paths,
        instance: &instance,
        resolved: &resolved,
        plan: &plan,
        java: &java,
        session: &session,
        memory_mb: memory,
        extra_jvm: ctx.settings.jvm_args.clone(),
    };
    let mut prepared = launcher.prepare()?;

    println!();
    println!(
        "launching {} | {} | Java {} | {} MB | {} assets",
        instance.name, resolved.id, java.version, memory, report.assets_total
    );
    println!("player: {username} (offline)  uuid: {}", session.uuid);
    println!("log: {}\\logs\\latest.log", prepared.game_dir.display());
    if detach {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            prepared.command.creation_flags(0x0000_0008); // DETACHED_PROCESS
        }
        let child = prepared.command.spawn().context("start Minecraft")?;
        println!("started (pid {})", child.id());
        // Record the launch even when we do not wait for the game, otherwise
        // `list` says "never" forever for the usual detached start.
        instance.last_played = Some(util::now_iso());
        let _ = instance.save(&ctx.paths);
        return Ok(());
    }

    let mut child = prepared
        .command
        .spawn()
        .context("start Minecraft (is the java path valid?)")?;

    instance.last_played = Some(util::now_iso());
    let _ = instance.save(&ctx.paths);

    let status = child.wait()?;

    if status.success() {
        println!("Minecraft exited normally");
    } else {
        println!("Minecraft exited with {status}");
        let log = prepared.game_dir.join("logs").join("latest.log");
        if log.exists() {
            println!("see {}", log.display());
        }
    }
    Ok(())
}

fn cmd_delete(ctx: &Ctx, name: &str, yes: bool) -> Result<()> {
    if !yes {
        bail!("refusing to delete '{}' without --yes", name);
    }
    Instance::delete(&ctx.paths, name)?;
    println!("deleted instance '{name}'");
    Ok(())
}

fn cmd_java(ctx: &Ctx, all: bool) -> Result<()> {
    let installs = java::detect(&ctx.paths);
    if installs.is_empty() {
        println!("no Java installation found");
        return Ok(());
    }
    for found in &installs {
        if !all && found.major < 8 {
            continue;
        }
        println!("  Java {:<3} {:<12} {}", found.major, found.version, found.path.display());
        println!("  {:<10} {}", "", found.source);
    }
    Ok(())
}

fn cmd_config(
    ctx: &Ctx,
    shared: Option<PathBuf>,
    user: Option<String>,
    memory: Option<u32>,
    reset_shared: bool,
) -> Result<()> {
    let file = ctx.paths.settings_file();
    let mut settings = Settings::load(&file);
    let mut changed = false;

    if reset_shared {
        settings.shared_dir = None;
        changed = true;
    }
    if let Some(shared) = shared {
        let shared = std::fs::canonicalize(&shared).unwrap_or(shared);
        if !shared.is_dir() {
            bail!("{} is not a directory", shared.display());
        }
        // canonicalize() returns \\?\C:\... on Windows; store the friendly form.
        let text = shared.display().to_string();
        let text = text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(str::to_string))
            .unwrap_or(text);
        settings.shared_dir = Some(text);
        changed = true;
    }
    if let Some(user) = user {
        settings.default_user = Some(user);
        changed = true;
    }
    if let Some(memory) = memory {
        settings.memory_mb = Some(memory.clamp(512, 32_768));
        changed = true;
    }

    if changed {
        settings.save(&file)?;
        println!("saved {}", file.display());
    }

    let effective = ctx.paths.clone();
    println!("  settings    {}", file.display());
    println!(
        "  shared      {}",
        settings
            .shared()
            .unwrap_or_else(|| Paths::at(effective.root.clone()).shared)
            .display()
    );
    println!("  user        {}", settings.user());
    println!("  memory      {} MB", settings.memory_mb());
    if !settings.jvm_args.is_empty() {
        println!("  jvm args    {}", settings.jvm_args.join(" "));
    }
    Ok(())
}

fn cmd_import(
    ctx: &Ctx,
    version_id: &str,
    name: Option<String>,
    user: Option<String>,
    with_mods: bool,
) -> Result<()> {
    let json = ctx.paths.version_json(version_id);
    if !json.exists() {
        bail!(
            "'{}' is not in {}. Point the launcher at your existing install with:\n  mcl config --shared \"%APPDATA%\\.minecraft\"",
            version_id,
            ctx.paths.versions().display()
        );
    }
    let resolved = version::load_local(&json)?;
    let base = resolved
        .inherits_from
        .clone()
        .unwrap_or_else(|| version_id.to_string());
    if resolved.inherits_from.is_some() && !ctx.paths.version_json(&base).exists() {
        bail!("'{}' inherits from '{}', which is not installed", version_id, base);
    }

    let cleaned: String = version_id
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            other => other,
        })
        .collect();
    let name = name.unwrap_or(cleaned);
    let instance = Instance::create(&ctx.paths, &name, version_id, user, None, None)?;
    let game_dir = ctx.paths.instance_dir(&instance.name);

    let mut copied_items: Vec<(String, u64)> = Vec::new();
    if with_mods {
        // TLauncher-style installs keep each modpack's mods inside its own
        // version folder (`versions/<pack>/mods`), with a fallback copy at the
        // store root. Take the version folder first, then fill the gaps from
        // the root, so the import becomes a self-contained isolated instance.
        let version_dir = ctx.paths.version_dir(version_id);
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
            let source = [version_dir.join(item), ctx.paths.shared.join(item)]
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
                copied_items.push((item.to_string(), bytes));
            }
        }
    }

    println!("imported '{}' as instance '{}'", version_id, instance.name);
    if with_mods {
        if copied_items.is_empty() {
            println!(
                "  no mods or configs found in {}",
                ctx.paths.shared.display()
            );
        } else {
            for (item, bytes) in &copied_items {
                println!("  copied      {item:<16} {}", util::human_size(*bytes));
            }
        }
    }
    println!("  next: mcl play {}", instance.name);
    Ok(())
}
