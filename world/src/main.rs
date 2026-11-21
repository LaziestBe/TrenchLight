//! `mcl-world` — native 3D viewer for a world that already exists on disk.
//!
//! Stage 0 proved the GPU and windowing stacks with a triangle. Stage 1 read a
//! real save: NBT, region files, block states, textures out of the version jar,
//! and one chunk meshed into triangles with a fixed camera. Stage 2 draws a
//! square of chunks in one draw call and lets you fly through them.
//!
//! Two entry points, deliberately:
//!
//! * `--selftest` decodes and reports, with no window and no GPU. It fails
//!   non-zero when the decoder disagrees with Mojang's own heightmaps.
//! * everything else opens a window and draws the area.
//!
//! "Correct textures" is not a thing you can see, so the checks that matter are
//! numeric: the self-test's face and block counts, a pixel comparison against
//! the jar's own PNG, and the frame time printed once a second.

mod mat4;
mod mesh;
mod nbt;
mod region;
mod render;
mod scene;
mod selftest;
mod textures;
mod world;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::mat4::FlyCamera;
use crate::render::Renderer;

const TITLE: &str = "mcl-world";

/// Degrees of turn per pixel of mouse movement.
const MOUSE_SENSITIVITY: f32 = 0.12;
/// Blocks per second, and the multiplier while Ctrl is held.
const FLY_SPEED: f32 = 12.0;
const FLY_FAST: f32 = 3.5;

#[derive(Default)]
struct Options {
    size: Option<PhysicalSize<u32>>,
    save: Option<PathBuf>,
    jar: Option<PathBuf>,
    selftest: bool,
    chunk: Option<(i32, i32)>,
    probes: Vec<(i32, i32, i32)>,
    /// yaw, pitch, distance in degrees and blocks.
    camera: (f32, f32, f32),
    dump_atlas: Option<PathBuf>,
    /// Treat an unloaded neighbour as solid instead of air. The window leaves
    /// this off so the edge of the area shows its own walls.
    outside_solid: bool,
    /// Chunk radius of the loaded square: 1 means 3×3 chunks.
    radius: i32,
    /// Exit after this many seconds, for scripted measurements. Zero runs until
    /// the window is closed.
    seconds: f32,
    /// Ignore mouse movement, keeping the starting view. Screenshots that are
    /// supposed to be reproducible need this: a confined cursor takes the raw
    /// deltas of whatever the hand on the mouse is doing.
    no_look: bool,
}

#[derive(Default)]
struct Input {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    fast: bool,
    /// Mouse movement accumulated since the last frame, in pixels.
    mouse: (f32, f32),
}

impl Input {
    fn clear(&mut self) {
        *self = Input::default();
    }
}

/// Rolling frame timing: everything measured between two redraws, reported once
/// a second. The gate for stage 2 is a *measured* frame time, so this prints the
/// distribution rather than a single number.
struct FrameStats {
    window_start: Instant,
    frames: u32,
    sum_ms: f64,
    min_ms: f64,
    max_ms: f64,
    reported: u32,
}

impl FrameStats {
    fn new() -> Self {
        FrameStats {
            window_start: Instant::now(),
            frames: 0,
            sum_ms: 0.0,
            min_ms: f64::MAX,
            max_ms: 0.0,
            reported: 0,
        }
    }

    fn add(&mut self, dt: Duration) {
        let ms = dt.as_secs_f64() * 1000.0;
        self.frames += 1;
        self.sum_ms += ms;
        self.min_ms = self.min_ms.min(ms);
        self.max_ms = self.max_ms.max(ms);
    }

    /// Print the last second, if it is over. Returns the frame count so a
    /// scripted run can spot a stall.
    fn tick(&mut self, renderer: &Renderer) -> Option<u32> {
        let elapsed = self.window_start.elapsed().as_secs_f64();
        if elapsed < 1.0 || self.frames == 0 {
            return None;
        }
        let frames = self.frames;
        let avg = self.sum_ms / frames as f64;
        println!(
            "fps:         {:.1} | {:.2} ms avg ({:.2}..{:.2} ms) over {frames} frames | surface {}x{}",
            1000.0 / avg,
            avg,
            self.min_ms,
            self.max_ms,
            renderer.width(),
            renderer.height()
        );
        self.reported += 1;
        self.window_start = Instant::now();
        self.frames = 0;
        self.sum_ms = 0.0;
        self.min_ms = f64::MAX;
        self.max_ms = 0.0;
        Some(frames)
    }
}

