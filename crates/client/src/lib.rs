//! The game client: window, input, chunk streaming, integrated host and renderer.
//!
//! v0.0.1 is a single-player tech preview: the client owns an [`ob_server::Host`]
//! directly (the same authoritative simulation the dedicated server uses) and
//! renders its world through the wgpu renderer.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ob_content::{BlockId, Registry};
use ob_math::{ChunkPos, SEA_LEVEL, SectionPos, WorldVec};
use ob_render::{Camera, OffscreenRenderer, Renderer};
use ob_server::{Host, HostConfig};
use ob_sim::{BlockAction, PlayerInput};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

pub mod mesh_scheduler;
pub mod ui_state;

use mesh_scheduler::MeshScheduler;

/// One fixed simulation tick (20 Hz, DEC-038).
const TICK: Duration = Duration::from_millis(50);
/// Maximum dirty sections handed to the mesh scheduler per frame. The rest are
/// re-marked dirty so they are retried on the next frame.
const MAX_MESH_SUBMISSIONS_PER_FRAME: usize = 32;
/// Upload budget per frame, in microseconds: GPU buffer creation and upload
/// are kept short so a burst of completed meshes cannot stall a frame.
const MESH_UPLOAD_BUDGET_US: u128 = 4_000;
/// Interval between mesh scheduler debug summaries.
const MESH_STATS_INTERVAL: Duration = Duration::from_secs(5);
/// Chunks generated per frame while streaming.
const GENERATE_BUDGET: usize = 2;
/// Autosave interval.
const AUTOSAVE: Duration = Duration::from_secs(60);
/// Horizontal streaming radius used before the world is first meshed.
const INITIAL_RADIUS: i32 = 4;
/// Fixed size of an offscreen screenshot.
const SCREENSHOT_WIDTH: u32 = 1280;
const SCREENSHOT_HEIGHT: u32 = 720;
/// Chunks generated per streaming step while preparing a screenshot.
const SCREENSHOT_STREAM_BUDGET: usize = 8;
/// Safety bound on screenshot streaming loops (a full 33x33 world needs ~137).
const SCREENSHOT_STREAM_GUARD: usize = 500;

/// Options parsed from the command line.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// World seed used when creating a new world.
    pub seed: u64,
    /// World name (directory under the data directory).
    pub world: String,
    /// Optional data directory override.
    pub data_dir: Option<std::path::PathBuf>,
    /// Streaming radius in chunks.
    pub render_distance_chunks: i32,
    /// Start in fly mode.
    pub fly: bool,
    /// Force the X11/XWayland backend on Linux (diagnostics and capture).
    pub force_x11: bool,
    /// Render one offscreen frame to this PNG path, then exit without a window.
    pub screenshot: Option<std::path::PathBuf>,
    /// Automatically close the window after this many seconds and log frame stats.
    pub seconds: Option<f32>,
    /// Number of dedicated section-meshing worker threads (1..=16).
    pub mesh_workers: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            seed: 0,
            world: "world".to_string(),
            data_dir: None,
            render_distance_chunks: 6,
            fly: false,
            force_x11: false,
            screenshot: None,
            seconds: None,
            mesh_workers: default_mesh_workers(),
        }
    }
}

/// Default mesh worker count: all but one core, capped at [`mesh_scheduler::MAX_MESH_WORKERS`].
fn default_mesh_workers() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).max(1))
        .unwrap_or(1)
        .min(mesh_scheduler::MAX_MESH_WORKERS)
}

