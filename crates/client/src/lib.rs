//! The game client: window, input, chunk streaming, integrated host and renderer.
//!
//! v0.0.1 is a single-player tech preview: the client owns an [`ob_server::Host`]
//! directly (the same authoritative simulation the dedicated server uses) and
//! renders its world through the wgpu renderer.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ob_content::{BlockId, Registry};
use ob_core::config::{Settings, TextureStyle, load_settings, save_settings};
use ob_math::{BlockPos, ChunkPos, SEA_LEVEL, SectionPos, WorldVec};
use ob_render::{
    BlockOutline, Camera, DAY_LENGTH_TICKS, Environment, OffscreenRenderer, Renderer, SceneParams, UiFrame,
};
use ob_server::{Host, HostConfig};
use ob_sim::{BlockAction, PlayerInput, REACH};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

pub mod inventory;
pub mod mesh_scheduler;
pub mod mining;
pub mod ui_state;

use inventory::inventory_blocks;
use mesh_scheduler::MeshScheduler;
use mining::MiningState;
use ui_state::{DebugStats, MenuAction, MenuItem, Screen, UiState, facing_label};

/// One fixed simulation tick (20 Hz, DEC-038).
const TICK: Duration = Duration::from_millis(50);
/// Cooldown between right-click block placements.
const PLACE_COOLDOWN: Duration = Duration::from_millis(200);
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
    /// Render the inventory screen offscreen to this PNG path, then exit.
    pub screenshot_inventory: Option<std::path::PathBuf>,
    /// Automatically close the window after this many seconds and log frame stats.
    pub seconds: Option<f32>,
    /// Number of dedicated section-meshing worker threads (1..=16).
    pub mesh_workers: usize,
    /// Texture style override for this run (persisted settings otherwise).
    pub texture_style: Option<TextureStyle>,
    /// Time-of-day override in 0..=1 for screenshots (0 midnight, 0.5 noon).
    pub time_of_day: Option<f32>,
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
            screenshot_inventory: None,
            seconds: None,
            mesh_workers: default_mesh_workers(),
            texture_style: None,
            time_of_day: None,
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
    if let Some(path) = &options.screenshot_inventory {
        return screenshot_with_mode(&options, path, ScreenshotMode::Inventory);
    }
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

/// Which UI overlay an offscreen screenshot should compose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenshotMode {
    /// Crosshair, 9-slot hotbar and selected block name.
    Hud,
    /// Inventory panel over the world with a block hovered.
    Inventory,
}

/// Renders one offscreen frame of the world and writes it as a PNG (RGBA8).
///
/// This requires no window or display; it is used both for screenshots and to
/// verify the GPU pipeline on headless hosts.
pub fn screenshot(options: &ClientOptions, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    screenshot_with_mode(options, path, ScreenshotMode::Hud)
}

