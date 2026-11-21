//! Trenchlight — the launcher window.
//!
//! WebView2 (via wry) draws `ui/menu.html`; this file owns the window, the IPC
//! and the launch worker. The engine lives in the `mcl` library, so a native
//! Rust UI can replace this file later without touching it.
//!
//! While a dive runs the session belongs to this process, not to the page: the
//! window docks to the taskbar, the menu is swapped for a stub page so it stops
//! costing memory, and a watcher thread on the game's own window decides when the
//! game is actually up. Clicking the taskbar button puts the menu back with the
//! live session in the boot payload.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::json;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use wry::{MemoryUsageLevel, WebView, WebViewBuilder, WebViewExtWindows};

use mcl::app::{App, InstancePatch, SettingsPatch};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const MENU: &str = include_str!("../../ui/menu.html");
const FONTS: &str = include_str!("../../ui/fonts.css");
/// 64x64 raw RGBA drawn by `tools/make-icon.ps1`, used as the window and
/// taskbar icon. Raw bytes keep the image crate out of the tree.
const ICON_RGBA: &[u8] = include_bytes!("../../assets/icon-64.rgba");

/// What the window holds while it is docked and nobody can see it. The menu
/// page is heavy (fonts, backdrop, lists); this is a few hundred bytes, and the
/// real page is navigated back in the moment the window is restored.
const DOCK_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>Trenchlight</title>
<style>
html,body{height:100%;margin:0}
body{background:radial-gradient(120% 90% at 50% 0%,#0b222f,#03080d 70%);color:#8fa9b8;
  font:500 14px/1.4 'Segoe UI',system-ui,sans-serif;display:grid;place-items:center;user-select:none}
.c{text-align:center}
.d{width:9px;height:9px;border-radius:50%;background:#3ac9cf;margin:0 auto 14px;box-shadow:0 0 12px #3ac9cf}
b{display:block;color:#eaf9fb;font-size:15px;font-weight:600;margin-bottom:6px;letter-spacing:-.01em}
</style>
<div class="c"><div class="d"></div><b>In a dive</b>Restoring the window&hellip;</div>"#;

/// A dive that is running right now. Owned by Rust; the page only reads it.
#[derive(Clone)]
struct Session {
    name: String,
    pid: u32,
    /// Unix seconds — the page counts from here, nothing is written to disk.
    started: u64,
}

/// Everything the IPC handler, the view builder and the launch worker share.
struct Shared {
    app: Arc<App>,
    cancel: AtomicBool,
    /// True when the user pressed Quit: the kill was asked for, so the exit is
    /// a finished dive, not a crash.
    killed: AtomicBool,
    child_pid: Mutex<Option<u32>>,
    /// The game's top-level window, found by pid.
    hwnd: Mutex<Option<isize>>,
    game_dir: Mutex<Option<PathBuf>>,
    session: Mutex<Option<Session>>,
    /// Events raised while the page was gone. They travel in the next boot
    /// payload instead of being dropped on the floor.
    queued: Mutex<Vec<serde_json::Value>>,
    /// Stops a Horizon build install. Separate from `cancel` because the two
    /// never overlap and a shared flag would let one stop the other.
    install_cancel: AtomicBool,
    /// Set while a Horizon dive runs: its `horizon-ctl`, so Quit can save the
    /// world instead of killing the process under it.
    horizon_ctl: Mutex<Option<PathBuf>>,
}

impl Shared {
    fn new(app: Arc<App>) -> Self {
        Self {
            app,
            cancel: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            child_pid: Mutex::new(None),
            hwnd: Mutex::new(None),
            game_dir: Mutex::new(None),
            session: Mutex::new(None),
            queued: Mutex::new(Vec::new()),
            install_cancel: AtomicBool::new(false),
            horizon_ctl: Mutex::new(None),
        }
    }

    fn session(&self) -> Option<Session> {
        self.session.lock().ok().and_then(|slot| slot.clone())
    }

    fn pid(&self) -> Option<u32> {
        self.child_pid.lock().ok().and_then(|slot| *slot)
    }

    /// Clear the per-launch slots. Called when a dive is over.
    fn clear(&self) {
        if let Ok(mut slot) = self.session.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.hwnd.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.child_pid.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.horizon_ctl.lock() {
            *slot = None;
        }
    }

    /// Ask a Horizon dive to save and exit, the way its own menu does.
    ///
    /// `taskkill` is what a Java dive gets; for Horizon it would throw away the
    /// world's unsaved chunks, so the save goes through ctl and the hard kill
    /// stays as the fallback in `kill`.
    fn quit_soft(&self) -> bool {
        let Some(ctl) = self.horizon_ctl.lock().ok().and_then(|c| c.clone()) else {
            return false;
        };
        let mut command = std::process::Command::new(ctl);
        command.arg("quit");
        #[cfg(windows)]
        {
            command.creation_flags(0x0800_0000);
        }
        command.spawn().is_ok()
    }

    fn kill(&self) {
        if let Some(pid) = self.pid() {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .creation_flags(0x0800_0000)
                .spawn();
        }
    }
}

enum UserEvent {
    Stage(String),
    /// The game's own window appeared: the session is real now.
    InGame { pid: u32, hwnd: isize },
    Exited {
        pid: u32,
        code: i32,
        tail: String,
        killed: bool,
        /// "java" or "horizon": the failure card names the right game.
        engine: &'static str,
    },
    Cancelled,
    Failure { message: String, tail: String, log: bool },
    Created {
        name: String,
        error: Option<String>,
        /// What the import copied, when the version folder had anything.
        note: Option<String>,
    },
    Trashed { name: String, path: String },
    Notice(String),
    /// A settings/instance answer. Every write comes back through here, with
    /// either fresh data or the reason it was refused.
    Data {        /// Which screen asked: `settings`, `instance`, `mod` or `detail`.
        where_: &'static str,
        error: Option<String>,
        payload: Option<serde_json::Value>,
        detail: Option<serde_json::Value>,
        toast: Option<String>,
    },
    Refresh,
    /// The Horizon build list came back — or the reason it did not.
    Horizon {
        list: Option<serde_json::Value>,
        error: Option<String>,
    },
    /// One step of a Horizon build install, already shaped for the page.
    HorizonInstall(serde_json::Value),
    Drag,
    Minimise,
    Quit,
    OpenLog,
    /// The native folder picker came back, either with a path or with nothing.
    Picked {
        what: String,
        path: Option<String>,
    },
    /// The page painted its first frame: the window can come out of hiding.
    Ready,
}

fn main() {
    if let Err(error) = run() {
        // Release builds have no console; leave a trail.
        let log = std::env::temp_dir().join("trenchlight-error.log");
        let _ = std::fs::write(&log, format!("{error:#}\n"));
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let root = std::env::var_os("MCL_ROOT").map(PathBuf::from);
    let app = Arc::new(App::open(root, None).context("opening the launcher store")?);
    let shared = Arc::new(Shared::new(app));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = Rc::new(
        WindowBuilder::new()
            .with_title("Trenchlight")
            .with_window_icon(
                tao::window::Icon::from_rgba(ICON_RGBA.to_vec(), 64, 64).ok(),
            )
            .with_inner_size(LogicalSize::new(1048.0, 655.0))
            .with_resizable(false)
            .with_maximizable(false)
            .with_decorations(false)
            // Hidden until the page has painted: a tao window is shown the
            // moment it is created, which put a blank white 1048x655 rectangle
            // on screen for most of a second before the menu existed.
            .with_visible(false)
            .build(&event_loop)
            .context("creating the window")?,
    );

    // One view for the life of the process: recreating a WebView2 from inside
    // the event handler pumps messages re-entrantly and panics tao, so the page
    // is navigated instead of rebuilt.
    let view = build_view(&window, &shared, &proxy)?;

    // A page that never reports itself ready (a broken script, a WebView2 that
    // fails to paint) must not leave the launcher invisible forever.
    let fallback = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        let _ = fallback.send_event(UserEvent::Ready);
    });

    // True once the window is on screen; it stays on screen for the rest of
    // the process.
    let mut shown = false;
    // True while the window sits in the taskbar with a dive running.
    let mut docked = false;
    // The tick that docks must not immediately decide the window is already back.
    let mut just_docked = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Where a live JS event can go right now. While docked there is nowhere:
        // the stub page is not listening, so events ride in the next boot payload.
        let live = (!docked).then_some(&view);

        match event {
            Event::UserEvent(found) => match found {
                UserEvent::Refresh => {
                    // While docked the next reload carries fresh data anyway.
                    if live.is_some() {
                        let payload = boot_payload(&shared);
                        let _ = view.evaluate_script(&format!("window.trench.boot({payload});"));
                    }
                }
                UserEvent::Stage(text) => {
                    deliver(&shared, live, json!({ "type": "stage", "text": text }));
                }
                UserEvent::InGame { pid, hwnd } => {
                    trace(&format!("in game: pid {pid} hwnd {hwnd}"));
                    // Bring the game forward, then get out of its way. The page
                    // is swapped for a stub so the menu stops costing memory
                    // while nobody can see it, and the window docks to the bar.
                    win::focus(hwnd);
                    window.set_minimized(true);
                    dock(&view);
                    docked = true;
                    just_docked = true;
                }
                UserEvent::Exited {
                    pid,
                    code,
                    tail,
                    killed,
                    engine,
                } => {
                    trace(&format!("exited: pid {pid} code {code} killed {killed}"));
                    if shared.session().map(|s| s.pid) != Some(pid) {
                        trace(&format!("ignoring the exit of pid {pid}: not the session"));
                    } else {
                        shared.clear();
                        let event = json!({
                            "type": "exited",
                            "pid": pid,
                            "code": code,
                            "killed": killed,
                            "tail": tail,
                            "engine": engine,
                        });
                        if docked {
                            // Held for the page that is about to be built, so the
                            // crash card cannot race the navigation and vanish.
                            deliver(&shared, None, event);
                            window.set_minimized(false);
                            undock(&view, &shared);
                            docked = false;
                        } else {
                            deliver(&shared, Some(&view), event);
                            let _ = proxy.send_event(UserEvent::Refresh);
                        }
                    }
                }
                UserEvent::Cancelled => {
                    shared.clear();
                    if docked {
                        deliver(&shared, None, json!({ "type": "cancelled" }));
                        window.set_minimized(false);
                        undock(&view, &shared);
                        docked = false;
                    } else {
                        deliver(&shared, Some(&view), json!({ "type": "cancelled" }));
                        let _ = proxy.send_event(UserEvent::Refresh);
                    }
                }
                UserEvent::Created { name, error, note } => {
                    let event = match &error {
                        Some(message) => json!({
                            "type": "created",
                            "name": name,
                            "error": message,
                        }),
                        None => {
                            let mut event = json!({ "type": "created", "name": name });
                            if let Some(note) = note {
                                event["note"] = json!(note);
                            }
                            event
                        }
                    };
                    deliver(&shared, live, event);
                    if error.is_none() && live.is_some() {
                        let _ = proxy.send_event(UserEvent::Refresh);
                    }
                }
                UserEvent::Trashed { name, path } => {
                    deliver(
                        &shared,
                        live,
                        json!({ "type": "trashed", "name": name, "path": path }),
                    );
                    if live.is_some() {
                        let _ = proxy.send_event(UserEvent::Refresh);
                    }
                }
                UserEvent::Data {
                    where_,
                    error,
                    payload,
                    detail,
                    toast,
                } => {
                    let mut event = json!({ "type": "data", "where": where_ });
                    if let Some(error) = error {
                        event["error"] = json!(error);
                    }
                    if let Some(payload) = payload {
                        event["payload"] = payload;
                    }
                    if let Some(detail) = detail {
                        event["detail"] = detail;
                    }
                    if let Some(toast) = toast {
                        event["toast"] = json!(toast);
                    }
                    deliver(&shared, live, event);
                }
                UserEvent::Notice(text) => {
                    deliver(&shared, live, json!({ "type": "toast", "text": text }));
                }
                UserEvent::Horizon { list, error } => {
                    let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                    let mut event = json!({
                        "type": "horizon",
                        "installed": paths.installed(),
                        "root": paths.root.display().to_string(),
                    });
                    if let Some(list) = list {
                        event["list"] = list;
                    }
                    if let Some(error) = error {
                        event["error"] = json!(error);
                    }
                    deliver(&shared, live, event);
                }
                UserEvent::HorizonInstall(event) => {
                    let done = event["finished"].as_bool().unwrap_or(false);
                    deliver(&shared, live, event);
                    // A finished install changes what is on disk, so the list's
                    // "Installed" chips are refreshed from the folder itself.
                    if done && live.is_some() {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        deliver(
                            &shared,
                            live,
                            json!({ "type": "horizon", "installed": paths.installed() }),
                        );
                    }
                }
                UserEvent::Failure {
                    message,
                    tail,
                    log,
                } => {
                    shared.clear();
                    let event = json!({
                        "type": "error",
                        "message": message,
                        "tail": tail,
                        "log": log,
                    });
                    if docked {
                        deliver(&shared, None, event);
                        window.set_minimized(false);
                        undock(&view, &shared);
                        docked = false;
                    } else {
                        deliver(&shared, Some(&view), event);
                    }
                }
                UserEvent::Drag => {
                    let _ = window.drag_window();
                }
                UserEvent::Minimise => window.set_minimized(true),
                UserEvent::Quit => {
                    // Closing the launcher must not leave a game running with
                    // nobody recording it or able to stop it.
                    shared.kill();
                    *control_flow = ControlFlow::Exit;
                }
                UserEvent::OpenLog => {
                    if let Some(dir) = shared.game_dir.lock().ok().and_then(|g| g.clone()) {
                        let _ = std::process::Command::new("explorer")
                            .arg(dir.join("logs"))
                            .spawn();
                    }
                }
                UserEvent::Picked { what, path } => {
                    deliver(
                        &shared,
                        live,
                        json!({ "type": "picked", "what": what, "path": path }),
                    );
                }
                // Shown only once there is something to look at. Every later
                // page load sends it again and the flag makes that a no-op,
                // so showing never steals focus mid-session.
                UserEvent::Ready => {
                    if !shown {
                        shown = true;
                        window.set_visible(true);
                        window.set_focus();
                        trace("first paint: window shown");
                    }
                }
            },
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                shared.kill();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }

        // Docked to the taskbar with a dive running: poll for the window coming
        // back rather than trusting a focus event, and put the menu back then.
        if docked {
            if window.is_minimized() || just_docked {
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(400));
            } else {
                trace("window restored: reloading the menu");
                undock(&view, &shared);
                docked = false;
            }
        }
        just_docked = false;
    });
}