/// Runs the client until the window closes, or writes a single screenshot and exits.
pub fn run(options: ClientOptions) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = &options.screenshot {
        return screenshot(&options, path);
    }
    #[cfg(target_os = "linux")]
    let event_loop = {
        use winit::platform::x11::EventLoopBuilderExtX11;
        let mut builder = EventLoop::builder();
        if options.force_x11 || std::env::var_os("OPENBLOCK_X11").is_some() {
            builder.with_x11();
        }
        builder.build()?
    };
    #[cfg(not(target_os = "linux"))]
    let event_loop = EventLoop::builder().build()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(options);
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Renders one offscreen frame of the world and writes it as a PNG (RGBA8).
///
/// This requires no window or display; it is used both for screenshots and to
/// verify the GPU pipeline on headless hosts.
pub fn screenshot(options: &ClientOptions, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::base());
    let config = HostConfig {
        seed: options.seed,
        world_name: options.world.clone(),
        data_dir: options.data_dir.clone(),
        render_distance_chunks: options.render_distance_chunks,
        generate_budget: GENERATE_BUDGET,
    };
    let mut host = Host::open(Arc::clone(&registry), config)?;

    // Drain streaming fully before meshing: the first frame must not be a
    // partially generated world.
    let mut steps = 0;
    loop {
        if host.ensure_streaming(SCREENSHOT_STREAM_BUDGET) == 0 {
            break;
        }
        steps += 1;
        if steps >= SCREENSHOT_STREAM_GUARD {
            log::warn!("screenshot streaming stopped after {steps} steps; some chunks may be missing");
            break;
        }
    }

    let (eye, yaw, pitch) = {
        let player = host.player();
        (player.eye(), player.yaw, player.pitch)
    };
    let mut camera = Camera::new(eye, yaw, pitch);
    camera.far = (host.render_distance_chunks() as f32 * 16.0 + 48.0).max(128.0);

    let mut renderer = OffscreenRenderer::new(SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT)?;
    renderer.render_world(host.world(), &camera)?;
    let rgba = renderer.read_rgba();
    if rgba.len() != (SCREENSHOT_WIDTH * SCREENSHOT_HEIGHT * 4) as usize {
        return Err(format!(
            "offscreen readback returned {} bytes, expected {}",
            rgba.len(),
            SCREENSHOT_WIDTH * SCREENSHOT_HEIGHT * 4
        )
        .into());
    }

    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgba)?;
    drop(writer);

    log::info!(
        "screenshot written to {} ({}x{}, GPU {})",
        path.display(),
        SCREENSHOT_WIDTH,
        SCREENSHOT_HEIGHT,
        renderer.adapter_summary()
    );
    Ok(())
}

struct App {
    options: ClientOptions,
    state: Option<GameState>,
}

impl App {
    fn new(options: ClientOptions) -> Self {
        Self { options, state: None }
    }
}

struct GameState {
    window: Arc<Window>,
    renderer: Renderer,
    host: Host,
    registry: Arc<Registry>,
    mesh_scheduler: MeshScheduler,
    mesh_stats_timer: Instant,
    keys: [bool; 8],
    input_yaw: f32,
    input_pitch: f32,
    fly_toggle: bool,
    captured: bool,
    selected: usize,
    hotbar: [&'static str; 8],
    accumulator: Duration,
    last_frame: Instant,
    prev_eye: WorldVec,
    current_eye: WorldVec,
    frames: u32,
    fps_timer: Instant,
    fps: u32,
    last_save: Instant,
    meshes: HashMap<SectionPos, usize>,
    started: Instant,
    auto_exit: Option<Duration>,
    frame_times: Vec<f32>,
}

impl GameState {
    fn update_title(&mut self) {
        let player = self.host.player();
        let block = self.hotbar[self.selected];
        let capture = if self.captured { "" } else { " | click to capture" };
        let mode = if player.flying { "fly" } else { "walk" };
        self.window.set_title(&format!(
            "OpenBlock {} | {} FPS | {:.0}, {:.0}, {:.0} | {} | {}{}",
            ob_core::VERSION,
            self.fps,
            player.pos.x,
            player.pos.y,
            player.pos.z,
            block,
            mode,
            capture,
        ));
    }

    fn log_frame_stats(&mut self) {
        if self.frame_times.is_empty() {
            return;
        }
        let mut times = self.frame_times.clone();
        times.sort_by(f32::total_cmp);
        let count = times.len();
        let percentile = |p: f32| -> f32 {
            let index = ((count as f32 - 1.0) * p / 100.0).round() as usize;
            times[index.min(count - 1)]
        };
        let mean = times.iter().sum::<f32>() / count as f32;
        log::info!(
            "frame stats: {} frames in {:.1}s, {:.1} FPS mean, p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
            count,
            self.started.elapsed().as_secs_f32(),
            1000.0 / mean,
            percentile(50.0),
            percentile(95.0),
            percentile(99.0),
            times[count - 1],
        );
        println!(
            "fps={:.1} frame_ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
            1000.0 / mean,
            percentile(50.0),
            percentile(95.0),
            percentile(99.0),
            times[count - 1],
        );
    }

    fn capture_pointer(&mut self) {
        let _ = self
            .window
            .set_cursor_grab(CursorGrabMode::Locked)
            .or_else(|_| self.window.set_cursor_grab(CursorGrabMode::Confined));
        self.window.set_cursor_visible(false);
        self.captured = true;
        self.update_title();
    }

    fn release_pointer(&mut self) {
        let _ = self.window.set_cursor_grab(CursorGrabMode::None);
        self.window.set_cursor_visible(true);
        self.captured = false;
        self.update_title();
    }

    fn selected_block(&self) -> BlockId {
        self.registry.id(self.hotbar[self.selected]).unwrap_or(BlockId::AIR)
    }