struct App {
    options: Options,
    scene: Option<scene::Scene>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    camera: Option<FlyCamera>,
    input: Input,
    stats: FrameStats,
    last_frame: Instant,
    started: Instant,
    quit_after: Option<Duration>,
    grabbed: bool,
    summary: String,
    /// Raw mouse deltas arrive for a moment after the pointer is confined,
    /// because Windows warps it into the window. Dropping them stops the view
    /// jerking the instant the window takes focus.
    ignore_mouse_until: Instant,
}

impl App {
    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) else {
            return;
        };
        let Some(camera) = &mut self.camera else {
            return;
        };

        let now = Instant::now();
        let raw = now - self.last_frame;
        self.last_frame = now;
        // A stall (the window was dragged, or the process was descheduled) must
        // not teleport the camera across the world on the next frame. The
        // *reported* frame time is the raw interval, because hiding a stall
        // would make the measurement a lie.
        let dt = raw.as_secs_f32().clamp(0.0, 0.1);

        let (dx, dy) = std::mem::take(&mut self.input.mouse);
        camera.turn(dx, dy, MOUSE_SENSITIVITY);

        let speed = if self.input.fast {
            FLY_SPEED * FLY_FAST
        } else {
            FLY_SPEED
        };
        let step = speed * dt;
        let forward = (self.input.forward as i32 - self.input.back as i32) as f32 * step;
        let strafe = (self.input.right as i32 - self.input.left as i32) as f32 * step;
        let rise = (self.input.up as i32 - self.input.down as i32) as f32 * step;
        camera.fly(forward, strafe, rise);

        let view_proj = camera.view_projection(renderer.aspect());
        if let Err(error) = renderer.render(&view_proj) {
            eprintln!("mcl-world: {error:#}");
            event_loop.exit();
            return;
        }
        self.stats.add(raw);

        if self.stats.tick(renderer).is_some() {
            println!(
                "position:    ({:.1}, {:.1}, {:.1}) yaw {:.0}° pitch {:.0}°",
                camera.eye[0], camera.eye[1], camera.eye[2], camera.yaw_degrees, camera.pitch_degrees
            );
        }

        if let Some(limit) = self.quit_after {
            if self.started.elapsed() >= limit {
                println!(
                    "summary:     {} | ran {:.1}s with {} fps reports, exited on --seconds",
                    self.summary,
                    limit.as_secs_f32(),
                    self.stats.reported
                );
                event_loop.exit();
                return;
            }
        }

        // One redraw per frame: with `ControlFlow::Wait` this makes the loop
        // vsync-paced (the next redraw is served as soon as the compositor
        // takes the last frame) without spinning the CPU while idle.
        window.request_redraw();
    }

    fn set_grab(&mut self, grab: bool) {
        let Some(window) = &self.window else { return };
        if grab == self.grabbed {
            return;
        }
        if grab {
            match window.set_cursor_grab(CursorGrabMode::Confined) {
                Ok(()) => {
                    window.set_cursor_visible(false);
                    self.grabbed = true;
                    self.ignore_mouse_until = Instant::now() + Duration::from_millis(250);
                    println!("input:       cursor confined to the window while focused");
                }
                // Losing the grab is not fatal: raw mouse motion still arrives,
                // the cursor just leaves the window at the edges.
                Err(error) => eprintln!("mcl-world: could not confine the cursor: {error}"),
            }
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.grabbed = false;
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        // A borderless window that covers the panel exactly is promoted to
        // independent-flip presentation, which GDI/BitBlt screen capture cannot
        // see (it returns a blank surface). 1600x900 keeps the window composited
        // by DWM so it stays capturable; `--size 1920x1080` opts into the
        // full-panel path when a screenshot is not needed.
        let size = self
            .options
            .size
            .unwrap_or_else(|| PhysicalSize::new(1600, 900));
        let attributes = Window::default_attributes()
            .with_title(TITLE)
            .with_decorations(false)
            .with_inner_size(size);

        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("mcl-world: could not create a window: {error}");
                event_loop.exit();
                return;
            }
        };

        // Windows places a new window on its own, and here it lands partly below
        // the bottom edge, which silently cuts the frame. Centre it inside the
        // monitor instead, clamped so it never hangs off.
        if let Some(monitor) = window.current_monitor() {
            let screen = monitor.size();
            let outer = window.outer_size();
            if outer.width <= screen.width && outer.height <= screen.height {
                let x = (screen.width - outer.width) as i32 / 2;
                let y = (screen.height - outer.height) as i32 / 2;
                window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
        }

        let scene = match self.scene.take() {
            Some(scene) => scene,
            None => {
                eprintln!("mcl-world: no scene was loaded");
                event_loop.exit();
                return;
            }
        };

        // Frame the whole loaded area, not one chunk: the orbit target is the
        // centre of the square at the middle of its surface.
        let mid = (scene.surface.0 + scene.surface.1) as f32 / 2.0;
        let target = [
            scene.centre.0 as f32 * 16.0 + 8.0,
            mid,
            scene.centre.1 as f32 * 16.0 + 8.0,
        ];
        let (yaw, pitch, distance) = self.options.camera;
        let camera = FlyCamera::from_orbit(target, yaw, pitch, distance);
        println!(
            "camera:      yaw {yaw}°, pitch {pitch}°, distance {distance} blocks from \
             ({:.0}, {mid:.0}, {:.0})",
            target[0], target[2]
        );

        match Renderer::new(
            window.clone(),
            &scene.vertices,
            &scene.indices,
            &scene.atlas,
        ) {
            Ok(mut renderer) => {
                let view_proj = camera.view_projection(renderer.aspect());
                if let Err(error) = renderer.render(&view_proj) {
                    eprintln!("mcl-world: {error:#}");
                    event_loop.exit();
                    return;
                }
                self.renderer = Some(renderer);
                self.window = Some(window.clone());
                self.camera = Some(camera);
                self.last_frame = Instant::now();
                println!(
                    "input:       W/S/A/D to move, Space/Shift up and down, Ctrl to go faster, \
                     mouse to look, Esc to quit"
                );
                window.request_redraw();
            }
            Err(error) => {
                eprintln!("mcl-world: {error:#}");
                event_loop.exit();
            }
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        // Raw motion, not `CursorMoved`: it is what keeps working once the
        // pointer is confined, and it is not scaled by the display DPI.
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.options.no_look || Instant::now() < self.ignore_mouse_until {
                return;
            }
            self.input.mouse.0 += delta.0 as f32;
            self.input.mouse.1 += delta.1 as f32;
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Focused(focused) => {
                // Confining the cursor while the window is in the background
                // would trap the pointer somewhere the user is not looking.
                self.set_grab(focused);
                if !focused {
                    // Keys do not deliver a release when focus moves away, so a
                    // held W would otherwise keep flying forever.
                    self.input.clear();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                match event.physical_key {
                    PhysicalKey::Code(code) => match code {
                        // Escape closes: a borderless window has no title bar.
                        KeyCode::Escape => {
                            if pressed {
                                event_loop.exit();
                            }
                            return;
                        }
                        KeyCode::KeyW | KeyCode::ArrowUp => self.input.forward = pressed,
                        KeyCode::KeyS | KeyCode::ArrowDown => self.input.back = pressed,
                        KeyCode::KeyA | KeyCode::ArrowLeft => self.input.left = pressed,
                        KeyCode::KeyD | KeyCode::ArrowRight => self.input.right = pressed,
                        KeyCode::Space => self.input.up = pressed,
                        KeyCode::ShiftLeft | KeyCode::ShiftRight => self.input.down = pressed,
                        KeyCode::ControlLeft | KeyCode::ControlRight => self.input.fast = pressed,
                        _ => {}
                    },
                    PhysicalKey::Unidentified(_) => {}
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                // A reconfigured surface holds no content: without this the
                // newly sized region stays black until something else forces a
                // redraw. The window is resized by the OS right after creation,
                // so this path always runs at least once.
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.draw(event_loop),
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let options = parse_args()?;

    // Write the atlas out for inspection. Headless like the self-test: the point
    // is to look at a file, and a window would only get in the way.
    if let Some(path) = &options.dump_atlas {
        let jar = options.jar.clone().context("--dump-atlas needs --jar <file.jar>")?;
        let mut jar = textures::Jar::open(&jar)?;
        let atlas = textures::build_atlas(&mut jar)?;
        textures::write_png(&atlas.image, path)?;
        println!(
            "atlas:       {} tiles in {}x{} px, written to {}",
            atlas.loaded,
            atlas.image.width,
            atlas.image.height,
            path.display()
        );
        return Ok(());
    }

    let save = options
        .save
        .clone()
        .context("--save <world folder> is required")?;

    // Headless: decode a save and print the numbers. No window, no GPU, so this
    // works over a remote session and fails loudly instead of drawing something
    // wrong.
    if options.selftest {
        let report = selftest::run(&save, options.chunk, &options.probes, options.jar.as_deref())?;
        let (agree, total) = report.heightmap_agreement;
        println!(
            "selftest:    chunk ({},{}) from {} — {} solid, {} faces, {} ids, heightmap {agree}/{total}",
            report.chunk.0,
            report.chunk.1,
            report
                .region_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            report.solid,
            report.faces,
            report.distinct
        );
        if let Some((culled, unculled)) = report.mesh_faces {
            println!("selftest:    mesher {culled} culled / {unculled} unculled faces");
        }
        // The strongest correctness check available: a decoder that misplaces a
        // section, misreads a palette or unpacks the longs wrongly cannot agree
        // with Mojang's own heightmap. Fail the run instead of printing OK.
        if total == 0 {
            bail!("no WORLD_SURFACE heightmap in that chunk; nothing to verify against");
        }
        if agree != total {
            bail!("heightmap disagreement: {agree}/{total} columns match — the decoder is wrong");
        }
        println!("selftest:    OK");
        return Ok(());
    }

    let jar = options
        .jar
        .clone()
        .context("--jar <file.jar> is required to draw textures")?;
    let loaded = scene::load(
        &save,
        &jar,
        options.chunk,
        options.radius,
        options.outside_solid,
    )?;
    let summary = loaded.summary();

    let event_loop = EventLoop::new()?;
    // The loop is paced by redraw requests, so it can sleep between frames; see
    // `App::draw`.
    event_loop.set_control_flow(ControlFlow::Wait);
    let quit_after = if options.seconds > 0.0 {
        Some(Duration::from_secs_f32(options.seconds))
    } else {
        None
    };
    let mut app = App {
        options,
        scene: Some(loaded),
        window: None,
        renderer: None,
        camera: None,
        input: Input::default(),
        stats: FrameStats::new(),
        last_frame: Instant::now(),
        started: Instant::now(),
        quit_after,
        grabbed: false,
        summary,
        ignore_mouse_until: Instant::now(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Command line.
///
/// Unknown arguments are reported and skipped rather than fatal: the launcher
/// may pass flags before this binary knows about them.
fn parse_args() -> Result<Options> {
    let mut options = Options {
        camera: (40.0, 30.0, 60.0),
        radius: 1,
        ..Options::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--size" => {
                let value = args.next().context("--size wants WxH")?;
                let mut parts = value.split(['x', 'X']);
                match (parts.next(), parts.next()) {
                    (Some(w), Some(h)) => match (w.parse(), h.parse()) {
                        (Ok(w), Ok(h)) => options.size = Some(PhysicalSize::new(w, h)),
                        _ => eprintln!("mcl-world: --size wants WxH, got '{value}'; using 1600x900"),
                    },
                    _ => eprintln!("mcl-world: --size wants WxH, got '{value}'; using 1600x900"),
                }
            }
            "--save" => {
                options.save = Some(PathBuf::from(args.next().context("--save wants a path")?))
            }
            "--jar" => options.jar = Some(PathBuf::from(args.next().context("--jar wants a path")?)),
            "--selftest" => options.selftest = true,
            "--no-look" => options.no_look = true,
            "--outside-solid" => options.outside_solid = true,
            "--radius" => {
                let value = args.next().context("--radius wants a number of chunks")?;
                options.radius = value
                    .parse()
                    .with_context(|| format!("--radius '{value}' is not a number"))?;
            }
            "--seconds" => {
                let value = args.next().context("--seconds wants a number")?;
                options.seconds = value
                    .parse()
                    .with_context(|| format!("--seconds '{value}' is not a number"))?;
            }
            "--chunk" => {
                let value = args.next().context("--chunk wants x,z")?;
                options.chunk = Some(selftest::parse_chunk(&value)?);
            }
            "--probe" => {
                let value = args.next().context("--probe wants x,y,z")?;
                options.probes.push(selftest::parse_probe(&value)?);
            }
            "--camera" => {
                let value = args.next().context("--camera wants yaw,pitch,distance")?;
                let parts: Vec<&str> = value.split(',').map(str::trim).collect();
                if parts.len() != 3 {
                    bail!("--camera wants yaw,pitch,distance, got '{value}'");
                }
                options.camera = (
                    parts[0].parse().with_context(|| format!("--camera '{value}'"))?,
                    parts[1].parse().with_context(|| format!("--camera '{value}'"))?,
                    parts[2].parse().with_context(|| format!("--camera '{value}'"))?,
                );
            }
            "--dump-atlas" => {
                let value = args.next().context("--dump-atlas wants a .png path")?;
                options.dump_atlas = Some(PathBuf::from(value));
            }
            other => eprintln!("mcl-world: ignoring unknown argument '{other}'"),
        }
    }
    Ok(options)
}