fn screenshot_with_mode(
    options: &ClientOptions,
    path: &std::path::Path,
    mode: ScreenshotMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let (settings, _data_dir) = effective_settings(options);
    let registry = Arc::new(Registry::base());
    let config = HostConfig {
        seed: options.seed,
        world_name: options.world.clone(),
        data_dir: options.data_dir.clone(),
        render_distance_chunks: settings.render_distance_chunks,
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

    let mut renderer = OffscreenRenderer::new_with_style(
        SCREENSHOT_WIDTH,
        SCREENSHOT_HEIGHT,
        render_texture_style(settings.texture_style),
    )?;

    // Compose a showcase frame: HUD or inventory drawn over the world.
    // Screenshot mode is also the headless verification path for the UI
    // renderer, since it needs no window or surface.
    let screen = [SCREENSHOT_WIDTH as f32, SCREENSHOT_HEIGHT as f32];
    let mut ui = UiState::default();
    let mut frame = UiFrame::new(screen[0], screen[1]);
    let (centre_x, centre_y) = (screen[0] / 2.0, screen[1] / 2.0);
    match mode {
        ScreenshotMode::Hud => {
            let cross = [0.95, 0.97, 1.0, 0.85];
            frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
            frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
            ui.block_name_seconds = ui_state::BLOCK_NAME_SECONDS;
            draw_hotbar(&mut frame, &ui, &registry, screen);
            if let Some(label) = ui
                .hotbar_block(&registry, ui.hotbar_selected)
                .map(|block| inventory::display_name(block.id))
            {
                frame.text_centered(
                    centre_x,
                    ui.hotbar_rects(screen)[0].y - 30.0,
                    2.0,
                    [1.0, 1.0, 1.0, 1.0],
                    &label,
                );
            }
        }
        ScreenshotMode::Inventory => {
            ui.open_inventory();
            let items = inventory_blocks(&registry);
            if let Some(index) = items.iter().position(|id| registry.block(*id).id == "openblock:log") {
                ui.inventory.selected = index;
                ui.inventory.hovered = Some(index);
                ui.hotbar_selected = 4;
            }
            inventory::draw(&mut frame, &ui, &registry, screen);
            // Keep the hotbar visible above the dimmed world, as in gameplay.
            draw_hotbar(&mut frame, &ui, &registry, screen);
        }
    }
    let environment = options.time_of_day.map(Environment::new).unwrap_or_default();
    let scene = SceneParams {
        environment,
        ui: Some(&frame),
        outline: None,
    };
    renderer.render_world_scene(host.world(), &camera, &scene)?;
    let rgba = renderer.read_rgba();
    if rgba.len() != (SCREENSHOT_WIDTH * SCREENSHOT_HEIGHT * 4) as usize {
        return Err(format!(
            "offscreen readback returned {} bytes, expected {}",
            rgba.len(),
            SCREENSHOT_WIDTH * SCREENSHOT_HEIGHT * 4
        )
        .into());
    }

    write_png(path, SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT, &rgba)?;

    log::info!(
        "screenshot written to {} ({}x{}, GPU {})",
        path.display(),
        SCREENSHOT_WIDTH,
        SCREENSHOT_HEIGHT,
        renderer.adapter_summary()
    );
    Ok(())
}

/// Draws the nine-slot hotbar, skipping slots whose blocks are missing from the
/// active registry.
fn draw_hotbar(frame: &mut UiFrame, ui: &UiState, registry: &Registry, screen: [f32; 2]) {
    for (index, slot) in ui.hotbar_rects(screen).iter().enumerate() {
        let selected = index == ui.hotbar_selected;
        let background = if selected {
            [0.24, 0.28, 0.34, 0.88]
        } else {
            [0.10, 0.11, 0.14, 0.66]
        };
        frame.rect(slot.x, slot.y, slot.w, slot.h, background);
        let border = if selected {
            [1.0, 0.92, 0.45, 1.0]
        } else {
            [0.62, 0.66, 0.72, 0.9]
        };
        frame.outline(slot.x, slot.y, slot.w, slot.h, if selected { 2.0 } else { 1.0 }, border);
        if let Some(block) = ui.hotbar_block(registry, index) {
            frame.block_icon(
                slot.x + 6.0,
                slot.y + 5.0,
                slot.w - 12.0,
                u32::from(block.textures.top.0),
                u32::from(block.textures.side.0),
            );
        }
        frame.text(
            slot.x + 3.0,
            slot.y + 2.0,
            1.0,
            [1.0, 1.0, 1.0, 0.75],
            &format!("{}", index + 1),
        );
    }
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
    mouse_left: bool,
    mining: MiningState,
    last_place: Option<Instant>,
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
    ui: UiState,
    data_dir: PathBuf,
    fullscreen: bool,
    applied_render_distance: i32,
    rolling_frame_ms: Vec<f32>,
    window_size: (f32, f32),
    debug_stats: DebugStats,
    screenshot_requested: bool,
}

impl GameState {
    fn persist_settings(&self) {
        if let Err(err) = save_settings(&self.data_dir, &self.ui.settings) {
            log::warn!("could not save settings: {err}");
        }
    }

    fn window_screen(&self) -> [f32; 2] {
        [self.window_size.0.max(1.0), self.window_size.1.max(1.0)]
    }

    fn apply_fullscreen(&mut self, fullscreen: bool) {
        if fullscreen == self.fullscreen {
            return;
        }
        use winit::window::Fullscreen;
        self.window.set_fullscreen(if fullscreen {
            Some(Fullscreen::Borderless(None))
        } else {
            None
        });
        self.fullscreen = fullscreen;
    }

    fn handle_escape(&mut self) {
        match self.ui.screen {
            Screen::Playing => {
                self.cancel_mining();
                self.ui.open_pause();
                self.release_pointer();
            }
            Screen::Inventory => {
                // Esc closes the inventory back to gameplay; it must not open
                // the pause menu.
                self.close_inventory();
            }
            Screen::Settings => {
                self.ui.apply(MenuItem::Back);
            }
            Screen::Pause => {
                self.ui.resume();
                self.capture_pointer();
            }
        }
        self.persist_settings();
    }

    /// Opens the creative inventory and releases the pointer.
    fn open_inventory(&mut self) {
        self.cancel_mining();
        self.ui.open_inventory();
        let item_count = inventory_blocks(&self.registry).len();
        self.ui.update_inventory_hover(self.window_screen(), item_count);
        self.release_pointer();
    }

    /// Closes the inventory, returns to gameplay and recaptures the pointer.
    fn close_inventory(&mut self) {
        self.ui.close_inventory();
        self.capture_pointer();
    }

    /// Stops any in-flight mining and clears the physical mouse-button latch.
    fn cancel_mining(&mut self) {
        self.mouse_left = false;
        self.mining.stop();
    }

    fn menu_move(&mut self, delta: i32) {
        let count = self.ui.menu_items().len();
        if count == 0 {
            return;
        }
        let rows = count as i32;
        self.ui.selected_row = (self.ui.selected_row as i32 + delta).rem_euclid(rows) as usize;
    }

    fn activate_selected(&mut self) -> bool {
        let Some(item) = self.ui.menu_items().get(self.ui.selected_row).copied() else {
            return false;
        };
        let action = match item {
            MenuItem::Resume => MenuAction::Resume,
            MenuItem::OpenSettings => MenuAction::OpenSettings,
            MenuItem::SaveAndQuit => MenuAction::SaveAndQuit,
            MenuItem::Back => MenuAction::Back,
            adjustment => {
                self.ui.apply(adjustment);
                MenuAction::Adjust(adjustment)
            }
        };
        self.handle_menu_action(action)
    }

    /// Applies a menu action; returns true when the process should exit.
    fn handle_menu_action(&mut self, action: MenuAction) -> bool {
        match action {
            MenuAction::Resume => {
                self.ui.resume();
                self.capture_pointer();
                false
            }
            MenuAction::OpenSettings => {
                self.ui.apply(MenuItem::OpenSettings);
                false
            }
            MenuAction::Back => {
                self.ui.apply(MenuItem::Back);
                false
            }
            MenuAction::SaveAndQuit => {
                if let Err(err) = self.host.save() {
                    log::warn!("save on quit failed: {err}");
                }
                self.persist_settings();
                true
            }
            MenuAction::Adjust(item) => {
                match item {
                    MenuItem::RenderDistanceDown | MenuItem::RenderDistanceUp => {
                        let requested = self.ui.settings.render_distance_chunks;
                        if self.host.set_render_distance_chunks(requested) {
                            self.applied_render_distance = requested;
                        }
                    }
                    MenuItem::ToggleFullscreen => {
                        self.apply_fullscreen(self.ui.settings.fullscreen);
                    }
                    MenuItem::TextureStylePrevious | MenuItem::TextureStyleNext => {
                        self.renderer
                            .set_texture_style(render_texture_style(self.ui.settings.texture_style));
                    }
                    _ => {}
                }
                self.persist_settings();
                false
            }
        }
    }

    /// Refreshes the F3 statistics snapshot from the host, renderer and scheduler.
    fn refresh_debug_stats(&mut self) {
        let player = self.host.player();
        let eye = player.eye();
        let pos = [eye.x, eye.y, eye.z];
        let chunk = [
            (eye.x.floor() as i32).div_euclid(16),
            (eye.y.floor() as i32).div_euclid(16),
            (eye.z.floor() as i32).div_euclid(16),
        ];
        let target = self.host.world().raycast(eye, player.forward(), 5.0).map(|hit| {
            let id = self
                .registry
                .block(self.host.world().get_block(hit.block))
                .id
                .to_string();
            (id, [hit.block.x, hit.block.y, hit.block.z])
        });
        let mut rolling = self.rolling_frame_ms.clone();
        rolling.sort_by(f32::total_cmp);
        let percentile = |p: f32| -> f32 {
            if rolling.is_empty() {
                return 0.0;
            }
            let index = ((rolling.len() as f32 - 1.0) * p / 100.0).round() as usize;
            rolling[index.min(rolling.len() - 1)]
        };
        let frame_p50 = percentile(50.0);
        let frame_p95 = percentile(95.0);
        let render_stats = self.renderer.stats();
        self.debug_stats = DebugStats {
            version: ob_core::VERSION.to_string(),
            fps: self.fps,
            frame_p50_ms: frame_p50,
            frame_p95_ms: frame_p95,
            position: pos,
            chunk,
            facing: facing_label(player.yaw),
            target,
            seed: self.host.world().seed(),
            render_distance: self.host.render_distance_chunks(),
            loaded_chunks: self.host.world().chunk_count(),
            drawn_sections: render_stats.sections as usize,
            cached_sections: self.meshes.len(),
            triangles: render_stats.vertices / 3,
            draw_calls: render_stats.draw_calls as usize,
            mesh_workers: self.mesh_scheduler.workers(),
            mesh_in_flight: self.mesh_scheduler.in_flight_count(),
            mesh_completed: self.mesh_scheduler.completed_count(),
            mesh_backlog: self.host.dirty_sections(),
            heap_bytes: self.host.memory_bytes(),
            ticks: self.host.tick_count(),
            adapter: self.renderer.adapter_summary().to_string(),
            window: [self.window_size.0 as u32, self.window_size.1 as u32],
            fov_degrees: self.ui.settings.fov_degrees,
            time_of_day: Environment::from_ticks(self.host.tick_count() + DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS)
                .time_of_day,
        };
    }

    fn update_title(&mut self) {
        let player = self.host.player();
        let block = self.ui.hotbar_label(self.ui.hotbar_selected).unwrap_or("unknown");
        let status = match self.ui.screen {
            Screen::Playing => {
                if self.captured {
                    ""
                } else {
                    " | click to capture | E inventory | F3 debug"
                }
            }
            Screen::Inventory => " | INVENTORY (E/Esc to close)",
            Screen::Pause => " | PAUSED (Esc to resume)",
            Screen::Settings => " | SETTINGS (Esc to go back)",
        };
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
            status,
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
        self.registry
            .id(self.ui.hotbar[self.ui.hotbar_selected])
            .unwrap_or(BlockId::AIR)
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
        if self.ui.screen == Screen::Playing {
            while self.accumulator >= TICK && ticks < 5 {
                self.accumulator -= TICK;
                self.tick();
                ticks += 1;
            }
        } else {
            // Menus pause simulation and movement input; rendering continues.
            self.accumulator = Duration::ZERO;
        }
        self.update_mining(dt);
        self.ui.tick(dt.as_secs_f32());
        self.rolling_frame_ms.push(dt.as_secs_f32() * 1000.0);
        if self.rolling_frame_ms.len() > 120 {
            self.rolling_frame_ms.drain(..60);
        }
        let alpha = (self.accumulator.as_secs_f32() / TICK.as_secs_f32()).clamp(0.0, 1.0);
        let eye = self.prev_eye.lerp(self.current_eye, alpha as f64);
        let player = self.host.player();
        let mut camera = Camera::new(eye, player.yaw, player.pitch);
        camera.fov_y = self.ui.settings.fov_degrees.to_radians();
        camera.far = (self.host.render_distance_chunks() as f32 * 16.0 + 48.0).max(128.0);
        self.stream_and_mesh();
        // Keep the window title in sync with the effective render distance.
        if self.applied_render_distance != self.host.render_distance_chunks() {
            self.applied_render_distance = self.host.render_distance_chunks();
        }
        // Day starts at sunrise: tick 0 is 06:00 rather than midnight.
        let environment = Environment::from_ticks(self.host.tick_count() + DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS);
        self.refresh_debug_stats();
        let ui_frame = self.build_ui_frame();
        let outline = self
            .mining
            .targeted_block()
            .map(|block| BlockOutline::for_block([block.x, block.y, block.z]));
        let scene = SceneParams {
            environment,
            ui: ui_frame.as_ref(),
            outline,
        };
        if let Err(err) = self.renderer.render_scene(&camera, &scene) {
            log::error!("render error: {err}");
        }
        if std::mem::take(&mut self.screenshot_requested) {
            self.save_screenshot(&camera, ui_frame.as_ref(), &environment, outline);
        }
        self.frames += 1;
        if self.fps_timer.elapsed() >= Duration::from_secs(1) {
            self.fps = self.frames;
            self.frames = 0;
            self.fps_timer = Instant::now();
            self.update_title();
        }
    }

    /// Renders the current world (and HUD) from the player's camera to a PNG (F2).
    fn save_screenshot(
        &mut self,
        camera: &Camera,
        ui: Option<&UiFrame>,
        environment: &Environment,
        outline: Option<BlockOutline>,
    ) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = self.data_dir.join("screenshots").join(format!("openblock-{stamp}.png"));
        let mut renderer = match OffscreenRenderer::new(SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT) {
            Ok(renderer) => renderer,
            Err(err) => {
                log::warn!("screenshot failed: {err}");
                return;
            }
        };
        // Build the UI for the offscreen target size so the HUD is included.
        let ui_frame = if ui.is_some() {
            self.build_ui_frame_at([SCREENSHOT_WIDTH as f32, SCREENSHOT_HEIGHT as f32])
        } else {
            None
        };
        let scene = SceneParams {
            environment: *environment,
            ui: ui_frame.as_ref().or(ui),
            outline,
        };
        if let Err(err) = renderer.render_world_scene(self.host.world(), camera, &scene) {
            log::warn!("screenshot render failed: {err}");
            return;
        }
        let rgba = renderer.read_rgba();
        match write_png(&path, SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT, &rgba) {
            Ok(()) => log::info!("screenshot saved to {}", path.display()),
            Err(err) => log::warn!("screenshot write failed: {err}"),
        }
    }

    /// Short display name of the block in a hotbar slot.
    fn hotbar_label(&self, index: usize) -> Option<&'static str> {
        self.ui.hotbar_label(index)
    }

    /// Builds the UI frame for the current window size.
    fn build_ui_frame(&self) -> Option<UiFrame> {
        self.build_ui_frame_at(self.window_screen())
    }

    /// Builds the UI frame for an explicit target size (screenshots).
    fn build_ui_frame_at(&self, screen: [f32; 2]) -> Option<UiFrame> {
        let [width, height] = screen;
        let playing = self.ui.screen == Screen::Playing;
        let inventory_open = self.ui.screen == Screen::Inventory;
        let show_hotbar = (playing || inventory_open) && self.ui.hud_visible;
        let show_crosshair = playing && self.ui.hud_visible;
        let show_debug = self.ui.debug_overlay;
        let show_menu = matches!(self.ui.screen, Screen::Pause | Screen::Settings);
        if !show_hotbar && !show_crosshair && !show_debug && !show_menu && !inventory_open {
            return None;
        }
        let mut frame = UiFrame::new(width, height);
        let white = [1.0, 1.0, 1.0, 1.0];
        let text_color = [0.94, 0.96, 0.99, 1.0];

        // --- F3 debug overlay: two columns in the top corners ---
        if show_debug {
            let scale = 2.0;
            let line_height = ob_render::line_height(scale);
            let left = self.debug_stats.left_lines();
            let right = self.debug_stats.right_lines();
            let left_width = left
                .iter()
                .map(|line| frame.text_width(scale, line))
                .fold(0.0_f32, f32::max)
                .max(420.0);
            frame.rect(
                6.0,
                6.0,
                left_width + 18.0,
                left.len() as f32 * line_height + 14.0,
                [0.03, 0.04, 0.07, 0.74],
            );
            for (index, text_line) in left.iter().enumerate() {
                frame.text(14.0, 13.0 + index as f32 * line_height, scale, text_color, text_line);
            }
            let right_width = right
                .iter()
                .map(|line| frame.text_width(scale, line))
                .fold(0.0_f32, f32::max);
            let right_x = width - right_width - 14.0;
            frame.rect(
                right_x - 10.0,
                6.0,
                right_width + 18.0,
                right.len() as f32 * line_height + 14.0,
                [0.03, 0.04, 0.07, 0.74],
            );
            for (index, text_line) in right.iter().enumerate() {
                frame.text(
                    right_x,
                    13.0 + index as f32 * line_height,
                    scale,
                    [0.80, 0.86, 0.94, 1.0],
                    text_line,
                );
            }
            frame.text(
                14.0,
                height - 30.0,
                1.5,
                [0.80, 0.86, 0.94, 0.9],
                "F3 debug overlay   F1 HUD   F2 screenshot   Esc menu",
            );
        }

        // --- Inventory: dimmed world, centred block grid and hotbar ---
        if inventory_open {
            inventory::draw(&mut frame, &self.ui, &self.registry, screen);
        }

        // --- HUD: crosshair, mining progress, hotbar and selected name ---
        if show_crosshair || show_hotbar {
            let centre_x = width / 2.0;
            let centre_y = height / 2.0;
            if show_crosshair {
                let cross = [0.95, 0.97, 1.0, 0.85];
                frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
                frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
                if self.mining.is_mining() {
                    mining::draw_progress(&mut frame, (centre_x, centre_y), self.mining.progress());
                }
                if self.ui.break_flash_seconds > 0.0
                    && let Some(name) = &self.ui.break_flash
                {
                    frame.text_centered(centre_x, centre_y + 30.0, 1.5, [0.98, 0.94, 0.78, 0.95], name);
                }
            }

            if show_hotbar {
                draw_hotbar(&mut frame, &self.ui, &self.registry, screen);
                if playing && self.ui.block_name_seconds > 0.0 {
                    let name = self.hotbar_label(self.ui.hotbar_selected).unwrap_or("unknown");
                    let y = self
                        .ui
                        .hotbar_rects(screen)
                        .first()
                        .map(|slot| slot.y - 30.0)
                        .unwrap_or(height - 70.0);
                    frame.text_centered(centre_x, y, 2.0, white, name);
                }
            }

            if playing {
                let mode = if self.host.player().flying {
                    "fly (F)"
                } else {
                    "walk (F to fly)"
                };
                frame.text(12.0, height - 26.0, 1.5, [0.85, 0.89, 0.94, 0.85], mode);
            }
        }

        // --- Pause and settings menus ---
        if show_menu {
            frame.rect(0.0, 0.0, width, height, [0.0, 0.0, 0.0, 0.55]);
            if let Some(panel) = self.ui.panel_rect(screen) {
                frame.rect(panel.x, panel.y, panel.w, panel.h, [0.07, 0.08, 0.11, 0.95]);
                frame.outline(panel.x, panel.y, panel.w, panel.h, 2.0, [0.42, 0.48, 0.56, 1.0]);
                let title = match self.ui.screen {
                    Screen::Pause => "Paused",
                    Screen::Settings => "Settings",
                    Screen::Playing | Screen::Inventory => "",
                };
                frame.text_centered(panel.x + panel.w / 2.0, panel.y + 14.0, 2.5, white, title);
                let rows = self.ui.menu_rects(screen);
                for (index, (item, rect)) in self.ui.menu_items().iter().zip(rows.iter()).enumerate() {
                    let active = index == self.ui.selected_row || self.ui.hovered_row == Some(index);
                    if active {
                        frame.rect(rect.x, rect.y, rect.w, rect.h, [0.23, 0.29, 0.37, 0.96]);
                        frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, [0.95, 0.80, 0.35, 1.0]);
                    }
                    frame.text(rect.x + 10.0, rect.y + 8.0, 1.6, text_color, &self.ui.row_label(*item));
                }
                let hint = match self.ui.screen {
                    Screen::Pause => "Esc resume   Enter/Click select   F3 debug   F2 screenshot",
                    Screen::Settings => "Esc back   Enter/Click adjust   changes are saved",
                    Screen::Playing | Screen::Inventory => "",
                };
                frame.text_centered(
                    panel.x + panel.w / 2.0,
                    panel.y + panel.h - 30.0,
                    1.2,
                    [0.72, 0.78, 0.85, 1.0],
                    hint,
                );
            }
        }
        Some(frame)
    }

    /// Advances hold-to-mine progress from the frame clock.
    ///
    /// Starts mining the breakable block under the crosshair while the left
    /// mouse button is held, resets when the target changes or the button is
    /// released, and applies exactly one break action on completion.
    fn update_mining(&mut self, dt: Duration) {
        if self.ui.screen != Screen::Playing || !self.captured || !self.mouse_left {
            self.mining.stop();
            return;
        }
        let player = self.host.player();
        let current = self
            .host
            .world()
            .raycast(player.eye(), player.forward(), REACH)
            .map(|hit| hit.block);
        let breakable_target = current.and_then(|pos| {
            let block = self.registry.block(self.host.world().get_block(pos));
            block.breakable().then_some((pos, block.hardness))
        });

        self.mining.update(dt.as_secs_f32(), current);
        if let Some(pos) = self.mining.take_completed() {
            self.break_block(pos);
            // Do not immediately start on the next block in the same frame;
            // the next frame re-raycasts the changed world.
            return;
        }
        if self.mining.target().is_none()
            && let Some((pos, hardness)) = breakable_target
        {
            self.mining.start(pos, hardness);
        }
    }

    /// Sends one break action for the completed mining target.
    fn break_block(&mut self, pos: BlockPos) {
        let name = {
            let block = self.registry.block(self.host.world().get_block(pos));
            if block.id == "openblock:air" {
                return;
            }
            inventory::display_name(block.id)
        };
        let action = BlockAction {
            break_block: true,
            place_block: false,
            place: BlockId::AIR,
        };
        if self.host.apply_action(&action) {
            self.ui.flash_broken_block(name);
        }
    }

    /// Starts hold-to-mine for the block currently under the crosshair.
    fn start_mining(&mut self) {
        if !self.captured || self.ui.screen != Screen::Playing {
            return;
        }
        self.mouse_left = true;
        let player = self.host.player();
        let Some(hit) = self.host.world().raycast(player.eye(), player.forward(), REACH) else {
            self.mining.stop();
            return;
        };
        let block = self.registry.block(self.host.world().get_block(hit.block));
        if block.breakable() {
            self.mining.start(hit.block, block.hardness);
        } else {
            self.mining.stop();
        }
    }

    /// Immediately places the selected block, throttled to one per cooldown.
    fn place_block(&mut self) {
        if !self.captured || self.ui.screen != Screen::Playing {
            return;
        }
        if self.last_place.is_some_and(|at| at.elapsed() < PLACE_COOLDOWN) {
            return;
        }
        self.last_place = Some(Instant::now());
        let action = BlockAction {
            break_block: false,
            place_block: true,
            place: self.selected_block(),
        };
        self.host.apply_action(&action);
    }

    /// Number of registry-backed inventory entries (every block except air).
    fn inventory_item_count(&self) -> usize {
        inventory_blocks(&self.registry).len()
    }

    /// Updates the inventory hover from the last cursor position.
    fn update_inventory_hover(&mut self) {
        let item_count = self.inventory_item_count();
        self.ui.update_inventory_hover(self.window_screen(), item_count);
    }

    /// Moves the inventory grid selection with the arrow keys.
    fn move_inventory_selection(&mut self, dx: i32, dy: i32) {
        let item_count = self.inventory_item_count();
        self.ui
            .move_inventory_selection(self.window_screen(), item_count, dx, dy);
    }

    /// Scrolls the inventory grid and re-evaluates the hover under the cursor.
    fn scroll_inventory(&mut self, rows: i32) {
        let item_count = self.inventory_item_count();
        let screen = self.window_screen();
        self.ui.scroll_inventory(screen, item_count, rows);
        self.ui.update_inventory_hover(screen, item_count);
    }

    /// Assigns the current inventory selection and returns to gameplay.
    fn assign_inventory_selection(&mut self) -> bool {
        if inventory::assign_selected_to_hotbar(&mut self.ui, &self.registry).is_some() {
            self.capture_pointer();
            self.update_title();
            true
        } else {
            false
        }
    }

    /// Handles a left click on the inventory: hovering a slot selects and
    /// assigns it. Clicks outside the panel keep the inventory open.
    fn inventory_click(&mut self) {
        self.update_inventory_hover();
        let Some(index) = self.ui.inventory.hovered else {
            return;
        };
        self.ui.inventory.selected = index;
        self.assign_inventory_selection();
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
        let (settings, data_dir) = effective_settings(&self.options);
        let renderer = match Renderer::new_with_style(Arc::clone(&window), render_texture_style(settings.texture_style))
        {
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
        log::info!(
            "settings: {} (render distance {} chunks, fov {:.0}, sensitivity {:.2}x, textures {})",
            ob_core::config::settings_path(&data_dir).display(),
            settings.render_distance_chunks,
            settings.fov_degrees,
            settings.mouse_sensitivity,
            settings.texture_style.name()
        );
        let registry = Arc::new(Registry::base());
        let mesh_scheduler = MeshScheduler::new(self.options.mesh_workers, Arc::clone(&registry));
        log::info!("mesh scheduler: {} worker threads", mesh_scheduler.workers());
        let config = HostConfig {
            seed: self.options.seed,
            world_name: self.options.world.clone(),
            data_dir: self.options.data_dir.clone(),
            render_distance_chunks: settings.render_distance_chunks,
            generate_budget: GENERATE_BUDGET,
        };
        let window_size = {
            let size = window.inner_size();
            (size.width.max(1) as f32, size.height.max(1) as f32)
        };
        let ui = UiState::from_settings(settings);
        let applied_render_distance = ui.settings.render_distance_chunks;
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
            mouse_left: false,
            mining: MiningState::default(),
            last_place: None,
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
            ui,
            data_dir,
            fullscreen: false,
            applied_render_distance,
            rolling_frame_ms: Vec::new(),
            window_size,
            debug_stats: DebugStats::default(),
            screenshot_requested: false,
        };
        if state.fly_toggle {
            let player = state.host.player_mut();
            player.flying = true;
            state.fly_toggle = false;
        }
        let fullscreen = state.ui.settings.fullscreen;
        state.apply_fullscreen(fullscreen);
        state.fullscreen = state.ui.settings.fullscreen;
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
                state.persist_settings();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width.max(1), size.height.max(1));
                state.window_size = (size.width.max(1) as f32, size.height.max(1) as f32);
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
                        KeyCode::KeyF if pressed && state.ui.screen == Screen::Playing => {
                            state.fly_toggle = true;
                        }
                        KeyCode::KeyE if pressed => match state.ui.screen {
                            Screen::Playing => state.open_inventory(),
                            Screen::Inventory => state.close_inventory(),
                            Screen::Pause | Screen::Settings => {}
                        },
                        KeyCode::Escape if pressed => state.handle_escape(),
                        KeyCode::F1 if pressed => {
                            state.ui.apply(MenuItem::ToggleHud);
                            state.persist_settings();
                        }
                        KeyCode::F2 if pressed && state.ui.screen == Screen::Playing => {
                            state.screenshot_requested = true;
                        }
                        KeyCode::F3 if pressed => {
                            state.ui.toggle_debug_overlay();
                            state.persist_settings();
                        }
                        KeyCode::ArrowUp if pressed => {
                            if state.ui.screen == Screen::Inventory {
                                state.move_inventory_selection(0, -1);
                            } else {
                                state.menu_move(-1);
                            }
                        }
                        KeyCode::ArrowDown if pressed => {
                            if state.ui.screen == Screen::Inventory {
                                state.move_inventory_selection(0, 1);
                            } else {
                                state.menu_move(1);
                            }
                        }
                        KeyCode::ArrowLeft if pressed && state.ui.screen == Screen::Inventory => {
                            state.move_inventory_selection(-1, 0);
                        }
                        KeyCode::ArrowRight if pressed && state.ui.screen == Screen::Inventory => {
                            state.move_inventory_selection(1, 0);
                        }
                        KeyCode::Enter if pressed => {
                            if state.ui.screen == Screen::Inventory {
                                state.assign_inventory_selection();
                            } else if state.activate_selected() {
                                state.persist_settings();
                                event_loop.exit();
                            }
                        }
                        KeyCode::Digit1
                        | KeyCode::Digit2
                        | KeyCode::Digit3
                        | KeyCode::Digit4
                        | KeyCode::Digit5
                        | KeyCode::Digit6
                        | KeyCode::Digit7
                        | KeyCode::Digit8
                        | KeyCode::Digit9
                            if state.ui.screen == Screen::Playing =>
                        {
                            if let Some(slot) = digit_slot(code) {
                                state.ui.select_hotbar(slot);
                            }
                        }
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
                if button == MouseButton::Left && element == ElementState::Released {
                    state.cancel_mining();
                }
                if element != ElementState::Pressed {
                    return;
                }
                match state.ui.screen {
                    Screen::Inventory => {
                        if button == MouseButton::Left {
                            state.inventory_click();
                        }
                        return;
                    }
                    Screen::Pause | Screen::Settings => {
                        if button == MouseButton::Left {
                            let screen = state.window_screen();
                            if let Some(action) = state.ui.click(screen)
                                && state.handle_menu_action(action)
                            {
                                state.persist_settings();
                                event_loop.exit();
                            }
                        }
                        return;
                    }
                    Screen::Playing => {}
                }
                if !state.captured {
                    state.capture_pointer();
                    return;
                }
                match button {
                    MouseButton::Left => state.start_mining(),
                    MouseButton::Right => state.place_block(),
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.ui.cursor = (position.x as f32, position.y as f32);
                match state.ui.screen {
                    Screen::Inventory => state.update_inventory_hover(),
                    Screen::Pause | Screen::Settings => {
                        let screen = state.window_screen();
                        state.ui.update_hover(screen);
                    }
                    Screen::Playing => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 40.0).round() as i32,
                };
                match state.ui.screen {
                    Screen::Playing if steps != 0 => {
                        state.ui.cycle_hotbar(steps);
                        state.update_title();
                    }
                    Screen::Inventory if steps != 0 => {
                        // Wheel up (positive y) reveals earlier rows.
                        state.scroll_inventory(-steps);
                    }
                    _ => {}
                }
            }
            WindowEvent::Focused(false) => {
                state.keys = [false; 8];
                state.cancel_mining();
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
        if !state.captured || state.ui.screen != Screen::Playing {
            return;
        }
        if let DeviceEvent::MouseMotion { delta } = event {
            let sensitivity = 0.0022f32 * state.ui.settings.mouse_sensitivity;
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
            state.persist_settings();
            state.log_frame_stats();
            event_loop.exit();
            return;
        }
        state.window.request_redraw();
    }
}