    fn stream_and_mesh(&mut self) {
        self.host.ensure_streaming(GENERATE_BUDGET);
        for chunk in self.host.take_unloaded_chunks() {
            self.unload_chunk_meshes(chunk);
        }

        // Snapshot and queue dirty sections. At most 32 snapshots are captured
        // on the main thread per frame; the rest go back into the world's dirty
        // set so no edit is lost. A section that is already in flight is also
        // deferred: submit() invalidates its old epoch so the completion is
        // dropped, and the retry picks up the latest world state.
        let dirty = self.host.take_dirty_sections();
        let mut deferred = Vec::new();
        let mut submitted = 0usize;
        for pos in dirty {
            if self.host.world().section(pos).is_none() {
                self.drop_section_mesh(pos);
                self.mesh_scheduler.forget_section(pos);
                continue;
            }
            if submitted >= MAX_MESH_SUBMISSIONS_PER_FRAME {
                self.mesh_scheduler.invalidate(pos);
                deferred.push(pos);
                continue;
            }
            if self.mesh_scheduler.submit(self.host.world(), pos) {
                submitted += 1;
            } else {
                deferred.push(pos);
            }
        }
        for pos in deferred {
            self.host.world_mut().mark_section_dirty(pos);
        }

        self.drain_mesh_results();

        if self.mesh_stats_timer.elapsed() >= MESH_STATS_INTERVAL {
            log::debug!(
                "mesh scheduler: submitted {} completed {} dropped {} in-flight {} backlog {}",
                self.mesh_scheduler.submitted_count(),
                self.mesh_scheduler.completed_count(),
                self.mesh_scheduler.dropped_count(),
                self.mesh_scheduler.in_flight_count(),
                self.host.dirty_sections(),
            );
            self.mesh_stats_timer = Instant::now();
        }
    }

    /// Uploads completed meshes until the per-frame budget is spent.
    ///
    /// Results that do not fit stay in the channel (and therefore keep their
    /// section in flight) and are picked up on the next frame.
    fn drain_mesh_results(&mut self) {
        let start = Instant::now();
        let mut uploaded = 0usize;
        loop {
            if uploaded > 0 && start.elapsed().as_micros() >= MESH_UPLOAD_BUDGET_US {
                break;
            }
            // try_recv drops stale epochs internally.
            let Some(result) = self.mesh_scheduler.try_recv() else {
                break;
            };
            if self.host.world().section(result.pos).is_none() {
                self.drop_section_mesh(result.pos);
                continue;
            }
            if result.mesh.vertices.is_empty() {
                self.drop_section_mesh(result.pos);
            } else {
                let vertex_count = result.mesh.vertices.len();
                self.renderer.upload_mesh(result.pos, result.mesh);
                self.meshes.insert(result.pos, vertex_count);
            }
            uploaded += 1;
        }
    }

    /// Removes a section mesh from the renderer and the client-side table.
    fn drop_section_mesh(&mut self, pos: SectionPos) {
        self.renderer.remove_mesh(pos);
        self.meshes.remove(&pos);
    }

    fn unload_chunk_meshes(&mut self, chunk: ChunkPos) {
        let positions: Vec<SectionPos> = self
            .meshes
            .keys()
            .filter(|p| p.x == chunk.x && p.z == chunk.z)
            .copied()
            .collect();
        for pos in positions {
            self.drop_section_mesh(pos);
        }
        // In-flight results for the unloaded column are discarded when they
        // complete because their epoch is no longer tracked.
        self.mesh_scheduler.forget_chunk(chunk);
    }