/// Send one JS event, or hold it for the next page when nothing is listening.
fn deliver(shared: &Shared, target: Option<&WebView>, event: serde_json::Value) {
    if let Some(view) = target {
        let _ = view.evaluate_script(&format!("window.trench.event({event});"));
        return;
    }
    if let Ok(mut queue) = shared.queued.lock() {
        if queue.len() >= 16 {
            queue.remove(0);
        }
        queue.push(event);
    }
}

/// The menu page with the current boot payload baked in.
fn page(shared: &Shared) -> String {
    let boot = boot_payload(shared);
    // The payload is escaped so no instance name, path or user name can close
    // the <script> tag and blank the window.
    MENU
        .replace("{{FONTS}}", FONTS)
        .replace("{{BOOT}}", &boot.to_string().replace('<', "\\u003c"))
}

/// Swap the page inside the existing view. `load_html` is an async COM call
/// (`NavigateToString`), so it is safe to call from the event handler — unlike
/// creating a WebView2, which pumps messages and panics tao.
fn reload(view: &WebView, shared: &Shared) {
    if let Err(error) = view.load_html(&page(shared)) {
        trace(&format!("loading the menu failed: {error:#}"));
    }
}

/// Going into a dive: hide the menu behind a stub and tell WebView2 it is
/// inactive, which is its own signal to give memory back. The window can sit in
/// the taskbar for hours, so this is where the launcher stops being a cost.
fn dock(view: &WebView) {
    let _ = view.set_memory_usage_level(MemoryUsageLevel::Low);
    if let Err(error) = view.load_html(DOCK_PAGE) {
        trace(&format!("docking the page failed: {error:#}"));
    }
}