/// Maps the top-row digit keys to zero-based hotbar slots.
fn digit_slot(code: KeyCode) -> Option<usize> {
    match code {
        KeyCode::Digit1 => Some(0),
        KeyCode::Digit2 => Some(1),
        KeyCode::Digit3 => Some(2),
        KeyCode::Digit4 => Some(3),
        KeyCode::Digit5 => Some(4),
        KeyCode::Digit6 => Some(5),
        KeyCode::Digit7 => Some(6),
        KeyCode::Digit8 => Some(7),
        KeyCode::Digit9 => Some(8),
        _ => None,
    }
}

/// Resolves persisted settings, with explicit CLI flags taking precedence.
fn effective_settings(options: &ClientOptions) -> (Settings, PathBuf) {
    let data_dir = options.data_dir.clone().unwrap_or_else(ob_core::data_dir);
    let mut settings = load_settings(&data_dir);
    if options.render_distance_chunks != ClientOptions::default().render_distance_chunks {
        settings.render_distance_chunks = options.render_distance_chunks;
    }
    if let Some(style) = options.texture_style {
        settings.texture_style = style;
    }
    settings.clamp();
    (settings, data_dir)
}

/// Maps a persisted texture style to the renderer's texture-pack style.
fn render_texture_style(style: TextureStyle) -> ob_render::TexturePackStyle {
    match style {
        TextureStyle::Classic => ob_render::TexturePackStyle::Classic,
        TextureStyle::Painterly => ob_render::TexturePackStyle::Painterly,
        TextureStyle::Lush => ob_render::TexturePackStyle::Lush,
        TextureStyle::Minimal => ob_render::TexturePackStyle::Minimal,
    }
}