    fn tick(&mut self) {
        let mut input = PlayerInput {
            forward: 0.0,
            strafe: 0.0,
            jump: false,
            sprint: false,
            fly_toggle: self.fly_toggle,
        };
        self.fly_toggle = false;
        if self.keys[0] {
            input.forward += 1.0;
        }
        if self.keys[1] {
            input.forward -= 1.0;
        }
        if self.keys[2] {
            input.strafe -= 1.0;
        }
        if self.keys[3] {
            input.strafe += 1.0;
        }
        input.jump = self.keys[4];
        input.sprint = self.keys[5];
        self.prev_eye = self.host.player().eye();
        self.host.tick(&input);
        self.current_eye = self.host.player().eye();
        if self.host.tick_count().is_multiple_of(20) {
            self.stream_and_mesh();
        }
        if self.last_save.elapsed() >= AUTOSAVE && self.host.player_dirty() {
            if let Err(err) = self.host.save() {
                log::warn!("autosave failed: {err}");
            }
            self.last_save = Instant::now();
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let dt = now - self.last_frame;
        self.last_frame = now;
        // Exclude the first two seconds: initial streaming, world generation
        // and shader/pipeline warm-up are not representative of steady state.
        if self.started.elapsed() >= Duration::from_secs(2) {
            self.frame_times.push(dt.as_secs_f32() * 1000.0);
            if self.frame_times.len() > 50_000 {
                self.frame_times.drain(..25_000);
            }
        }
        self.accumulator += dt.min(Duration::from_millis(250));
        let mut ticks = 0;
        while self.accumulator >= TICK && ticks < 5 {
            self.accumulator -= TICK;
            self.tick();
            ticks += 1;
        }
        let alpha = (self.accumulator.as_secs_f32() / TICK.as_secs_f32()).clamp(0.0, 1.0);
        let eye = self.prev_eye.lerp(self.current_eye, alpha as f64);
        let player = self.host.player();
        let mut camera = Camera::new(eye, player.yaw, player.pitch);
        camera.far = (self.host.render_distance_chunks() as f32 * 16.0 + 48.0).max(128.0);
        self.stream_and_mesh();
        if let Err(err) = self.renderer.render(&camera) {
            log::error!("render error: {err}");
        }
        self.frames += 1;
        if self.fps_timer.elapsed() >= Duration::from_secs(1) {
            self.fps = self.frames;
            self.frames = 0;
            self.fps_timer = Instant::now();
            self.update_title();
        }
    }

    fn interact(&mut self, place: bool) {
        if !self.captured {
            self.capture_pointer();
            return;
        }
        let action = BlockAction {
            break_block: !place,
            place_block: place,
            place: self.selected_block(),
        };
        self.host.apply_action(&action);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("OpenBlock {}", ob_core::VERSION))
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(err) => {
                log::error!("failed to create window: {err}");
                event_loop.exit();
                return;
            }
        };
        let renderer = match Renderer::new(Arc::clone(&window)) {
            Ok(renderer) => renderer,
            Err(err) => {
                log::error!("{err}");
                log::error!(
                    "OpenBlock requires a working Vulkan driver (Fedora: mesa-vulkan-drivers or the NVIDIA driver)."
                );
                event_loop.exit();
                return;
            }
        };
        log::info!("GPU: {}", renderer.adapter_summary());
        let registry = Arc::new(Registry::base());
        let mesh_scheduler = MeshScheduler::new(self.options.mesh_workers, Arc::clone(&registry));
        log::info!("mesh scheduler: {} worker threads", mesh_scheduler.workers());
        let config = HostConfig {
            seed: self.options.seed,
            world_name: self.options.world.clone(),
            data_dir: self.options.data_dir.clone(),
            render_distance_chunks: self.options.render_distance_chunks,
            generate_budget: GENERATE_BUDGET,
        };
        let mut host = match Host::open(Arc::clone(&registry), config) {
            Ok(host) => host,
            Err(err) => {
                log::error!("failed to open world: {err}");
                event_loop.exit();
                return;
            }
        };
        host.ensure_streaming(INITIAL_RADIUS as usize * 2);
        let (eye, yaw, pitch) = {
            let player = host.player();
            (player.eye(), player.yaw, player.pitch)
        };
        let mut state = GameState {
            window,
            renderer,
            host,
            registry,
            mesh_scheduler,
            mesh_stats_timer: Instant::now(),
            keys: [false; 8],
            input_yaw: yaw,
            input_pitch: pitch,
            fly_toggle: self.options.fly,
            captured: false,
            selected: 0,
            hotbar: [
                "openblock:stone",
                "openblock:dirt",
                "openblock:grass",
                "openblock:sand",
                "openblock:log",
                "openblock:leaves",
                "openblock:water",
                "openblock:bedrock",
            ],
            accumulator: Duration::ZERO,
            last_frame: Instant::now(),
            prev_eye: eye,
            current_eye: eye,
            frames: 0,
            fps_timer: Instant::now(),
            fps: 0,
            last_save: Instant::now(),
            meshes: HashMap::new(),
            started: Instant::now(),
            auto_exit: self
                .options
                .seconds
                .map(|seconds| Duration::from_secs_f32(seconds.max(0.5))),
            frame_times: Vec::new(),
        };
        if state.fly_toggle {
            let player = state.host.player_mut();
            player.flying = true;
            state.fly_toggle = false;
        }
        state.update_title();
        state.stream_and_mesh();
        self.state = Some(state);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                if let Err(err) = state.host.save() {
                    log::warn!("save on exit failed: {err}");
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width.max(1), size.height.max(1));
            }
            WindowEvent::RedrawRequested => {
                state.frame();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::KeyW => state.keys[0] = pressed,
                        KeyCode::KeyS => state.keys[1] = pressed,
                        KeyCode::KeyA => state.keys[2] = pressed,
                        KeyCode::KeyD => state.keys[3] = pressed,
                        KeyCode::Space => state.keys[4] = pressed,
                        KeyCode::ShiftLeft => state.keys[5] = pressed,
                        KeyCode::KeyF if pressed => state.fly_toggle = true,
                        KeyCode::Escape if pressed => state.release_pointer(),
                        KeyCode::Digit1 => state.selected = 0,
                        KeyCode::Digit2 => state.selected = 1,
                        KeyCode::Digit3 => state.selected = 2,
                        KeyCode::Digit4 => state.selected = 3,
                        KeyCode::Digit5 => state.selected = 4,
                        KeyCode::Digit6 => state.selected = 5,
                        KeyCode::Digit7 => state.selected = 6,
                        KeyCode::Digit8 => state.selected = 7,
                        _ => {}
                    }
                }
                if pressed {
                    state.update_title();
                }
            }
            WindowEvent::MouseInput {
                state: element, button, ..
            } => {
                if element == ElementState::Pressed {
                    match button {
                        MouseButton::Left => state.interact(false),
                        MouseButton::Right => state.interact(true),
                        _ => {}
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 40.0).round() as i32,
                };
                if steps != 0 {
                    let count = state.hotbar.len() as i32;
                    state.selected = ((state.selected as i32 + steps).rem_euclid(count)) as usize;
                    state.update_title();
                }
            }
            WindowEvent::Focused(false) => {
                state.keys = [false; 8];
            }
            WindowEvent::DroppedFile(path) => {
                log::info!("ignoring dropped file: {}", path.display())
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, event: DeviceEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if !state.captured {
            return;
        }
        if let DeviceEvent::MouseMotion { delta } = event {
            let sensitivity = 0.0022f32;
            state.input_yaw -= delta.0 as f32 * sensitivity;
            state.input_pitch -= delta.1 as f32 * sensitivity;
            state.input_pitch = state.input_pitch.clamp(-1.5533, 1.5533);
            let player = state.host.player_mut();
            player.yaw = state.input_yaw;
            player.pitch = state.input_pitch;
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Some(limit) = state.auto_exit
            && state.started.elapsed() >= limit
        {
            if let Err(err) = state.host.save() {
                log::warn!("save on auto-exit failed: {err}");
            }
            state.log_frame_stats();
            event_loop.exit();
            return;
        }
        state.window.request_redraw();
    }
}