/// Coming out of a dive: normal memory, real menu.
fn undock(view: &WebView, shared: &Shared) {
    let _ = view.set_memory_usage_level(MemoryUsageLevel::Normal);
    reload(view, shared);
}

fn build_view(
    window: &Window,
    shared: &Arc<Shared>,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<WebView> {
    let html = page(shared);

    let ipc = shared.clone();
    let ipc_proxy = proxy.clone();
    WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(move |request| {
            let body = request.body().to_string();
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
                return;
            };
            let cmd = value.get("cmd").and_then(|c| c.as_str()).unwrap_or_default();
            let name = value
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            match cmd {
                "play" => start_launch(ipc.clone(), ipc_proxy.clone(), name),
                "cancel" => {
                    ipc.cancel.store(true, Ordering::SeqCst);
                    ipc.kill();
                }
                // Quit from the in-game card: a kill the user asked for, so the
                // dive still counts once the child is gone.
                "quit" => {
                    ipc.killed.store(true, Ordering::SeqCst);
                    // Horizon saves through ctl; the game's pid is only a fallback.
                    if ipc.quit_soft() {
                        // A ctl that cannot reach the socket would leave the game
                        // running for ever, so the hard stop is still there.
                        let shared = ipc.clone();
                        std::thread::spawn(move || {
                            for _ in 0..40 {
                                std::thread::sleep(Duration::from_millis(500));
                                if shared.pid().is_none() {
                                    return;
                                }
                            }
                            trace("horizon: quit did not take, stopping the game");
                            shared.kill();
                        });
                    } else {
                        ipc.kill();
                    }
                }
                "show" => {
                    if let Some(hwnd) = ipc.hwnd.lock().ok().and_then(|h| *h) {
                        win::focus(hwnd);
                    }
                }
                "create" => {
                    let version = value
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let from = value
                        .get("from")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let with_mods = value
                        .get("with_mods")
                        .and_then(|m| m.as_bool())
                        .unwrap_or(false);
                    start_create(ipc_proxy.clone(), ipc.app.clone(), name, version, from, with_mods);
                }
                // The native picker runs on a worker: a modal dialog on the
                // event-loop thread would freeze the page behind it.
                "pick" => {
                    let what = value
                        .get("what")
                        .and_then(|w| w.as_str())
                        .unwrap_or("shared")
                        .to_string();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        // A skin is one PNG; the store is a folder. Same worker,
                        // same answer, different dialog.
                        let picked = if what == "skin" {
                            rfd::FileDialog::new()
                                .set_title("Choose a skin PNG")
                                .add_filter("Minecraft skin", &["png"])
                                .pick_file()
                        } else {
                            rfd::FileDialog::new()
                                .set_title("Choose the shared Minecraft store")
                                .pick_folder()
                        };
                        let _ = proxy.send_event(UserEvent::Picked {
                            what,
                            path: picked.map(|path| path.display().to_string()),
                        });
                    });
                }
                // Importing a skin writes a pack into every installation, so it
                // happens off the UI thread like the other file work.
                "apply_skin" => {
                    let path = value
                        .get("path")
                        .and_then(|p| p.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let event = match shared.app.set_skin(std::path::Path::new(&path)) {
                            Ok(note) => UserEvent::Data {
                                where_: "skin",
                                error: None,
                                payload: Some(boot_payload(&shared)),
                                detail: None,
                                toast: Some(note),
                            },
                            Err(error) => UserEvent::Data {
                                where_: "skin",
                                error: Some(short(&error)),
                                payload: None,
                                detail: None,
                                toast: None,
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                "clear_skin" => {
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let event = match shared.app.clear_skin() {
                            Ok(note) => UserEvent::Data {
                                where_: "skin",
                                error: None,
                                payload: Some(boot_payload(&shared)),
                                detail: None,
                                toast: Some(note),
                            },
                            Err(error) => UserEvent::Data {
                                where_: "skin",
                                error: Some(short(&error)),
                                payload: None,
                                detail: None,
                                toast: None,
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                "trash" => {
                    let app = ipc.app.clone();
                    let proxy = ipc_proxy.clone();
                    let target = name.clone();
                    std::thread::spawn(move || match app.trash_instance(&target) {
                        Ok(path) => {
                            let _ = proxy.send_event(UserEvent::Trashed { name: target, path });
                        }
                        Err(error) => {
                            let _ = proxy.send_event(UserEvent::Notice(format!("{error:#}")));
                        }
                    });
                }
                "folder" => {
                    if !name.is_empty() {
                        let mut dir = mcl::instance::Instance::dir(&ipc.app.paths, &name);
                        // A sub folder is only ever one of the two the window
                        // offers; nothing else reaches the filesystem.
                        match value.get("sub").and_then(|s| s.as_str()) {
                            Some("mods") => dir.push("mods"),
                            Some("saves") => dir.push("saves"),
                            _ => {}
                        }
                        let _ = std::process::Command::new("explorer").arg(dir).spawn();
                    }
                }
                "open" => {
                    let paths = &ipc.app.paths;
                    match value.get("what").and_then(|w| w.as_str()) {
                        Some("config") => {
                            let _ = std::process::Command::new("explorer")
                                .arg("/select,")
                                .arg(paths.settings_file())
                                .spawn();
                        }
                        Some("shared") => {
                            let _ = std::process::Command::new("explorer")
                                .arg(&paths.shared)
                                .spawn();
                        }
                        _ => {
                            let _ = std::process::Command::new("explorer")
                                .arg(&paths.root)
                                .spawn();
                        }
                    };
                }
                "detail" => {
                    let app = ipc.app.clone();
                    let proxy = ipc_proxy.clone();
                    let target = name.clone();
                    std::thread::spawn(move || {
                        let event = match app.instance_detail(&target) {
                            Ok(detail) => UserEvent::Data {
                                where_: "detail",
                                error: None,
                                payload: None,
                                detail: Some(json!(detail)),
                                toast: None,
                            },
                            Err(error) => UserEvent::Data {
                                where_: "detail",
                                error: Some(short(&error)),
                                payload: None,
                                detail: None,
                                toast: None,
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                "save_settings" => {
                    let patch: SettingsPatch = match serde_json::from_value(
                        value.get("patch").cloned().unwrap_or(json!({})),
                    ) {
                        Ok(patch) => patch,
                        Err(error) => {
                            let _ = ipc_proxy.send_event(UserEvent::Data {
                                where_: "settings",
                                error: Some(format!("bad settings payload: {error}")),
                                payload: None,
                                detail: None,
                                toast: None,
                            });
                            return;
                        }
                    };
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let event = match shared.app.save_settings(patch) {
                            Ok(()) => UserEvent::Data {
                                where_: "settings",
                                error: None,
                                payload: Some(boot_payload(&shared)),
                                detail: None,
                                toast: Some("Settings saved.".to_string()),
                            },
                            Err(error) => UserEvent::Data {
                                where_: "settings",
                                error: Some(short(&error)),
                                payload: None,
                                detail: None,
                                toast: None,
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                "reload_settings" => {
                    let event = UserEvent::Data {
                        where_: "settings",
                        error: None,
                        payload: Some(boot_payload(&ipc)),
                        detail: None,
                        toast: Some("Settings reloaded.".to_string()),
                    };
                    let _ = ipc_proxy.send_event(event);
                }
                "save_instance" => {
                    let patch: InstancePatch = match serde_json::from_value(
                        value.get("patch").cloned().unwrap_or(json!({})),
                    ) {
                        Ok(patch) => patch,
                        Err(error) => {
                            let _ = ipc_proxy.send_event(UserEvent::Data {
                                where_: "instance",
                                error: Some(format!("bad instance payload: {error}")),
                                payload: None,
                                detail: None,
                                toast: None,
                            });
                            return;
                        }
                    };
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    let target = name.clone();
                    std::thread::spawn(move || {
                        let event = match shared.app.save_instance(&target, patch) {
                            Ok(()) => UserEvent::Data {
                                where_: "instance",
                                error: None,
                                payload: Some(boot_payload(&shared)),
                                detail: shared
                                    .app
                                    .instance_detail(&target)
                                    .ok()
                                    .map(|d| json!(d)),
                                toast: Some("Installation saved.".to_string()),
                            },
                            Err(error) => UserEvent::Data {
                                where_: "instance",
                                error: Some(short(&error)),
                                payload: None,
                                detail: None,
                                toast: None,
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                "mod" => {
                    let enabled = value
                        .get("enabled")
                        .and_then(|e| e.as_bool())
                        .unwrap_or(false);
                    let file = value
                        .get("file")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    let target = name.clone();
                    std::thread::spawn(move || {
                        let event = match shared.app.set_mod_enabled(&target, &file, enabled) {
                            Ok(()) => UserEvent::Data {
                                where_: "mod",
                                error: None,
                                payload: None,
                                detail: shared
                                    .app
                                    .instance_detail(&target)
                                    .ok()
                                    .map(|d| json!(d)),
                                toast: None,
                            },
                            Err(error) => UserEvent::Data {
                                where_: "mod",
                                error: Some(short(&error)),
                                payload: None,
                                detail: shared
                                    .app
                                    .instance_detail(&target)
                                    .ok()
                                    .map(|d| json!(d)),
                                toast: None,
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                // Backup and trash one world. Both refuse a name that is not a
                // folder the detail listed, and neither ever deletes.
                "world" => {
                    let world = value
                        .get("world")
                        .and_then(|w| w.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let action = value
                        .get("action")
                        .and_then(|a| a.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    let target = name.clone();
                    std::thread::spawn(move || {
                        let result = match action.as_str() {
                            "backup" => shared
                                .app
                                .backup_world(&target, &world)
                                .map(|(path, bytes)| {
                                    format!("Backed up {world} ({}, {path})", mcl::app::human_bytes(bytes))
                                }),
                            "trash" => shared
                                .app
                                .trash_world(&target, &world)
                                .map(|path| format!("{world} moved to trash ({path})")),
                            other => Err(anyhow::anyhow!("unknown world action '{other}'")),
                        };
                        let event = match result {
                            Ok(text) => UserEvent::Data {
                                where_: "world",
                                error: None,
                                payload: None,
                                detail: shared
                                    .app
                                    .instance_detail(&target)
                                    .ok()
                                    .map(|d| json!(d)),
                                toast: Some(text),
                            },
                            Err(error) => UserEvent::Data {
                                where_: "world",
                                error: Some(short(&error)),
                                payload: None,
                                detail: shared
                                    .app
                                    .instance_detail(&target)
                                    .ok()
                                    .map(|d| json!(d)),
                                toast: None,
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                "open_log" => {
                    let _ = ipc_proxy.send_event(UserEvent::OpenLog);
                }
                "refresh" => {
                    let _ = ipc_proxy.send_event(UserEvent::Refresh);
                }
                // Install one build. The archive is hashed while it streams and
                // unpacked only if the hash matches the release's SHA256SUMS.
                "horizon-install" => {
                    let folder = value
                        .get("folder")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if folder.is_empty() {
                        let _ = ipc_proxy.send_event(UserEvent::HorizonInstall(json!({
                            "type": "hzinstall", "finished": true,
                            "error": "no build was named",
                        })));
                    } else {
                        let shared = ipc.clone();
                        let proxy = ipc_proxy.clone();
                        shared.install_cancel.store(false, Ordering::SeqCst);
                        std::thread::spawn(move || {
                            install_build(&shared, &proxy, &folder);
                        });
                    }
                }
                "horizon-cancel" => {
                    ipc.install_cancel.store(true, Ordering::SeqCst);
                }
                // Prepare one installed build's resources. Slow — a first run
                // converts the whole asset store — so the page gets a fraction.
                "horizon-prepare" => {
                    let folder = value
                        .get("folder")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if folder.is_empty() {
                        let _ = ipc_proxy.send_event(UserEvent::HorizonInstall(json!({
                            "type": "hzprepare", "finished": true,
                            "error": "no build was named",
                        })));
                    } else {
                        let shared = ipc.clone();
                        let proxy = ipc_proxy.clone();
                        shared.install_cancel.store(false, Ordering::SeqCst);
                        std::thread::spawn(move || {
                            prepare_build(&shared, &proxy, &folder);
                        });
                    }
                }
                // Start a Horizon dive. The build is resolved before anything is
                // spawned, so an unprepared build is refused with a reason.
                "horizon-play" => {
                    let folder = value
                        .get("folder")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let world = value
                        .get("world")
                        .and_then(|w| w.as_str())
                        .unwrap_or("New World")
                        .to_string();
                    if folder.is_empty() {
                        let _ = ipc_proxy.send_event(UserEvent::Failure {
                            message: "no Horizon build was named".into(),
                            tail: String::new(),
                            log: false,
                        });
                    } else {
                        let shared = ipc.clone();
                        let proxy = ipc_proxy.clone();
                        std::thread::spawn(move || {
                            start_horizon(shared, proxy, folder, world);
                        });
                    }
                }
                // Worlds, backups and screenshots: every one of them is a ctl
                // command against the running game, so they go to a worker and
                // the page gets one event back.
                "horizon-worlds" => {
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        let instance = paths.instance(mcl::horizon::SHARED_INSTANCE);
                        let ctl = horizon_ctl(&shared, &paths);
                        let running = shared.pid().is_some();
                        let mut event = json!({
                            "type": "worlds",
                            "running": running,
                            "backups": mcl::horizon::list_backups(&instance),
                        });
                        let Some(ctl) = ctl else {
                            event["error"] = json!("no Horizon build is installed");
                            let _ = proxy.send_event(UserEvent::HorizonInstall(event));
                            return;
                        };
                        match mcl::horizon::worlds(&ctl) {
                            Ok(rows) => {
                                let rows: Vec<serde_json::Value> = rows
                                    .iter()
                                    .map(|row| {
                                        json!({
                                            "folder": row.folder,
                                            "name": row.name,
                                            "mode": row.mode,
                                            "version": row.version,
                                            "played": row.played(),
                                            "corrupted": row.corrupted,
                                            "by_ctl": mcl::horizon::ctl_can_take(&row.folder),
                                        })
                                    })
                                    .collect();
                                event["rows"] = json!(rows);
                            }
                            Err(error) => event["error"] = json!(short(&error)),
                        }
                        let _ = proxy.send_event(UserEvent::HorizonInstall(event));
                    });
                }
                "horizon-backup" => {
                    let folder = value
                        .get("folder")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        let instance = paths.instance(mcl::horizon::SHARED_INSTANCE);
                        let ctl = horizon_ctl(&shared, &paths);
                        let event = match mcl::horizon::backup(ctl.as_deref(), &instance, &folder) {
                            Ok(made) => json!({
                                "type": "backup", "folder": folder,
                                "file": made.file, "bytes": made.bytes, "by_ctl": made.by_ctl,
                                "backups": mcl::horizon::list_backups(&instance),
                            }),
                            Err(error) => json!({
                                "type": "backup", "folder": folder, "error": short(&error),
                            }),
                        };
                        let _ = proxy.send_event(UserEvent::HorizonInstall(event));
                    });
                }
                "horizon-restore" => {
                    let file = value
                        .get("file")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        let instance = paths.instance(mcl::horizon::SHARED_INSTANCE);
                        let event = match mcl::horizon::restore(&instance, &file) {
                            Ok(folder) => json!({ "type": "restored", "folder": folder, "file": file }),
                            Err(error) => json!({ "type": "restored", "file": file, "error": short(&error) }),
                        };
                        let _ = proxy.send_event(UserEvent::HorizonInstall(event));
                    });
                }
                "horizon-delete" => {
                    let folder = value
                        .get("folder")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    trace(&format!("horizon: delete {folder} asked"));
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        let instance = paths.instance(mcl::horizon::SHARED_INSTANCE);
                        let ctl = horizon_ctl(&shared, &paths);
                        let event = match mcl::horizon::delete_world(ctl.as_deref(), &instance, &folder) {
                            Ok(()) => json!({ "type": "deleted", "folder": folder }),
                            Err(error) => json!({ "type": "deleted", "folder": folder, "error": short(&error) }),
                        };
                        let _ = proxy.send_event(UserEvent::HorizonInstall(event));
                    });
                }
                "horizon-shot" => {
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        let ctl = horizon_ctl(&shared, &paths);
                        let stamp = mcl::util::now_iso().replace(' ', "_").replace(':', "-");
                        let out = paths
                            .instance(mcl::horizon::SHARED_INSTANCE)
                            .join("screenshots")
                            .join(format!("{stamp}.png"));
                        let event = match ctl {
                            Some(ctl) => match mcl::horizon::screenshot(&ctl, &out) {
                                Ok((width, height)) => json!({
                                    "type": "shot", "file": out.display().to_string(),
                                    "width": width, "height": height,
                                }),
                                Err(error) => json!({ "type": "shot", "error": short(&error) }),
                            },
                            None => json!({ "type": "shot", "error": "no Horizon build is installed" }),
                        };
                        let _ = proxy.send_event(UserEvent::HorizonInstall(event));
                    });
                }
                // A fast question: are this build's resources already there?
                "horizon-check" => {                    let folder = value
                        .get("folder")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        let event = match mcl::horizon::prepare_check(&paths, &folder) {
                            Ok(None) => json!({
                                "type": "hzprepare", "folder": folder,
                                "ready": true, "finished": true,
                            }),
                            Ok(Some(gap)) => json!({
                                "type": "hzprepare", "folder": folder,
                                "ready": false, "note": gap, "finished": true,
                            }),
                            Err(error) => json!({
                                "type": "hzprepare", "folder": folder,
                                "ready": false, "error": short(&error), "finished": true,
                            }),
                        };
                        let _ = proxy.send_event(UserEvent::HorizonInstall(event));
                    });
                }
                // The Horizon screen's build list. Network work, so it runs on
                // a worker; the screen shows the cached list until it answers.
                "horizon" => {
                    let shared = ipc.clone();
                    let proxy = ipc_proxy.clone();
                    std::thread::spawn(move || {
                        let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
                        let dl = mcl::download::Downloader::new();
                        let event = match mcl::horizon::fetch_builds(&dl) {
                            Ok(list) => {
                                let _ = mcl::horizon::cache_builds(&paths, &list.builds);
                                UserEvent::Horizon {
                                    list: Some(json!(list)),
                                    error: None,
                                }
                            }
                            Err(error) => UserEvent::Horizon {
                                list: None,
                                error: Some(short(&error)),
                            },
                        };
                        let _ = proxy.send_event(event);
                    });
                }
                "store" => {
                    let _ = std::process::Command::new("explorer")
                        .arg(&ipc.app.paths.shared)
                        .spawn();
                }
                // The page has painted; the window can be shown.
                "ready" => {
                    let _ = ipc_proxy.send_event(UserEvent::Ready);
                }
                "drag" => {
                    let _ = ipc_proxy.send_event(UserEvent::Drag);
                }
                "min" => {
                    let _ = ipc_proxy.send_event(UserEvent::Minimise);
                }
                "close" => {
                    let _ = ipc_proxy.send_event(UserEvent::Quit);
                }
                _ => {}
            }
        })
        .build(window)
        .context("creating the WebView2 view")
}

fn boot_payload(shared: &Shared) -> serde_json::Value {
    let app = &shared.app;
    let horizon_paths = mcl::horizon::HorizonPaths::at(&app.paths);
    // The screen opens on the last good list: no network call on boot, and an
    // offline launch still shows what it knows instead of an empty panel.
    let horizon = json!({
        "root": horizon_paths.root.display().to_string(),
        "installed": horizon_paths.installed(),
        "builds": mcl::horizon::cached(&horizon_paths).map(|list| list.builds).unwrap_or_default(),
    });
    let mut payload = json!({
        "user": app.user(),
        "shared": app.shared_dir(),
        "version": env!("CARGO_PKG_VERSION"),
        "instances": app.instances().unwrap_or_default(),
        "java": app.java_runtimes(),
        "versions": app.versions(),
        "settings": app.settings_view(),
        "skin": app.skin_view(),
        "horizon": horizon,
    });
    // Capture/automation aid: open a given screen instead of Play. Not a
    // setting, not persisted — it exists so a screenshot can be taken of a page
    // without driving the mouse.
    if let Ok(page) = std::env::var("MCL_START_PAGE") {
        trace(&format!("automation: MCL_START_PAGE={page}"));
        payload["page"] = json!(page);
    }
    // The same idea for the two long Horizon actions: name a build folder and
    // the page issues the command it would issue if the button were pressed.
    for (var, field) in [
        ("MCL_HORIZON_INSTALL", "autoInstall"),
        ("MCL_HORIZON_PREPARE", "autoPrepare"),
        ("MCL_HORIZON_PLAY", "autoPlay"),
    ] {
        if let Ok(folder) = std::env::var(var) {
            if !folder.trim().is_empty() {
                trace(&format!("automation: {var}={}", folder.trim()));
                payload[field] = json!(folder.trim());
            }
        }
    }
    if let Some(session) = shared.session() {
        payload["session"] = json!({
            "name": session.name,
            "pid": session.pid,
            "started": session.started,
        });
    }
    // Anything raised while the page was gone rides along, in order.
    let queued = shared
        .queued
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default();
    if !queued.is_empty() {
        payload["events"] = json!(queued);
    }
    let summary = payload["instances"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    format!(
                        "{}({})",
                        row["name"].as_str().unwrap_or("?"),
                        row["last_played"].as_str().unwrap_or("never")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    trace(&format!("boot payload: {summary}"));
    payload
}

/// Copying an instance can move gigabytes, so creation runs off the UI thread.
fn start_create(
    proxy: EventLoopProxy<UserEvent>,
    app: Arc<App>,
    name: String,
    version: String,
    from: String,
    with_mods: bool,
) {
    std::thread::spawn(move || {
        let copy_from = Some(from.as_str()).filter(|f| !f.trim().is_empty());
        // Copying an instance already brings its mods; the import path is only
        // for a version folder nobody has made an instance of yet.
        let result = if with_mods && copy_from.is_none() {
            app.import_instance(&version, &name, true).map(|(created, copied)| {
                let note = if copied.is_empty() {
                    Some("No mods or configs were found for this version.".to_string())
                } else {
                    let bytes: u64 = copied.iter().map(|(_, bytes)| *bytes).sum();
                    Some(format!(
                        "Brought {} item(s), {}",
                        copied.len(),
                        mcl::app::human_bytes(bytes)
                    ))
                };
                (created, note)
            })
        } else {
            app.create_instance(&name, &version, copy_from)
                .map(|created| (created, None))
        };
        let event = match result {
            Ok((name, note)) => UserEvent::Created {
                name,
                error: None,
                note,
            },
            Err(error) => UserEvent::Created {
                name,
                error: Some(format!("{error:#}")),
                note: None,
            },
        };
        let _ = proxy.send_event(event);
    });
}

fn start_launch(shared: Arc<Shared>, proxy: EventLoopProxy<UserEvent>, name: String) {
    // One dive at a time: a second Play would clobber the pid that kills this one.
    if shared.session().is_some() || shared.pid().is_some() {
        let _ = proxy.send_event(UserEvent::Notice(
            "A dive is already running. Quit it before starting another.".into(),
        ));
        return;
    }
    shared.cancel.store(false, Ordering::SeqCst);
    shared.killed.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = shared.hwnd.lock() {
        *slot = None;
    }
    std::thread::spawn(move || {
        let stage_proxy = proxy.clone();
        let mut on_stage = move |text: &str| {
            let _ = stage_proxy.send_event(UserEvent::Stage(text.to_string()));
        };

        let mut prepared = match shared.app.prepare_launch_with(
            &name,
            &mut on_stage,
            Some(&|| shared.cancel.load(Ordering::SeqCst)),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                // A cancel is not a failure: the user asked for it.
                if shared.cancel.load(Ordering::SeqCst) {
                    let _ = proxy.send_event(UserEvent::Cancelled);
                } else {
                    let _ = proxy.send_event(UserEvent::Failure {
                        message: format!("{error:#}"),
                        tail: String::new(),
                        log: false,
                    });
                }
                return;
            }
        };

        if shared.cancel.load(Ordering::SeqCst) {
            let _ = proxy.send_event(UserEvent::Cancelled);
            return;
        }

        if let Ok(mut slot) = shared.game_dir.lock() {
            *slot = Some(prepared.game_dir.clone());
        }

        let _ = proxy.send_event(UserEvent::Stage("Minecraft is starting".into()));

        prepared.command.stdout(Stdio::piped());
        prepared.command.stderr(Stdio::piped());
        prepared.command.stdin(Stdio::null());
        // No stray console window behind the game.
        #[cfg(windows)]
        prepared.command.creation_flags(0x0800_0000);

        let mut child = match prepared.command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = proxy.send_event(UserEvent::Failure {
                    message: format!("could not start Java: {error}"),
                    tail: String::new(),
                    log: true,
                });
                return;
            }
        };

        let pid = child.id();
        // Cancel can land between the last check and the spawn; do not let the
        // game run and then report it as a cancellation.
        if shared.cancel.load(Ordering::SeqCst) {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .creation_flags(0x0800_0000)
                .spawn();
            let _ = child.wait();
            let _ = proxy.send_event(UserEvent::Cancelled);
            return;
        }
        let started = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        trace(&format!("spawned java pid {pid} for {name}"));
        if let Ok(mut slot) = shared.child_pid.lock() {
            *slot = Some(pid);
        }
        if let Ok(mut slot) = shared.session.lock() {
            *slot = Some(Session {
                name: name.clone(),
                pid,
                started,
            });
        }

        let tail = Arc::new(Mutex::new(VecDeque::<String>::with_capacity(256)));
        if let Some(stdout) = child.stdout.take() {
            drain(stdout, tail.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            drain(stderr, tail.clone());
        }

        // The game owning a visible window is the only honest "it is running".
        let finished = Arc::new(AtomicBool::new(false));
        {
            let watch_shared = shared.clone();
            let watch_proxy = proxy.clone();
            let finished = finished.clone();
            std::thread::spawn(move || {
                for _ in 0..1200 {
                    if finished.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Some(hwnd) = win::find_main_window(pid) {
                        if let Ok(mut slot) = watch_shared.hwnd.lock() {
                            *slot = Some(hwnd);
                        }
                        trace(&format!(
                            "game window up: pid {pid} hwnd {hwnd} {}",
                            win::describe(hwnd)
                        ));
                        let _ = watch_proxy.send_event(UserEvent::InGame { pid, hwnd });
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                trace(&format!("no game window for pid {pid} after 10 minutes"));
            });
        }

        let code = match child.wait() {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };
        finished.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = shared.child_pid.lock() {
            *slot = None;
        }
        let killed = shared.killed.load(Ordering::SeqCst);
        if shared.cancel.load(Ordering::SeqCst) && !killed {
            let _ = proxy.send_event(UserEvent::Cancelled);
            return;
        }
        // A dive only counts once the game has actually run and closed. A failed
        // record is reported instead of swallowed.
        if let Err(error) = shared.app.mark_played(&name) {
            let _ = proxy.send_event(UserEvent::Notice(format!(
                "the dive was not recorded: {error:#}"
            )));
        } else {
            trace(&format!("dive recorded for {name}"));
        }
        let joined = {
            let guard = match tail.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .iter()
                .filter_map(|line| tidy_line(line))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .take(8)
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        };
        let _ = proxy.send_event(UserEvent::Exited {
            pid,
            code,
            tail: joined,
            killed,
            engine: "java",
        });
    });
}

/// Install one Horizon build, reporting each step to the page.
///
/// The build's URLs come from the cached list rather than from the page: the
/// page names a folder, and the launcher decides what that folder means. A
/// folder that is not in the list installs nothing.
fn install_build(
    shared: &Arc<Shared>,
    proxy: &EventLoopProxy<UserEvent>,
    folder: &str,
) {
    let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
    let Some(build) = mcl::horizon::cached(&paths)
        .and_then(|list| list.builds.into_iter().find(|b| b.folder == folder))
    else {
        let _ = proxy.send_event(UserEvent::HorizonInstall(json!({
            "type": "hzinstall",
            "folder": folder,
            "finished": true,
            "error": format!("{folder} is not in the build list; refresh and try again"),
        })));
        return;
    };

    let mut send = |event: mcl::horizon::InstallEvent| {
        let payload = match event {
            mcl::horizon::InstallEvent::Stage { stage, note } => json!({
                "type": "hzinstall",
                "folder": build.folder,
                "stage": stage,
                "note": note,
            }),
            mcl::horizon::InstallEvent::Bytes { done, total } => json!({
                "type": "hzinstall",
                "folder": build.folder,
                "stage": "download",
                "done": done,
                "total": total,
            }),
        };
        let _ = proxy.send_event(UserEvent::HorizonInstall(payload));
    };

    let outcome = mcl::horizon::install(
        &paths,
        &build,
        &build.sums_url,
        &shared.install_cancel,
        &mut send,
    );

    let event = match outcome {
        Ok(_) => json!({
            "type": "hzinstall",
            "folder": build.folder,
            "stage": "done",
            "note": format!("{} installed", build.folder),
            "finished": true,
        }),
        Err(error) => {
            let cancelled = shared.install_cancel.load(Ordering::SeqCst);
            json!({
                "type": "hzinstall",
                "folder": build.folder,
                "finished": true,
                "cancelled": cancelled,
                "error": short(&error),
            })
        }
    };
    let _ = proxy.send_event(UserEvent::HorizonInstall(event));
}

/// Prepare one installed build's resources, reporting progress to the page.
///
/// `horizon-prepare --check` answers first: a build that is already prepared is
/// not prepared again, because the full run rewrites a gigabyte.
fn prepare_build(
    shared: &Arc<Shared>,
    proxy: &EventLoopProxy<UserEvent>,
    folder: &str,
) {
    let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
    let mut send = |event: mcl::horizon::PrepareEvent| {
        let _ = proxy.send_event(UserEvent::HorizonInstall(prepare_event(folder, event)));
    };

    match mcl::horizon::prepare_check(&paths, folder) {
        Ok(None) => {
            let _ = proxy.send_event(UserEvent::HorizonInstall(json!({
                "type": "hzprepare", "folder": folder,
                "ready": true, "finished": true,
                "note": "Resources already complete",
            })));
            return;
        }
        Ok(Some(gap)) => send(mcl::horizon::PrepareEvent::Message { text: gap }),
        Err(error) => {
            let _ = proxy.send_event(UserEvent::HorizonInstall(json!({
                "type": "hzprepare", "folder": folder,
                "finished": true, "error": short(&error),
            })));
            return;
        }
    }

    let Some(minecraft) = mcl::horizon::official_minecraft_dir() else {
        let _ = proxy.send_event(UserEvent::HorizonInstall(json!({
            "type": "hzprepare", "folder": folder, "finished": true,
            "error": "no official Minecraft folder found; Horizon needs one to import from",
        })));
        return;
    };

    // The pinned server jar is one file the preparer refuses to fetch itself.
    // It comes from Mojang's content store, checked against the build's pin.
    let jar = match mcl::horizon::ensure_server_jar(&paths, folder, &shared.install_cancel, &mut send)
    {
        Ok(jar) => jar,
        Err(error) => {
            let cancelled = shared.install_cancel.load(Ordering::SeqCst);
            let _ = proxy.send_event(UserEvent::HorizonInstall(json!({
                "type": "hzprepare", "folder": folder,
                "finished": true, "cancelled": cancelled,
                "ready": false, "error": short(&error),
            })));
            return;
        }
    };

    let outcome = mcl::horizon::prepare(
        &paths,
        folder,
        &minecraft,
        Some(&jar),
        &shared.install_cancel,
        &mut send,
    );

    // Whether or not the run reported success, the preparer's own verdict is
    // the one that counts: a step can print "done" and still leave a gap.
    let ready = mcl::horizon::prepare_check(&paths, folder).ok().flatten();
    let event = match (outcome, ready) {
        (Err(error), _) => {
            let cancelled = shared.install_cancel.load(Ordering::SeqCst);
            json!({
                "type": "hzprepare", "folder": folder,
                "finished": true, "cancelled": cancelled,
                "ready": false, "error": short(&error),
            })
        }
        (Ok(()), None) => json!({
            "type": "hzprepare", "folder": folder,
            "finished": true, "ready": true,
            "note": format!("{folder} is ready to play"),
        }),
        (Ok(()), Some(gap)) => json!({
            "type": "hzprepare", "folder": folder,
            "finished": true, "ready": false, "error": gap,
        }),
    };
    let _ = proxy.send_event(UserEvent::HorizonInstall(event));
}

/// One preparer event as the page wants it.
fn prepare_event(folder: &str, event: mcl::horizon::PrepareEvent) -> serde_json::Value {
    use mcl::horizon::PrepareEvent;
    match event {
        PrepareEvent::Message { text } => json!({
            "type": "hzprepare", "folder": folder, "kind": "message", "note": text,
        }),
        PrepareEvent::Fraction { value } => json!({
            "type": "hzprepare", "folder": folder, "kind": "fraction", "value": value,
        }),
        PrepareEvent::Error { text } => json!({
            "type": "hzprepare", "folder": folder, "kind": "error", "note": text,
        }),
        PrepareEvent::Bytes { done, total, text } => json!({
            "type": "hzprepare", "folder": folder, "kind": "bytes",
            "done": done, "total": total, "note": text,
        }),
    }
}

/// Start a Horizon dive and own it until the game exits.
///
/// `horizon-ctl launch --wait` is a starter, not a parent: it answers once the
/// player has spawned and the chunks are loaded, then exits while the game keeps
/// running. So the session is the game's pid — found in the process list — and
/// the exit is observed by asking that pid, not by waiting on a child.
/// The ctl of the running dive, or of the newest installed build.
///
/// `worlds` and friends need a live game, but naming a build is still useful:
/// the error ctl gives ("is the game running with --control?") is the honest
/// answer, and the page turns it into "start a dive first".
fn horizon_ctl(shared: &Shared, paths: &mcl::horizon::HorizonPaths) -> Option<PathBuf> {
    if let Some(ctl) = shared.horizon_ctl.lock().ok().and_then(|c| c.clone()) {
        return Some(ctl);
    }
    paths
        .installed()
        .into_iter()
        .find_map(|folder| paths.control(&folder))
}

fn start_horizon(
    shared: Arc<Shared>,
    proxy: EventLoopProxy<UserEvent>,
    folder: String,
    world: String,
) {
    if shared.session().is_some() || shared.pid().is_some() {
        let _ = proxy.send_event(UserEvent::Notice(
            "A dive is already running. Quit it before starting another.".into(),
        ));
        return;
    }
    shared.cancel.store(false, Ordering::SeqCst);
    shared.killed.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = shared.hwnd.lock() {
        *slot = None;
    }

    let paths = mcl::horizon::HorizonPaths::at(&shared.app.paths);
    let name = shared.app.settings_view().user;
    let plan = match mcl::horizon::plan(
        &paths,
        &folder,
        mcl::horizon::SHARED_INSTANCE,
        &world,
        &name,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            // The negative control lives here: an unprepared build never spawns.
            let _ = proxy.send_event(UserEvent::Failure {
                message: format!("{error:#}"),
                tail: String::new(),
                log: false,
            });
            return;
        }
    };
    if let Ok(mut slot) = shared.game_dir.lock() {
        // The log button opens `<this>/logs`: for Horizon that is the home
        // folder, which is where the game writes latest.log and crash reports.
        *slot = Some(plan.home.clone());
    }
    if let Ok(mut slot) = shared.horizon_ctl.lock() {
        *slot = Some(plan.ctl.clone());
    }

    let _ = proxy.send_event(UserEvent::Stage("Horizon is starting".into()));
    trace(&format!(
        "horizon: launching {folder} into '{}' at {}",
        plan.world,
        plan.game_dir.display()
    ));

    let started = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (pid, ctl_tail) = match mcl::horizon::launch(&plan, &shared.cancel) {
        Ok(pair) => pair,
        Err(error) => {
            shared.clear();
            if shared.cancel.load(Ordering::SeqCst) {
                let _ = proxy.send_event(UserEvent::Cancelled);
            } else {
                let _ = proxy.send_event(UserEvent::Failure {
                    message: format!("{error:#}"),
                    tail: String::new(),
                    log: true,
                });
            }
            return;
        }
    };

    // Cancel can land while ctl is starting the game; do not leave it running.
    if shared.cancel.load(Ordering::SeqCst) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .creation_flags(0x0800_0000)
            .spawn();
        shared.clear();
        let _ = proxy.send_event(UserEvent::Cancelled);
        return;
    }

    trace(&format!("horizon: game pid {pid}"));
    if let Ok(mut slot) = shared.child_pid.lock() {
        *slot = Some(pid);
    }
    if let Ok(mut slot) = shared.session.lock() {
        *slot = Some(Session {
            name: format!("Horizon \u{b7} {}", plan.world),
            pid,
            started,
        });
    }

    // The window is the only honest "it is really on screen".
    {
        let watch_shared = shared.clone();
        let watch_proxy = proxy.clone();
        let watch_pid = pid;
        std::thread::spawn(move || {
            for _ in 0..600 {
                if let Some(hwnd) = win::find_window_of(watch_pid) {
                    if let Ok(mut slot) = watch_shared.hwnd.lock() {
                        *slot = Some(hwnd);
                    }
                    trace(&format!(
                        "horizon window up: pid {watch_pid} hwnd {hwnd} {}",
                        win::describe(hwnd)
                    ));
                    let _ = watch_proxy.send_event(UserEvent::InGame {
                        pid: watch_pid,
                        hwnd,
                    });
                    return;
                }
                if !win::process_alive(watch_pid) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            trace(&format!("no horizon window for pid {watch_pid} after 5 minutes"));
        });
    }

    while win::process_alive(pid) {
        std::thread::sleep(Duration::from_millis(700));
    }
    if let Ok(mut slot) = shared.child_pid.lock() {
        *slot = None;
    }
    let killed = shared.killed.load(Ordering::SeqCst);
    let verdict = if killed {
        mcl::horizon::ExitVerdict {
            crash: false,
            tail: String::new(),
        }
    } else {
        mcl::horizon::exit_verdict(&plan.home, started)
    };
    let mut tail = ctl_tail;
    if !verdict.tail.is_empty() {
        if !tail.is_empty() {
            tail.push('\n');
        }
        tail.push_str(&verdict.tail);
    }
    trace(&format!(
        "horizon: pid {pid} gone, killed {killed}, crash {}",
        verdict.crash
    ));
    let _ = proxy.send_event(UserEvent::Exited {
        pid,
        code: if verdict.crash { -1 } else { 0 },
        tail,
        killed,
        engine: "horizon",
    });
}

/// The first line of an error chain: what a form can show on one line.
fn short(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let first = text.lines().next().unwrap_or("failed");
    let mut out: String = first.chars().take(220).collect();
    if out.len() < first.len() {
        out.push('…');
    }
    out
}

/// Appends one line to `%TEMP%\trenchlight-trace.log`. Launch bookkeeping used to
/// fail silently; this is the trail that says what happened.
fn trace(line: &str) {
    let path = std::env::temp_dir().join("trenchlight-trace.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{} {line}", mcl::util::now_iso());
    }
}

fn drain<R: Read + Send + 'static>(stream: R, tail: Arc<Mutex<VecDeque<String>>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Ok(mut guard) = tail.lock() {
                if guard.len() == 256 {
                    guard.pop_front();
                }
                guard.push_back(line);
            }
        }
    });
}

/// Java logs through log4j with one XML event per message, which buries the
/// readable text. Strip the markup so the card shows what actually broke.
fn tidy_line(line: &str) -> Option<String> {
    let mut text = line.trim().to_string();
    for tag in [
        "<log4j:Message><![CDATA[",
        "]]></log4j:Message>",
        "<log4j:Throwable><![CDATA[",
        "]]></log4j:Throwable>",
    ] {
        text = text.replace(tag, "");
    }
    if text.starts_with('<') || text.is_empty() {
        return None;
    }
    Some(text)
}

/// Just enough User32 to answer "is the game up?" without another dependency.
#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub type Hwnd = *mut c_void;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(callback: extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, buffer: *mut u16, len: i32) -> i32;
        fn GetClassNameW(hwnd: Hwnd, buffer: *mut u16, len: i32) -> i32;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
        fn ShowWindow(hwnd: Hwnd, command: i32) -> i32;
        fn IsIconic(hwnd: Hwnd) -> i32;
    }

    struct Search {
        pid: u32,
        found: isize,
    }

    fn text(read: unsafe extern "system" fn(Hwnd, *mut u16, i32) -> i32, hwnd: Hwnd) -> String {
        let mut buffer = [0u16; 160];
        let len = unsafe { read(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
    }

    /// A "game window" is a real client window, not the console, the GLFW error
    /// box or an IME helper: Minecraft's own window is GLFW (LWJGL 3) or LWJGL2
    /// and is titled "Minecraft …".
    fn is_game_window(hwnd: Hwnd) -> bool {
        let class = text(GetClassNameW, hwnd);
        let title = text(GetWindowTextW, hwnd);
        class.contains("GLFW") || class.contains("LWJGL") || title.starts_with("Minecraft")
    }

    extern "system" fn visit(hwnd: Hwnd, lparam: isize) -> i32 {
        let search = unsafe { &mut *(lparam as *mut Search) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
        if owner == search.pid && unsafe { IsWindowVisible(hwnd) } != 0 && is_game_window(hwnd) {
            search.found = hwnd as isize;
            return 0; // stop enumerating
        }
        1
    }

    /// The game's own top-level window, if it is up yet.
    pub fn find_main_window(pid: u32) -> Option<isize> {
        let mut search = Search { pid, found: 0 };
        unsafe { EnumWindows(visit, &mut search as *mut Search as isize) };
        (search.found != 0).then_some(search.found)
    }

    extern "system" fn visit_any(hwnd: Hwnd, lparam: isize) -> i32 {
        let search = unsafe { &mut *(lparam as *mut Search) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
        if owner != search.pid || unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        // A titled, visible window is the game; an untitled one is a helper.
        if !text(GetWindowTextW, hwnd).is_empty() {
            search.found = hwnd as isize;
            return 0;
        }
        1
    }

    /// Any titled window owned by `pid`.
    ///
    /// Horizon draws a winit window whose class says nothing about the game, so
    /// the pid is the only reliable link; its class would fail `is_game_window`.
    pub fn find_window_of(pid: u32) -> Option<isize> {
        let mut search = Search { pid, found: 0 };
        unsafe { EnumWindows(visit_any, &mut search as *mut Search as isize) };
        (search.found != 0).then_some(search.found)
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetExitCodeProcess(handle: *mut c_void, code: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    /// True while the process exists and has not exited.
    ///
    /// A game started through `horizon-ctl` is not our child, so there is no
    /// handle to wait on; the pid has to be asked instead.
    pub fn process_alive(pid: u32) -> bool {
        const QUERY_LIMITED: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        unsafe {
            let handle = OpenProcess(QUERY_LIMITED, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE
        }
    }

    pub fn describe(hwnd: isize) -> String {
        let hwnd = hwnd as Hwnd;
        format!(
            "class '{}' title '{}'",
            text(GetClassNameW, hwnd),
            text(GetWindowTextW, hwnd)
        )
    }

    pub fn focus(hwnd: isize) {
        let hwnd = hwnd as Hwnd;
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, 9); // SW_RESTORE
            }
            SetForegroundWindow(hwnd);
        }
    }
}