/// Writes an RGBA8 image to a PNG file.
fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    drop(writer);
    Ok(())
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
            "time-of-day" => {
                let parsed: f32 = value
                    .parse()
                    .map_err(|_| format!("invalid --time-of-day value '{value}'"))?;
                if !parsed.is_finite() || !(0.0..=1.0).contains(&parsed) {
                    return Err("--time-of-day must be between 0 and 1".into());
                }
                options.time_of_day = Some(parsed);
            }
            "texture-style" => {
                options.texture_style =
                    Some(TextureStyle::from_name(value).ok_or_else(|| format!("unknown texture style '{value}'"))?);
            }
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
            "screenshot-inventory" => {
                if value.is_empty() {
                    return Err("--screenshot-inventory requires a path".into());
                }
                options.screenshot_inventory = Some(value.into());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_keys_map_to_all_nine_hotbar_slots() {
        let codes = [
            KeyCode::Digit1,
            KeyCode::Digit2,
            KeyCode::Digit3,
            KeyCode::Digit4,
            KeyCode::Digit5,
            KeyCode::Digit6,
            KeyCode::Digit7,
            KeyCode::Digit8,
            KeyCode::Digit9,
        ];
        for (expected, code) in codes.into_iter().enumerate() {
            assert_eq!(digit_slot(code), Some(expected));
        }
        assert_eq!(digit_slot(KeyCode::Digit0), None);
        assert_eq!(digit_slot(KeyCode::KeyE), None);
    }

    #[test]
    fn screenshot_inventory_flag_is_parsed_and_documented_by_parser() {
        let path = std::path::PathBuf::from("/tmp/inventory.png");
        let options = options_from_args(&[("screenshot-inventory".to_string(), path.to_string_lossy().into_owned())])
            .expect("valid flag");
        assert_eq!(options.screenshot_inventory.as_deref(), Some(path.as_path()));
        assert_eq!(options.screenshot, None);

        let error = options_from_args(&[("screenshot-inventory".to_string(), String::new())])
            .expect_err("empty path must fail");
        assert!(error.contains("--screenshot-inventory"));
    }
}