/// Parses client CLI options; returns `Err` with a message for bad input.
pub fn options_from_args(flags: &[(String, String)]) -> Result<ClientOptions, String> {
    let mut options = ClientOptions::default();
    for (name, value) in flags {
        match name.as_str() {
            "seed" => options.seed = value.parse().map_err(|_| format!("invalid --seed value '{value}'"))?,
            "world" => options.world = value.clone(),
            "data-dir" => options.data_dir = Some(value.into()),
            "render-distance" => {
                options.render_distance_chunks = value
                    .parse()
                    .map_err(|_| format!("invalid --render-distance value '{value}'"))?;
                if !(2..=16).contains(&options.render_distance_chunks) {
                    return Err("--render-distance must be between 2 and 16 chunks".into());
                }
            }
            "fly" => options.fly = true,
            "x11" => options.force_x11 = true,
            "mesh-workers" => {
                options.mesh_workers = value
                    .parse()
                    .map_err(|_| format!("invalid --mesh-workers value '{value}'"))?;
                if !(1..=mesh_scheduler::MAX_MESH_WORKERS).contains(&options.mesh_workers) {
                    return Err(format!(
                        "--mesh-workers must be between 1 and {}",
                        mesh_scheduler::MAX_MESH_WORKERS
                    ));
                }
            }
            "seconds" => {
                options.seconds = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --seconds value '{value}'"))?,
                );
            }
            "screenshot" => {
                if value.is_empty() {
                    return Err("--screenshot requires a path".into());
                }
                options.screenshot = Some(value.into());
            }
            "help" | "version" => {}
            other => return Err(format!("unknown option --{other}")),
        }
    }
    Ok(options)
}

/// The spawn island hint used before the world loads.
pub fn default_spawn_hint() -> WorldVec {
    WorldVec::new(0.5, SEA_LEVEL as f64 + 2.0, 0.5)
}

/// Stable hash of the loaded world (used by tools and the smoke test).
pub fn world_hash(host: &Host) -> u64 {
    host.state_hash()
}
