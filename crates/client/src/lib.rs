//! The game client: window, input, chunk streaming, renderer and multiplayer.
//!
//! The single-player runtime owns an [`ob_server::Host`] directly (the same
//! authoritative simulation the dedicated server uses) and renders its world
//! through the wgpu renderer. `--connect HOST:PORT` selects the separate
//! network runtime: it replicates the dedicated server's world
//! with [`net::NetworkClient`], simulates only the local player, mirrors the
//! server's survival HUD, inventory, equipment, container windows and
//! mobs/dropped items, relays chat and never opens or saves a local host.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_audio::{AudioEngine, Material as AudioMaterial, Sfx as AudioSfx};
use ob_content::{ArmorSlot, BlockId, MobKind, Registry};
use ob_core::config::{
    Action, BindKey, GameMode as SettingsGameMode, KeyName, Settings, TextureStyle, load_settings, save_settings,
};
use ob_math::{BlockPos, ChunkPos, N_SECTIONS, SEA_LEVEL, SECTION_SIZE, SectionPos, WorldVec};
use ob_render::{
    BlockOutline, Camera, DAY_LENGTH_TICKS, EntityInstance, Environment, LodBatchKey, LodConfig, OffscreenRenderer,
    Renderer, SceneParams, TextureSource, UiFrame, far_plane_for_render_distance,
};
use ob_server::{
    CHEST_SLOTS, DedicatedServer, FurnaceSlot, GameMode as HostGameMode, Host, HostConfig, ItemEntity, ItemStack,
    MOB_ATTACK_REACH, NetConfig,
};
use ob_sim::{BlockAction, PlayerInput, REACH};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

pub mod containers;
pub mod crafting;
pub mod frontend;
pub mod inventory;
pub mod lod_state;
mod menu_app;
pub mod mesh_scheduler;
pub mod mining;
pub mod mobs;
pub mod name_tags;
pub mod net;
mod net_app;
pub mod net_state;
mod net_ui;
pub mod ui_font;
pub mod ui_scale;
pub mod ui_state;

use containers::{ClickButton, ContainerAction, ContainerKind, ContainerSession, ContainerSnapshot};
use crafting::{CraftingRow, crafting_rows};
use inventory::inventory_blocks;
use lod_state::LodState;
use mesh_scheduler::MeshScheduler;
use mining::MiningState;
use mobs::MobRenderCache;
use ui_scale::{physical_to_ui_point, rescale_cursor, resolved_ui_scale_percent, ui_scale_factor, ui_screen};
use ui_state::{
    DamageFlash, DeathAction, DebugColumns, DebugStats, Screen, SettingsAction, SettingsInfo, SettingsRow, SettingsTab,
    UiState, facing_label,
};

/// One fixed simulation tick (20 Hz, DEC-038).
const TICK: Duration = Duration::from_millis(50);
/// Cooldown between right-click block placements.
const PLACE_COOLDOWN: Duration = Duration::from_millis(200);
/// Cooldown between two melee attacks at a mob.
const ATTACK_COOLDOWN: Duration = Duration::from_millis(400);
/// Peak camera pitch offset, in radians, after the player takes damage.
const DAMAGE_SHAKE_RADIANS: f32 = 0.05;
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
const SCREENSHOT_STREAM_BUDGET: usize = 64;
/// Safety bound on screenshot streaming loops (a full 65x65 world needs ~67).
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
    /// External Minecraft-style resource pack for this run; persisted when given.
    pub texture_pack: Option<std::path::PathBuf>,
    /// Clear the persisted resource pack and use the procedural style.
    pub no_texture_pack: bool,
    /// External TTF/OTF UI font for this run; persisted when given.
    pub ui_font: Option<std::path::PathBuf>,
    /// Clear the persisted UI-font override and use the embedded Monocraft.
    pub default_ui_font: bool,
    /// Time-of-day override in 0..=1 for screenshots (0 midnight, 0.5 noon).
    pub time_of_day: Option<f32>,
    /// Explicit gameplay-mode override for this run (persisted when given).
    pub game_mode: Option<SettingsGameMode>,
    /// GUI-scale override in percent (`0` = Auto) for this run.
    ///
    /// Normalised to the supported choices. Used by `--screenshot` audits and
    /// diagnostics; the settings screen persists its own choice.
    pub ui_scale: Option<u16>,
    /// Render the death screen in the offscreen screenshot frame.
    pub screenshot_dead: bool,
    /// Use the network death wording (server keep-inventory, Disconnect).
    pub screenshot_dead_network: bool,
    /// Render the settings screen in the offscreen screenshot frame.
    pub screenshot_settings: bool,
    /// Render the crafting screen in the offscreen screenshot frame.
    pub screenshot_crafting: bool,
    /// Omit the nearby crafting table so the hand-crafting gate is visible.
    pub screenshot_crafting_hand: bool,
    /// Render the survival HUD with the F3 debug overlay in the screenshot.
    pub screenshot_debug: bool,
    /// Seed the host with sample items and vitals for the offscreen screenshot.
    pub screenshot_items: bool,
    /// Render a few spawned mob avatars with the HUD.
    pub screenshot_mobs: bool,
    /// Keep the F3 mob counts overlay in the offscreen mob screenshot.
    pub screenshot_mobs_f3: bool,
    /// Place and open a seeded furnace in the offscreen screenshot frame.
    pub screenshot_furnace: bool,
    /// Furnace contents for the offscreen screenshot.
    pub screenshot_furnace_demo: ContainerDemo,
    /// Place and open a seeded chest in the offscreen screenshot frame.
    pub screenshot_chest: bool,
    /// Chest contents for the offscreen screenshot.
    pub screenshot_chest_demo: ContainerDemo,
    /// Select the settings tab used by an offscreen settings screenshot.
    pub screenshot_settings_tab: Option<SettingsTab>,
    /// Do not open an audio device at all (headless/streaming setups).
    pub no_audio: bool,
    /// Start muted for this run without changing the persisted setting.
    pub mute: bool,
    /// Multiplayer endpoint (`HOST:PORT`); mutually exclusive with local worlds.
    pub connect: Option<String>,
    /// Requested multiplayer player name; `None` chooses `playerNNNN` at start.
    pub name: Option<String>,
    /// Force the title screen even when a bypassing flag (such as --data-dir)
    /// was supplied. `--title` / `--menu`.
    pub force_title: bool,
    /// Render the title screen offscreen with deterministic temporary data.
    pub screenshot_title: bool,
    /// Render the world list offscreen with deterministic temporary worlds.
    pub screenshot_worlds: bool,
    /// Render the multiplayer form offscreen with deterministic recent servers.
    pub screenshot_connect: bool,
    /// Show a deterministic validation error on the offscreen multiplayer form.
    pub screenshot_connect_error: bool,
    /// Render the new-world form offscreen.
    pub screenshot_new_world: bool,
    /// Render the host-optional settings screen offscreen.
    pub screenshot_settings_title: bool,
    /// Render a network survival frame from an in-process loopback server.
    pub screenshot_network: bool,
    /// Render synthetic multiplayer name tags over a world.
    pub screenshot_name_tags: bool,
    /// Render a multiplayer loading/failure/roster overlay.
    pub screenshot_net_ui: Option<NetUiKind>,
    /// Width of offscreen screenshots in pixels (default 1280).
    pub screenshot_width: u32,
    /// Height of offscreen screenshots in pixels (default 720).
    pub screenshot_height: u32,
    /// Whether `--world` was passed explicitly (the title is bypassed).
    pub saw_world: bool,
    /// Whether `--seed` was passed explicitly (the title is bypassed).
    pub saw_seed: bool,
    /// Whether `--data-dir` was passed explicitly (the title is bypassed).
    pub saw_data_dir: bool,
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
            texture_pack: None,
            no_texture_pack: false,
            ui_font: None,
            default_ui_font: false,
            time_of_day: None,
            game_mode: None,
            ui_scale: None,
            screenshot_dead: false,
            screenshot_dead_network: false,
            screenshot_settings: false,
            screenshot_crafting: false,
            screenshot_crafting_hand: false,
            screenshot_debug: false,
            screenshot_items: false,
            screenshot_mobs: false,
            screenshot_mobs_f3: true,
            screenshot_furnace: false,
            screenshot_furnace_demo: ContainerDemo::Active,
            screenshot_chest: false,
            screenshot_chest_demo: ContainerDemo::Full,
            screenshot_settings_tab: None,
            no_audio: false,
            mute: false,
            connect: None,
            name: None,
            force_title: false,
            screenshot_title: false,
            screenshot_worlds: false,
            screenshot_connect: false,
            screenshot_connect_error: false,
            screenshot_new_world: false,
            screenshot_settings_title: false,
            screenshot_network: false,
            screenshot_name_tags: false,
            screenshot_net_ui: None,
            screenshot_width: SCREENSHOT_WIDTH,
            screenshot_height: SCREENSHOT_HEIGHT,
            saw_world: false,
            saw_seed: false,
            saw_data_dir: false,
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

/// Offscreen screenshot dimensions, clamped to a sane range.
pub(crate) fn screenshot_dimensions(options: &ClientOptions) -> (u32, u32) {
    (
        options.screenshot_width.clamp(320, 3840),
        options.screenshot_height.clamp(240, 2160),
    )
}

/// Logs UI geometry for offscreen audit captures.
///
/// The UI renderer batches by texture (solid, UI font, bitmap title font and
/// block icons), so the quad count is the meaningful per-screen workload and
/// draw calls are at most the four texture batches.
pub(crate) fn log_ui_frame(label: &str, frame: &UiFrame) {
    let (vertices, quads) = frame.measure();
    log::info!("ui frame {label}: {quads} quads, {vertices} vertices, <=4 texture draws");
}

/// Runs the client until the window closes, or writes a single screenshot and exits.
///
/// Explicit world/seed/data-dir/connect/screenshot flags preserve the pre-menu
/// behaviour and bypass the title. A plain launch opens the front-end, which
/// returns a world or multiplayer choice and then starts the matching runtime.
pub fn run(options: ClientOptions) -> Result<(), Box<dyn std::error::Error>> {
    // Persist an explicit UI-font choice before any front-end/screenshot path
    // starts so `--ui-font` affects this and future runs exactly like
    // `--texture-pack`. The network demo paths below resolve it separately
    // because they build their renderer from a temporary data directory.
    let (mut startup_settings, startup_data_dir) = effective_settings(&options);
    persist_ui_font_choice(&options, &mut startup_settings, &startup_data_dir);
    if let Some(mode) = menu_screenshot_mode(&options) {
        return menu_app::screenshot_menu(&options, mode);
    }
    if options.connect.is_some() {
        return net_app::run(options);
    }
    if let Some(path) = &options.screenshot_inventory {
        return screenshot_with_mode(&options, path, ScreenshotMode::Inventory);
    }
    if options.screenshot_network {
        let path = options
            .screenshot
            .as_ref()
            .ok_or("--screenshot-network requires --screenshot PATH")?;
        let (width, height) = screenshot_dimensions(&options);
        let startup_font = ui_font::resolve_ui_font(&options, &startup_settings, &startup_data_dir);
        return screenshot_network_demo_at_scaled(
            path,
            None,
            options.seed,
            width,
            height,
            options.ui_scale,
            Some(&startup_font),
        );
    }
    if options.screenshot_name_tags {
        let path = options
            .screenshot
            .as_ref()
            .ok_or("--screenshot-name-tags requires --screenshot PATH")?;
        let (width, height) = screenshot_dimensions(&options);
        let startup_font = ui_font::resolve_ui_font(&options, &startup_settings, &startup_data_dir);
        return screenshot_name_tags_demo_at_scaled(
            path,
            options.seed,
            width,
            height,
            options.ui_scale,
            Some(&startup_font),
        );
    }
    if let Some(path) = &options.screenshot {
        let mode = if let Some(kind) = options.screenshot_net_ui {
            ScreenshotMode::NetUi(kind)
        } else if options.screenshot_settings {
            ScreenshotMode::Settings
        } else if options.screenshot_crafting {
            ScreenshotMode::Crafting
        } else if options.screenshot_dead {
            ScreenshotMode::Dead
        } else if options.screenshot_debug {
            ScreenshotMode::Debug
        } else if options.screenshot_mobs {
            ScreenshotMode::Mobs
        } else if options.screenshot_furnace {
            ScreenshotMode::Furnace
        } else if options.screenshot_chest {
            ScreenshotMode::Chest
        } else {
            ScreenshotMode::Hud
        };
        return screenshot_with_mode(&options, path, mode);
    }
    if should_show_title(&options) {
        let event_loop = build_event_loop(&options)?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let mut app = RootApp::new(options);
        event_loop.run_app(&mut app)?;
        return Ok(());
    }
    run_world(options)
}

/// Builds the process-wide winit event loop. Only one may exist at a time.
fn build_event_loop(options: &ClientOptions) -> Result<EventLoop<()>, Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        use winit::platform::x11::EventLoopBuilderExtX11;
        let mut builder = EventLoop::builder();
        if options.force_x11 || std::env::var_os("OPENBLOCK_X11").is_some() {
            builder.with_x11();
        }
        Ok(builder.build()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // The X11 override is Linux-only; keep the signature uniform.
        let _ = options;
        Ok(EventLoop::builder().build()?)
    }
}

/// Runs the single-player window event loop for an already chosen world.
fn run_world(options: ClientOptions) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = build_event_loop(&options)?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(options);
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Whether a normal launch should open the title screen.
///
/// Any explicit world/seed/data-dir/connect or screenshot flag bypasses the
/// front-end exactly as it did before the title screen existed. `--title`/
/// `--menu` forces it back on for screenshots and debugging.
pub fn should_show_title(options: &ClientOptions) -> bool {
    if options.force_title {
        return true;
    }
    !options.saw_world
        && !options.saw_seed
        && !options.saw_data_dir
        && options.connect.is_none()
        && options.screenshot.is_none()
        && options.screenshot_inventory.is_none()
        && !options.screenshot_title
        && !options.screenshot_worlds
        && !options.screenshot_connect
        && !options.screenshot_new_world
        && !options.screenshot_settings_title
}

/// Clears one-shot UI-font CLI overrides after the front-end has applied and
/// persisted them, so later runtime construction reads the saved settings
/// (including a Reset UI font action) instead of re-applying the flag.
fn clear_ui_font_cli_overrides(options: &mut ClientOptions) {
    options.ui_font = None;
    options.default_ui_font = false;
}

/// Which front-end screen an offscreen screenshot should render.
pub fn menu_screenshot_mode(options: &ClientOptions) -> Option<menu_app::MenuScreenshot> {
    let modes = [
        (options.screenshot_title, menu_app::MenuScreenshot::Title),
        (options.screenshot_new_world, menu_app::MenuScreenshot::NewWorld),
        (options.screenshot_worlds, menu_app::MenuScreenshot::Worlds),
        (
            options.screenshot_connect_error,
            menu_app::MenuScreenshot::MultiplayerError,
        ),
        (options.screenshot_connect, menu_app::MenuScreenshot::Multiplayer),
        (options.screenshot_settings_title, menu_app::MenuScreenshot::Settings),
    ];
    let selected: Vec<_> = modes
        .into_iter()
        .filter_map(|(enabled, mode)| enabled.then_some(mode))
        .collect();
    selected.first().copied()
}

/// Which multiplayer overlay an audit screenshot should compose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetUiKind {
    /// Connecting/loading panel with the spawn-area progress bar.
    Loading,
    /// Connection failure panel.
    Failed,
    /// Disconnected panel.
    Disconnected,
    /// Chat history and the Tab player roster over the HUD.
    Roster,
}

impl NetUiKind {
    /// Parses a `--screenshot-net-ui` value.
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "loading" => Some(Self::Loading),
            "failed" | "failure" | "error" => Some(Self::Failed),
            "disconnected" | "disconnect" => Some(Self::Disconnected),
            "roster" | "chat" | "players" => Some(Self::Roster),
            _ => None,
        }
    }
}

/// Deterministic contents for a container audit screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerDemo {
    /// In-progress furnace / full chest (the release default).
    Active,
    /// Empty slots.
    Empty,
    /// Full input/fuel/output (furnace) or every chest slot filled.
    Full,
}

impl ContainerDemo {
    fn from_name(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "empty" => Some(Self::Empty),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// Which UI overlay an offscreen screenshot should compose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenshotMode {
    /// Crosshair, 9-slot hotbar and selected block name.
    Hud,
    /// Inventory panel over the world with a block hovered.
    Inventory,
    /// Modal death screen over the world.
    Dead,
    /// Tabbed settings screen over the world with a deterministic selection.
    Settings,
    /// Crafting screen over the world with a deterministic selection.
    Crafting,
    /// Survival HUD with the F3 debug overlay and deterministic statistics.
    Debug,
    /// Spawned mob avatars with the survival HUD and F3 mob counts.
    Mobs,
    /// Open, seeded furnace screen.
    Furnace,
    /// Open, seeded chest screen.
    Chest,
    /// Multiplayer loading/failure/roster overlay over a world.
    NetUi(NetUiKind),
}

/// Renders one offscreen frame of the world and writes it as a PNG (RGBA8).
///
/// This requires no window or display; it is used both for screenshots and to
/// verify the GPU pipeline on headless hosts.
pub fn screenshot(options: &ClientOptions, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    screenshot_with_mode(options, path, ScreenshotMode::Hud)
}

/// Renders one offscreen protocol v2 survival frame from an in-process server.
///
/// This is the documentation/demo probe for the network runtime. It opens a
/// temporary world, installs a small survival scene (mobs, dropped items and a
/// server-authoritative hotbar/armor set), starts a real [`DedicatedServer`] on
/// loopback, connects the real [`crate::net::NetworkClient`] and renders one
/// frame with the same HUD helpers the windowed client uses. It needs a Vulkan
/// adapter but no window and never opens a local single-player Host.
///
/// The normal client entry points do not call this function; it exists so a
/// one-off probe can produce a network screenshot without weakening the
/// `--connect` or `--screenshot` CLI contract.
#[doc(hidden)]
pub fn screenshot_network_demo(
    path: &Path,
    debug_path: Option<&Path>,
    seed: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    screenshot_network_demo_at(path, debug_path, seed, SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT)
}

/// Like [`screenshot_network_demo`] with an explicit target size.
#[doc(hidden)]
pub fn screenshot_network_demo_at(
    path: &Path,
    debug_path: Option<&Path>,
    seed: u64,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    screenshot_network_demo_at_scaled(path, debug_path, seed, width, height, None, None)
}

/// Like [`screenshot_network_demo_at`] but applies a GUI-scale override.
pub(crate) fn screenshot_network_demo_at_scaled(
    path: &Path,
    debug_path: Option<&Path>,
    seed: u64,
    width: u32,
    height: u32,
    ui_scale: Option<u16>,
    ui_font: Option<&ui_font::ResolvedUiFont>,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::base());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let data_dir = std::env::temp_dir().join(format!("openblock-net-shot-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&data_dir)?;
    let result = screenshot_network_demo_inner(
        path, debug_path, seed, &registry, &data_dir, width, height, ui_scale, ui_font,
    );
    let _ = std::fs::remove_dir_all(&data_dir);
    result
}

/// Renders a synthetic multiplayer name-tag probe over a real single-player
/// world. The fake remote players exercise the same camera projection, distance
/// fade and overlap culling the network runtime uses, without needing a second
/// live connection. Used only by an ignored screenshot probe.
#[doc(hidden)]
pub fn screenshot_name_tags_demo(path: &Path, seed: u64) -> Result<(), Box<dyn std::error::Error>> {
    screenshot_name_tags_demo_at(path, seed, SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT)
}

/// Like [`screenshot_name_tags_demo`] with an explicit target size.
#[doc(hidden)]
pub fn screenshot_name_tags_demo_at(
    path: &Path,
    seed: u64,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    screenshot_name_tags_demo_at_scaled(path, seed, width, height, None, None)
}

/// Like [`screenshot_name_tags_demo_at`] but applies a GUI-scale override.
pub(crate) fn screenshot_name_tags_demo_at_scaled(
    path: &Path,
    seed: u64,
    width: u32,
    height: u32,
    ui_scale: Option<u16>,
    ui_font: Option<&ui_font::ResolvedUiFont>,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::base());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let data_dir = std::env::temp_dir().join(format!("openblock-name-tags-shot-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&data_dir)?;
    let result = screenshot_name_tags_demo_inner(path, seed, &registry, &data_dir, width, height, ui_scale, ui_font);
    let _ = std::fs::remove_dir_all(&data_dir);
    result
}

#[allow(clippy::too_many_arguments)] // public demo wrapper passes the capture target explicitly
fn screenshot_name_tags_demo_inner(
    path: &Path,
    seed: u64,
    registry: &Arc<Registry>,
    data_dir: &Path,
    width: u32,
    height: u32,
    ui_scale: Option<u16>,
    ui_font_override: Option<&ui_font::ResolvedUiFont>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut host = Host::open(
        Arc::clone(registry),
        HostConfig {
            seed,
            world_name: "name-tags-demo".to_string(),
            data_dir: Some(data_dir.to_path_buf()),
            render_distance_chunks: 4,
            generate_budget: GENERATE_BUDGET,
        },
    )?;
    let mut steps = 0;
    loop {
        if host.ensure_streaming(SCREENSHOT_STREAM_BUDGET) == 0 {
            break;
        }
        steps += 1;
        if steps >= SCREENSHOT_STREAM_GUARD {
            break;
        }
    }
    host.set_game_mode(HostGameMode::Survival);
    seed_screenshot_items(&mut host, registry);
    let mut options = ClientOptions {
        seed,
        data_dir: Some(data_dir.to_path_buf()),
        no_audio: true,
        render_distance_chunks: 4,
        screenshot_width: width,
        screenshot_height: height,
        ui_scale,
        ..ClientOptions::default()
    };
    options.game_mode = Some(SettingsGameMode::Survival);
    let (settings, _) = effective_settings(&options);
    let (mut renderer, _texture_source, _font_source) =
        create_offscreen_renderer(&options, &settings, data_dir, ui_font_override)?;
    let player = host.player();
    let eye = player.eye();
    let mut camera = Camera::new(eye, player.yaw, player.pitch);
    camera.fov_y = settings.fov_degrees.to_radians();
    camera.far = 256.0;
    let forward = camera.forward();
    let right = camera.right();
    let at = |distance: f32, offset: f32| -> [f64; 3] {
        let x = eye.x + f64::from(forward[0]) * f64::from(distance) + f64::from(right[0]) * f64::from(offset);
        let y = eye.y + f64::from(forward[1]) * f64::from(distance) + f64::from(right[1]) * f64::from(offset);
        let z = eye.z + f64::from(forward[2]) * f64::from(distance) + f64::from(right[2]) * f64::from(offset);
        [x, y - 1.6, z]
    };
    let players = [
        (1_u64, "alice", at(7.0, -3.0)),
        (2, "bob_the_builder", at(15.0, 4.4)),
        (3, "carol", at(24.0, -4.6)),
        (4, "dave_far", at(38.0, 5.0)),
    ];
    let avatars: Vec<crate::net::AvatarState> = players
        .iter()
        .map(|(id, name, feet)| crate::net::AvatarState {
            id: *id,
            name: (*name).to_string(),
            pos: *feet,
            yaw: 0.35,
        })
        .collect();
    renderer.set_entities(&crate::net::avatar_instances(&avatars));
    let physical = [width as f32, height as f32];
    let effective_ui_scale = resolved_ui_scale_percent(settings.ui_scale_percent, physical[1]);
    let screen = ui_screen(physical, settings.ui_scale_percent);
    let tags = name_tags::project_name_tags(&camera, screen, &players, name_tags::NAME_TAG_MAX_DISTANCE);
    let mut frame = UiFrame::new(screen[0], screen[1]);
    let centre_x = screen[0] / 2.0;
    let centre_y = screen[1] / 2.0;
    let cross = [0.95, 0.97, 1.0, 0.85];
    frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
    frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
    let mut vitals_ui = UiState::from_settings(settings.clone());
    vitals_ui.ui_scale_percent = effective_ui_scale;
    draw_vitals_for_mode(&mut frame, &vitals_ui, &host, screen);
    let ui = UiState::from_settings(settings.clone());
    draw_hotbar_for_mode(&mut frame, &ui, registry, &host, screen);
    name_tags::draw_name_tags(&mut frame, &tags, screen);
    let environment = Environment::default();
    log_ui_frame("name-tags", &frame);
    renderer.render_world_scene(
        host.world(),
        &camera,
        &SceneParams {
            environment,
            ui: Some(&frame),
            outline: None,
        },
    )?;
    let rgba = renderer.read_rgba();
    write_png(path, width, height, &rgba)?;
    log::info!(
        "name-tag screenshot written to {} ({}x{}, {} projected, {} culled by distance/overlap)",
        path.display(),
        width,
        height,
        tags.len(),
        players.len().saturating_sub(tags.len())
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)] // public demo wrapper passes the capture target explicitly
fn screenshot_network_demo_inner(
    path: &Path,
    debug_path: Option<&Path>,
    seed: u64,
    registry: &Arc<Registry>,
    data_dir: &Path,
    width: u32,
    height: u32,
    ui_scale: Option<u16>,
    ui_font_override: Option<&ui_font::ResolvedUiFont>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut host = Host::open(
        Arc::clone(registry),
        HostConfig {
            seed,
            world_name: "network-demo".to_string(),
            data_dir: Some(data_dir.to_path_buf()),
            render_distance_chunks: 4,
            generate_budget: 8,
        },
    )?;
    for _ in 0..128 {
        if host.ensure_streaming(16) == 0 {
            break;
        }
    }
    host.set_game_mode(HostGameMode::Survival);
    let spawn = host.spawn();
    // A readable line-up in front of the default camera (yaw zero faces -Z).
    let mut seeded_mobs: Vec<u64> = Vec::new();
    for (kind, dx, dz) in [
        (MobKind::Cow, -2.4, -5.2),
        (MobKind::Pig, 1.8, -5.8),
        (MobKind::Sheep, 3.8, -4.6),
        (MobKind::Chicken, 0.2, -3.8),
    ] {
        let mut spawned = None;
        for dy in [0.6, 1.6, 2.6] {
            let pos = WorldVec::new(spawn.x + dx, spawn.y + dy, spawn.z + dz);
            if let Some(id) = host.spawn_mob(kind, pos) {
                spawned = Some(id);
                seeded_mobs.push(id);
                break;
            }
        }
        if spawned.is_none() {
            log::warn!("network demo: could not spawn {kind:?} near the camera");
        }
    }
    for (item_name, dx, dz, count) in [
        ("openblock:raw_iron", -2.0, -5.0, 2),
        ("openblock:apple", 1.6, -5.4, 3),
        ("openblock:stone", 0.2, -4.6, 12),
    ] {
        if let Some(item) = registry.item_id(item_name) {
            host.spawn_item_entity(
                WorldVec::new(spawn.x + dx, spawn.y + 0.6, spawn.z + dz),
                ItemStack::new(item, count),
            );
        }
    }
    let server = DedicatedServer::start(
        host,
        NetConfig {
            listen: "127.0.0.1:0".parse::<SocketAddr>()?,
            max_players: 8,
            motd: "OpenBlock survival demo".to_string(),
            interest_radius_chunks: 4,
            chunks_per_tick: 16,
            save_interval_seconds: 0,
            whitelist: Vec::new(),
            game_mode: HostGameMode::Survival,
        },
    )?;
    let mut client = crate::net::NetworkClient::connect(crate::net::ConnectOptions::new(
        server.local_addr().to_string(),
        "survivor",
        4,
        Arc::clone(registry),
    ))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        client.poll();
        client.maintain_chunks(8);
        if client.state().is_playing()
            && client.survival().vitals_received
            && client.survival().inventory_received
            && client.entities().mob_count() >= 3
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !client.state().is_playing() {
        client.shutdown();
        let _ = server.stop();
        return Err("network demo client did not reach Playing".into());
    }
    for (item_name, count) in [
        ("openblock:iron_pickaxe", 1),
        ("openblock:iron_sword", 1),
        ("openblock:apple", 3),
        ("openblock:dirt", 12),
        ("openblock:iron_helmet", 1),
        ("openblock:iron_chestplate", 1),
        ("openblock:iron_leggings", 1),
        ("openblock:iron_boots", 1),
    ] {
        if !server.give_network_item(client.local_id(), item_name, count) {
            log::warn!("network demo: could not give {item_name}");
        }
    }
    let wait_until = |client: &mut crate::net::NetworkClient, seconds: u64| {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            client.poll();
            client.maintain_chunks(4);
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_until(&mut client, 1);
    // Equip the full iron set through the authoritative equip intent.
    for item_name in [
        "openblock:iron_helmet",
        "openblock:iron_chestplate",
        "openblock:iron_leggings",
        "openblock:iron_boots",
    ] {
        let Some(item) = registry.item_id(item_name) else {
            continue;
        };
        let Some(slot) = client
            .inventory()
            .iter()
            .position(|stack| stack.is_some_and(|stack| stack.item == item))
        else {
            continue;
        };
        if client.send_equip(slot) {
            wait_until(&mut client, 1);
        }
    }
    let pickaxe = registry.item_id("openblock:iron_pickaxe");
    if let Some(slot) = pickaxe.and_then(|item| {
        client
            .inventory()
            .iter()
            .position(|stack| stack.is_some_and(|stack| stack.item == item))
    }) {
        let _ = client.select_slot(slot);
        wait_until(&mut client, 1);
    }
    let _ = server.broadcast_chat("Survival is server-authoritative: mobs, items and containers sync live");
    let _ = server.broadcast_chat("HUD: hearts, hunger, armor, hotbar counts and durability");
    wait_until(&mut client, 1);
    // Aim the demo camera at the nearest replicated mob so the frame always
    // shows a real server mob, not only terrain.
    let now = Instant::now();
    let eye = client.player().eye();
    let mut candidates: Vec<(f64, WorldVec, MobKind, u64)> = Vec::new();
    for id in client.entities().ids() {
        let Some(crate::net_state::ReplicatedKind::Mob(kind)) = client.entities().kind(id) else {
            continue;
        };
        let Some(pos) = client.entities().position(id, now) else {
            continue;
        };
        candidates.push((eye.distance(pos), pos, *kind, id));
    }
    // Prefer one of the mobs seeded before the server started, then any
    // medium-distance quadruped. A chicken that walked up to the camera would
    // fill the frame with its head, so it is the last fallback.
    let seeded = |candidate: &&(f64, WorldVec, MobKind, u64)| seeded_mobs.contains(&candidate.3);
    let target = candidates
        .iter()
        .filter(|candidate| {
            seeded(candidate)
                && candidate.0 >= 3.0
                && matches!(candidate.2, MobKind::Cow | MobKind::Pig | MobKind::Sheep)
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .or_else(|| {
            candidates
                .iter()
                .filter(|candidate| {
                    candidate.0 >= 3.0 && matches!(candidate.2, MobKind::Cow | MobKind::Pig | MobKind::Sheep)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
        })
        .or_else(|| {
            candidates
                .iter()
                .filter(|candidate| candidate.0 >= 3.0)
                .min_by(|a, b| a.0.total_cmp(&b.0))
        })
        .or_else(|| candidates.iter().min_by(|a, b| a.0.total_cmp(&b.0)));
    if let Some((distance, target, _, _)) = target {
        let delta = *target + WorldVec::new(0.0, 0.85, 0.0) - eye;
        if *distance > 0.1 {
            let horizontal = (delta.x * delta.x + delta.z * delta.z).sqrt();
            client.player_mut().yaw = (-delta.x).atan2(-delta.z) as f32;
            client.player_mut().pitch = delta.y.atan2(horizontal) as f32;
        }
    } else {
        client.player_mut().yaw = 0.0;
        client.player_mut().pitch = -0.04;
    }
    wait_until(&mut client, 1);

    let options = ClientOptions {
        seed,
        data_dir: Some(data_dir.to_path_buf()),
        no_audio: true,
        render_distance_chunks: 4,
        screenshot_width: width,
        screenshot_height: height,
        ui_scale,
        ..ClientOptions::default()
    };
    let (settings, _) = effective_settings(&options);
    let (mut renderer, texture_source, _font_source) =
        create_offscreen_renderer(&options, &settings, data_dir, ui_font_override)?;
    let physical = [width as f32, height as f32];
    let effective_ui_scale = resolved_ui_scale_percent(settings.ui_scale_percent, physical[1]);
    let screen = ui_screen(physical, settings.ui_scale_percent);
    let eye = client.player().eye();
    let mut camera = Camera::new(eye, client.player().yaw, client.player().pitch);
    camera.far = far_plane_for_render_distance(client.render_radius());
    let environment = Environment::from_ticks(client.server_tick() + DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS);
    let now = Instant::now();
    let entities = client.entity_instances(now);
    let (mobs, items) = client.entity_counts();
    renderer.set_entities(&entities);
    renderer.render_world_scene(
        client.world(),
        &camera,
        &SceneParams {
            environment,
            ui: None,
            outline: None,
        },
    )?;
    let render_stats = renderer.stats();
    let selected = client.selected_stack();
    let held = selected
        .map(|stack| inventory::item_display_name(registry, stack.item))
        .unwrap_or_else(|| "empty".to_string());
    let held_durability = selected.and_then(|stack| inventory::stack_durability(registry, stack));
    let network_line = format!(
        "Network {}  ping {}  players {}/{}  mobs {mobs}  items {items}",
        server.local_addr(),
        client
            .ping_ms()
            .map(|millis| format!("{millis}ms"))
            .unwrap_or_else(|| "--".to_string()),
        client.player_count(),
        client.server_info().max_players,
    );
    let time_of_day = environment.time_of_day;
    let stats = DebugStats {
        version: ob_core::VERSION.to_string(),
        fps: 60,
        frame_p50_ms: 16.67,
        frame_p95_ms: 17.42,
        position: [eye.x, eye.y, eye.z],
        chunk: [
            (eye.x.floor() as i32).div_euclid(16),
            (eye.y.floor() as i32).div_euclid(16),
            (eye.z.floor() as i32).div_euclid(16),
        ],
        facing: facing_label(client.player().yaw),
        target: None,
        seed,
        render_distance: client.render_radius(),
        loaded_chunks: client.loaded_chunks(),
        drawn_sections: render_stats.sections as usize,
        cached_sections: renderer.cached_mesh_count(),
        lod_batches: render_stats.lod_batches as usize,
        lod_sections: 0,
        lod_skipped_sections: 0,
        mesh_bytes: render_stats.vertices as usize * std::mem::size_of::<ob_render::Vertex>() * 2,
        triangles: render_stats.vertices / 3,
        draw_calls: render_stats.draw_calls as usize,
        mesh_workers: 0,
        mesh_in_flight: 0,
        mesh_completed: 0,
        mesh_backlog: 0,
        heap_bytes: client.world().section_count().saturating_mul(8 * 1024),
        ticks: client.session_ticks(),
        adapter: format!("{} (offscreen demo)", renderer.adapter_summary()),
        window: [width, height],
        fov_degrees: settings.fov_degrees,
        ui_scale: crate::ui_scale::debug_line(settings.ui_scale_percent, [width as f32, height as f32]),
        time_of_day,
        texture_source: describe_texture_source(&texture_source, renderer.texture_resolution()),
        ui_font: renderer.ui_font_name().to_string(),
        game_mode: "survival",
        health: client.health(),
        hunger: client.hunger(),
        movement: "walk",
        held,
        held_durability,
        mining_speed: 1.0,
        audio: "off (headless demo)".to_string(),
        network: Some(network_line),
        mobs_total: mobs,
        mobs_passive: client.entities().passive_mob_count(registry),
        mobs_hostile: client.entities().hostile_mob_count(registry),
        day_phase: client.server_tick() % 24_000,
        day: time_of_day > 0.25 && time_of_day < 0.75,
        defense: client.armor_defense(),
    };

    let mut ui = UiState::from_settings(settings.clone());
    ui.ui_scale_percent = effective_ui_scale;
    ui.hotbar_selected = client.selected_slot();
    ui.block_name_seconds = ui_state::BLOCK_NAME_SECONDS;
    ui.toasts.push("Picked up Raw Iron x2");
    ui.toasts.push("Picked up Apple x3");
    ui.toasts.push_success("Equipped Iron Armor");
    let mut frame = UiFrame::new(screen[0], screen[1]);
    let centre_x = screen[0] / 2.0;
    let centre_y = screen[1] / 2.0;
    let cross = [0.95, 0.97, 1.0, 0.85];
    frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
    frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
    ui_state::draw_vitals(&mut frame, screen, client.health(), client.hunger());
    ui_state::draw_armor(&mut frame, screen, client.armor_defense());
    inventory::draw_survival_hotbar(
        &mut frame,
        &ui,
        registry,
        client.inventory(),
        client.selected_slot(),
        screen,
    );
    if let Some(stack) = selected {
        let label = inventory::item_display_name(registry, stack.item);
        let y = selected_name_y(screen, &ui.hotbar_rects(screen));
        let width = frame.text_width(2.0, &label);
        frame.rect(
            screen[0] / 2.0 - width / 2.0 - 7.0,
            y - 3.0,
            width + 14.0,
            ob_render::line_height(2.0) + 6.0,
            [0.02, 0.03, 0.05, 0.62],
        );
        frame.text_centered(screen[0] / 2.0, y, 2.0, [1.0, 1.0, 1.0, 1.0], &label);
    }
    ui_state::draw_toasts(&mut frame, &ui.toasts, screen);
    crate::net_ui::draw_chat(&mut frame, client.chat_log(), None, screen);
    let render_and_write =
        |renderer: &mut OffscreenRenderer, frame: &UiFrame, path: &Path| -> Result<(), Box<dyn std::error::Error>> {
            let scene = SceneParams {
                environment,
                ui: Some(frame),
                outline: None,
            };
            renderer.render_world_scene(client.world(), &camera, &scene)?;
            let rgba = renderer.read_rgba();
            if rgba.len() != (width * height * 4) as usize {
                return Err(format!(
                    "offscreen readback returned {} bytes, expected {}",
                    rgba.len(),
                    width * height * 4
                )
                .into());
            }
            write_png(path, width, height, &rgba)?;
            Ok(())
        };
    log_ui_frame("network-survival", &frame);
    render_and_write(&mut renderer, &frame, path)?;
    log::info!(
        "network survival screenshot written to {} ({}x{}, {mobs} mobs, {items} items, {} chunks, health {:.1}, hunger {:.1}, defence {})",
        path.display(),
        width,
        height,
        client.loaded_chunks(),
        client.health(),
        client.hunger(),
        client.armor_defense(),
    );
    if let Some(debug_path) = debug_path {
        draw_debug_overlay(&mut frame, &stats, screen);
        render_and_write(&mut renderer, &frame, debug_path)?;
        log::info!(
            "network survival F3 screenshot written to {} ({mobs} mobs, {items} items)",
            debug_path.display()
        );
    }
    client.shutdown();
    let _ = server.stop();
    Ok(())
}

fn screenshot_with_mode(
    options: &ClientOptions,
    path: &std::path::Path,
    mode: ScreenshotMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut settings, data_dir) = effective_settings(options);
    persist_texture_pack_choice(options, &mut settings, &data_dir);
    persist_game_mode_choice(options, &mut settings, &data_dir);
    persist_ui_font_choice(options, &mut settings, &data_dir);
    let registry = Arc::new(Registry::base());
    let config = HostConfig {
        seed: options.seed,
        world_name: options.world.clone(),
        data_dir: options.data_dir.clone(),
        render_distance_chunks: settings.render_distance_chunks,
        generate_budget: GENERATE_BUDGET,
    };
    let mut host = Host::open(Arc::clone(&registry), config)?;
    host.set_game_mode(map_game_mode(settings.game_mode));
    log::info!("game mode: {}", host.game_mode().name());
    if options.screenshot_items
        || matches!(
            mode,
            ScreenshotMode::Crafting
                | ScreenshotMode::Debug
                | ScreenshotMode::Mobs
                | ScreenshotMode::Furnace
                | ScreenshotMode::Chest
                | ScreenshotMode::NetUi(_)
        )
    {
        seed_screenshot_items(&mut host, &registry);
    }
    if mode == ScreenshotMode::Crafting && !options.screenshot_crafting_hand {
        seed_near_crafting_table(&mut host, &registry);
    }
    if mode == ScreenshotMode::Dead {
        host.damage(ob_sim::MAX_HEALTH);
    }

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

    // Demo-only world seeding needs loaded chunks for mob collision checks and
    // block placement raycasts, so it runs after the first full stream.
    let mut screenshot_container: Option<(ContainerKind, BlockPos)> = None;
    match mode {
        ScreenshotMode::Mobs => {
            let count = seed_screenshot_mobs(&mut host, &registry);
            log::info!(
                "mob screenshot: {} mobs spawned ({} passive, {} hostile), {} avatar cubes",
                count,
                host.passive_mob_count(),
                host.hostile_mob_count(),
                mobs::mob_instances(&registry, host.mobs(), 1.0).len(),
            );
        }
        ScreenshotMode::Furnace => {
            if let Some(pos) = seed_screenshot_container(&mut host, &registry, "openblock:furnace") {
                if options.screenshot_furnace_demo != ContainerDemo::Empty {
                    seed_screenshot_furnace(
                        &mut host,
                        &registry,
                        pos,
                        options.screenshot_furnace_demo == ContainerDemo::Full,
                    );
                }
                screenshot_container = Some((ContainerKind::Furnace, pos));
            } else {
                log::warn!("furnace screenshot: could not place a furnace");
            }
        }
        ScreenshotMode::Chest => {
            if let Some(pos) = seed_screenshot_container(&mut host, &registry, "openblock:chest") {
                if options.screenshot_chest_demo != ContainerDemo::Empty {
                    seed_screenshot_chest(&mut host, &registry, pos);
                }
                screenshot_container = Some((ContainerKind::Chest, pos));
            } else {
                log::warn!("chest screenshot: could not place a chest");
            }
        }
        _ => {}
    }

    let (eye, yaw, pitch) = {
        let player = host.player();
        (player.eye(), player.yaw, player.pitch)
    };
    let mut camera = Camera::new(eye, yaw, pitch);
    camera.far = far_plane_for_render_distance(host.render_distance_chunks());

    let (mut renderer, texture_source, font_source) = create_offscreen_renderer(options, &settings, &data_dir, None)?;
    log_texture_source(&texture_source);

    // Compose a showcase frame: HUD, inventory or death screen over the world.
    // Screenshot mode is also the headless verification path for the UI
    // renderer, since it needs no window or surface.
    let (width, height) = screenshot_dimensions(options);
    let physical = [width as f32, height as f32];
    let effective_ui_scale = resolved_ui_scale_percent(settings.ui_scale_percent, physical[1]);
    let screen = ui_screen(physical, settings.ui_scale_percent);
    let mut ui = UiState::from_settings(settings.clone());
    ui.ui_scale_percent = effective_ui_scale;
    ui.active_texture_pack = active_texture_pack_label(&texture_source);
    ui.active_ui_font_name = renderer.ui_font_name().to_string();
    ui.active_ui_font_source = font_source.display_label();
    ui.hotbar_selected = host.hotbar_selection();
    if options.screenshot_items
        || matches!(
            mode,
            ScreenshotMode::Crafting
                | ScreenshotMode::Debug
                | ScreenshotMode::Mobs
                | ScreenshotMode::Furnace
                | ScreenshotMode::Chest
        )
    {
        ui.toasts.push("Picked up Cobblestone x8");
        ui.toasts.push("Picked up Apple x3");
        ui.toasts.push_warning("Inventory full - dropped Log x2");
        ui.block_name_seconds = ui_state::BLOCK_NAME_SECONDS;
    }
    let environment = options.time_of_day.map(Environment::new).unwrap_or_default();
    // The debug screenshot reports real world counters. Render the world once
    // without the overlay to fill `RenderStats` deterministically, then compose
    // the final frame with the HUD and F3 overlay on top.
    let debug_render_stats = if matches!(mode, ScreenshotMode::Debug | ScreenshotMode::Mobs) {
        renderer.render_world_scene_lod(
            host.world(),
            &camera,
            &SceneParams {
                environment,
                ui: None,
                outline: None,
            },
            &LodConfig::for_render_distance(host.render_distance_chunks()),
        )?;
        Some(renderer.stats())
    } else {
        None
    };
    let mut frame = UiFrame::new(screen[0], screen[1]);
    let centre_x = screen[0] / 2.0;
    let centre_y = screen[1] / 2.0;
    let survival = host.game_mode() == HostGameMode::Survival;
    match mode {
        ScreenshotMode::Hud => {
            ui.block_name_seconds = ui_state::BLOCK_NAME_SECONDS;
            let cross = [0.95, 0.97, 1.0, 0.85];
            frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
            frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
            draw_vitals_for_mode(&mut frame, &ui, &host, screen);
            draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
            draw_selected_name(&mut frame, &ui, &registry, &host, screen);
            if survival {
                ui_state::draw_toasts(&mut frame, &ui.toasts, screen);
            }
        }
        ScreenshotMode::Inventory => {
            ui.open_inventory();
            if survival {
                // Hover the equipped chestplate so the footer shows its
                // slot/defence/durability line; the hotbar keeps the held iron
                // tool selected. With `--screenshot-items` a full iron set is
                // equipped, so the armor row shows four pieces and 15 defence.
                let focus = host.hotbar_selection();
                ui.inventory.selected = focus;
                ui.inventory.hovered = Some(focus);
                ui.inventory.armor_hovered = Some(ArmorSlot::Chestplate);
                inventory::draw_survival(
                    &mut frame,
                    &ui,
                    &registry,
                    &inventory::SurvivalInventoryView {
                        inventory: host.inventory(),
                        equipment: host.equipment(),
                        defense: host.armor_defense(),
                        selected: host.hotbar_selection(),
                        near_table: host.near_crafting_table(),
                        craft_hint: None,
                    },
                    screen,
                );
                log::info!(
                    "inventory screenshot: focus slot {} item {}, armor {:?}, defence {}",
                    focus,
                    host.inventory()[focus]
                        .map(|stack| inventory::item_display_name(&registry, stack.item))
                        .unwrap_or_else(|| "empty".to_string()),
                    host.equipment(),
                    host.armor_defense(),
                );
            } else {
                let items = inventory_blocks(&registry);
                if let Some(index) = items.iter().position(|id| registry.block(*id).id == "openblock:log") {
                    ui.inventory.selected = index;
                    ui.inventory.hovered = Some(index);
                    ui.hotbar_selected = 4;
                }
                inventory::draw(&mut frame, &ui, &registry, screen);
            }
            // The survival inventory includes its own hotbar row; only the
            // creative palette needs the gameplay hotbar underneath it. Drawing
            // both at 960x540 with 150% scale overlapped the footer.
            if !survival {
                draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
            }
        }
        ScreenshotMode::Dead => {
            ui.open_death();
            draw_vitals_for_mode(&mut frame, &ui, &host, screen);
            draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
            if options.screenshot_dead_network {
                ui.death_cause = Some("Cause: slain by a zombie".to_string());
                ui_state::draw_death_screen_custom(
                    &mut frame,
                    &ui,
                    ui.death_cause.as_deref(),
                    ["Respawn", "Disconnect"],
                    "The server keeps your inventory",
                    "Enter/Click select   Up/Down choose   Esc focus buttons   F3 debug",
                    screen,
                );
            } else {
                ui.death_cause = Some("Cause: lost all health".to_string());
                ui_state::draw_death_screen(&mut frame, &ui, screen);
            }
        }
        ScreenshotMode::Settings => {
            ui.open_settings();
            if let Some(tab) = options.screenshot_settings_tab {
                ui.settings_select_tab(tab);
            }
            if ui.settings_tab == SettingsTab::Display
                && let Some(index) = ui.settings_rows().iter().position(|row| *row == SettingsRow::UiFont)
            {
                // Focus the UI-font row so the capture shows both the active
                // font/source and the placement hint.
                ui.settings_selected_row = index;
            }
            if ui.settings_tab == SettingsTab::Controls {
                // Show the capture affordance on the row it belongs to rather
                // than leaving Move forward highlighted while Jump captures.
                if let Some(index) = ui
                    .settings_rows()
                    .iter()
                    .position(|row| *row == SettingsRow::Keybind(Action::Jump))
                {
                    ui.settings_selected_row = index;
                }
                ui.begin_bind(Action::Jump);
            }
            let info = SettingsInfo {
                health: host.player().health,
                hunger: host.player().hunger,
                texture_source: texture_source.label(),
                texture_resolution: renderer.texture_resolution(),
                mesh_workers: options.mesh_workers,
                ui_scale_percent: effective_ui_scale,
                audio_status: "off (headless screenshot)".to_string(),
            };
            ui_state::draw_settings_screen(&mut frame, &ui, &info, screen);
            log::info!(
                "settings screenshot: tab {}, row {}, source {} {}px",
                ui.settings_tab.name(),
                ui.settings_focused_row().map(|row| row.label()).unwrap_or("Resume"),
                info.texture_source,
                info.texture_resolution,
            );
        }
        ScreenshotMode::Crafting => {
            ui.open_crafting();
            let rows = crafting_rows_for_host(&host, &registry);
            if let Some(index) = rows.iter().position(|row| row.output_name == "Iron Pickaxe") {
                ui.crafting.selected = index;
            }
            ui.crafting.ensure_visible(screen, &rows);
            ui.toasts.push_success("crafted 4 planks");
            crafting::draw_crafting_screen(
                &mut frame,
                &ui.crafting,
                &rows,
                &registry,
                host.near_crafting_table(),
                host.game_mode() == HostGameMode::Creative,
                screen,
            );
            // The crafting panel is modal; toasts would sit on top of it.
            let _ = survival;
            let sections: Vec<&str> =
                rows.iter()
                    .map(|row| row.section.title())
                    .fold(Vec::new(), |mut sections, title| {
                        if sections.last().copied() != Some(title) {
                            sections.push(title);
                        }
                        sections
                    });
            log::info!(
                "crafting screenshot: {} recipes, sections {:?}, near table {}, selected {}, scroll {}, mode {}",
                rows.len(),
                sections,
                host.near_crafting_table(),
                rows.get(ui.crafting.selected)
                    .map(|row| row.output_name.as_str())
                    .unwrap_or("none"),
                ui.crafting.scroll,
                host.game_mode().name(),
            );
        }
        ScreenshotMode::Debug => {
            ui.block_name_seconds = ui_state::BLOCK_NAME_SECONDS;
            ui.debug_overlay = true;
            let cross = [0.95, 0.97, 1.0, 0.85];
            frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
            frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
            draw_vitals_for_mode(&mut frame, &ui, &host, screen);
            draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
            draw_selected_name(&mut frame, &ui, &registry, &host, screen);
            if survival {
                ui_state::draw_toasts(&mut frame, &ui.toasts, screen);
            }
            let stats = screenshot_debug_stats(
                &host,
                &registry,
                &renderer,
                &settings,
                options,
                &texture_source,
                debug_render_stats.unwrap_or_default(),
            );
            let layout = ui_state::debug_overlay_layout(screen, &stats);
            draw_debug_overlay(&mut frame, &stats, screen);
            log::info!(
                "debug screenshot: {} stats lines, scale {:.1}, columns {:?}, compact {}, overlay {}x{}",
                if layout.compact {
                    stats.compact_left_lines().len()
                } else {
                    stats.left_lines().len()
                },
                layout.left.scale,
                layout.columns,
                layout.compact,
                layout.left.panel.w,
                layout.left.panel.h,
            );
        }
        ScreenshotMode::Mobs => {
            ui.block_name_seconds = ui_state::BLOCK_NAME_SECONDS;
            ui.debug_overlay = options.screenshot_mobs_f3;
            let cross = [0.95, 0.97, 1.0, 0.85];
            frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
            frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
            draw_vitals_for_mode(&mut frame, &ui, &host, screen);
            draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
            draw_selected_name(&mut frame, &ui, &registry, &host, screen);
            if survival && options.screenshot_mobs_f3 {
                ui_state::draw_toasts(&mut frame, &ui.toasts, screen);
            }
            if options.screenshot_mobs_f3 {
                let stats = screenshot_debug_stats(
                    &host,
                    &registry,
                    &renderer,
                    &settings,
                    options,
                    &texture_source,
                    debug_render_stats.unwrap_or_default(),
                );
                draw_debug_overlay(&mut frame, &stats, screen);
                log::info!(
                    "mob screenshot: {} live mobs ({} passive, {} hostile), {} cubes, {} debug lines",
                    host.mob_count(),
                    host.passive_mob_count(),
                    host.hostile_mob_count(),
                    mobs::mob_instances(&registry, host.mobs(), 1.0).len(),
                    stats.left_lines().len(),
                );
            } else {
                log::info!(
                    "mob screenshot (plain): {} live mobs ({} passive, {} hostile), {} cubes",
                    host.mob_count(),
                    host.passive_mob_count(),
                    host.hostile_mob_count(),
                    mobs::mob_instances(&registry, host.mobs(), 1.0).len(),
                );
            }
        }
        ScreenshotMode::Furnace | ScreenshotMode::Chest => {
            let (kind, pos) = screenshot_container.unwrap_or((
                if mode == ScreenshotMode::Furnace {
                    ContainerKind::Furnace
                } else {
                    ContainerKind::Chest
                },
                BlockPos::new(0, 0, 0),
            ));
            let mut session = ContainerSession::new(kind, pos);
            let mut view = container_snapshot(&host, kind, pos);
            view.inventory = *host.inventory();
            let demo_slot = match kind {
                ContainerKind::Furnace => containers::SlotId::FurnaceOutput,
                ContainerKind::Chest => containers::SlotId::Chest(0),
            };
            if let Some(rect) = session.layout(screen).slot_rect(demo_slot) {
                session.selected = demo_slot;
                ui.cursor = rect.center();
            }
            ui.screen = match kind {
                ContainerKind::Furnace => Screen::Furnace,
                ContainerKind::Chest => Screen::Chest,
            };
            match kind {
                ContainerKind::Furnace => {
                    let state = host.furnace_at(pos).cloned();
                    containers::draw_furnace(
                        &mut frame,
                        &registry,
                        &session,
                        state.as_ref(),
                        &view,
                        ui.cursor,
                        screen,
                    );
                    log::info!(
                        "furnace screenshot: pos {:?}, cursor {:?}, input {:?}, fuel {:?}, output {:?}, lit {}",
                        pos,
                        ui.cursor,
                        state.as_ref().and_then(|state| state.input),
                        state.as_ref().and_then(|state| state.fuel),
                        state.as_ref().and_then(|state| state.output),
                        state.as_ref().is_some_and(|state| state.is_lit()),
                    );
                }
                ContainerKind::Chest => {
                    let state = host.chest_at(pos).cloned();
                    containers::draw_chest(
                        &mut frame,
                        &registry,
                        &session,
                        state.as_ref(),
                        &view,
                        ui.cursor,
                        screen,
                    );
                    log::info!(
                        "chest screenshot: pos {:?}, cursor {:?}, filled {} of {} slots",
                        pos,
                        ui.cursor,
                        state.as_ref().map_or(0, |state| state.contents().len()),
                        CHEST_SLOTS,
                    );
                }
            }
            // Containers are modal; keep the panel clear.
            let _ = survival;
        }
        ScreenshotMode::NetUi(kind) => {
            if kind == NetUiKind::Roster {
                let cross = [0.95, 0.97, 1.0, 0.85];
                frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
                frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
                draw_vitals_for_mode(&mut frame, &ui, &host, screen);
                draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
                let mut chat = crate::net::ChatLog::default();
                chat.push("alice", "hello from the network");
                chat.push("bob_the_builder", "building a harbor at spawn");
                chat.push("carol", "the furnace is ready");
                crate::net_ui::draw_chat(&mut frame, &chat, None, screen);
                let entries = [
                    crate::net::PlayerEntry {
                        id: 1,
                        name: "alexandria_the_builder".to_string(),
                        is_self: true,
                    },
                    crate::net::PlayerEntry {
                        id: 2,
                        name: "bob_the_builder".to_string(),
                        is_self: false,
                    },
                    crate::net::PlayerEntry {
                        id: 3,
                        name: "carol".to_string(),
                        is_self: false,
                    },
                    crate::net::PlayerEntry {
                        id: 4,
                        name: "dave_far".to_string(),
                        is_self: false,
                    },
                ];
                crate::net_ui::draw_player_list(&mut frame, &entries, 8, Some(23), screen);
            } else {
                let panel = match kind {
                    NetUiKind::Loading => crate::net_ui::ConnectPanel {
                        title: "Connecting",
                        endpoint: "127.0.0.1:25565",
                        player_name: "player1234",
                        status: "Streaming the spawn area",
                        spinner: "...",
                        world_name: Some("openblock"),
                        motd: Some("A self-hosted OpenBlock world"),
                        players: Some((2, 8)),
                        progress: Some((12, 49)),
                        failure: None,
                        selected: 0,
                        hovered: None,
                    },
                    NetUiKind::Failed => crate::net_ui::ConnectPanel {
                        title: "Connection failed",
                        endpoint: "play.example.com:25565",
                        player_name: "player1234",
                        status: "The connection attempt failed.",
                        spinner: "",
                        world_name: None,
                        motd: None,
                        players: None,
                        progress: None,
                        failure: Some("server_full: 8/8 players"),
                        selected: 0,
                        hovered: Some(0),
                    },
                    NetUiKind::Disconnected => crate::net_ui::ConnectPanel {
                        title: "Disconnected",
                        endpoint: "127.0.0.1:25565",
                        player_name: "player1234",
                        status: "The server closed the connection.",
                        spinner: "",
                        world_name: None,
                        motd: None,
                        players: None,
                        progress: None,
                        failure: Some("timeout: the server stopped responding"),
                        selected: 0,
                        hovered: Some(0),
                    },
                    NetUiKind::Roster => unreachable!(),
                };
                crate::net_ui::draw_connect_panel(&mut frame, &panel, screen);
            }
        }
    }
    let mut entities = entity_instances_for(host.item_entities(), &registry, 0.8);
    entities.extend(mobs::mob_instances(&registry, host.mobs(), 1.0));
    renderer.set_entities(&entities);
    log_ui_frame(&format!("{:?}", mode), &frame);
    let scene = SceneParams {
        environment,
        ui: Some(&frame),
        outline: None,
    };
    renderer.render_world_scene_lod(
        host.world(),
        &camera,
        &scene,
        &LodConfig::for_render_distance(host.render_distance_chunks()),
    )?;
    let rgba = renderer.read_rgba();
    if rgba.len() != (width * height * 4) as usize {
        return Err(format!(
            "offscreen readback returned {} bytes, expected {}",
            rgba.len(),
            width * height * 4
        )
        .into());
    }

    write_png(path, width, height, &rgba)?;

    log::info!(
        "screenshot written to {} ({}x{}, GPU {}, mode {}, health {:.1}, hunger {:.1})",
        path.display(),
        width,
        height,
        renderer.adapter_summary(),
        host.game_mode().name(),
        host.player().health,
        host.player().hunger,
    );
    Ok(())
}

/// Builds the deterministic F3 statistics snapshot used by `--screenshot-debug`.
///
/// World-derived values (position, seed, chunk counts, memory, ticks, target)
/// and render counters come from the real host and renderer; the frame-rate
/// fields are fixed because the screenshot has no frame loop, and the movement
/// label is the idle `walk` state. The result is stable for a given seed.
fn screenshot_debug_stats(
    host: &Host,
    registry: &Registry,
    renderer: &OffscreenRenderer,
    settings: &Settings,
    options: &ClientOptions,
    texture_source: &TextureSource,
    render_stats: ob_render::RenderStats,
) -> DebugStats {
    let player = host.player();
    let eye = player.eye();
    let target_hit = host.world().raycast(eye, player.forward(), 5.0);
    let target_block = target_hit.map(|hit| host.world().get_block(hit.block));
    let target = target_hit.map(|hit| {
        (
            registry.block(host.world().get_block(hit.block)).id.to_string(),
            [hit.block.x, hit.block.y, hit.block.z],
        )
    });
    let selected_stack = host.selected_stack();
    let (held, held_durability) = if let Some(stack) = selected_stack {
        let item = registry.item(stack.item);
        let durability = item.tool.and_then(|spec| {
            stack
                .durability
                .map(|remaining| (remaining.min(spec.durability), spec.durability))
        });
        (inventory::display_name(item.id), durability)
    } else {
        ("empty".to_string(), None)
    };
    let held_item = selected_stack.map(|stack| stack.item);
    let mining_speed = match target_block {
        Some(block) => registry.mining_speed(block, held_item),
        None => selected_stack
            .and_then(|stack| registry.item(stack.item).tool)
            .map(|spec| spec.speed)
            .unwrap_or(1.0),
    };
    DebugStats {
        version: ob_core::VERSION.to_string(),
        fps: 60,
        frame_p50_ms: 16.67,
        frame_p95_ms: 17.42,
        position: [eye.x, eye.y, eye.z],
        chunk: [
            (eye.x.floor() as i32).div_euclid(16),
            (eye.y.floor() as i32).div_euclid(16),
            (eye.z.floor() as i32).div_euclid(16),
        ],
        facing: facing_label(player.yaw),
        target,
        seed: host.world().seed(),
        render_distance: host.render_distance_chunks(),
        loaded_chunks: host.world().chunk_count(),
        drawn_sections: render_stats.sections as usize,
        cached_sections: renderer.cached_mesh_count(),
        lod_batches: render_stats.lod_batches as usize,
        lod_sections: 0,
        lod_skipped_sections: 0,
        mesh_bytes: render_stats.vertices as usize * std::mem::size_of::<ob_render::Vertex>() * 2,
        triangles: render_stats.vertices / 3,
        draw_calls: render_stats.draw_calls as usize,
        mesh_workers: options.mesh_workers,
        mesh_in_flight: 0,
        mesh_completed: 0,
        mesh_backlog: 0,
        heap_bytes: host.memory_bytes(),
        ticks: host.tick_count(),
        adapter: renderer.adapter_summary().to_string(),
        window: [options.screenshot_width, options.screenshot_height],
        fov_degrees: settings.fov_degrees,
        ui_scale: crate::ui_scale::debug_line(
            settings.ui_scale_percent,
            [options.screenshot_width as f32, options.screenshot_height as f32],
        ),
        time_of_day: Environment::from_ticks(host.tick_count() + DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS).time_of_day,
        texture_source: describe_texture_source(texture_source, renderer.texture_resolution()),
        ui_font: renderer.ui_font_name().to_string(),
        game_mode: host.game_mode().name(),
        health: player.health,
        hunger: player.hunger,
        movement: "walk",
        held,
        held_durability,
        mining_speed,
        audio: "off (screenshot)".to_string(),
        network: None,
        mobs_total: host.mob_count(),
        mobs_passive: host.passive_mob_count(),
        mobs_hostile: host.hostile_mob_count(),
        day_phase: host.day_phase(),
        day: host.is_day(),
        defense: host.armor_defense(),
    }
}

/// Gives the screenshot host a deterministic, representative inventory and
/// wounded vitals.
///
/// The nine hotbar slots are chosen to exercise the expanded catalogue: an iron
/// tool (held, so the F3 line reports `251/251 speed 6.0`), iron armor, cooked
/// food, coal, an ingot, raw ore and enough planks/sticks/cobblestone for the
/// crafting availability colours. This uses only the public host API.
fn seed_screenshot_items(host: &mut Host, registry: &Registry) {
    // Equip a full iron set first: `equip_armor` frees the source hotbar slot,
    // leaving room for the tool/food/material showcase below. Armor icons and
    // the durability bars are the point of the survival inventory screenshot.
    let armor_names = [
        "openblock:iron_helmet",
        "openblock:iron_chestplate",
        "openblock:iron_leggings",
        "openblock:iron_boots",
    ];
    for name in armor_names {
        if let Some(item) = registry.item_id(name) {
            let leftover = host.give_item(item, 1);
            if leftover > 0 {
                log::warn!("screenshot items: {name} did not fit");
            }
        }
    }
    let slot_of = |host: &Host, name: &str| {
        let item = registry.item_id(name)?;
        host.inventory()
            .iter()
            .position(|stack| stack.is_some_and(|stack| stack.item == item))
    };
    let mut equipped = 0usize;
    for name in armor_names {
        let Some(slot) = slot_of(host, name) else {
            continue;
        };
        if host.equip_armor(slot) {
            equipped += 1;
        }
    }
    log::info!(
        "screenshot armor: {} equipped pieces, defence {}",
        equipped,
        host.armor_defense()
    );
    for (name, count) in [
        ("openblock:iron_pickaxe", 1_u8),
        ("openblock:iron_sword", 1),
        ("openblock:cooked_beef", 3),
        ("openblock:coal", 8),
        ("openblock:iron_ingot", 8),
        ("openblock:planks", 16),
        ("openblock:stick", 8),
        ("openblock:cobblestone", 16),
    ] {
        if let Some(item) = registry.item_id(name) {
            let leftover = host.give_item(item, count);
            if leftover > 0 {
                log::warn!("screenshot items: {name} x{leftover} did not fit");
            }
        }
    }
    let iron_slot = slot_of(host, "openblock:iron_pickaxe");
    host.set_hotbar_selection(iron_slot.unwrap_or(0));
    log::info!(
        "screenshot items: selected slot {} held {:?}, armor {:?}",
        host.hotbar_selection(),
        host.selected_stack(),
        host.equipment(),
    );
    // A few deterministic dropped-item entities in front of the player show
    // the entity renderer and the new item tints in the HUD screenshots.
    let eye = host.player().eye();
    let forward = host.player().forward();
    let base = WorldVec::new(
        eye.x + forward[0] * 3.0,
        host.player().pos.y + 0.2,
        eye.z + forward[2] * 3.0,
    );
    for (index, (name, count)) in [
        ("openblock:coal", 2_u8),
        ("openblock:iron_ingot", 1),
        ("openblock:cooked_beef", 1),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(stack) = registry.item_id(name).and_then(|item| host.make_stack(item, count)) {
            let offset = (index as f64 - 1.0) * 0.55;
            let pos = WorldVec::new(base.x + offset, base.y, base.z);
            host.spawn_item_entity(pos, stack);
        }
    }
    let player = host.player_mut();
    player.health = 14.0;
    player.hunger = 12.0;
    player.dead = false;
}

/// Aims the player camera at a world-space point (demo screenshots only).
fn aim_player_at(host: &mut Host, target: WorldVec) {
    let eye = host.player().eye();
    let direction = (target - eye).normalize_or_zero();
    if direction.length_squared() <= f64::EPSILON {
        return;
    }
    let yaw = (-direction.x).atan2(-direction.z) as f32;
    let pitch = direction.y.clamp(-1.0, 1.0).asin() as f32;
    let player = host.player_mut();
    player.yaw = yaw;
    player.pitch = pitch;
}

/// Spawns a fan of passive and hostile mobs in front of the screenshot player.
fn seed_screenshot_mobs(host: &mut Host, registry: &Registry) -> usize {
    let _ = host.ensure_streaming(SCREENSHOT_STREAM_BUDGET);
    let player = host.player().clone();
    // Spawn the fan south of the player and look south so the camera sees the
    // mobs' fronts (their yaw zero faces -Z) instead of their backs.
    let forward = WorldVec::new(0.0, 0.0, 1.0);
    let right = WorldVec::new(-1.0, 0.0, 0.0);
    // Close enough that the upper bodies clear the F3 panel, with a slight
    // rightward camera offset so the tall zombie is not hidden by the overlay.
    let specs = [
        (ob_content::MobKind::Cow, 4.8_f64, -1.0_f64),
        (ob_content::MobKind::Pig, 4.6, -0.4),
        (ob_content::MobKind::Sheep, 5.2, 0.3),
        (ob_content::MobKind::Chicken, 4.2, 0.9),
        (ob_content::MobKind::Zombie, 5.6, 1.6),
        (ob_content::MobKind::Cow, 6.2, 2.3),
    ];
    let mut spawned = Vec::new();
    for (kind, distance, side) in specs {
        'placement: for dy in [0.0_f64, 0.05, 0.25, 0.5, -0.05] {
            for extra in [0.0_f64, 0.8, -0.8, 1.6] {
                let pos = WorldVec::new(
                    player.pos.x + forward.x * (distance + extra) + right.x * side,
                    player.pos.y + dy,
                    player.pos.z + forward.z * (distance + extra) + right.z * side,
                );
                if host.spawn_mob(kind, pos).is_some() {
                    spawned.push(pos);
                    break 'placement;
                }
            }
        }
    }
    if !spawned.is_empty() {
        let mut centre = WorldVec::ZERO;
        for pos in &spawned {
            centre += *pos;
        }
        centre /= spawned.len() as f64;
        aim_player_at(host, centre + WorldVec::new(0.0, 0.9, 0.0));
        let player = host.player_mut();
        // Nearly horizontal view puts feet and bodies below the F3 panel, and
        // the yaw offset moves the group clear of its right edge.
        player.pitch = -0.03;
        // Put the whole group to the right of the wide F3 panel.
        player.yaw += 0.5;
    }
    log::info!(
        "mob screenshot: spawned {} of {} requested avatars around {:?}",
        spawned.len(),
        specs.len(),
        player.pos,
    );
    if host.mob_count() != spawned.len() {
        log::warn!(
            "mob screenshot: {} mobs live after spawning {} demo mobs",
            host.mob_count(),
            spawned.len()
        );
    }
    let _ = registry;
    host.mob_count()
}

/// Places a furnace/chest block in front of the player through the normal
/// survival placement path, so the host registers the block entity.
fn seed_screenshot_container(host: &mut Host, registry: &Registry, block_name: &str) -> Option<BlockPos> {
    let block = registry.id_or_air(block_name);
    if block == BlockId::AIR {
        log::warn!("screenshot container: {block_name} is not in the registry");
        return None;
    }
    let item = registry.item_for_block(block)?;
    let mut last_target = None;
    for pitch in [-0.55_f32, -0.35, -0.75, -0.9, -0.2] {
        {
            let player = host.player_mut();
            player.pitch = pitch;
        }
        let player = host.player();
        let Some(hit) = host.world().raycast(player.eye(), player.forward(), REACH) else {
            continue;
        };
        let target = hit.block.offset(hit.normal.x, hit.normal.y, hit.normal.z);
        if host.world().get_block(target) != BlockId::AIR {
            continue;
        }
        // Keep the placed block out of the player's own collision box.
        let dx = f64::from(target.x) + 0.5 - player.pos.x;
        let dz = f64::from(target.z) + 0.5 - player.pos.z;
        if dx * dx + dz * dz < 1.44 {
            continue;
        }
        last_target = Some(target);
        if host.give_item(item, 2) > 0 {
            log::warn!("screenshot container: {block_name} did not fit in the hotbar");
        }
        let Some(slot) = host
            .inventory()
            .iter()
            .position(|stack| stack.is_some_and(|stack| stack.item == item))
        else {
            continue;
        };
        host.set_hotbar_selection(slot);
        let action = BlockAction {
            break_block: false,
            place_block: true,
            place: block,
        };
        let outcome = host.apply_action_with_outcome(&action);
        if host.world().get_block(target) == block {
            log::info!(
                "screenshot container: placed {} at {:?} (action changed {})",
                block_name,
                target,
                outcome.changed
            );
            aim_player_at(
                host,
                WorldVec::new(
                    f64::from(target.x) + 0.5,
                    f64::from(target.y) + 0.5,
                    f64::from(target.z) + 0.5,
                ),
            );
            return Some(target);
        }
        log::warn!("screenshot container: placement action did not fill {target:?}");
    }
    let _ = last_target;
    None
}

/// Fills a screenshot furnace with a representative in-progress smelt.
fn seed_screenshot_furnace(host: &mut Host, registry: &Registry, pos: BlockPos, full: bool) {
    let (input_count, fuel_count) = if full { (64, 64) } else { (3, 8) };
    let input = registry
        .item_id("openblock:raw_iron")
        .and_then(|item| host.make_stack(item, input_count));
    let fuel = registry
        .item_id("openblock:coal")
        .and_then(|item| host.make_stack(item, fuel_count));
    if let Some(stack) = input {
        let replaced = host.put_furnace_slot(pos, FurnaceSlot::Input, stack);
        log::info!("furnace seed: input {:?}, replaced {:?}", stack, replaced);
    }
    if let Some(stack) = fuel {
        let replaced = host.put_furnace_slot(pos, FurnaceSlot::Fuel, stack);
        log::info!("furnace seed: fuel {:?}, replaced {:?}", stack, replaced);
    }
    let output_count = if full { 64 } else { 2 };
    let output = registry
        .item_id("openblock:iron_ingot")
        .and_then(|item| host.make_stack(item, output_count));
    if let Some(state) = host.furnace_at_mut(pos) {
        state.burn_remaining = if full { 1600 } else { 800 };
        state.burn_total = 1600;
        state.cook_progress = if full { 199 } else { 120 };
        // The output is take-only through the slot API; seed it directly for
        // the demo so the screenshot shows a finished item.
        state.output = output;
        log::info!(
            "furnace seed: lit {} burn {}/{} cook {}/200",
            state.is_lit(),
            state.burn_remaining,
            state.burn_total,
            state.cook_progress
        );
    }
}

/// Fills all 27 screenshot chest slots with a deterministic variety of items.
fn seed_screenshot_chest(host: &mut Host, registry: &Registry, pos: BlockPos) {
    let entries = [
        ("openblock:cobblestone", 64_u8),
        ("openblock:planks", 32),
        ("openblock:coal", 16),
        ("openblock:iron_ingot", 8),
        ("openblock:raw_iron", 12),
        ("openblock:raw_copper", 9),
        ("openblock:stick", 24),
        ("openblock:apple", 5),
        ("openblock:cooked_beef", 3),
        ("openblock:leather", 6),
        ("openblock:iron_pickaxe", 1),
        ("openblock:stone_sword", 1),
        ("openblock:iron_helmet", 1),
        ("openblock:iron_chestplate", 1),
        ("openblock:iron_leggings", 1),
        ("openblock:iron_boots", 1),
        ("openblock:furnace", 1),
        ("openblock:chest", 1),
        ("openblock:crafting_table", 1),
        ("openblock:stone", 64),
        ("openblock:dirt", 64),
        ("openblock:sand", 48),
        ("openblock:gravel", 40),
        ("openblock:snow", 32),
        ("openblock:log", 24),
        ("openblock:clay", 16),
        ("openblock:leaves", 8),
    ];
    let pickaxe = registry.item_id("openblock:iron_pickaxe");
    let mut filled = 0usize;
    for slot in 0..CHEST_SLOTS {
        let (name, count) = entries[slot % entries.len()];
        let Some(stack) = registry.item_id(name).and_then(|item| host.make_stack(item, count)) else {
            continue;
        };
        let stack = if Some(stack.item) == pickaxe {
            stack.with_durability(Some(117))
        } else {
            stack
        };
        let _ = host.put_chest_slot(pos, slot, stack);
        if host.chest_at(pos).is_some_and(|state| state.slots[slot].is_some()) {
            filled += 1;
        }
    }
    log::info!("chest seed: filled {} of {} slots at {:?}", filled, CHEST_SLOTS, pos);
}

/// Snapshot used by container drawing and click decisions.
fn container_snapshot(host: &Host, kind: ContainerKind, pos: BlockPos) -> ContainerSnapshot {
    let mut snapshot = ContainerSnapshot {
        inventory: *host.inventory(),
        ..ContainerSnapshot::default()
    };
    match kind {
        ContainerKind::Furnace => {
            snapshot.furnace = host.furnace_at(pos).cloned();
        }
        ContainerKind::Chest => {
            snapshot.chest = host.chest_at(pos).map_or([None; CHEST_SLOTS], |state| state.slots);
        }
    }
    snapshot
}

/// Places a hidden crafting table below the screenshot player so the 3x3
/// crafting recipes are available in the crafting screenshot.
fn seed_near_crafting_table(host: &mut Host, registry: &Registry) {
    let table = registry.id_or_air("openblock:crafting_table");
    if table == BlockId::AIR {
        return;
    }
    let eye = host.player().eye();
    let pos = BlockPos::new(eye.x.floor() as i32, (eye.y - 3.0).floor() as i32, eye.z.floor() as i32);
    host.world_mut().set_block(pos, table);
}

/// Builds the renderer instance for one dropped item entity.
///
/// Block items sample the block's top texture layer (matching the mesher/HUD
/// mapping), while non-block items use the representative icon tint from
/// [`inventory::item_tint_for`]. Position adds the requested 0.15 m bob and the
/// yaw spins deterministically with time and entity id.
pub fn entity_instance_for(entity: &ItemEntity, registry: &Registry, elapsed: f32) -> EntityInstance {
    item_entity_instance(entity.id, entity.pos, entity.stack, registry, elapsed)
}

/// Builds the renderer instance for one replicated dropped item.
///
/// This is the network counterpart of [`entity_instance_for`]: the client
/// only knows the opaque wire id, the interpolated base position and the
/// authoritative stack, so velocity and age are not needed for presentation.
pub fn item_entity_instance(
    id: u64,
    pos: WorldVec,
    stack: ItemStack,
    registry: &Registry,
    elapsed: f32,
) -> EntityInstance {
    let position = [pos.x as f32, pos.y as f32 + 0.15, pos.z as f32];
    let yaw = elapsed * 1.8 + id as f32 * 0.7;
    if stack.item.0 as usize >= registry.item_len() {
        return EntityInstance {
            position,
            scale: 0.22,
            yaw,
            layer: 0,
            tint: [0.80, 0.80, 0.80],
            textured: false,
        };
    }
    let item = registry.item(stack.item);
    if let Some(block) = item.block {
        let block = registry.block(block);
        EntityInstance {
            position,
            scale: 0.22,
            yaw,
            layer: u32::from(block.textures.top.0),
            tint: [1.0, 1.0, 1.0],
            textured: true,
        }
    } else {
        EntityInstance {
            position,
            scale: 0.22,
            yaw,
            layer: 0,
            tint: entity_item_tint(registry, stack.item),
            textured: false,
        }
    }
}

/// Flat colour used when a non-block dropped item is rendered as a cube.
fn entity_item_tint(registry: &Registry, item: ob_content::ItemId) -> [f32; 3] {
    inventory::item_tint_for(registry, item).unwrap_or([0.80, 0.80, 0.80])
}

/// Maps every dropped entity to a renderer instance, preserving host order.
pub fn entity_instances_for(entities: &[ItemEntity], registry: &Registry, elapsed: f32) -> Vec<EntityInstance> {
    entities
        .iter()
        .map(|entity| entity_instance_for(entity, registry, elapsed))
        .collect()
}

/// Current crafting rows for the host (availability included).
fn crafting_rows_for_host(host: &Host, registry: &Registry) -> Vec<CraftingRow> {
    crafting_rows(
        registry,
        &host.available_recipes(),
        host.near_crafting_table(),
        host.game_mode() == HostGameMode::Creative,
        host.inventory(),
    )
}

/// Draws the survival vitals; creative keeps a clean HUD.
fn draw_vitals_for_mode(frame: &mut UiFrame, _ui: &UiState, host: &Host, screen: [f32; 2]) {
    if host.game_mode() == HostGameMode::Survival {
        let player = host.player();
        ui_state::draw_vitals(frame, screen, player.health, player.hunger);
        ui_state::draw_armor(frame, screen, host.armor_defense());
    }
}

/// Width below which the held-item label moves below the crosshair.
///
/// At 960x540 the centred label above the hotbar overlaps the armour row and
/// `15 DEF`; the narrower fallback keeps it clear of every HUD element.
pub(crate) const SELECTED_NAME_NARROW_WIDTH: f32 = 1100.0;

/// Y position of the held-item label for a screen and hotbar.
pub(crate) fn selected_name_y(screen: [f32; 2], hotbar: &[ui_state::UiRect]) -> f32 {
    if screen[0] < SELECTED_NAME_NARROW_WIDTH {
        screen[1] / 2.0 + 24.0
    } else {
        hotbar.first().map(|slot| slot.y - 30.0).unwrap_or(screen[1] - 70.0)
    }
}

/// Draws the selected hotbar slot's display name, if the name timer is active.
fn draw_selected_name(frame: &mut UiFrame, ui: &UiState, registry: &Registry, host: &Host, screen: [f32; 2]) {
    if ui.block_name_seconds <= 0.0 {
        return;
    }
    // On narrow targets the fallback label sits below the crosshair, which is
    // inside the F3 column; suppress it rather than drawing it under the debug
    // text. Wide screens keep the label above the hotbar, clear of the panels.
    if ui.debug_overlay && screen[0] < SELECTED_NAME_NARROW_WIDTH {
        return;
    }
    let label = if host.game_mode() == HostGameMode::Survival {
        host.selected_stack()
            .map(|stack| inventory::item_display_name(registry, stack.item))
    } else {
        ui.hotbar_block(registry, ui.hotbar_selected)
            .map(|block| inventory::display_name(block.id))
    };
    if let Some(label) = label {
        let y = selected_name_y(screen, &ui.hotbar_rects(screen));
        let width = frame.text_width(2.0, &label);
        frame.rect(
            screen[0] / 2.0 - width / 2.0 - 7.0,
            y - 3.0,
            width + 14.0,
            ob_render::line_height(2.0) + 6.0,
            [0.02, 0.03, 0.05, 0.62],
        );
        frame.text_centered(screen[0] / 2.0, y, 2.0, [1.0, 1.0, 1.0, 1.0], &label);
    }
}

/// Draws the hotbar using the host inventory in survival and the fixed creative
/// palette in creative.
fn draw_hotbar_for_mode(frame: &mut UiFrame, ui: &UiState, registry: &Registry, host: &Host, screen: [f32; 2]) {
    match host.game_mode() {
        HostGameMode::Survival => {
            inventory::draw_survival_hotbar(frame, ui, registry, host.inventory(), host.hotbar_selection(), screen)
        }
        HostGameMode::Creative => draw_creative_hotbar(frame, ui, registry, screen),
    }
}

/// Draws the nine-slot hotbar, skipping slots whose blocks are missing from the
/// active registry.
fn draw_creative_hotbar(frame: &mut UiFrame, ui: &UiState, registry: &Registry, screen: [f32; 2]) {
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

/// Top-level handler for the menu launch path.
///
/// winit allows exactly one `EventLoop` per process, so the front-end cannot
/// create its own loop and hand off to `App`/`NetApp` through a second one.
/// This router owns the single loop and swaps the active application handler
/// when the menu returns a world or server choice. Each runtime still creates
/// its own window in its `resumed` method; windows are cheap and independent.
enum RootState {
    Menu(Box<menu_app::MenuApp>),
    World(Box<App>),
    Network(Box<net_app::NetApp>),
    Empty,
}

struct RootApp {
    options: ClientOptions,
    state: RootState,
}

impl RootApp {
    fn new(options: ClientOptions) -> Self {
        Self {
            options: options.clone(),
            state: RootState::Menu(Box::new(menu_app::MenuApp::new(options))),
        }
    }

    fn dispatch_resumed(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.state {
            RootState::Menu(menu) => menu.resumed(event_loop),
            RootState::World(app) => app.resumed(event_loop),
            RootState::Network(net) => net.resumed(event_loop),
            RootState::Empty => {}
        }
    }

    /// Applies a pending front-end choice inside the existing event loop.
    fn transition(&mut self, event_loop: &ActiveEventLoop) {
        let outcome = match &mut self.state {
            RootState::Menu(menu) => menu.take_outcome(),
            _ => None,
        };
        let Some(outcome) = outcome else {
            return;
        };
        // The front-end already applied and persisted any `--ui-font` /
        // `--default-ui-font` choice. Clear the run-only flags before the game
        // or network runtime loads settings, otherwise a Reset UI font action
        // performed in the title-screen Settings panel would be undone when a
        // world starts in the same process.
        clear_ui_font_cli_overrides(&mut self.options);
        match outcome {
            menu_app::MenuOutcome::Quit => {
                self.state = RootState::Empty;
                event_loop.exit();
            }
            menu_app::MenuOutcome::PlayWorld { world, seed } => {
                self.options.world = world;
                self.options.seed = seed;
                self.options.force_title = false;
                self.options.saw_world = true;
                self.options.saw_seed = true;
                self.options.connect = None;
                self.state = RootState::World(Box::new(App::new(self.options.clone())));
                self.dispatch_resumed(event_loop);
            }
            menu_app::MenuOutcome::Connect { address, name } => {
                self.options.connect = Some(address);
                self.options.name = Some(name);
                self.options.force_title = false;
                self.state = RootState::Network(Box::new(net_app::NetApp::new(self.options.clone())));
                self.dispatch_resumed(event_loop);
            }
        }
    }
}

impl ApplicationHandler for RootApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.dispatch_resumed(event_loop);
        self.transition(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        match &mut self.state {
            RootState::Menu(menu) => menu.window_event(event_loop, window_id, event),
            RootState::World(app) => app.window_event(event_loop, window_id, event),
            RootState::Network(net) => net.window_event(event_loop, window_id, event),
            RootState::Empty => {}
        }
        self.transition(event_loop);
    }

    fn device_event(&mut self, event_loop: &ActiveEventLoop, device_id: DeviceId, event: DeviceEvent) {
        match &mut self.state {
            RootState::Menu(menu) => menu.device_event(event_loop, device_id, event),
            RootState::World(app) => app.device_event(event_loop, device_id, event),
            RootState::Network(net) => net.device_event(event_loop, device_id, event),
            RootState::Empty => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.state {
            RootState::Menu(menu) => menu.about_to_wait(event_loop),
            RootState::World(app) => app.about_to_wait(event_loop),
            RootState::Network(net) => net.about_to_wait(event_loop),
            RootState::Empty => {}
        }
        self.transition(event_loop);
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

/// Maximum merged far batches a worker may be building at once.
const MAX_LOD_IN_FLIGHT: usize = 4;
/// Seconds before the initial loading overlay stops waiting for a straggler.
const INITIAL_LOAD_TIMEOUT: Duration = Duration::from_secs(45);

struct GameState {
    window: Arc<Window>,
    renderer: Renderer,
    /// Active texture source, kept so the F3 overlay and settings row can name
    /// it and toggle back to the procedural style.
    texture_source: TextureSource,
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
    current_fov: f32,
    damage_flash: DamageFlash,
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
    /// Distance-LOD state shared with the multiplayer runtime.
    lod: LodState,
    /// True while the initial near ring is still streaming/meshing.
    initial_load: bool,
    /// Initial near-ring radius in chunks (smaller than the full setting).
    initial_target_radius: i32,
    /// Coarse 0..=1 progress shown by the loading overlay.
    loading_progress: f32,
    /// Human-readable progress detail for the overlay.
    loading_detail: String,
    /// Start time of the initial load, used for the timeout fallback.
    loading_started: Instant,
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
    /// Audio engine, or `None` when silenced/unavailable.
    audio: Option<AudioEngine>,
    /// One-line status shown by F3 and the Audio settings tab.
    audio_status: String,
    /// Last health value observed for hurt/death sound edges.
    last_audio_health: f32,
    /// Horizontal ground distance accumulated for the next footstep.
    footstep_distance: f64,
    /// Water state from the previous frame (splash edge detection).
    was_in_water: bool,
    /// Saved `audio_enabled` when `--mute` overrode it for this run.
    audio_enabled_before_mute: Option<bool>,
    /// Whether the user changed the Sound row after a `--mute` launch.
    audio_setting_touched: bool,
    /// Open furnace/chest screen state, if any.
    container: Option<ContainerSession>,
    /// Per-mob tick interpolation cache for smooth avatar movement.
    mob_render: MobRenderCache,
    /// Earliest instant the next melee attack is allowed.
    attack_ready: Option<Instant>,
    /// Decaying camera pitch offset after taking damage.
    camera_shake: f32,
}

impl GameState {
    fn persist_settings(&self) {
        let mut settings = self.ui.settings.clone();
        if let Some(saved) = self.audio_enabled_before_mute
            && !self.audio_setting_touched
        {
            // `--mute` is a run-only override until the player actually edits
            // the Sound row.
            settings.audio_enabled = saved;
        }
        if let Err(err) = save_settings(&self.data_dir, &settings) {
            log::warn!("could not save settings: {err}");
        }
    }

    /// Pushes the persisted audio settings into the live engine.
    fn sync_audio_settings(&mut self) {
        if let Some(engine) = &mut self.audio {
            apply_audio_settings(engine, &self.ui.settings);
        }
    }

    /// Plays a quiet UI tick or confirmation chime.
    fn play_ui_sound(&mut self, confirm: bool) {
        if let Some(engine) = &mut self.audio {
            engine.play_sfx(if confirm {
                AudioSfx::UiConfirm
            } else {
                AudioSfx::UiClick
            });
        }
    }

    /// Plays a block event at the target position, with distance attenuation
    /// and a yaw-relative pan. Sound is advisory: a missing engine is a no-op.
    fn play_block_sfx(&mut self, sfx: AudioSfx, position: BlockPos) {
        if self.audio.is_none() {
            return;
        }
        let player = self.host.player();
        let eye = player.eye();
        let dx = position.x as f64 + 0.5 - eye.x;
        let dz = position.z as f64 + 0.5 - eye.z;
        let distance = (dx * dx + dz * dz).sqrt();
        let gain = (1.0 - distance / 24.0).clamp(0.0, 1.0) as f32;
        if gain <= 0.0 {
            return;
        }
        let right = dx * f64::from(player.yaw).cos() - dz * f64::from(player.yaw).sin();
        let pan = if distance > 0.25 {
            (right / distance * 0.8).clamp(-1.0, 1.0) as f32
        } else {
            0.0
        };
        if let Some(engine) = &mut self.audio {
            engine.play_sfx_at(sfx, gain, Some(pan));
        }
    }

    /// Updates footstep, splash, hurt and listener state for this frame.
    fn update_audio_state(&mut self, dt: f32) {
        let playing = self.ui.screen == Screen::Playing && !self.host.is_dead();
        let survival = self.host.game_mode() == HostGameMode::Survival;
        let player = self.host.player();
        let health = player.health;
        let on_ground = playing && player.on_ground && !player.flying;
        let feet = BlockPos::new(
            player.pos.x.floor() as i32,
            player.pos.y.floor() as i32,
            player.pos.z.floor() as i32,
        );
        let below = BlockPos::new(feet.x, (player.pos.y - 0.1).floor() as i32, feet.z);
        let below_block = self.host.world().get_block(below);
        let material = audio_material_for(&self.registry, below_block).unwrap_or(AudioMaterial::Grass);
        let in_water = player_in_water(&self.host, &self.registry);
        let distance = std::mem::take(&mut self.footstep_distance) as f32;
        let yaw = player.yaw;
        let listener_eye = player.eye();
        let listener = [listener_eye.x as f32, listener_eye.y as f32, listener_eye.z as f32];

        if let Some(engine) = &mut self.audio {
            engine.tick(dt);
            engine.set_listener(listener, yaw);
            if playing && in_water && !self.was_in_water {
                engine.play_sfx(AudioSfx::Splash);
            }
            engine.update_footsteps(
                if playing && !in_water { distance } else { 0.0 },
                material,
                on_ground && !in_water,
            );
            if survival && playing && health + 0.01 < self.last_audio_health {
                engine.play_sfx(AudioSfx::Hurt);
            }
        }
        self.was_in_water = in_water;
        self.last_audio_health = health;
    }

    /// Effective GUI-scale percentage for the current physical window.
    fn resolved_ui_percent(&self) -> u16 {
        resolved_ui_scale_percent(self.ui.settings.ui_scale_percent, self.window_size.1)
    }

    /// Physical-to-logical pointer conversion for the current GUI scale.
    fn ui_point(&self, point: (f32, f32)) -> (f32, f32) {
        physical_to_ui_point(
            point,
            ui_scale_factor(self.ui.settings.ui_scale_percent, self.window_size.1),
        )
    }

    /// Refreshes the cached effective scale after a resize or setting change.
    fn refresh_ui_scale(&mut self) {
        let old = self.ui.ui_scale_percent;
        let new = self.resolved_ui_percent();
        if new != old {
            self.ui.cursor = rescale_cursor(self.ui.cursor, old, new);
            self.ui.ui_scale_percent = new;
        }
    }

    /// Logical UI size for the current window and GUI scale.
    fn window_screen(&self) -> [f32; 2] {
        ui_screen(
            [self.window_size.0.max(1.0), self.window_size.1.max(1.0)],
            self.ui.settings.ui_scale_percent,
        )
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
        if self.host.is_dead() {
            self.begin_death_screen();
            self.ui.focus_death_buttons();
            return;
        }
        match self.ui.screen {
            Screen::Playing => {
                self.cancel_mining();
                self.keys = [false; 8];
                self.ui.open_settings();
                self.release_pointer();
            }
            Screen::Inventory => {
                // Esc closes the inventory back to gameplay; it must not open
                // settings.
                self.close_inventory();
            }
            Screen::Crafting => {
                // Esc closes crafting back to gameplay; it must not open
                // settings.
                self.close_crafting();
            }
            Screen::Furnace | Screen::Chest => self.close_container(),
            Screen::Settings => {
                self.ui.close_settings();
                self.capture_pointer();
            }
            Screen::Death => {
                // Focus the buttons instead of opening settings.
                self.ui.focus_death_buttons();
            }
            Screen::Connecting => {
                // The single-player client never connects; keep the pointer
                // released if this screen is ever forced externally.
                self.release_pointer();
            }
        }
        self.persist_settings();
    }

    /// Enters the modal death screen exactly once, releasing gameplay input.
    fn begin_death_screen(&mut self) {
        if self.ui.screen == Screen::Death {
            return;
        }
        self.cancel_mining();
        self.keys = [false; 8];
        if self.ui.container_open() {
            self.flush_container_cursor();
            self.container = None;
        }
        self.ui.open_death();
        self.ui.death_cause = Some("Cause: lost all health".to_string());
        self.release_pointer();
        self.damage_flash.observe(self.host.player().health);
        if let Some(engine) = &mut self.audio {
            engine.play_sfx(AudioSfx::Death);
        }
    }

    /// Respawn action from the death screen.
    fn respawn_after_death(&mut self) {
        self.host.respawn();
        self.ui.resume();
        self.ui.toasts.clear();
        self.keys = [false; 8];
        self.cancel_mining();
        let player = self.host.player();
        self.input_yaw = player.yaw;
        self.input_pitch = player.pitch;
        self.prev_eye = player.eye();
        self.current_eye = player.eye();
        self.accumulator = Duration::ZERO;
        self.damage_flash.reset();
        self.camera_shake = 0.0;
        self.attack_ready = None;
        self.container = None;
        self.mob_render.sync(self.host.mobs(), 0.0);
        self.last_audio_health = self.host.player().health;
        self.footstep_distance = 0.0;
        self.was_in_water = player_in_water(&self.host, &self.registry);
        self.capture_pointer();
        self.update_title();
    }

    /// Applies a death-screen action; returns whether the process should exit.
    fn handle_death_action(&mut self, action: DeathAction) -> bool {
        match action {
            DeathAction::Respawn => {
                self.respawn_after_death();
                false
            }
            DeathAction::SaveAndQuit => {
                if let Err(err) = self.host.save() {
                    log::warn!("save on death-screen quit failed: {err}");
                }
                self.persist_settings();
                true
            }
        }
    }

    /// Pushes the configured gameplay mode into the authoritative host.
    fn apply_game_mode_setting(&mut self) {
        let mode = map_game_mode(self.ui.settings.game_mode);
        self.host.set_game_mode(mode);
        if mode == HostGameMode::Creative {
            self.damage_flash.reset();
            if self.ui.screen == Screen::Death {
                self.ui.clear_death();
                self.capture_pointer();
            }
        } else {
            // The survival HUD is driven by the authoritative host selection.
            self.ui.hotbar_selected = self.host.hotbar_selection();
            self.damage_flash.observe(self.host.player().health);
        }
        log::info!("game mode: {}", mode.name());
        self.last_audio_health = self.host.player().health;
        self.footstep_distance = 0.0;
        self.update_title();
    }

    /// Selects a hotbar slot in both the host and the UI model.
    fn select_hotbar_slot(&mut self, slot: usize) {
        if self.host.game_mode() == HostGameMode::Survival {
            self.host.set_hotbar_selection(slot);
        }
        self.ui.select_hotbar(slot);
        if self.host.game_mode() == HostGameMode::Survival {
            self.ui.hotbar_selected = self.host.hotbar_selection();
        }
        self.update_title();
    }

    /// Cycles the hotbar by `steps` slots, using the host selection in survival.
    fn cycle_hotbar_slots(&mut self, steps: i32) {
        let current = if self.host.game_mode() == HostGameMode::Survival {
            self.host.hotbar_selection()
        } else {
            self.ui.hotbar_selected
        };
        let slot = (current as i32 + steps).rem_euclid(ui_state::HOTBAR_SLOTS as i32) as usize;
        self.select_hotbar_slot(slot);
    }

    /// Opens the survival inventory or creative palette and releases the pointer.
    fn open_inventory(&mut self) {
        self.cancel_mining();
        self.ui.open_inventory();
        if self.host.game_mode() == HostGameMode::Survival {
            self.ui.inventory.selected = self.host.hotbar_selection();
            self.ui.hotbar_selected = self.host.hotbar_selection();
        } else {
            let item_count = inventory_blocks(&self.registry).len();
            self.ui.update_inventory_hover(self.window_screen(), item_count);
        }
        self.release_pointer();
    }

    /// Closes the inventory, returns to gameplay and recaptures the pointer.
    fn close_inventory(&mut self) {
        self.ui.close_inventory();
        self.capture_pointer();
    }

    /// Opens the crafting screen from gameplay and releases the pointer.
    fn open_crafting(&mut self) {
        if self.ui.screen != Screen::Playing || self.host.is_dead() {
            return;
        }
        self.cancel_mining();
        self.ui.open_crafting();
        self.release_pointer();
    }

    /// Closes crafting, returns to gameplay and recaptures the pointer.
    fn close_crafting(&mut self) {
        self.ui.close_crafting();
        self.capture_pointer();
    }

    /// Toggles the crafting screen from gameplay.
    fn toggle_crafting(&mut self) {
        if self.ui.screen == Screen::Crafting {
            self.close_crafting();
        } else {
            self.open_crafting();
        }
    }

    /// Opens a furnace or chest modal screen for the authoritative block
    /// entity at `pos`.
    fn open_container(&mut self, kind: ContainerKind, pos: BlockPos) {
        if self.ui.screen != Screen::Playing || self.host.is_dead() {
            return;
        }
        let exists = match kind {
            ContainerKind::Furnace => self.host.furnace_at(pos).is_some(),
            ContainerKind::Chest => self.host.chest_at(pos).is_some(),
        };
        if !exists {
            self.ui.toasts.push_warning("That container is no longer there");
            return;
        }
        self.cancel_mining();
        self.keys = [false; 8];
        self.container = Some(ContainerSession::new(kind, pos));
        match kind {
            ContainerKind::Furnace => self.ui.open_furnace(),
            ContainerKind::Chest => self.ui.open_chest(),
        }
        self.release_pointer();
    }

    /// Closes the open container, returning any cursor stack to the inventory.
    fn close_container(&mut self) {
        self.flush_container_cursor();
        self.container = None;
        self.ui.close_container();
        self.capture_pointer();
    }

    /// Verifies that the open container still exists; closes it otherwise.
    ///
    /// Breaking the block is handled explicitly in [`GameState::break_block`],
    /// but a host edit from any other path must not leave a stale modal open.
    fn validate_container(&mut self) {
        let Some(session) = &self.container else {
            return;
        };
        let exists = match session.kind {
            ContainerKind::Furnace => self.host.furnace_at(session.pos).is_some(),
            ContainerKind::Chest => self.host.chest_at(session.pos).is_some(),
        };
        if !exists {
            self.flush_container_cursor();
            self.container = None;
            self.ui.close_container();
            self.capture_pointer();
            self.ui.toasts.push_warning("The container was removed");
        }
    }

    /// Read-only snapshot backing container drawing and click decisions.
    fn container_snapshot(&self) -> ContainerSnapshot {
        match &self.container {
            Some(session) => container_snapshot(&self.host, session.kind, session.pos),
            None => ContainerSnapshot::default(),
        }
    }

    /// Puts the cursor stack back into the inventory, dropping it when the
    /// count-based API would silently repair a damaged tool or armor piece.
    fn deposit_cursor_stack(&mut self, stack: ItemStack, context: &str) {
        if stack.count == 0 {
            return;
        }
        let item = stack.item;
        if item.0 as usize >= self.registry.item_len() {
            return;
        }
        let max_durability = self
            .registry
            .item(item)
            .tool
            .map(|spec| spec.durability)
            .or_else(|| self.registry.item(item).armor.map(|spec| spec.durability));
        let damaged = stack
            .durability
            .zip(max_durability)
            .is_some_and(|(remaining, max)| remaining < max);
        let name = inventory::item_display_name(&self.registry, item);
        if damaged {
            self.drop_cursor_stack(stack);
            self.ui
                .toasts
                .push_warning(format!("Dropped {name} to keep its durability"));
            return;
        }
        let leftover = self.host.give_item(item, stack.count);
        if leftover > 0 {
            let mut rest = stack;
            rest.count = leftover;
            self.drop_cursor_stack(rest);
            self.ui
                .toasts
                .push_warning(format!("Inventory full - dropped {name} x{leftover}"));
        }
        let _ = context;
    }

    /// Spawns a dropped item entity at the player's feet.
    fn drop_cursor_stack(&mut self, stack: ItemStack) {
        let pos = self.host.player().pos;
        self.host
            .spawn_item_entity(WorldVec::new(pos.x, pos.y + 0.4, pos.z), stack);
    }

    /// Returns the cursor stack to the inventory and clears it.
    fn flush_container_cursor(&mut self) {
        let Some(session) = &mut self.container else {
            return;
        };
        if let Some(stack) = session.cursor.take() {
            self.deposit_cursor_stack(stack, "container close");
        }
    }

    /// Removes `count` items from a hotbar slot through the selected-slot
    /// consume API, restoring the previous selection.
    ///
    /// The interim host exposes no general "take from inventory" call; this is
    /// the only public route that removes an exact stack while preserving its
    /// durability. It is survival-only, so creative containers keep the
    /// cursor/deposit direction only.
    fn take_hotbar_stack(&mut self, slot: usize, count: u8) -> Option<ItemStack> {
        if self.host.game_mode() != HostGameMode::Survival || count == 0 {
            return None;
        }
        let source = self.host.inventory().get(slot).copied().flatten()?;
        if source.count < count {
            return None;
        }
        let previous = self.host.hotbar_selection();
        self.host.set_hotbar_selection(slot);
        let consumed = self.host.consume_selected(count);
        self.host.set_hotbar_selection(previous);
        if !consumed {
            return None;
        }
        let mut stack = source;
        stack.count = count;
        Some(stack)
    }

    /// Applies one container click to the authoritative host.
    fn apply_container_action(&mut self, action: ContainerAction) -> bool {
        match action {
            ContainerAction::TakeFurnace { pos, slot, count } => {
                if self.host.furnace_at(pos).is_none() {
                    return false;
                }
                let Some(mut whole) = self.host.take_furnace_slot(pos, slot) else {
                    return false;
                };
                if count < whole.count {
                    if slot == FurnaceSlot::Output {
                        // The output slot has no writable host API; restore the
                        // whole stack rather than silently overfilling the
                        // cursor. Normal clicks never request a partial output.
                        if let Some(state) = self.host.furnace_at_mut(pos) {
                            state.output = Some(whole);
                        }
                        return false;
                    }
                    let remainder = whole.count - count;
                    whole.count = remainder;
                    // A successful put returns the previous slot content, which
                    // is `None` here because the whole stack was just taken.
                    let _ = self.host.put_furnace_slot(pos, slot, whole);
                }
                true
            }
            ContainerAction::PutFurnace { pos, slot, stack } => {
                if slot == FurnaceSlot::Output || self.host.furnace_at(pos).is_none() {
                    return false;
                }
                let _ = self.host.put_furnace_slot(pos, slot, stack);
                true
            }
            ContainerAction::TakeChest { pos, slot, count } => {
                if self.host.chest_at(pos).is_none() {
                    return false;
                }
                let Some(mut whole) = self.host.take_chest_slot(pos, slot) else {
                    return false;
                };
                if count < whole.count {
                    let remainder = whole.count - count;
                    whole.count = remainder;
                    let _ = self.host.put_chest_slot(pos, slot, whole);
                }
                true
            }
            ContainerAction::PutChest { pos, slot, stack } => {
                if self.host.chest_at(pos).is_none() {
                    return false;
                }
                let _ = self.host.put_chest_slot(pos, slot, stack);
                true
            }
            ContainerAction::TakeHotbar { slot, count } => self.take_hotbar_stack(slot, count).is_some(),
            ContainerAction::GiveInventory { stack } => {
                self.deposit_cursor_stack(stack, "container click");
                true
            }
        }
    }

    /// Handles a left/right click on the currently hovered container slot.
    ///
    /// A click outside every slot returns the cursor stack to the inventory,
    /// matching the documented close behaviour.
    fn container_click(&mut self, button: ClickButton) {
        if !self.ui.container_open() {
            return;
        }
        let Some(session) = self.container.clone() else {
            return;
        };
        let layout = session.layout(self.window_screen());
        let Some(slot) = layout.hit_test(self.ui.cursor.0, self.ui.cursor.1) else {
            self.flush_container_cursor();
            return;
        };
        let view = self.container_snapshot();
        let outcome = session.click(&self.registry, &view, slot, button);
        if let Some(message) = outcome.message {
            self.ui.toasts.push(message);
            self.play_ui_sound(false);
            return;
        }
        let Some(action) = outcome.action else {
            return;
        };
        if matches!(action, ContainerAction::TakeHotbar { .. }) && self.host.game_mode() != HostGameMode::Survival {
            self.ui
                .toasts
                .push_warning("Taking from the hotbar needs survival mode");
            return;
        }
        if self.apply_container_action(action) {
            if let Some(session) = &mut self.container {
                session.cursor = outcome.cursor_after;
            }
            self.play_ui_sound(false);
        } else {
            self.ui.toasts.push_warning("That slot cannot accept the item");
        }
    }

    /// Activates the keyboard-selected container slot.
    fn activate_container_slot(&mut self) {
        let Some(session) = &self.container else {
            return;
        };
        let slot = session.selected;
        let layout = session.layout(self.window_screen());
        if layout.slot_rect(slot).is_none() {
            return;
        }
        let view = self.container_snapshot();
        let outcome = session.click(&self.registry, &view, slot, ClickButton::Left);
        if let Some(message) = outcome.message {
            self.ui.toasts.push(message);
            return;
        }
        let Some(action) = outcome.action else {
            return;
        };
        if matches!(action, ContainerAction::TakeHotbar { .. }) && self.host.game_mode() != HostGameMode::Survival {
            self.ui
                .toasts
                .push_warning("Taking from the hotbar needs survival mode");
            return;
        }
        if self.apply_container_action(action) {
            if let Some(session) = &mut self.container {
                session.cursor = outcome.cursor_after;
            }
            self.play_ui_sound(true);
        }
    }

    /// Moves container focus with the arrow keys.
    fn move_container_selection(&mut self, dx: i32, dy: i32) {
        let screen = self.window_screen();
        let Some(session) = &mut self.container else {
            return;
        };
        let layout = session.layout(screen);
        session.selected = containers::move_selection(&layout, session.selected, dx, dy);
    }

    /// Sends the selected recipe to the authoritative host and reports the
    /// outcome in the HUD toast queue.
    fn craft_selected(&mut self) {
        let rows = crafting_rows_for_host(&self.host, &self.registry);
        let Some(recipe) = self.ui.crafting.selected_recipe(&rows) else {
            return;
        };
        let outcome = self.host.craft(recipe);
        if outcome.crafted {
            self.ui.toasts.push_success(outcome.message);
        } else {
            // Failure messages are normal info text (`inventory full`,
            // `needs a crafting table`, `need 2 more planks`).
            self.ui.toasts.push(outcome.message);
        }
        if let Some(engine) = &mut self.audio {
            engine.play_sfx(if outcome.crafted {
                AudioSfx::Craft
            } else {
                AudioSfx::CraftFail
            });
        }
        self.update_title();
    }

    /// Plays a world sound at `position` with distance attenuation and pan.
    fn play_world_sfx(&mut self, sfx: AudioSfx, position: WorldVec, gain_scale: f32) {
        if self.audio.is_none() {
            return;
        }
        let player = self.host.player();
        let eye = player.eye();
        let dx = position.x - eye.x;
        let dz = position.z - eye.z;
        let distance = (dx * dx + dz * dz).sqrt();
        let gain = ((1.0 - distance / 24.0).clamp(0.0, 1.0) as f32) * gain_scale.clamp(0.0, 1.0);
        if gain <= 0.0 {
            return;
        }
        let right = dx * f64::from(player.yaw).cos() - dz * f64::from(player.yaw).sin();
        let pan = if distance > 0.25 {
            (right / distance * 0.8).clamp(-1.0, 1.0) as f32
        } else {
            0.0
        };
        if let Some(engine) = &mut self.audio {
            engine.play_sfx_at(sfx, gain, Some(pan));
        }
    }

    /// Sends one melee attack to the host, respecting the 0.4 s cooldown.
    ///
    /// The crosshair mob is selected by the same AABB raycast the host uses for
    /// validation. The first-person hit marker and red hurt tint come from the
    /// server-side hurt timer; the toasts report the damage/kill.
    fn try_attack_mob(&mut self, id: u64) {
        if self.host.is_dead() || self.ui.screen != Screen::Playing {
            return;
        }
        if self.attack_ready.is_some_and(|ready| ready > Instant::now()) {
            return;
        }
        let Some(entity) = self.host.mob_entity(id) else {
            return;
        };
        let kind = entity.mob.kind;
        let position = entity.mob.pos;
        let name = self.registry.mob(kind).display_name;
        let outcome = self.host.attack_mob(id);
        if !outcome.hit {
            return;
        }
        self.attack_ready = Some(Instant::now() + ATTACK_COOLDOWN);
        self.ui.flash_mob_hit();
        self.ui
            .toasts
            .push(format!("Hit {name} for {:.1}", outcome.damage.max(0.0)));
        self.play_world_sfx(AudioSfx::Hurt, position, 0.45);
        if outcome.killed {
            self.ui.toasts.push_success(format!("Killed {name}"));
            self.play_world_sfx(AudioSfx::Death, position, 0.35);
            if !outcome.drops.is_empty() {
                let drops = outcome
                    .drops
                    .iter()
                    .map(|drop| {
                        format!(
                            "{} x{}",
                            inventory::item_display_name(&self.registry, drop.item),
                            drop.count
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                self.ui.toasts.push(format!("Dropped {drops}"));
            }
        }
        if let Some(item) = outcome.tool_broke {
            let name = inventory::item_display_name(&self.registry, item);
            self.ui.toasts.push_warning(format!("Your {name} broke!"));
            self.play_world_sfx(AudioSfx::ToolBreak, position, 1.0);
            self.mouse_left = false;
            self.mining.stop();
        }
        self.update_title();
    }

    /// Stops any in-flight mining and clears the physical mouse-button latch.
    fn cancel_mining(&mut self) {
        self.mouse_left = false;
        self.mining.stop();
    }

    /// Drains authoritative pickup events into the bounded toast queue.
    ///
    /// The server collects dropped entities during its fixed ticks; this is the
    /// only place that announces them so break drops are never double-toasted.
    fn drain_pickup_events(&mut self) {
        for stack in self.host.take_pickup_events() {
            if stack.count == 0 || stack.item.0 as usize >= self.registry.item_len() {
                continue;
            }
            let name = inventory::item_display_name(&self.registry, stack.item);
            self.ui.toasts.push(format!("Picked up {name} x{}", stack.count));
            if let Some(engine) = &mut self.audio {
                engine.play_sfx(AudioSfx::Pickup);
            }
        }
    }

    /// Applies the live side effect of a settings row and persists settings.
    fn apply_settings_row(&mut self, row: SettingsRow) {
        match row {
            SettingsRow::RenderDistance => {
                let previous = self.host.render_distance_chunks();
                let requested = self.ui.settings.render_distance_chunks;
                if self.host.set_render_distance_chunks(requested) {
                    self.applied_render_distance = requested;
                    if requested > 16 && previous <= 16 {
                        self.ui
                            .toasts
                            .push_warning("Above 16 chunks uses far LOD; terrain memory and GPU cost rise");
                    }
                }
            }
            SettingsRow::Fullscreen => self.apply_fullscreen(self.ui.settings.fullscreen),
            SettingsRow::GameMode => self.apply_game_mode_setting(),
            SettingsRow::TextureStyle => {
                // Style rows switch the active source to Style(new style) for
                // this run. A persisted external pack stays saved, so the
                // texture-pack row (or the next launch) can re-enable it.
                let style = render_texture_style(self.ui.settings.texture_style);
                self.set_active_texture_style(style);
            }
            SettingsRow::TexturePack => self.toggle_texture_pack(),
            SettingsRow::AudioEnabled => {
                self.audio_setting_touched = true;
                self.sync_audio_settings();
            }
            SettingsRow::MasterVolume | SettingsRow::SfxVolume | SettingsRow::MusicVolume => {
                self.sync_audio_settings();
            }
            SettingsRow::UiScale => self.refresh_ui_scale(),
            SettingsRow::ReducedMotion
            | SettingsRow::ShowHud
            | SettingsRow::DebugOverlay
            | SettingsRow::Fov
            | SettingsRow::MouseSensitivity
            | SettingsRow::MeshWorkers
            | SettingsRow::UiFont
            | SettingsRow::ResetUiFont
            | SettingsRow::ReloadUiFont
            | SettingsRow::Keybind(_)
            | SettingsRow::ResetKeybinds => {}
        }
        // The settings screen promises that every change is saved immediately.
        self.persist_settings();
    }

    /// Applies a settings action; returns true when the process should exit.
    fn handle_settings_action(&mut self, action: SettingsAction) -> bool {
        match action {
            SettingsAction::Resume => {
                self.ui.resume();
                self.capture_pointer();
                self.persist_settings();
                false
            }
            SettingsAction::SaveAndQuit => {
                if let Err(err) = self.host.save() {
                    log::warn!("save on quit failed: {err}");
                }
                self.persist_settings();
                true
            }
            SettingsAction::Adjust(row) => {
                self.apply_settings_row(row);
                false
            }
            SettingsAction::BeginBind(action) => {
                self.ui.begin_bind(action);
                false
            }
            SettingsAction::ResetKeybinds => {
                self.ui.reset_keybinds();
                self.persist_settings();
                false
            }
            SettingsAction::ResetUiFont => {
                let source = reset_ui_font(&mut self.renderer, &mut self.ui.settings);
                self.ui.active_ui_font_name = self.renderer.ui_font_name().to_string();
                self.ui.active_ui_font_source = source.display_label();
                self.persist_settings();
                false
            }
            SettingsAction::ReloadUiFont => {
                let source = reload_ui_font(&mut self.renderer, &self.ui.settings, &self.data_dir);
                self.ui.active_ui_font_name = self.renderer.ui_font_name().to_string();
                self.ui.active_ui_font_source = source.display_label();
                log::info!("reloaded UI font: {}", source.display_label());
                false
            }
        }
    }

    /// Switches to a procedural style and remembers it as the active source.
    fn set_active_texture_style(&mut self, style: ob_render::TexturePackStyle) {
        let source = TextureSource::Style(style);
        match self.renderer.set_texture_source(source.clone()) {
            Ok(()) => {
                self.texture_source = source;
                self.sync_ui_texture_source();
                log::info!("textures: {}", self.texture_source.label());
            }
            Err(err) => log::warn!("could not switch to texture style {}: {err}", style.name()),
        }
    }

    /// Toggles between the active external pack and the procedural fallback.
    ///
    /// Enabling is only possible when a persisted pack path exists; disabling
    /// always clears the persisted path. Failed switches leave the active
    /// source and settings untouched.
    fn toggle_texture_pack(&mut self) {
        match texture_toggle_target(&self.texture_source, &self.ui.settings) {
            TextureToggle::Noop => {}
            TextureToggle::Disable => {
                let style = render_texture_style(self.ui.settings.texture_style);
                let source = TextureSource::Style(style);
                match self.renderer.set_texture_source(source.clone()) {
                    Ok(()) => {
                        self.texture_source = source;
                        self.ui.settings.texture_pack = None;
                        self.sync_ui_texture_source();
                        log::info!("textures: {}", self.texture_source.label());
                    }
                    Err(err) => log::warn!("could not switch to the built-in texture style: {err}"),
                }
            }
            TextureToggle::Enable(path) => {
                let source = TextureSource::ResourcePack(path.clone());
                match self.renderer.set_texture_source(source.clone()) {
                    Ok(()) => {
                        self.texture_source = source;
                        self.ui.settings.texture_pack = Some(path.to_string_lossy().into_owned());
                        self.sync_ui_texture_source();
                        log::info!("textures: {}", self.texture_source.label());
                    }
                    Err(err) => log::warn!(
                        "could not load texture pack {}: {err}; keeping the built-in style",
                        path.display()
                    ),
                }
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
        let target_hit = self.host.world().raycast(eye, player.forward(), 5.0);
        let target_block = target_hit.map(|hit| self.host.world().get_block(hit.block));
        let target = target_hit.map(|hit| {
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
        let selected_stack = self.host.selected_stack();
        let (held, held_durability) = if let Some(stack) = selected_stack {
            let item = self.registry.item(stack.item);
            let durability = item.tool.and_then(|spec| {
                stack
                    .durability
                    .map(|remaining| (remaining.min(spec.durability), spec.durability))
            });
            (inventory::display_name(item.id), durability)
        } else if self.host.game_mode() == HostGameMode::Survival {
            ("empty".to_string(), None)
        } else {
            let name = self
                .ui
                .hotbar_block(&self.registry, self.ui.hotbar_selected)
                .map(|block| inventory::display_name(block.id))
                .unwrap_or_else(|| "empty".to_string());
            (name, None)
        };
        let held_item = selected_stack.map(|stack| stack.item);
        let mining_speed = match target_block {
            Some(block) => self.registry.mining_speed(block, held_item),
            None => selected_stack
                .and_then(|stack| self.registry.item(stack.item).tool)
                .map(|spec| spec.speed)
                .unwrap_or(1.0),
        };
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
            lod_batches: self.lod.batches().len(),
            lod_sections: self.lod.represented_sections() as usize,
            lod_skipped_sections: self.lod.skipped_sections() as usize,
            mesh_bytes: self.mesh_bytes_estimate(),
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
            ui_scale: crate::ui_scale::debug_line(
                self.ui.settings.ui_scale_percent,
                [self.window_size.0, self.window_size.1],
            ),
            time_of_day: Environment::from_ticks(self.host.tick_count() + DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS)
                .time_of_day,
            texture_source: self.texture_source_description(),
            ui_font: self.renderer.ui_font_name().to_string(),
            game_mode: self.host.game_mode().name(),
            health: player.health,
            hunger: player.hunger,
            held,
            held_durability,
            mining_speed,
            network: None,
            movement: if player.flying {
                "fly"
            } else if player.sprinting {
                "sprint"
            } else if player.sneaking {
                "sneak"
            } else {
                "walk"
            },
            audio: self.audio_status.clone(),
            mobs_total: self.host.mob_count(),
            mobs_passive: self.host.passive_mob_count(),
            mobs_hostile: self.host.hostile_mob_count(),
            day_phase: self.host.day_phase(),
            day: self.host.is_day(),
            defense: self.host.armor_defense(),
        };
    }

    /// F3 label for the active texture source. Procedural styles include their
    /// resolution; the renderer does not expose a loaded pack's resolution.
    fn texture_source_description(&self) -> String {
        describe_texture_source(&self.texture_source, self.renderer.texture_resolution())
    }

    /// Keeps the settings-row display in sync with the active source.
    fn sync_ui_texture_source(&mut self) {
        self.ui.active_texture_pack = active_texture_pack_label(&self.texture_source);
    }

    /// Runtime values the settings screen displays.
    fn settings_info(&self) -> SettingsInfo {
        SettingsInfo {
            health: self.host.player().health,
            hunger: self.host.player().hunger,
            texture_source: self.texture_source.label(),
            texture_resolution: self.renderer.texture_resolution(),
            mesh_workers: self.mesh_scheduler.workers(),
            ui_scale_percent: resolved_ui_scale_percent(self.ui.settings.ui_scale_percent, self.window_size.1),
            audio_status: self.audio_status.clone(),
        }
    }

    fn update_title(&mut self) {
        let player = self.host.player();
        let survival = self.host.game_mode() == HostGameMode::Survival;
        let block = if survival {
            self.host
                .selected_stack()
                .map(|stack| inventory::item_display_name(&self.registry, stack.item))
                .unwrap_or_else(|| "empty".to_string())
        } else {
            self.ui
                .hotbar_label(self.ui.hotbar_selected)
                .unwrap_or("unknown")
                .to_string()
        };
        let status = match self.ui.screen {
            Screen::Playing => {
                if self.captured {
                    ""
                } else {
                    " | click to capture | E inventory | F3 debug"
                }
            }
            Screen::Inventory => " | INVENTORY (E/Esc to close)",
            Screen::Crafting => " | CRAFTING (C/Esc to close)",
            Screen::Furnace => " | FURNACE (E/Esc to close)",
            Screen::Chest => " | CHEST (E/Esc to close)",
            Screen::Settings => " | SETTINGS (Esc to resume)",
            Screen::Death => " | DEAD (Enter respawn)",
            Screen::Connecting => " | CONNECTING",
        };
        let movement = if player.flying {
            "fly"
        } else if player.sprinting {
            "sprint"
        } else if player.sneaking {
            "sneak"
        } else {
            "walk"
        };
        let mode = if survival {
            format!("survival \u{2665}{:.0} food {:.0}", player.health, player.hunger)
        } else {
            "creative".to_string()
        };
        self.window.set_title(&format!(
            "OpenBlock {} | {} FPS | {:.0}, {:.0}, {:.0} | {} | {} | {}{}",
            ob_core::VERSION,
            self.fps,
            player.pos.x,
            player.pos.y,
            player.pos.z,
            block,
            movement,
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

    /// Chunk containing the local player.
    fn player_chunk(&self) -> ChunkPos {
        let pos = self.host.player().pos;
        ChunkPos::new(
            (pos.x.floor() as i32).div_euclid(SECTION_SIZE as i32),
            (pos.z.floor() as i32).div_euclid(SECTION_SIZE as i32),
        )
    }

    /// Columns generated by the next streaming call.
    ///
    /// The initial load spends a large budget to get the near ring playable,
    /// then degrades gracefully: a full 65×65 field is drained over a few
    /// seconds in the background instead of one column per frame for minutes.
    /// Generation itself is rayon-parallel inside [`Host::ensure_streaming`].
    fn stream_budget(&self) -> usize {
        let radius = if self.initial_load {
            self.initial_target_radius
        } else {
            self.host.render_distance_chunks()
        };
        let center = self.player_chunk();
        let mut missing = 0usize;
        for dz in -radius..=radius {
            for dx in -radius..=radius {
                if self
                    .host
                    .world()
                    .chunk(ChunkPos::new(center.x + dx, center.z + dz))
                    .is_none()
                {
                    missing += 1;
                }
            }
        }
        if self.initial_load {
            if missing > 0 { 64 } else { 2 }
        } else if missing > 1024 {
            32
        } else if missing > 256 {
            16
        } else if missing > 0 {
            8
        } else {
            2
        }
    }

    /// Current LOD policy derived from the render-distance setting.
    fn lod_config_for(&self) -> LodConfig {
        LodConfig::for_render_distance(self.host.render_distance_chunks())
    }

    /// Estimated CPU+GPU vertex-buffer bytes for the F3 overlay.
    fn mesh_bytes_estimate(&self) -> usize {
        let section_vertices: usize = self.meshes.values().copied().sum();
        let section_bytes = section_vertices as u64 * std::mem::size_of::<ob_render::Vertex>() as u64;
        (section_bytes + self.lod.vertex_bytes()) as usize * 2
    }

    /// Recomputes the initial-loading overlay percentage and finishes it when
    /// the near ring is resident and its section backlog is drained.
    fn update_initial_load(&mut self) {
        if !self.initial_load {
            return;
        }
        let radius = self.initial_target_radius.max(1);
        let center = self.player_chunk();
        let expected_columns = ((2 * radius + 1) * (2 * radius + 1)) as usize;
        let mut loaded_columns = 0usize;
        let mut expected_sections = 0usize;
        let mut ready_sections = 0usize;
        for dz in -radius..=radius {
            for dx in -radius..=radius {
                let chunk = ChunkPos::new(center.x + dx, center.z + dz);
                let Some(column) = self.host.world().chunk(chunk) else {
                    continue;
                };
                loaded_columns += 1;
                for index in 0..N_SECTIONS {
                    let pos = SectionPos::new(
                        chunk.x,
                        ob_math::WORLD_MIN_Y / SECTION_SIZE as i32 + index as i32,
                        chunk.z,
                    );
                    if column.section(pos).is_some_and(|section| !section.is_empty()) {
                        expected_sections += 1;
                        if self.meshes.contains_key(&pos) {
                            ready_sections += 1;
                        }
                    }
                }
            }
        }
        let column_fraction = loaded_columns as f32 / expected_columns.max(1) as f32;
        let mesh_fraction = if expected_sections == 0 {
            1.0
        } else {
            ready_sections as f32 / expected_sections as f32
        };
        self.loading_progress = (0.55 * column_fraction + 0.45 * mesh_fraction).clamp(0.0, 0.99);
        self.loading_detail = format!(
            "{} / {} columns   {} / {} sections meshed   {:.0}%",
            loaded_columns,
            expected_columns,
            ready_sections,
            expected_sections,
            self.loading_progress * 100.0
        );
        let drained = self.host.dirty_sections() == 0 && self.mesh_scheduler.in_flight_count() == 0;
        if loaded_columns == expected_columns && drained {
            self.finish_initial_load();
        } else if self.loading_started.elapsed() >= INITIAL_LOAD_TIMEOUT {
            log::warn!(
                "initial load did not fully drain after {} s; entering play with {} columns and {} mesh jobs in flight",
                INITIAL_LOAD_TIMEOUT.as_secs(),
                loaded_columns,
                self.mesh_scheduler.in_flight_count()
            );
            self.finish_initial_load();
        }
    }

    fn finish_initial_load(&mut self) {
        self.initial_load = false;
        self.loading_progress = 1.0;
        self.loading_detail = "world ready".to_string();
        self.ui
            .toasts
            .push_success("World ready - far terrain streams in the background");
    }

    fn stream_and_mesh(&mut self) {
        let budget = self.stream_budget();
        self.host.ensure_streaming(budget);
        for chunk in self.host.take_unloaded_chunks() {
            self.unload_chunk_meshes(chunk);
        }

        // Keep the near/far split current before touching the dirty set, so a
        // section in a far group is never queued for a full 16³ mesh. A render
        // distance change is applied inside maintain() and promotes/demotes
        // groups at the new boundary.
        let camera = self.player_chunk();
        let config = self.lod_config_for();
        self.lod.maintain(
            self.host.world_mut(),
            camera,
            config,
            &self.registry,
            &mut self.mesh_scheduler,
            &mut self.renderer,
            &mut self.meshes,
        );

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
            if self.lod.is_far(pos) {
                // A far edit invalidates the merged batch instead of producing
                // a full section mesh; the next maintain pass rebuilds it.
                let key = LodBatchKey::containing(pos.chunk(), self.lod.config().group_chunks);
                self.lod.invalidate(key, &mut self.renderer, &mut self.mesh_scheduler);
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
        self.lod
            .drain(self.host.world(), &mut self.mesh_scheduler, &mut self.renderer);
        self.update_initial_load();

        if self.mesh_stats_timer.elapsed() >= MESH_STATS_INTERVAL {
            log::debug!(
                "mesh scheduler: submitted {} completed {} dropped {} in-flight {} backlog {}; lod {} batches, {} in flight, {} done, {} dropped",
                self.mesh_scheduler.submitted_count(),
                self.mesh_scheduler.completed_count(),
                self.mesh_scheduler.dropped_count(),
                self.mesh_scheduler.in_flight_count(),
                self.host.dirty_sections(),
                self.lod.batches().len(),
                self.mesh_scheduler.lod_in_flight_count(),
                self.mesh_scheduler.lod_completed_count(),
                self.mesh_scheduler.lod_dropped_count(),
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
            if self.host.world().section(result.pos).is_none() || self.lod.is_far(result.pos) {
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
        // A far batch covering the unloaded column is now stale.
        self.lod.drop_chunk(chunk, &mut self.renderer, &mut self.mesh_scheduler);
    }

    fn tick(&mut self) {
        let mut input = PlayerInput {
            forward: 0.0,
            strafe: 0.0,
            jump: false,
            sprint: false,
            sneak: false,
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
        // Shift crouches/sneaks; Ctrl sprints (Minecraft-style bindings).
        input.sneak = self.keys[5];
        input.sprint = self.keys[6];
        self.prev_eye = self.host.player().eye();
        let before = self.host.player().pos;
        self.host.tick(&input);
        let after = self.host.player().pos;
        self.current_eye = self.host.player().eye();
        let dx = after.x - before.x;
        let dz = after.z - before.z;
        self.footstep_distance += (dx * dx + dz * dz).sqrt();
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
        if self.host.is_dead() && self.ui.screen != Screen::Death {
            self.begin_death_screen();
        } else if !self.host.is_dead() && self.ui.screen == Screen::Death {
            // A creative-mode switch (or an external respawn) revives the player.
            self.ui.clear_death();
            self.capture_pointer();
        }
        self.accumulator += dt.min(Duration::from_millis(250));
        let mut ticks = 0;
        if self.ui.screen == Screen::Playing && !self.host.is_dead() && !self.initial_load {
            while self.accumulator >= TICK && ticks < 5 {
                self.accumulator -= TICK;
                self.tick();
                ticks += 1;
            }
        } else {
            // Menus, settings and death stop simulation and movement.
            self.accumulator = Duration::ZERO;
        }
        self.drain_pickup_events();
        self.validate_container();
        let damaged = if self.host.game_mode() == HostGameMode::Survival {
            self.damage_flash.observe(self.host.player().health)
        } else {
            self.damage_flash.reset();
            false
        };
        if damaged {
            self.camera_shake = DAMAGE_SHAKE_RADIANS;
        }
        self.camera_shake *= (-dt.as_secs_f32() * 9.0).exp();
        if self.camera_shake < 0.0002 {
            self.camera_shake = 0.0;
        }
        self.damage_flash.tick(dt.as_secs_f32());
        self.update_mining(dt);
        self.update_audio_state(dt.as_secs_f32());
        self.ui.tick(dt.as_secs_f32());
        let simulation_dt = if self.ui.screen == Screen::Playing && !self.host.is_dead() {
            dt.as_secs_f32()
        } else {
            0.0
        };
        self.mob_render.sync(self.host.mobs(), simulation_dt);
        self.rolling_frame_ms.push(dt.as_secs_f32() * 1000.0);
        if self.rolling_frame_ms.len() > 120 {
            self.rolling_frame_ms.drain(..60);
        }
        let alpha = (self.accumulator.as_secs_f32() / TICK.as_secs_f32()).clamp(0.0, 1.0);
        let eye = self.prev_eye.lerp(self.current_eye, alpha as f64);
        let player = self.host.player();
        let shake = if !self.ui.settings.reduced_motion && self.camera_shake > 0.0 {
            (self.started.elapsed().as_secs_f32() * 72.0).sin() * self.camera_shake
        } else {
            0.0
        };
        let mut camera = Camera::new(eye, player.yaw, (player.pitch + shake).clamp(-1.5533, 1.5533));
        // Sprinting slightly widens the FOV; crouching leaves it alone. Reduced
        // motion disables the pump entirely.
        let target_fov = target_fov(&self.ui.settings, player.sprinting);
        let blend = 1.0 - (-dt.as_secs_f32() * 12.0).exp();
        self.current_fov += (target_fov - self.current_fov) * blend;
        camera.fov_y = self.current_fov.to_radians();
        camera.far = far_plane_for_render_distance(self.host.render_distance_chunks());
        self.stream_and_mesh();
        // Keep the window title in sync with the effective render distance.
        if self.applied_render_distance != self.host.render_distance_chunks() {
            self.applied_render_distance = self.host.render_distance_chunks();
        }
        // Day starts at sunrise: tick 0 is 06:00 rather than midnight.
        let environment = Environment::from_ticks(self.host.tick_count() + DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS);
        self.refresh_debug_stats();
        let elapsed = self.started.elapsed().as_secs_f32();
        let mut entity_instances = entity_instances_for(self.host.item_entities(), &self.registry, elapsed);
        entity_instances.extend(self.mob_render.instances(&self.registry, alpha));
        self.renderer.set_entities(&entity_instances);
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
        let mut renderer = match OffscreenRenderer::new_with_source(
            SCREENSHOT_WIDTH,
            SCREENSHOT_HEIGHT,
            self.texture_source.clone(),
        ) {
            Ok(renderer) => renderer,
            Err(err) => {
                log::warn!("screenshot failed: {err}");
                return;
            }
        };
        // F2 must capture the same UI font as the window, including a saved
        // Minecraftia override or a font discovered in <data>/fonts/.
        let resolved_font = ui_font::resolve_ui_font_for_settings(&self.ui.settings, &self.data_dir);
        ui_font::install_ui_font(&mut renderer, &resolved_font);
        // Build the UI for the offscreen target size so the HUD is included.
        let ui_frame = if ui.is_some() {
            self.build_ui_frame_at([SCREENSHOT_WIDTH as f32, SCREENSHOT_HEIGHT as f32])
        } else {
            None
        };
        let elapsed = self.started.elapsed().as_secs_f32();
        let mut entities = entity_instances_for(self.host.item_entities(), &self.registry, elapsed);
        entities.extend(mobs::mob_instances(&self.registry, self.host.mobs(), elapsed));
        renderer.set_entities(&entities);
        let scene = SceneParams {
            environment: *environment,
            ui: ui_frame.as_ref().or(ui),
            outline,
        };
        if let Err(err) = renderer.render_world_scene_lod(self.host.world(), camera, &scene, &self.lod.config()) {
            log::warn!("screenshot render failed: {err}");
            return;
        }
        let rgba = renderer.read_rgba();
        match write_png(&path, SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT, &rgba) {
            Ok(()) => log::info!("screenshot saved to {}", path.display()),
            Err(err) => log::warn!("screenshot write failed: {err}"),
        }
    }

    /// Builds the UI frame for the current window size.
    fn build_ui_frame(&self) -> Option<UiFrame> {
        self.build_ui_frame_at([self.window_size.0.max(1.0), self.window_size.1.max(1.0)])
    }

    /// Builds the UI frame for an explicit physical target size.
    ///
    /// The frame is laid out in logical pixels and `UiFrame::new` uses that
    /// logical size for its NDC conversion, so the GPU applies exactly one
    /// scale transform to every rectangle, glyph and block icon.
    fn build_ui_frame_at(&self, physical: [f32; 2]) -> Option<UiFrame> {
        let screen = ui_screen(physical, self.ui.settings.ui_scale_percent);
        let [width, height] = screen;
        let playing = self.ui.screen == Screen::Playing;
        let inventory_open = self.ui.screen == Screen::Inventory;
        let crafting_open = self.ui.screen == Screen::Crafting;
        let container_open = self.ui.container_open();
        let death_open = self.ui.screen == Screen::Death;
        let survival = self.host.game_mode() == HostGameMode::Survival;
        // The survival inventory panel contains its own hotbar and footer; the
        // gameplay hotbar/vitals would overlap them at 150% on 960x540.
        let inventory_hud_hotbar = inventory_open && !survival;
        let show_hotbar = (playing || death_open || inventory_hud_hotbar) && self.ui.hud_visible;
        let show_crosshair = playing && self.ui.hud_visible && !self.host.is_dead();
        let show_debug = self.ui.debug_overlay;
        let settings_open = self.ui.screen == Screen::Settings;
        if !show_hotbar
            && !show_crosshair
            && !show_debug
            && !settings_open
            && !inventory_open
            && !crafting_open
            && !container_open
            && !death_open
            && !self.initial_load
        {
            return None;
        }
        let mut frame = UiFrame::new(width, height);

        // --- Inventory: dimmed world, hotbar row or creative block grid ---
        if inventory_open {
            if survival {
                inventory::draw_survival(
                    &mut frame,
                    &self.ui,
                    &self.registry,
                    &inventory::SurvivalInventoryView {
                        inventory: self.host.inventory(),
                        equipment: self.host.equipment(),
                        defense: self.host.armor_defense(),
                        selected: self.host.hotbar_selection(),
                        near_table: self.host.near_crafting_table(),
                        craft_hint: None,
                    },
                    screen,
                );
            } else {
                inventory::draw(&mut frame, &self.ui, &self.registry, screen);
            }
        }

        // --- HUD: crosshair, mining progress, vitals, hotbar and item name ---
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
                if self.ui.hit_marker_seconds > 0.0 {
                    let alpha = (self.ui.hit_marker_seconds / 0.22).clamp(0.0, 1.0);
                    let marker = [1.0, 0.42, 0.36, alpha];
                    for (dx, dy) in [(-14.0, -14.0), (14.0, -14.0), (-14.0, 14.0), (14.0, 14.0)] {
                        frame.rect(centre_x + dx - 3.0, centre_y + dy - 1.0, 6.0, 2.0, marker);
                        frame.rect(centre_x + dx - 1.0, centre_y + dy - 3.0, 2.0, 6.0, marker);
                    }
                }
                if self.ui.break_flash_seconds > 0.0
                    && let Some(name) = &self.ui.break_flash
                {
                    frame.text_centered(centre_x, centre_y + 30.0, 1.5, [0.98, 0.94, 0.78, 0.95], name);
                }
            }

            if show_hotbar {
                draw_hotbar_for_mode(&mut frame, &self.ui, &self.registry, &self.host, screen);
                if playing {
                    draw_selected_name(&mut frame, &self.ui, &self.registry, &self.host, screen);
                }
            }

            if survival && show_hotbar {
                draw_vitals_for_mode(&mut frame, &self.ui, &self.host, screen);
                if playing {
                    ui_state::draw_toasts(&mut frame, &self.ui.toasts, screen);
                    ui_state::draw_damage_flash(&mut frame, screen, self.damage_flash.alpha());
                }
            }

            if playing {
                let mode = if survival {
                    "survival - Shift crouch, Ctrl sprint, RMB eat/place"
                } else if self.host.player().flying {
                    "fly (F)"
                } else {
                    "walk (F to fly)"
                };
                frame.text(12.0, height - 26.0, 1.5, [0.85, 0.89, 0.94, 0.85], mode);
            }
        }

        // --- Crafting screen: modal, with toasts and the F3 overlay on top ---
        if crafting_open {
            let rows = crafting_rows_for_host(&self.host, &self.registry);
            crafting::draw_crafting_screen(
                &mut frame,
                &self.ui.crafting,
                &rows,
                &self.registry,
                self.host.near_crafting_table(),
                self.host.game_mode() == HostGameMode::Creative,
                screen,
            );
        }

        // --- Furnace / chest: modal block-entity screens ---
        if container_open && let Some(session) = &self.container {
            let view = self.container_snapshot();
            match session.kind {
                ContainerKind::Furnace => containers::draw_furnace(
                    &mut frame,
                    &self.registry,
                    session,
                    self.host.furnace_at(session.pos),
                    &view,
                    self.ui.cursor,
                    screen,
                ),
                ContainerKind::Chest => containers::draw_chest(
                    &mut frame,
                    &self.registry,
                    session,
                    self.host.chest_at(session.pos),
                    &view,
                    self.ui.cursor,
                    screen,
                ),
            }
        }

        // --- Tabbed settings screen: modal, with the F3 overlay still on top ---
        if settings_open {
            let info = self.settings_info();
            ui_state::draw_settings_screen(&mut frame, &self.ui, &info, screen);
        }

        // --- Death screen: drawn over the HUD, below the F3 overlay ---
        if death_open {
            ui_state::draw_death_screen(&mut frame, &self.ui, screen);
        }

        // --- Initial streaming overlay: covers the HUD until the near ring is
        // resident and meshed. Play starts then; far batches keep building in
        // the background.
        if self.initial_load {
            frame.rect(0.0, 0.0, width, height, [0.02, 0.03, 0.05, 0.55]);
            let panel_w = (width * 0.62).clamp(300.0, 560.0);
            let panel_h = 118.0;
            let x = (width - panel_w) * 0.5;
            let y = (height - panel_h) * 0.5;
            frame.rect(x, y, panel_w, panel_h, [0.05, 0.07, 0.11, 0.97]);
            frame.rect(x, y, panel_w, 2.0, [0.98, 0.82, 0.45, 0.9]);
            frame.text_centered(width * 0.5, y + 16.0, 2.0, [0.98, 0.82, 0.45, 1.0], "Loading world");
            let radius = self.host.render_distance_chunks();
            frame.text_centered(
                width * 0.5,
                y + 44.0,
                1.3,
                [0.86, 0.91, 0.98, 0.95],
                &format!("{radius} chunks / {} blocks", radius * 16),
            );
            let bar_x = x + 30.0;
            let bar_y = y + 68.0;
            let bar_w = panel_w - 60.0;
            let bar_h = 10.0;
            frame.rect(bar_x, bar_y, bar_w, bar_h, [0.10, 0.12, 0.16, 1.0]);
            frame.rect(
                bar_x,
                bar_y,
                bar_w * self.loading_progress.clamp(0.0, 1.0),
                bar_h,
                [0.35, 0.75, 0.45, 1.0],
            );
            frame.text_centered(
                width * 0.5,
                y + 86.0,
                1.05,
                [0.78, 0.84, 0.92, 0.95],
                &self.loading_detail,
            );
        }

        // --- F3 debug overlay: always on top so it stays usable while dead ---
        if show_debug {
            draw_debug_overlay(&mut frame, &self.debug_stats, screen);
        }
        Some(frame)
    }

    /// Advances hold-to-mine progress from the frame clock.
    ///
    /// Starts mining the breakable block under the crosshair while the left
    /// mouse button is held, resets when the target changes or the button is
    /// released, and applies exactly one break action on completion.
    fn update_mining(&mut self, dt: Duration) {
        if self.ui.screen != Screen::Playing || !self.captured || !self.mouse_left || self.host.is_dead() {
            self.mining.stop();
            return;
        }
        let player = self.host.player();
        // Mobs take priority over blocks: while a mob is under the crosshair,
        // hold-to-mine is cancelled and each click is a melee attack.
        let mob_target = self
            .host
            .mob_hit_by_ray(player.eye(), player.forward(), MOB_ATTACK_REACH);
        if let Some(id) = mob_target {
            self.mining.stop();
            self.try_attack_mob(id);
            return;
        }
        let current = self
            .host
            .world()
            .raycast(player.eye(), player.forward(), REACH)
            .map(|hit| hit.block);
        let breakable_target = current.and_then(|pos| {
            let block_id = self.host.world().get_block(pos);
            let block = self.registry.block(block_id);
            let seconds = self.host.mining_seconds(block_id) as f32;
            (block.breakable() && seconds.is_finite() && seconds > 0.0).then_some((pos, seconds))
        });

        // A hotbar switch can change the effective duration while the same
        // block stays under the crosshair; restart instead of keeping a stale
        // timer that would desynchronise client prediction from host checks.
        if let Some((pos, seconds)) = breakable_target
            && self.mining.target() == Some(pos)
            && (self.mining.duration_seconds() - seconds).abs() > 1e-6
        {
            self.mining.start(pos, seconds);
        }

        self.mining.update(dt.as_secs_f32(), current);
        if let Some(pos) = self.mining.take_completed() {
            self.break_block(pos);
            // Do not immediately start on the next block in the same frame;
            // the next frame re-raycasts the changed world.
            return;
        }
        if self.mining.target().is_none()
            && let Some((pos, seconds)) = breakable_target
        {
            self.mining.start(pos, seconds);
        }
    }

    /// Sends one break action for the completed mining target.
    fn break_block(&mut self, pos: BlockPos) {
        let (name, break_sfx) = {
            let block_id = self.host.world().get_block(pos);
            let block = self.registry.block(block_id);
            if block.id == "openblock:air" {
                return;
            }
            (
                inventory::display_name(block.id),
                audio_material_for(&self.registry, block_id).map(AudioSfx::BlockBreak),
            )
        };
        let action = BlockAction {
            break_block: true,
            place_block: false,
            place: BlockId::AIR,
        };
        let survival = self.host.game_mode() == HostGameMode::Survival;
        let outcome = self.host.apply_action_with_outcome(&action);
        if outcome.changed {
            if let Some(sfx) = break_sfx {
                self.play_block_sfx(sfx, pos);
            }
            if survival && !outcome.drops.is_empty() {
                // Survival breaks spawn item entities; this is only a subtle
                // acknowledgement, not a pickup (pickup toasts come from the
                // host's drained pickup events).
                for drop in &outcome.drops {
                    let name = inventory::item_display_name(&self.registry, drop.item);
                    self.ui.toasts.push(format!("Dropped {name} x{}", drop.count));
                }
            }
            if self.container.as_ref().is_some_and(|session| session.pos == pos) {
                self.close_container();
            }
            if let Some(item) = outcome.tool_broke {
                let name = inventory::item_display_name(&self.registry, item);
                self.ui.toasts.push_warning(format!("Your {name} broke!"));
                if let Some(engine) = &mut self.audio {
                    engine.play_sfx(AudioSfx::ToolBreak);
                }
                // The host removed the tool; stop the current hold so the
                // player must click again rather than silently mining by hand.
                self.mouse_left = false;
                self.mining.stop();
            }
            self.ui.flash_broken_block(name);
        }
    }

    /// Starts hold-to-mine for the block currently under the crosshair.
    fn start_mining(&mut self) {
        if !self.captured || self.ui.screen != Screen::Playing || self.host.is_dead() {
            return;
        }
        self.mouse_left = true;
        let player = self.host.player();
        if let Some(id) = self
            .host
            .mob_hit_by_ray(player.eye(), player.forward(), MOB_ATTACK_REACH)
        {
            self.mining.stop();
            self.try_attack_mob(id);
            return;
        }
        let Some(hit) = self.host.world().raycast(player.eye(), player.forward(), REACH) else {
            self.mining.stop();
            return;
        };
        let block_id = self.host.world().get_block(hit.block);
        let block = self.registry.block(block_id);
        let seconds = self.host.mining_seconds(block_id) as f32;
        if block.breakable() && seconds.is_finite() && seconds > 0.0 {
            self.mining.start(hit.block, seconds);
        } else {
            self.mining.stop();
        }
    }

    /// Handles right-click: eat food or place a block, throttled per cooldown.
    ///
    /// Survival eats a selected edible item when hunger is missing, then falls
    /// through to placement rules. Placement must match the selected stack's
    /// block item; creative keeps the fixed palette pick.
    fn place_block(&mut self) {
        if !self.captured || self.ui.screen != Screen::Playing || self.host.is_dead() {
            return;
        }
        // Right-click always tries a block interaction first. A targeted
        // crafting table opens the crafting screen; every other block keeps
        // the normal eat/place rules below.
        let player = self.host.player();
        if let Some(hit) = self.host.world().raycast(player.eye(), player.forward(), REACH) {
            let block = self.host.world().get_block(hit.block);
            match self.registry.block(block).id {
                "openblock:crafting_table" => {
                    self.open_crafting();
                    return;
                }
                "openblock:furnace" => {
                    self.open_container(ContainerKind::Furnace, hit.block);
                    return;
                }
                "openblock:chest" => {
                    self.open_container(ContainerKind::Chest, hit.block);
                    return;
                }
                _ => {}
            }
        }
        if self.host.game_mode() == HostGameMode::Survival {
            let stack = self.host.selected_stack();
            if inventory::should_eat(&self.registry, stack, self.host.player().hunger) {
                let name = stack
                    .map(|stack| inventory::item_display_name(&self.registry, stack.item))
                    .unwrap_or_else(|| "item".to_string());
                if self.host.eat_selected() {
                    self.ui.toasts.push(format!("Ate {name}"));
                    if let Some(engine) = &mut self.audio {
                        engine.play_sfx(AudioSfx::Eat);
                    }
                    return;
                }
            }
            if self.last_place.is_some_and(|at| at.elapsed() < PLACE_COOLDOWN) {
                return;
            }
            self.last_place = Some(Instant::now());
            let Some(block) = inventory::placement_target(HostGameMode::Survival, BlockId::AIR, &self.registry, stack)
            else {
                let hint = if stack.is_none() {
                    "No item in the selected slot"
                } else {
                    "Selected item cannot be placed"
                };
                self.ui.toasts.push(hint);
                return;
            };
            let action = BlockAction {
                break_block: false,
                place_block: true,
                place: block,
            };
            let outcome = self.host.apply_action_with_outcome(&action);
            if !outcome.changed {
                self.ui.toasts.push("Cannot place there");
            } else if let Some(material) = audio_material_for(&self.registry, block)
                && let Some(engine) = &mut self.audio
            {
                engine.play_sfx(AudioSfx::BlockPlace(material));
            }
            return;
        }

        if self.last_place.is_some_and(|at| at.elapsed() < PLACE_COOLDOWN) {
            return;
        }
        self.last_place = Some(Instant::now());
        let Some(block) = inventory::placement_target(
            HostGameMode::Creative,
            self.selected_block(),
            &self.registry,
            self.host.selected_stack(),
        ) else {
            return;
        };
        let action = BlockAction {
            break_block: false,
            place_block: true,
            place: block,
        };
        let outcome = self.host.apply_action_with_outcome(&action);
        if outcome.changed
            && let Some(material) = audio_material_for(&self.registry, block)
            && let Some(engine) = &mut self.audio
        {
            engine.play_sfx(AudioSfx::BlockPlace(material));
        }
    }

    /// Number of registry-backed inventory entries (every block except air).
    fn inventory_item_count(&self) -> usize {
        inventory_blocks(&self.registry).len()
    }

    /// Updates the inventory hover from the last cursor position.
    fn update_inventory_hover(&mut self) {
        let screen = self.window_screen();
        if self.host.game_mode() == HostGameMode::Survival {
            self.ui.inventory.hovered = inventory::survival_hit_test(screen, self.ui.cursor.0, self.ui.cursor.1);
            self.ui.inventory.armor_hovered =
                inventory::survival_armor_hit_test(screen, self.ui.cursor.0, self.ui.cursor.1);
        } else {
            self.ui.inventory.armor_hovered = None;
            let item_count = self.inventory_item_count();
            self.ui.update_inventory_hover(screen, item_count);
        }
    }

    /// Equips the armor stack in a hotbar slot and reports the result.
    fn equip_armor_slot(&mut self, slot: usize) {
        let Some(stack) = self.host.inventory().get(slot).copied().flatten() else {
            return;
        };
        if self.registry.item(stack.item).armor.is_none() {
            return;
        }
        let name = inventory::item_display_name(&self.registry, stack.item);
        if self.host.equip_armor(slot) {
            self.play_ui_sound(true);
            self.ui.toasts.push_success(format!("Equipped {name}"));
        } else {
            self.ui.toasts.push_warning(format!("Could not equip {name}"));
        }
        self.update_inventory_hover();
        self.update_title();
    }

    /// Unequips one armor slot and reports the result.
    fn unequip_armor_slot(&mut self, slot: ArmorSlot) {
        let Some(stack) = self.host.equipment_slot(slot) else {
            self.ui.toasts.push(format!(
                "Empty {} slot",
                inventory::armor_slot_label(slot).to_lowercase()
            ));
            return;
        };
        let name = inventory::item_display_name(&self.registry, stack.item);
        if self.host.unequip_armor(slot) {
            self.play_ui_sound(false);
            self.ui.toasts.push(format!("Unequipped {name}"));
        } else {
            self.ui.toasts.push_warning("Inventory full - cannot unequip");
        }
        self.update_inventory_hover();
        self.update_title();
    }

    /// Moves the inventory selection with the arrow keys.
    fn move_inventory_selection(&mut self, dx: i32, dy: i32) {
        if self.host.game_mode() == HostGameMode::Survival {
            self.ui.inventory.selected =
                inventory::move_survival_selection(self.ui.inventory.selected, dx, dy, ui_state::HOTBAR_SLOTS);
        } else {
            let item_count = self.inventory_item_count();
            self.ui
                .move_inventory_selection(self.window_screen(), item_count, dx, dy);
        }
    }

    /// Scrolls the inventory (or cycles the survival hotbar row selection).
    fn scroll_inventory(&mut self, rows: i32) {
        if self.host.game_mode() == HostGameMode::Survival {
            self.ui.inventory.selected =
                inventory::cycle_survival_selection(self.ui.inventory.selected, rows, ui_state::HOTBAR_SLOTS);
            return;
        }
        let item_count = self.inventory_item_count();
        let screen = self.window_screen();
        self.ui.scroll_inventory(screen, item_count, rows);
        self.ui.update_inventory_hover(screen, item_count);
    }

    /// Mouse-wheel navigation in the inventory screen.
    fn inventory_wheel(&mut self, steps: i32) {
        if self.host.game_mode() == HostGameMode::Survival {
            self.scroll_inventory(steps);
        } else {
            self.scroll_inventory(-steps);
        }
    }

    /// Assigns the current inventory selection and returns to gameplay.
    fn assign_inventory_selection(&mut self) -> bool {
        if self.host.game_mode() == HostGameMode::Survival {
            let slot = self.ui.inventory.selected.min(ui_state::HOTBAR_SLOTS - 1);
            self.ui.inventory.selected = slot;
            if self
                .host
                .inventory()
                .get(slot)
                .copied()
                .flatten()
                .is_some_and(|stack| self.registry.item(stack.item).armor.is_some())
            {
                self.equip_armor_slot(slot);
                return true;
            }
            self.select_hotbar_slot(slot);
            self.ui.close_inventory();
            self.capture_pointer();
            self.update_title();
            return true;
        }
        if inventory::assign_selected_to_hotbar(&mut self.ui, &self.registry).is_some() {
            self.capture_pointer();
            self.update_title();
            true
        } else {
            false
        }
    }

    /// Handles a left click on the inventory: hovering a slot selects and
    /// activates it. Clicks outside keep the inventory open.
    fn inventory_click(&mut self) {
        self.update_inventory_hover();
        if self.host.game_mode() == HostGameMode::Survival
            && let Some(slot) = self.ui.inventory.armor_hovered
        {
            self.unequip_armor_slot(slot);
            return;
        }
        let Some(index) = self.ui.inventory.hovered else {
            return;
        };
        self.ui.inventory.selected = index;
        if self.host.game_mode() == HostGameMode::Survival
            && self
                .host
                .inventory()
                .get(index)
                .copied()
                .flatten()
                .is_some_and(|stack| self.registry.item(stack.item).armor.is_some())
        {
            self.equip_armor_slot(index);
            return;
        }
        self.assign_inventory_selection();
    }

    /// Moves the crafting selection by `steps` rows and keeps it on screen.
    fn move_crafting_selection(&mut self, steps: i32) {
        let rows = crafting_rows_for_host(&self.host, &self.registry);
        let screen = self.window_screen();
        self.ui.crafting.move_selection(steps, rows.len());
        self.ui.crafting.ensure_visible(screen, &rows);
    }

    /// Scrolls the crafting list by wheel notches.
    fn scroll_crafting(&mut self, steps: i32) {
        if steps == 0 {
            return;
        }
        let rows = crafting_rows_for_host(&self.host, &self.registry);
        let screen = self.window_screen();
        self.ui.crafting.scroll_by(steps, screen, &rows);
    }

    /// Moves the crafting selection by one viewport page (PageUp/PageDown).
    fn page_crafting(&mut self, direction: i32) {
        if direction == 0 {
            return;
        }
        let rows = crafting_rows_for_host(&self.host, &self.registry);
        let screen = self.window_screen();
        self.ui.crafting.page_by(direction, screen, &rows);
    }

    /// Updates the crafting hover from the last cursor position.
    fn update_crafting_hover(&mut self) {
        let rows = crafting_rows_for_host(&self.host, &self.registry);
        let screen = self.window_screen();
        self.ui.crafting.update_hover(self.ui.cursor, screen, &rows);
    }

    /// Handles a left click on a crafting row: select it and craft it.
    fn crafting_click(&mut self) {
        let rows = crafting_rows_for_host(&self.host, &self.registry);
        let screen = self.window_screen();
        if self.ui.crafting.click(self.ui.cursor, screen, &rows).is_some() {
            self.craft_selected();
        }
    }
}

/// Draws the sectioned two-column F3 debug overlay.
///
/// Geometry comes from [`ui_state::debug_overlay_layout`], which guarantees a
/// line step with breathing room, visible section gaps, and two columns that
/// either sit side by side or stack instead of overlapping. The hardware
/// column is drawn dimmer and one step smaller than the statistics column.
fn draw_debug_overlay(frame: &mut UiFrame, stats: &DebugStats, screen: [f32; 2]) {
    let layout = ui_state::debug_overlay_layout(screen, stats);
    let left_lines = if layout.compact {
        stats.compact_left_lines()
    } else {
        stats.left_lines()
    };
    let right_lines = stats.right_lines();

    draw_debug_column(frame, &layout.left, &left_lines, true);
    if let Some(right) = &layout.right {
        draw_debug_column(frame, right, &right_lines, false);
    }
    // Header separator under the version/FPS group.
    let left_panel = layout.left.panel;
    if !layout.left.lines.is_empty() {
        frame.rect(
            left_panel.x + ui_state::DEBUG_PADDING,
            layout.header_separator_y,
            left_panel.w - ui_state::DEBUG_PADDING * 2.0,
            1.0,
            [0.52, 0.59, 0.69, 0.9],
        );
    }
    let footer_text = "F3 debug overlay   F1 HUD   F2 screenshot   Esc menu";
    let footer_width = frame.text_width(ui_state::DEBUG_FOOTER_SCALE, footer_text);
    frame.rect(
        ui_state::DEBUG_MARGIN,
        layout.footer_y - 3.0,
        footer_width + 8.0,
        ob_render::line_height(ui_state::DEBUG_FOOTER_SCALE) + 6.0,
        [0.03, 0.04, 0.07, 0.97],
    );
    frame.text(
        ui_state::DEBUG_MARGIN + 4.0,
        layout.footer_y,
        ui_state::DEBUG_FOOTER_SCALE,
        [0.86, 0.91, 0.98, 0.95],
        footer_text,
    );
    if layout.columns == DebugColumns::Stacked {
        // Leave a trace in the logs so layout regressions at narrow sizes are
        // obvious during manual verification.
        log::debug!("F3 overlay stacked at {}x{}", screen[0], screen[1]);
    }
}

/// Draws one F3 column panel and its text lines.
fn draw_debug_column(frame: &mut UiFrame, column: &ui_state::DebugColumnLayout, lines: &[String], primary: bool) {
    frame.rect(
        column.panel.x,
        column.panel.y,
        column.panel.w,
        column.panel.h,
        [0.03, 0.04, 0.07, 0.97],
    );
    for placement in &column.lines {
        let Some(text) = lines.get(placement.index) else {
            continue;
        };
        let color = if primary && placement.index == 0 {
            [0.98, 0.82, 0.45, 1.0]
        } else if primary {
            [0.93, 0.96, 0.99, 1.0]
        } else {
            [0.78, 0.84, 0.92, 1.0]
        };
        frame.text(placement.x, placement.y, column.scale, color, text);
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
        let (mut settings, data_dir) = effective_settings(&self.options);
        persist_texture_pack_choice(&self.options, &mut settings, &data_dir);
        persist_game_mode_choice(&self.options, &mut settings, &data_dir);
        persist_ui_font_choice(&self.options, &mut settings, &data_dir);
        // Continue on the title screen points at the most recently opened
        // single-player world, including worlds started from the CLI.
        menu_app::remember_last_world_choice(&data_dir, &self.options.world);
        settings.last_world = Some(self.options.world.clone());
        let (renderer, texture_source, font_source) =
            match create_window_renderer(Arc::clone(&window), &self.options, &settings, &data_dir) {
                Ok(triple) => triple,
                Err(err) => {
                    log::error!("{err}");
                    log::error!(
                        "OpenBlock requires a working Vulkan driver (Fedora: mesa-vulkan-drivers or the NVIDIA driver)."
                    );
                    event_loop.exit();
                    return;
                }
            };
        log_texture_source(&texture_source);
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
        let initial_fov = settings.fov_degrees;
        // At 32 chunks the full-detail radius is capped at 10; smaller render
        // distances keep every column at full detail. The initial load waits
        // for two fewer chunks than that so the player can start before the
        // merged far batches have all finished.
        let lod_config = LodConfig::for_render_distance(settings.render_distance_chunks);
        let initial_target_radius =
            (lod_config.full_detail_radius_chunks - 2).clamp(2, settings.render_distance_chunks.max(2));
        let mut ui = UiState::from_settings(settings);
        ui.active_texture_pack = active_texture_pack_label(&texture_source);
        ui.active_ui_font_name = renderer.ui_font_name().to_string();
        ui.active_ui_font_source = font_source.display_label();
        ui.ui_scale_percent = resolved_ui_scale_percent(ui.settings.ui_scale_percent, window_size.1);
        let applied_render_distance = ui.settings.render_distance_chunks;
        let mut host = match Host::open(Arc::clone(&registry), config) {
            Ok(host) => host,
            Err(err) => {
                log::error!("failed to open world: {err}");
                event_loop.exit();
                return;
            }
        };
        host.set_game_mode(map_game_mode(ui.settings.game_mode));
        ui.hotbar_selected = host.hotbar_selection();
        log::info!("game mode: {}", host.game_mode().name());
        host.ensure_streaming(INITIAL_RADIUS as usize * 2);
        let (eye, yaw, pitch) = {
            let player = host.player();
            (player.eye(), player.yaw, player.pitch)
        };
        let audio_enabled_before_mute = if self.options.mute {
            Some(load_settings(&data_dir).audio_enabled)
        } else {
            None
        };
        let (audio, audio_status) = open_audio(&self.options, &ui.settings);
        let was_in_water = player_in_water(&host, &registry);
        let last_audio_health = host.player().health;
        let mut state = GameState {
            window,
            renderer,
            texture_source,
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
            current_fov: initial_fov,
            damage_flash: DamageFlash::default(),
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
            lod: LodState::new(lod_config),
            initial_load: true,
            initial_target_radius,
            loading_progress: 0.0,
            loading_detail: String::new(),
            loading_started: Instant::now(),
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
            audio,
            audio_status,
            last_audio_health,
            footstep_distance: 0.0,
            was_in_water,
            audio_enabled_before_mute,
            audio_setting_touched: false,
            container: None,
            mob_render: MobRenderCache::new(),
            attack_ready: None,
            camera_shake: 0.0,
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

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // The menu router can swap handlers inside the same event loop; ignore
        // late events addressed to the window that has already been dropped.
        if state.window.id() != window_id {
            return;
        }
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
                state.refresh_ui_scale();
            }
            WindowEvent::RedrawRequested => {
                state.frame();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                if state.ui.screen == Screen::Settings {
                    // Rebinding capture owns every key while it is active, so a
                    // new binding can never trigger gameplay or UI navigation.
                    if state.ui.bind_capture.is_some() {
                        if pressed {
                            match code {
                                KeyCode::Escape => state.ui.cancel_bind(),
                                KeyCode::Backspace | KeyCode::Delete => {
                                    if state.ui.clear_binding().is_some() {
                                        state.persist_settings();
                                    }
                                }
                                _ => {
                                    if let Some(key) = bind_key_for_code(code)
                                        && state.ui.apply_bind(key).is_ok()
                                    {
                                        state.persist_settings();
                                    }
                                }
                            }
                        }
                        state.update_title();
                        return;
                    }
                    if pressed
                        && matches!(code, KeyCode::Delete | KeyCode::Backspace)
                        && state.ui.clear_binding().is_some()
                    {
                        state.persist_settings();
                        state.update_title();
                        return;
                    }
                    match code {
                        KeyCode::Escape if pressed => state.handle_escape(),
                        KeyCode::Tab if pressed => {
                            if state.keys[5] {
                                state.ui.settings_tab_prev();
                            } else {
                                state.ui.settings_tab_next();
                            }
                        }
                        KeyCode::ArrowUp if pressed => state.ui.settings_row_up(),
                        KeyCode::ArrowDown if pressed => state.ui.settings_row_down(),
                        KeyCode::ArrowLeft if pressed => {
                            if let Some(row) = state.ui.settings_adjust(-1) {
                                state.apply_settings_row(row);
                            }
                        }
                        KeyCode::ArrowRight if pressed => {
                            if let Some(row) = state.ui.settings_adjust(1) {
                                state.apply_settings_row(row);
                            }
                        }
                        KeyCode::Enter if pressed => {
                            if let Some(action) = state.ui.settings_activate() {
                                state.play_ui_sound(matches!(
                                    action,
                                    SettingsAction::Resume | SettingsAction::SaveAndQuit
                                ));
                                if state.handle_settings_action(action) {
                                    state.persist_settings();
                                    event_loop.exit();
                                }
                            }
                        }
                        _ => {}
                    }
                    if pressed {
                        state.update_title();
                    }
                    return;
                }

                // Gameplay and menu actions are resolved through the persisted
                // keybind model rather than hard-coded physical keys.
                if let Some(action) = bind_key_for_code(code).and_then(|key| state.ui.settings.keybinds.action_for(key))
                {
                    if let Some(slot) = movement_slot(action) {
                        state.keys[slot] = pressed;
                    } else if pressed {
                        match action {
                            Action::Fly => {
                                if state.ui.screen == Screen::Playing && !state.host.is_dead() {
                                    state.fly_toggle = true;
                                }
                            }
                            Action::Inventory => match state.ui.screen {
                                Screen::Playing if !state.host.is_dead() => state.open_inventory(),
                                Screen::Inventory => state.close_inventory(),
                                Screen::Crafting => state.close_crafting(),
                                Screen::Furnace | Screen::Chest => state.close_container(),
                                Screen::Settings | Screen::Death | Screen::Connecting => {}
                                Screen::Playing => {}
                            },
                            Action::Crafting => state.toggle_crafting(),
                            Action::Pause => state.handle_escape(),
                            Action::DebugOverlay => {
                                if overlay_shortcuts_allowed(state.ui.screen) {
                                    state.ui.toggle_debug_overlay();
                                    state.persist_settings();
                                }
                            }
                            Action::ToggleHud => {
                                if overlay_shortcuts_allowed(state.ui.screen) {
                                    state.ui.toggle_hud();
                                    state.persist_settings();
                                }
                            }
                            Action::Screenshot => {
                                if state.ui.screen == Screen::Playing && !state.host.is_dead() {
                                    state.screenshot_requested = true;
                                }
                            }
                            Action::Chat | Action::Mine | Action::Use => {}
                            Action::MoveForward
                            | Action::MoveBack
                            | Action::MoveLeft
                            | Action::MoveRight
                            | Action::Jump
                            | Action::Sneak
                            | Action::Sprint => {}
                        }
                    }
                    if pressed {
                        state.update_title();
                    }
                    return;
                }

                // UI-only navigation remains available regardless of bindings:
                // arrows drive menus, Enter activates and digits select hotbar
                // slots. Escape still backs out of a modal screen when Pause has
                // been rebound away from it.
                match code {
                    KeyCode::Escape if pressed && state.ui.screen != Screen::Playing => {
                        state.handle_escape();
                    }
                    KeyCode::ArrowUp if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(-1),
                        Screen::Inventory => state.move_inventory_selection(0, -1),
                        Screen::Crafting => state.move_crafting_selection(-1),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(0, -1),
                        Screen::Playing | Screen::Settings | Screen::Connecting => {}
                    },
                    KeyCode::ArrowDown if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(1),
                        Screen::Inventory => state.move_inventory_selection(0, 1),
                        Screen::Crafting => state.move_crafting_selection(1),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(0, 1),
                        Screen::Playing | Screen::Settings | Screen::Connecting => {}
                    },
                    KeyCode::PageUp if pressed && state.ui.screen == Screen::Crafting => {
                        state.page_crafting(-1);
                    }
                    KeyCode::PageDown if pressed && state.ui.screen == Screen::Crafting => {
                        state.page_crafting(1);
                    }
                    KeyCode::ArrowLeft if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(-1),
                        Screen::Inventory => state.move_inventory_selection(-1, 0),
                        Screen::Crafting => state.move_crafting_selection(-1),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(-1, 0),
                        Screen::Playing | Screen::Settings | Screen::Connecting => {}
                    },
                    KeyCode::ArrowRight if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(1),
                        Screen::Inventory => state.move_inventory_selection(1, 0),
                        Screen::Crafting => state.move_crafting_selection(1),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(1, 0),
                        Screen::Playing | Screen::Settings | Screen::Connecting => {}
                    },
                    KeyCode::Enter if pressed => match state.ui.screen {
                        Screen::Death => {
                            if let Some(action) = state.ui.activate_death()
                                && state.handle_death_action(action)
                            {
                                state.persist_settings();
                                event_loop.exit();
                            }
                        }
                        Screen::Inventory => {
                            state.assign_inventory_selection();
                        }
                        Screen::Crafting => state.craft_selected(),
                        Screen::Furnace | Screen::Chest => state.activate_container_slot(),
                        Screen::Playing | Screen::Settings | Screen::Connecting => {}
                    },
                    KeyCode::Digit1
                    | KeyCode::Digit2
                    | KeyCode::Digit3
                    | KeyCode::Digit4
                    | KeyCode::Digit5
                    | KeyCode::Digit6
                    | KeyCode::Digit7
                    | KeyCode::Digit8
                    | KeyCode::Digit9
                        if pressed && state.ui.screen == Screen::Playing && !state.host.is_dead() =>
                    {
                        if let Some(slot) = digit_slot(code) {
                            state.select_hotbar_slot(slot);
                        }
                    }
                    _ => {}
                }
                if pressed {
                    state.update_title();
                }
            }
            WindowEvent::MouseInput {
                state: element, button, ..
            } => {
                if element == ElementState::Released {
                    state.cancel_mining();
                }
                if element != ElementState::Pressed {
                    return;
                }
                if state.ui.screen == Screen::Settings && state.ui.bind_capture.is_some() {
                    if let Some(key) = bind_key_for_mouse(button)
                        && state.ui.apply_bind(key).is_ok()
                    {
                        state.persist_settings();
                    }
                    state.update_title();
                    return;
                }
                match state.ui.screen {
                    Screen::Death => {
                        if button == MouseButton::Left {
                            let screen = state.window_screen();
                            if let Some(action) = state.ui.click_death(screen)
                                && state.handle_death_action(action)
                            {
                                state.persist_settings();
                                event_loop.exit();
                            }
                        }
                        return;
                    }
                    Screen::Inventory => {
                        if button == MouseButton::Left {
                            state.inventory_click();
                        }
                        return;
                    }
                    Screen::Settings => {
                        if button == MouseButton::Left {
                            let screen = state.window_screen();
                            let action = state.ui.settings_click(screen);
                            let interactive = state.ui.settings_hovered_tab.is_some()
                                || state.ui.settings_hovered_row.is_some()
                                || state.ui.settings_hovered_button.is_some();
                            if interactive {
                                state.play_ui_sound(matches!(
                                    action,
                                    Some(SettingsAction::Resume | SettingsAction::SaveAndQuit)
                                ));
                            }
                            if let Some(action) = action
                                && state.handle_settings_action(action)
                            {
                                state.persist_settings();
                                event_loop.exit();
                            }
                        }
                        return;
                    }
                    Screen::Crafting => {
                        if button == MouseButton::Left {
                            state.crafting_click();
                        }
                        return;
                    }
                    Screen::Furnace | Screen::Chest => {
                        match button {
                            MouseButton::Left => state.container_click(ClickButton::Left),
                            MouseButton::Right => state.container_click(ClickButton::Right),
                            _ => {}
                        }
                        return;
                    }
                    Screen::Connecting => return,
                    Screen::Playing => {}
                }
                if state.host.is_dead() {
                    state.begin_death_screen();
                    return;
                }
                if !state.captured {
                    state.capture_pointer();
                    return;
                }
                match bind_key_for_mouse(button).and_then(|key| state.ui.settings.keybinds.action_for(key)) {
                    Some(Action::Mine) => state.start_mining(),
                    Some(Action::Use) => state.place_block(),
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.ui.cursor = state.ui_point((position.x as f32, position.y as f32));
                match state.ui.screen {
                    Screen::Inventory => state.update_inventory_hover(),
                    Screen::Settings => {
                        let screen = state.window_screen();
                        state.ui.settings_hover(screen);
                    }
                    Screen::Crafting => state.update_crafting_hover(),
                    Screen::Furnace | Screen::Chest => {}
                    Screen::Death => {
                        let screen = state.window_screen();
                        state.ui.update_death_hover(screen);
                    }
                    Screen::Playing | Screen::Connecting => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 40.0).round() as i32,
                };
                match state.ui.screen {
                    Screen::Playing if steps != 0 && !state.host.is_dead() => {
                        // Scrolling down moves to the next hotbar slot (positive
                        // steps), matching the usual block-game convention.
                        state.cycle_hotbar_slots(-steps);
                    }
                    Screen::Inventory if steps != 0 => {
                        state.inventory_wheel(steps);
                    }
                    Screen::Death if steps != 0 => {
                        state.ui.move_death_selection(-steps.signum());
                    }
                    Screen::Settings if steps != 0 => {
                        if let Some(row) = state.ui.settings_adjust(steps.signum()) {
                            state.apply_settings_row(row);
                        }
                    }
                    Screen::Crafting if steps != 0 => {
                        state.scroll_crafting(steps);
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
        if !state.captured || state.ui.screen != Screen::Playing || state.host.is_dead() {
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

/// F1/F3 overlay shortcuts are disabled while the settings screen owns input.
fn overlay_shortcuts_allowed(screen: Screen) -> bool {
    screen != Screen::Settings
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

/// Maps a physical winit key to the serialisable keybind set.
///
/// Only keys present in [`KeyName`] are mappable, so a persisted binding can
/// never refer to a key the game cannot recognise. `None` also means the key is
/// reserved for text input or UI navigation (for example an unmapped media key).
fn key_name_for_code(code: KeyCode) -> Option<KeyName> {
    Some(match code {
        KeyCode::KeyA => KeyName::KeyA,
        KeyCode::KeyB => KeyName::KeyB,
        KeyCode::KeyC => KeyName::KeyC,
        KeyCode::KeyD => KeyName::KeyD,
        KeyCode::KeyE => KeyName::KeyE,
        KeyCode::KeyF => KeyName::KeyF,
        KeyCode::KeyG => KeyName::KeyG,
        KeyCode::KeyH => KeyName::KeyH,
        KeyCode::KeyI => KeyName::KeyI,
        KeyCode::KeyJ => KeyName::KeyJ,
        KeyCode::KeyK => KeyName::KeyK,
        KeyCode::KeyL => KeyName::KeyL,
        KeyCode::KeyM => KeyName::KeyM,
        KeyCode::KeyN => KeyName::KeyN,
        KeyCode::KeyO => KeyName::KeyO,
        KeyCode::KeyP => KeyName::KeyP,
        KeyCode::KeyQ => KeyName::KeyQ,
        KeyCode::KeyR => KeyName::KeyR,
        KeyCode::KeyS => KeyName::KeyS,
        KeyCode::KeyT => KeyName::KeyT,
        KeyCode::KeyU => KeyName::KeyU,
        KeyCode::KeyV => KeyName::KeyV,
        KeyCode::KeyW => KeyName::KeyW,
        KeyCode::KeyX => KeyName::KeyX,
        KeyCode::KeyY => KeyName::KeyY,
        KeyCode::KeyZ => KeyName::KeyZ,
        KeyCode::Digit0 => KeyName::Digit0,
        KeyCode::Digit1 => KeyName::Digit1,
        KeyCode::Digit2 => KeyName::Digit2,
        KeyCode::Digit3 => KeyName::Digit3,
        KeyCode::Digit4 => KeyName::Digit4,
        KeyCode::Digit5 => KeyName::Digit5,
        KeyCode::Digit6 => KeyName::Digit6,
        KeyCode::Digit7 => KeyName::Digit7,
        KeyCode::Digit8 => KeyName::Digit8,
        KeyCode::Digit9 => KeyName::Digit9,
        KeyCode::F1 => KeyName::F1,
        KeyCode::F2 => KeyName::F2,
        KeyCode::F3 => KeyName::F3,
        KeyCode::F4 => KeyName::F4,
        KeyCode::F5 => KeyName::F5,
        KeyCode::F6 => KeyName::F6,
        KeyCode::F7 => KeyName::F7,
        KeyCode::F8 => KeyName::F8,
        KeyCode::F9 => KeyName::F9,
        KeyCode::F10 => KeyName::F10,
        KeyCode::F11 => KeyName::F11,
        KeyCode::F12 => KeyName::F12,
        KeyCode::ArrowUp => KeyName::ArrowUp,
        KeyCode::ArrowDown => KeyName::ArrowDown,
        KeyCode::ArrowLeft => KeyName::ArrowLeft,
        KeyCode::ArrowRight => KeyName::ArrowRight,
        KeyCode::Space => KeyName::Space,
        KeyCode::Enter => KeyName::Enter,
        KeyCode::Tab => KeyName::Tab,
        KeyCode::Escape => KeyName::Escape,
        KeyCode::Backspace => KeyName::Backspace,
        KeyCode::Delete => KeyName::Delete,
        KeyCode::Insert => KeyName::Insert,
        KeyCode::Home => KeyName::Home,
        KeyCode::End => KeyName::End,
        KeyCode::PageUp => KeyName::PageUp,
        KeyCode::PageDown => KeyName::PageDown,
        KeyCode::ShiftLeft => KeyName::ShiftLeft,
        KeyCode::ShiftRight => KeyName::ShiftRight,
        KeyCode::ControlLeft => KeyName::ControlLeft,
        KeyCode::ControlRight => KeyName::ControlRight,
        KeyCode::AltLeft => KeyName::AltLeft,
        KeyCode::AltRight => KeyName::AltRight,
        KeyCode::CapsLock => KeyName::CapsLock,
        KeyCode::Minus => KeyName::Minus,
        KeyCode::Equal => KeyName::Equal,
        KeyCode::BracketLeft => KeyName::BracketLeft,
        KeyCode::BracketRight => KeyName::BracketRight,
        KeyCode::Backslash => KeyName::Backslash,
        KeyCode::Semicolon => KeyName::Semicolon,
        KeyCode::Quote => KeyName::Quote,
        KeyCode::Comma => KeyName::Comma,
        KeyCode::Period => KeyName::Period,
        KeyCode::Slash => KeyName::Slash,
        KeyCode::Backquote => KeyName::Backquote,
        KeyCode::Numpad0 => KeyName::Numpad0,
        KeyCode::Numpad1 => KeyName::Numpad1,
        KeyCode::Numpad2 => KeyName::Numpad2,
        KeyCode::Numpad3 => KeyName::Numpad3,
        KeyCode::Numpad4 => KeyName::Numpad4,
        KeyCode::Numpad5 => KeyName::Numpad5,
        KeyCode::Numpad6 => KeyName::Numpad6,
        KeyCode::Numpad7 => KeyName::Numpad7,
        KeyCode::Numpad8 => KeyName::Numpad8,
        KeyCode::Numpad9 => KeyName::Numpad9,
        KeyCode::NumpadAdd => KeyName::NumpadAdd,
        KeyCode::NumpadSubtract => KeyName::NumpadSubtract,
        KeyCode::NumpadMultiply => KeyName::NumpadMultiply,
        KeyCode::NumpadDivide => KeyName::NumpadDivide,
        KeyCode::NumpadDecimal => KeyName::NumpadDecimal,
        KeyCode::NumpadEnter => KeyName::NumpadEnter,
        _ => return None,
    })
}

/// Converts a physical key into a bindable input.
fn bind_key_for_code(code: KeyCode) -> Option<BindKey> {
    key_name_for_code(code).map(BindKey::Key)
}

/// Converts a mouse button into a bindable input.
fn bind_key_for_mouse(button: MouseButton) -> Option<BindKey> {
    match button {
        MouseButton::Left => Some(BindKey::Mouse(ob_core::config::MouseButton::Left)),
        MouseButton::Right => Some(BindKey::Mouse(ob_core::config::MouseButton::Right)),
        MouseButton::Middle => Some(BindKey::Mouse(ob_core::config::MouseButton::Middle)),
        _ => None,
    }
}

/// Input-array slot for movement actions; non-movement actions return `None`.
fn movement_slot(action: Action) -> Option<usize> {
    match action {
        Action::MoveForward => Some(0),
        Action::MoveBack => Some(1),
        Action::MoveLeft => Some(2),
        Action::MoveRight => Some(3),
        Action::Jump => Some(4),
        Action::Sneak => Some(5),
        Action::Sprint => Some(6),
        _ => None,
    }
}

/// Target vertical FOV for the current sprint state.
///
/// Reduced motion disables the sprint FOV pump entirely; this is a pure helper
/// so the accessibility behaviour is testable without a window.
fn target_fov(settings: &Settings, sprinting: bool) -> f32 {
    if settings.reduced_motion || !sprinting {
        settings.fov_degrees
    } else {
        settings.fov_degrees * 1.10
    }
}

/// Opens the audio engine unless `--no-audio` was given.
///
/// A missing device is an expected state: it logs exactly one line and returns
/// `None`, and every later sound call is skipped. This is what keeps headless
/// screenshots and CI hosts independent of the audio stack.
fn open_audio(options: &ClientOptions, settings: &Settings) -> (Option<AudioEngine>, String) {
    if options.no_audio {
        log::info!("audio: disabled by --no-audio");
        return (None, "off (--no-audio)".to_string());
    }
    match AudioEngine::new() {
        Ok(mut engine) => {
            apply_audio_settings(&mut engine, settings);
            let status = engine.status().description();
            log::info!(
                "audio: {status} ({} clips, {:.1} ms synthesis)",
                engine.status().clip_count,
                engine.status().synthesis_ms,
            );
            (Some(engine), status)
        }
        Err(error) => {
            log::info!("audio: unavailable ({error}); continuing silently");
            (None, format!("off ({error})"))
        }
    }
}

/// Applies every persisted audio setting to a live engine.
fn apply_audio_settings(engine: &mut AudioEngine, settings: &Settings) {
    engine.set_master_volume(settings.master_volume);
    engine.set_sfx_volume(settings.sfx_volume);
    engine.set_music_volume(settings.music_volume);
    engine.set_enabled(settings.audio_enabled);
}

/// Whether the player's feet or waist are inside a fluid block.
fn player_in_water(host: &Host, registry: &Registry) -> bool {
    let player = host.player();
    let feet = BlockPos::new(
        player.pos.x.floor() as i32,
        player.pos.y.floor() as i32,
        player.pos.z.floor() as i32,
    );
    let waist = BlockPos::new(feet.x, feet.y + 1, feet.z);
    [feet, waist].into_iter().any(|position| {
        let block = host.world().get_block(position);
        registry.block(block).fluid
    })
}

/// Maps a block id to the procedural material family.
///
/// Water and air have no material; callers use the `None` to skip block audio
/// (splashes are handled separately).
fn audio_material_for(registry: &Registry, block: BlockId) -> Option<AudioMaterial> {
    let block_type = registry.block(block);
    if block_type.fluid || block_type.id == "openblock:air" {
        return None;
    }
    Some(match block_type.id {
        "openblock:grass" | "openblock:leaves" => AudioMaterial::Grass,
        "openblock:dirt" => AudioMaterial::Dirt,
        "openblock:stone" | "openblock:cobblestone" | "openblock:bedrock" => AudioMaterial::Stone,
        "openblock:sand" => AudioMaterial::Sand,
        "openblock:snow" => AudioMaterial::Snow,
        "openblock:log" | "openblock:planks" | "openblock:crafting_table" => AudioMaterial::Wood,
        "openblock:gravel" => AudioMaterial::Gravel,
        _ => AudioMaterial::Stone,
    })
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
    if let Some(mode) = options.game_mode {
        settings.game_mode = mode;
    }
    if let Some(percent) = options.ui_scale {
        settings.ui_scale_percent = percent;
    }
    if options.mute {
        settings.audio_enabled = false;
    }
    if options.screenshot_dead || options.screenshot_crafting || options.screenshot_debug || options.screenshot_items {
        settings.game_mode = SettingsGameMode::Survival;
    }
    settings.clamp();
    (settings, data_dir)
}

/// Maps the persisted mode to the authoritative host mode.
fn map_game_mode(mode: SettingsGameMode) -> HostGameMode {
    match mode {
        SettingsGameMode::Survival => HostGameMode::Survival,
        SettingsGameMode::Creative => HostGameMode::Creative,
    }
}

/// Persists an explicit `--survival`/`--creative` choice without saving
/// unrelated CLI overrides.
fn persist_game_mode_choice(options: &ClientOptions, settings: &mut Settings, data_dir: &Path) {
    let Some(mode) = options.game_mode else {
        return;
    };
    settings.game_mode = mode;
    let path = ob_core::config::settings_path(data_dir);
    let mut disk_settings = load_settings(data_dir);
    if disk_settings.game_mode == mode && path.exists() {
        return;
    }
    disk_settings.game_mode = mode;
    match save_settings(data_dir, &disk_settings) {
        Ok(()) => log::info!(
            "saved game mode {} to {}",
            mode.name(),
            ob_core::config::settings_path(data_dir).display()
        ),
        Err(err) => log::warn!("could not save game mode: {err}"),
    }
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

/// Resolves which texture source this run should use.
///
/// Precedence: explicit `--texture-pack`/`--resource-pack` wins, then
/// `--no-texture-pack` forces the procedural style, then a persisted pack path,
/// and finally the persisted procedural style.
pub fn resolve_texture_source(options: &ClientOptions, settings: &Settings) -> TextureSource {
    if let Some(path) = &options.texture_pack {
        return TextureSource::ResourcePack(path.clone());
    }
    if options.no_texture_pack {
        return TextureSource::Style(render_texture_style(settings.texture_style));
    }
    if let Some(path) = settings.texture_pack.as_deref().filter(|path| !path.is_empty()) {
        return TextureSource::ResourcePack(PathBuf::from(path));
    }
    TextureSource::Style(render_texture_style(settings.texture_style))
}

/// What the texture-pack settings row does when activated.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TextureToggle {
    /// Re-enable the persisted resource pack.
    Enable(PathBuf),
    /// Return to the procedural style and clear the persisted pack.
    Disable,
    /// No pack is active and none is saved.
    Noop,
}

/// Pure toggle decision used by the settings row and its tests.
fn texture_toggle_target(source: &TextureSource, settings: &Settings) -> TextureToggle {
    match source {
        TextureSource::ResourcePack(_) => TextureToggle::Disable,
        TextureSource::Style(_) => match settings.texture_pack.as_deref().filter(|path| !path.is_empty()) {
            Some(path) => TextureToggle::Enable(PathBuf::from(path)),
            None => TextureToggle::Noop,
        },
    }
}

/// Persists an explicit CLI pack choice without saving unrelated CLI overrides.
///
/// The in-memory settings are updated immediately so the rest of startup sees
/// the selection; the save itself starts from the settings on disk so a
/// `--render-distance` or `--texture-style` override is not silently persisted.
fn persist_texture_pack_choice(options: &ClientOptions, settings: &mut Settings, data_dir: &Path) {
    let selection = if let Some(path) = &options.texture_pack {
        Some(Some(path.to_string_lossy().into_owned()))
    } else if options.no_texture_pack {
        Some(None)
    } else {
        None
    };
    let Some(selection) = selection else {
        return;
    };
    settings.texture_pack = selection.clone();

    let mut disk_settings = load_settings(data_dir);
    if disk_settings.texture_pack == selection {
        return;
    }
    disk_settings.texture_pack = selection;
    match save_settings(data_dir, &disk_settings) {
        Ok(()) => log::info!(
            "saved texture pack selection to {}",
            ob_core::config::settings_path(data_dir).display()
        ),
        Err(err) => log::warn!("could not save texture pack selection: {err}"),
    }
}

/// Persists an explicit `--ui-font`/`--default-ui-font` choice.
///
/// Like `--texture-pack`, the in-memory settings are updated immediately so the
/// rest of startup sees the selection, while the save starts from the settings
/// on disk so unrelated CLI overrides are not persisted. A bad explicit path is
/// still stored (for the user to fix with `--default-ui-font`); the renderer
/// warns and falls back to Monocraft instead of refusing to start.
fn persist_ui_font_choice(options: &ClientOptions, settings: &mut Settings, data_dir: &Path) {
    let selection = if options.default_ui_font {
        Some(None)
    } else {
        options
            .ui_font
            .as_ref()
            .map(|path| Some(path.to_string_lossy().into_owned()))
    };
    let Some(selection) = selection else {
        return;
    };
    settings.ui_font_path = selection.clone();

    let mut disk_settings = load_settings(data_dir);
    if disk_settings.ui_font_path == selection {
        return;
    }
    disk_settings.ui_font_path = selection;
    match save_settings(data_dir, &disk_settings) {
        Ok(()) => log::info!(
            "saved UI font selection to {}",
            ob_core::config::settings_path(data_dir).display()
        ),
        Err(err) => log::warn!("could not save UI font selection: {err}"),
    }
}

/// Clears the saved UI-font override and restores the embedded Monocraft atlas.
pub(crate) fn reset_ui_font<T: ui_font::UiFontTarget>(
    target: &mut T,
    settings: &mut Settings,
) -> ui_font::UiFontSource {
    settings.ui_font_path = None;
    ui_font::install_ui_font(target, &ui_font::ResolvedUiFont::builtin())
}

/// Re-scans the data/system font folders and applies the current choice.
pub(crate) fn reload_ui_font<T: ui_font::UiFontTarget>(
    target: &mut T,
    settings: &Settings,
    data_dir: &Path,
) -> ui_font::UiFontSource {
    let resolved = ui_font::resolve_ui_font_for_settings(settings, data_dir);
    ui_font::install_ui_font(target, &resolved)
}

/// Human-readable description of a texture source and its texel resolution.
fn describe_texture_source(source: &TextureSource, resolution: u32) -> String {
    format!("{} {}px", source.label(), resolution)
}

/// Label shown by the settings row for the active source, if it is a pack.
fn active_texture_pack_label(source: &TextureSource) -> Option<String> {
    match source {
        TextureSource::ResourcePack(_) => Some(source.label()),
        TextureSource::Style(_) => None,
    }
}

/// Logs the active source at startup.
fn log_texture_source(source: &TextureSource) {
    match source {
        TextureSource::ResourcePack(path) => {
            log::info!("textures: resource pack {} ({})", path.display(), source.label());
        }
        TextureSource::Style(_) => log::info!("textures: {}", source.label()),
    }
}

/// Builds the offscreen renderer, falling back to the procedural style when a
/// requested resource pack cannot be loaded, then installs the resolved UI font.
fn create_offscreen_renderer(
    options: &ClientOptions,
    settings: &Settings,
    data_dir: &Path,
    font_override: Option<&ui_font::ResolvedUiFont>,
) -> Result<(OffscreenRenderer, TextureSource, ui_font::UiFontSource), ob_render::RenderError> {
    let source = resolve_texture_source(options, settings);
    let (width, height) = screenshot_dimensions(options);
    let (mut renderer, active_source) = match OffscreenRenderer::new_with_source(width, height, source.clone()) {
        Ok(renderer) => (renderer, source),
        Err(ob_render::RenderError::Texture(reason)) => {
            if let TextureSource::ResourcePack(path) = &source {
                log::warn!(
                    "could not load texture pack {}: {reason}; falling back to the built-in {} style",
                    path.display(),
                    settings.texture_style.name()
                );
                let style = render_texture_style(settings.texture_style);
                (
                    OffscreenRenderer::new_with_style(width, height, style)?,
                    TextureSource::Style(style),
                )
            } else {
                return Err(ob_render::RenderError::Texture(reason));
            }
        }
        Err(err) => return Err(err),
    };
    let resolved = font_override
        .cloned()
        .unwrap_or_else(|| ui_font::resolve_ui_font(options, settings, data_dir));
    let font_source = ui_font::install_ui_font(&mut renderer, &resolved);
    Ok((renderer, active_source, font_source))
}

/// Builds the windowed renderer, falling back to the procedural style when a
/// requested resource pack cannot be loaded, then installs the resolved UI font.
fn create_window_renderer(
    window: Arc<Window>,
    options: &ClientOptions,
    settings: &Settings,
    data_dir: &Path,
) -> Result<(Renderer, TextureSource, ui_font::UiFontSource), ob_render::RenderError> {
    let source = resolve_texture_source(options, settings);
    let (mut renderer, active_source) = match Renderer::new_with_source(Arc::clone(&window), source.clone()) {
        Ok(renderer) => (renderer, source),
        Err(ob_render::RenderError::Texture(reason)) => {
            if let TextureSource::ResourcePack(path) = &source {
                log::warn!(
                    "could not load texture pack {}: {reason}; falling back to the built-in {} style",
                    path.display(),
                    settings.texture_style.name()
                );
                let style = render_texture_style(settings.texture_style);
                (Renderer::new_with_style(window, style)?, TextureSource::Style(style))
            } else {
                return Err(ob_render::RenderError::Texture(reason));
            }
        }
        Err(err) => return Err(err),
    };
    let resolved = ui_font::resolve_ui_font(options, settings, data_dir);
    let font_source = ui_font::install_ui_font(&mut renderer, &resolved);
    Ok((renderer, active_source, font_source))
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
    let mut saw_world = false;
    let mut saw_seed = false;
    for (name, value) in flags {
        match name.as_str() {
            "seed" => {
                options.seed = value.parse().map_err(|_| format!("invalid --seed value '{value}'"))?;
                saw_seed = true;
                options.saw_seed = true;
            }
            "world" => {
                options.world = value.clone();
                saw_world = true;
                options.saw_world = true;
            }
            "connect" => {
                if value.is_empty() {
                    return Err("--connect requires HOST:PORT".into());
                }
                options.connect = Some(value.clone());
            }
            "server-password" => {
                return Err("--server-password is not supported by the interim protocol".into());
            }
            "name" => {
                if value.is_empty() {
                    return Err("--name requires a player name".into());
                }
                if let Err(error) = ob_protocol::validate_player_name(value) {
                    return Err(format!("invalid --name value '{value}': {error}"));
                }
                options.name = Some(value.clone());
            }
            "data-dir" => {
                options.data_dir = Some(value.into());
                options.saw_data_dir = true;
            }
            "render-distance" => {
                options.render_distance_chunks = value
                    .parse()
                    .map_err(|_| format!("invalid --render-distance value '{value}'"))?;
                if !(2..=ob_core::config::MAX_RENDER_DISTANCE).contains(&options.render_distance_chunks) {
                    return Err(format!(
                        "--render-distance must be between 2 and {} chunks",
                        ob_core::config::MAX_RENDER_DISTANCE
                    ));
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
            "texture-pack" | "resource-pack" => {
                if value.is_empty() {
                    return Err(format!("--{name} requires a path"));
                }
                options.texture_pack = Some(value.into());
            }
            "no-texture-pack" => options.no_texture_pack = true,
            "ui-font" => {
                if let Some(reason) = ob_core::config::ui_font_path_rejection_reason(value) {
                    return Err(format!("invalid --ui-font path '{value}': {reason}"));
                }
                options.ui_font = Some(value.into());
            }
            "default-ui-font" => options.default_ui_font = true,
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
            "screenshot-size" => {
                let (width, height) = value
                    .split_once('x')
                    .or_else(|| value.split_once('X'))
                    .ok_or_else(|| "--screenshot-size must look like 1280x720".to_string())?;
                let width: u32 = width
                    .parse()
                    .map_err(|_| format!("invalid --screenshot-size width '{width}'"))?;
                let height: u32 = height
                    .parse()
                    .map_err(|_| format!("invalid --screenshot-size height '{height}'"))?;
                if !(320..=3840).contains(&width) || !(240..=2160).contains(&height) {
                    return Err("--screenshot-size width 320-3840, height 240-2160".into());
                }
                options.screenshot_width = width;
                options.screenshot_height = height;
            }
            "screenshot-dead" => {
                options.screenshot_dead = true;
                match value.as_str() {
                    "" | "local" => {}
                    "network" => options.screenshot_dead_network = true,
                    other => {
                        return Err(format!(
                            "unknown --screenshot-dead variant '{other}' (expected network)"
                        ));
                    }
                }
            }
            "screenshot-settings" => {
                options.screenshot_settings = true;
                if !value.is_empty() {
                    options.screenshot_settings_tab =
                        Some(SettingsTab::from_name(value).ok_or_else(|| format!("unknown settings tab '{value}'"))?);
                }
            }
            "screenshot-settings-tab" => {
                if value.is_empty() {
                    return Err("--screenshot-settings-tab requires a tab name".into());
                }
                options.screenshot_settings_tab =
                    Some(SettingsTab::from_name(value).ok_or_else(|| format!("unknown settings tab '{value}'"))?);
                options.screenshot_settings = true;
            }
            "screenshot-crafting" => {
                options.screenshot_crafting = true;
                match value.as_str() {
                    "" | "table" => {}
                    "hand" => options.screenshot_crafting_hand = true,
                    other => {
                        return Err(format!(
                            "unknown --screenshot-crafting variant '{other}' (expected hand)"
                        ));
                    }
                }
            }
            "screenshot-debug" => options.screenshot_debug = true,
            "screenshot-mobs" => {
                options.screenshot_mobs = true;
                match value.as_str() {
                    "" | "debug" => options.screenshot_mobs_f3 = true,
                    "plain" => options.screenshot_mobs_f3 = false,
                    other => {
                        return Err(format!(
                            "unknown --screenshot-mobs variant '{other}' (expected debug or plain)"
                        ));
                    }
                }
            }
            "screenshot-furnace" => {
                options.screenshot_furnace = true;
                if !value.is_empty() {
                    options.screenshot_furnace_demo = ContainerDemo::from_name(value).ok_or_else(|| {
                        format!("unknown --screenshot-furnace variant '{value}' (expected empty, active or full)")
                    })?;
                }
            }
            "screenshot-chest" => {
                options.screenshot_chest = true;
                if !value.is_empty() {
                    options.screenshot_chest_demo = ContainerDemo::from_name(value).ok_or_else(|| {
                        format!("unknown --screenshot-chest variant '{value}' (expected empty, active or full)")
                    })?;
                }
            }
            "screenshot-items" => options.screenshot_items = true,
            "no-audio" => options.no_audio = true,
            "mute" => options.mute = true,
            "survival" => options.game_mode = Some(SettingsGameMode::Survival),
            "creative" => options.game_mode = Some(SettingsGameMode::Creative),
            "ui-scale" | "gui-scale" => {
                let percent = if value.eq_ignore_ascii_case("auto") {
                    0
                } else {
                    value.trim_end_matches('%').parse::<u16>().map_err(|_| {
                        format!("invalid --ui-scale value '{value}' (expected auto, 75, 100, 125 or 150)")
                    })?
                };
                let normalised = ob_core::config::normalise_ui_scale_percent(percent);
                if normalised != percent && percent != 0 {
                    return Err(format!(
                        "--ui-scale must be one of auto, 75, 100, 125 or 150 (got '{value}')"
                    ));
                }
                options.ui_scale = Some(normalised);
            }
            "title" | "menu" => options.force_title = true,
            "screenshot-title" => options.screenshot_title = true,
            "screenshot-worlds" => options.screenshot_worlds = true,
            "screenshot-connect" => {
                options.screenshot_connect = true;
                match value.as_str() {
                    "" => {}
                    "error" => options.screenshot_connect_error = true,
                    other => {
                        return Err(format!(
                            "unknown --screenshot-connect variant '{other}' (expected error)"
                        ));
                    }
                }
            }
            "screenshot-new-world" => options.screenshot_new_world = true,
            "screenshot-settings-title" => options.screenshot_settings_title = true,
            "screenshot-network" => options.screenshot_network = true,
            "screenshot-name-tags" => options.screenshot_name_tags = true,
            "screenshot-net-ui" => {
                if value.is_empty() {
                    return Err("--screenshot-net-ui requires loading, failed, disconnected or roster".into());
                }
                options.screenshot_net_ui = Some(
                    NetUiKind::from_name(value)
                        .ok_or_else(|| format!("unknown --screenshot-net-ui variant '{value}'"))?,
                );
            }
            "help" | "version" => {}
            other => return Err(format!("unknown option --{other}")),
        }
    }
    if options.screenshot_dead && options.screenshot.is_none() {
        return Err("--screenshot-dead requires --screenshot PATH".into());
    }
    let menu_screenshot_modes = usize::from(options.screenshot_title)
        + usize::from(options.screenshot_new_world)
        + usize::from(options.screenshot_worlds)
        + usize::from(options.screenshot_connect)
        + usize::from(options.screenshot_settings_title);
    let screenshot_modes = usize::from(options.screenshot_dead)
        + usize::from(options.screenshot_settings)
        + usize::from(options.screenshot_crafting)
        + usize::from(options.screenshot_debug)
        + usize::from(options.screenshot_mobs)
        + usize::from(options.screenshot_furnace)
        + usize::from(options.screenshot_chest)
        + usize::from(options.screenshot_network)
        + usize::from(options.screenshot_name_tags)
        + usize::from(options.screenshot_net_ui.is_some())
        + usize::from(options.screenshot_inventory.is_some())
        + menu_screenshot_modes;
    if screenshot_modes > 1 {
        return Err("--screenshot screen modes cannot be combined".into());
    }
    if menu_screenshot_modes > 0 && options.screenshot.is_none() {
        return Err(
            "--screenshot-title/--screenshot-new-world/--screenshot-worlds/--screenshot-connect/--screenshot-settings-title require --screenshot PATH"
                .into(),
        );
    }
    if options.screenshot_connect_error && !options.screenshot_connect {
        return Err("--screenshot-connect=error requires --screenshot-connect".into());
    }
    if menu_screenshot_modes > 0 && options.screenshot_items {
        return Err("front-end screenshot flags cannot be combined with --screenshot-items".into());
    }
    if options.screenshot_settings
        && (options.screenshot_dead
            || options.screenshot_crafting
            || options.screenshot_mobs
            || options.screenshot_furnace
            || options.screenshot_chest
            || options.screenshot_inventory.is_some())
    {
        return Err("--screenshot-settings cannot be combined with another screenshot screen mode".into());
    }
    if (options.screenshot_crafting
        || options.screenshot_mobs
        || options.screenshot_furnace
        || options.screenshot_chest)
        && (options.screenshot_dead || options.screenshot_inventory.is_some())
    {
        return Err(
            "--screenshot-crafting/--screenshot-mobs/--screenshot-furnace/--screenshot-chest cannot be combined with --screenshot-dead or --screenshot-inventory"
                .into(),
        );
    }
    if options.screenshot_settings && options.screenshot.is_none() {
        return Err("--screenshot-settings requires --screenshot PATH".into());
    }
    if options.screenshot_crafting && options.screenshot.is_none() {
        return Err("--screenshot-crafting requires --screenshot PATH".into());
    }
    if options.screenshot_debug && options.screenshot.is_none() {
        return Err("--screenshot-debug requires --screenshot PATH".into());
    }
    if options.screenshot_mobs && options.screenshot.is_none() {
        return Err("--screenshot-mobs requires --screenshot PATH".into());
    }
    if options.screenshot_furnace && options.screenshot.is_none() {
        return Err("--screenshot-furnace requires --screenshot PATH".into());
    }
    if options.screenshot_chest && options.screenshot.is_none() {
        return Err("--screenshot-chest requires --screenshot PATH".into());
    }
    if options.screenshot_network && options.screenshot.is_none() {
        return Err("--screenshot-network requires --screenshot PATH".into());
    }
    if options.screenshot_name_tags && options.screenshot.is_none() {
        return Err("--screenshot-name-tags requires --screenshot PATH".into());
    }
    if options.screenshot_net_ui.is_some() && options.screenshot.is_none() {
        return Err("--screenshot-net-ui requires --screenshot PATH".into());
    }
    if options.screenshot_items && options.screenshot.is_none() && options.screenshot_inventory.is_none() {
        return Err("--screenshot-items requires --screenshot or --screenshot-inventory PATH".into());
    }
    if (options.screenshot_dead
        || options.screenshot_crafting
        || options.screenshot_debug
        || options.screenshot_mobs
        || options.screenshot_furnace
        || options.screenshot_chest
        || options.screenshot_items)
        && options.game_mode == Some(SettingsGameMode::Creative)
    {
        return Err(
            "--screenshot-dead, --screenshot-crafting, --screenshot-debug, --screenshot-mobs, --screenshot-furnace, --screenshot-chest and --screenshot-items require survival mode"
                .into(),
        );
    }
    if options.connect.is_some() {
        if saw_world || saw_seed {
            return Err(
                "--connect cannot be combined with --world or --seed (multiplayer uses the server world)".into(),
            );
        }
        let screenshot = options.screenshot.is_some()
            || options.screenshot_inventory.is_some()
            || options.screenshot_dead
            || options.screenshot_settings
            || options.screenshot_crafting
            || options.screenshot_debug
            || options.screenshot_mobs
            || options.screenshot_furnace
            || options.screenshot_chest
            || options.screenshot_network
            || options.screenshot_name_tags
            || options.screenshot_net_ui.is_some()
            || options.screenshot_items
            || options.screenshot_title
            || options.screenshot_new_world
            || options.screenshot_worlds
            || options.screenshot_connect
            || options.screenshot_settings_title;
        if screenshot {
            return Err("--connect is windowed only; offscreen screenshots are supported for local worlds".into());
        }
    }
    if options.force_title && (saw_world || saw_seed || options.connect.is_some()) {
        return Err("--title/--menu cannot be combined with --world, --seed or --connect".into());
    }
    if options.ui_font.is_some() && options.default_ui_font {
        return Err("--ui-font and --default-ui-font cannot be combined".into());
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
    fn title_is_the_default_and_explicit_world_flags_bypass_it() {
        assert!(should_show_title(&ClientOptions::default()));
        let options = options_from_args(&[("world".to_string(), "survival".to_string())]).expect("world");
        assert!(!should_show_title(&options));
        let options = options_from_args(&[("seed".to_string(), "7".to_string())]).expect("seed");
        assert!(!should_show_title(&options));
        let options = options_from_args(&[("data-dir".to_string(), "/tmp/ob".to_string())]).expect("data dir");
        assert!(!should_show_title(&options));
        let options = options_from_args(&[("connect".to_string(), "127.0.0.1:25565".to_string())]).expect("connect");
        assert!(!should_show_title(&options));
        let options = options_from_args(&[
            ("title".to_string(), String::new()),
            ("data-dir".to_string(), "/tmp/ob".to_string()),
        ])
        .expect("title with data dir");
        assert!(should_show_title(&options), "--title forces the menu with a data dir");
        let options = options_from_args(&[("menu".to_string(), String::new())]).expect("menu alias");
        assert!(should_show_title(&options));

        let error = options_from_args(&[
            ("title".to_string(), String::new()),
            ("world".to_string(), "survival".to_string()),
        ])
        .expect_err("--title and --world conflict");
        assert!(error.contains("--title"));
    }

    #[test]
    fn menu_screenshot_flags_parse_and_require_a_screenshot_path() {
        for (name, expected) in [
            ("screenshot-title", menu_app::MenuScreenshot::Title),
            ("screenshot-new-world", menu_app::MenuScreenshot::NewWorld),
            ("screenshot-worlds", menu_app::MenuScreenshot::Worlds),
            ("screenshot-connect", menu_app::MenuScreenshot::Multiplayer),
            ("screenshot-settings-title", menu_app::MenuScreenshot::Settings),
        ] {
            let error = options_from_args(&[(name.to_string(), String::new())])
                .expect_err("menu screenshots need --screenshot");
            assert!(error.contains("require --screenshot"), "{name}: {error}");
            let options = options_from_args(&[
                ("screenshot".to_string(), "/tmp/menu.png".to_string()),
                (name.to_string(), String::new()),
            ])
            .expect("valid menu screenshot flags");
            assert_eq!(menu_screenshot_mode(&options), Some(expected));
        }
        let options = options_from_args(&[
            ("screenshot".to_string(), "/tmp/menu.png".to_string()),
            ("screenshot-connect".to_string(), "error".to_string()),
        ])
        .expect("error variant");
        assert!(options.screenshot_connect);
        assert!(options.screenshot_connect_error);
        assert_eq!(
            menu_screenshot_mode(&options),
            Some(menu_app::MenuScreenshot::MultiplayerError)
        );
        let error = options_from_args(&[
            ("screenshot".to_string(), "/tmp/menu.png".to_string()),
            ("screenshot-connect".to_string(), "nope".to_string()),
        ])
        .expect_err("unknown variant");
        assert!(error.contains("unknown --screenshot-connect variant"));
        let error = options_from_args(&[
            ("screenshot".to_string(), "/tmp/menu.png".to_string()),
            ("screenshot-title".to_string(), String::new()),
            ("screenshot-worlds".to_string(), String::new()),
        ])
        .expect_err("only one menu screen");
        assert!(error.contains("cannot be combined"));

        let error = options_from_args(&[
            ("screenshot".to_string(), "/tmp/menu.png".to_string()),
            ("screenshot-connect".to_string(), String::new()),
            ("connect".to_string(), "127.0.0.1:25565".to_string()),
        ])
        .expect_err("menu screenshots are offscreen local only");
        assert!(error.contains("windowed only"));
    }

    #[test]
    fn key_mapping_resolves_the_documented_defaults() {
        for (code, action) in [
            (KeyCode::KeyW, Action::MoveForward),
            (KeyCode::KeyS, Action::MoveBack),
            (KeyCode::KeyA, Action::MoveLeft),
            (KeyCode::KeyD, Action::MoveRight),
            (KeyCode::Space, Action::Jump),
            (KeyCode::ShiftLeft, Action::Sneak),
            (KeyCode::ControlLeft, Action::Sprint),
            (KeyCode::KeyF, Action::Fly),
            (KeyCode::KeyE, Action::Inventory),
            (KeyCode::KeyC, Action::Crafting),
            (KeyCode::KeyT, Action::Chat),
            (KeyCode::F1, Action::ToggleHud),
            (KeyCode::F2, Action::Screenshot),
            (KeyCode::F3, Action::DebugOverlay),
            (KeyCode::Escape, Action::Pause),
        ] {
            let key = bind_key_for_code(code).unwrap_or_else(|| panic!("{code:?} is mappable"));
            assert_eq!(Settings::default().keybinds.action_for(key), Some(action), "{code:?}");
        }
        assert_eq!(
            bind_key_for_mouse(MouseButton::Left),
            Some(BindKey::Mouse(ob_core::config::MouseButton::Left))
        );
        assert_eq!(
            bind_key_for_mouse(MouseButton::Right),
            Some(BindKey::Mouse(ob_core::config::MouseButton::Right))
        );
        assert_eq!(bind_key_for_code(KeyCode::F24), None);
        assert_eq!(bind_key_for_mouse(MouseButton::Other(9)), None);
        assert_eq!(movement_slot(Action::Jump), Some(4));
        assert_eq!(movement_slot(Action::Fly), None);
    }

    #[test]
    fn reduced_motion_disables_the_sprint_fov_pump() {
        let mut settings = Settings::default();
        assert!((target_fov(&settings, true) - settings.fov_degrees * 1.10).abs() < 0.001);
        assert_eq!(target_fov(&settings, false), settings.fov_degrees);
        settings.reduced_motion = true;
        assert_eq!(target_fov(&settings, true), settings.fov_degrees);
        assert_eq!(target_fov(&settings, false), settings.fov_degrees);
    }

    #[test]
    fn settings_screen_suppresses_f1_and_f3_shortcuts() {
        assert!(!overlay_shortcuts_allowed(Screen::Settings));
        assert!(overlay_shortcuts_allowed(Screen::Playing));
        assert!(overlay_shortcuts_allowed(Screen::Inventory));
        assert!(overlay_shortcuts_allowed(Screen::Death));
    }

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
    fn multiplayer_connect_flags_parse_and_are_mutually_exclusive() {
        let options = options_from_args(&[
            ("connect".to_string(), "example.com:25565".to_string()),
            ("name".to_string(), "alice".to_string()),
        ])
        .expect("valid connect flags");
        assert_eq!(options.connect.as_deref(), Some("example.com:25565"));
        assert_eq!(options.name.as_deref(), Some("alice"));

        let error = options_from_args(&[("connect".to_string(), String::new())]).expect_err("empty endpoint must fail");
        assert!(error.contains("--connect requires"));

        let error = options_from_args(&[
            ("connect".to_string(), "127.0.0.1:25565".to_string()),
            ("name".to_string(), "not valid".to_string()),
        ])
        .expect_err("invalid name must fail");
        assert!(error.contains("invalid --name"));

        let error = options_from_args(&[
            ("connect".to_string(), "127.0.0.1:25565".to_string()),
            ("world".to_string(), "local".to_string()),
        ])
        .expect_err("connect cannot open a local world");
        assert!(error.contains("--world"));

        let error = options_from_args(&[
            ("connect".to_string(), "127.0.0.1:25565".to_string()),
            ("seed".to_string(), "7".to_string()),
        ])
        .expect_err("connect cannot use a local seed");
        assert!(error.contains("--seed"));

        let error = options_from_args(&[
            ("connect".to_string(), "127.0.0.1:25565".to_string()),
            ("screenshot".to_string(), "/tmp/net.png".to_string()),
        ])
        .expect_err("network screenshots are not supported");
        assert!(error.contains("windowed only"));

        // Explicit game mode flags are accepted for launcher compatibility, but
        // the server owns the mode; they must not be interpreted as world
        // selection.
        let options = options_from_args(&[
            ("connect".to_string(), "127.0.0.1:25565".to_string()),
            ("survival".to_string(), String::new()),
        ])
        .expect("survival flag is harmless with connect");
        assert!(options.connect.is_some());
        assert_eq!(options.game_mode, Some(SettingsGameMode::Survival));
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

    #[test]
    fn texture_pack_flags_are_parsed_and_aliased() {
        let path = PathBuf::from("/tmp/packs/example");
        let options = options_from_args(&[("texture-pack".to_string(), path.to_string_lossy().into_owned())])
            .expect("valid flag");
        assert_eq!(options.texture_pack.as_deref(), Some(path.as_path()));
        assert!(!options.no_texture_pack);

        let options = options_from_args(&[("resource-pack".to_string(), path.to_string_lossy().into_owned())])
            .expect("alias must parse");
        assert_eq!(options.texture_pack.as_deref(), Some(path.as_path()));

        let options = options_from_args(&[("no-texture-pack".to_string(), String::new())]).expect("valid flag");
        assert!(options.no_texture_pack);
        assert_eq!(options.texture_pack, None);

        let error =
            options_from_args(&[("texture-pack".to_string(), String::new())]).expect_err("empty path must fail");
        assert!(error.contains("--texture-pack"));
        let error =
            options_from_args(&[("resource-pack".to_string(), String::new())]).expect_err("empty alias path must fail");
        assert!(error.contains("--resource-pack"));
    }

    #[test]
    fn resolve_texture_source_precedence_is_cli_pack_then_no_pack_then_saved_then_style() {
        let mut settings = Settings {
            texture_style: TextureStyle::Lush,
            texture_pack: Some("/saved/pack".into()),
            ..Settings::default()
        };
        let mut options = ClientOptions::default();

        // Persisted pack wins over the persisted style.
        match resolve_texture_source(&options, &settings) {
            TextureSource::ResourcePack(path) => assert_eq!(path, PathBuf::from("/saved/pack")),
            TextureSource::Style(_) => panic!("persisted pack must be used"),
        }

        // `--no-texture-pack` beats the persisted pack.
        options.no_texture_pack = true;
        match resolve_texture_source(&options, &settings) {
            TextureSource::Style(style) => assert_eq!(style, ob_render::TexturePackStyle::Lush),
            TextureSource::ResourcePack(path) => panic!("no-pack must win, got {}", path.display()),
        }

        // `--texture-pack` beats both no-pack and the persisted pack.
        options.texture_pack = Some("/cli/pack".into());
        match resolve_texture_source(&options, &settings) {
            TextureSource::ResourcePack(path) => assert_eq!(path, PathBuf::from("/cli/pack")),
            TextureSource::Style(_) => panic!("CLI pack must win"),
        }

        // Nothing saved and no flags: procedural style.
        settings.texture_pack = None;
        let options = ClientOptions::default();
        match resolve_texture_source(&options, &settings) {
            TextureSource::Style(style) => assert_eq!(style, ob_render::TexturePackStyle::Lush),
            TextureSource::ResourcePack(path) => panic!("unexpected pack {}", path.display()),
        }

        // Empty persisted value is treated as unset.
        settings.texture_pack = Some(String::new());
        match resolve_texture_source(&options, &settings) {
            TextureSource::Style(_) => {}
            TextureSource::ResourcePack(path) => panic!("empty path must be ignored, got {}", path.display()),
        }
    }

    #[test]
    fn texture_pack_row_toggle_target_depends_on_active_source_and_saved_path() {
        let saved = Settings {
            texture_pack: Some("/saved/pack".into()),
            ..Settings::default()
        };
        let active_pack = TextureSource::ResourcePack(PathBuf::from("/saved/pack"));
        assert_eq!(texture_toggle_target(&active_pack, &saved), TextureToggle::Disable);

        let style = TextureSource::Style(ob_render::TexturePackStyle::Classic);
        assert_eq!(
            texture_toggle_target(&style, &saved),
            TextureToggle::Enable(PathBuf::from("/saved/pack"))
        );

        let none = Settings::default();
        assert_eq!(texture_toggle_target(&style, &none), TextureToggle::Noop);
        let empty = Settings {
            texture_pack: Some(String::new()),
            ..Settings::default()
        };
        assert_eq!(texture_toggle_target(&style, &empty), TextureToggle::Noop);
    }

    #[test]
    fn texture_source_labels_are_reported_for_f3_and_logs() {
        let style = TextureSource::Style(ob_render::TexturePackStyle::Painterly);
        assert_eq!(describe_texture_source(&style, 32), "painterly 32px");
        assert_eq!(active_texture_pack_label(&style), None);

        let pack = TextureSource::ResourcePack(PathBuf::from("/tmp/packs/rotrBLOCKS V88"));
        assert_eq!(describe_texture_source(&pack, 128), "rotrBLOCKS V88 128px");
        assert_eq!(active_texture_pack_label(&pack).as_deref(), Some("rotrBLOCKS V88"));
    }

    #[test]
    fn cli_texture_pack_choice_is_persisted_without_other_cli_overrides() {
        let dir = std::env::temp_dir().join(format!(
            "openblock-client-pack-choice-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let disk = Settings {
            render_distance_chunks: 5,
            texture_pack: Some("/old/pack".into()),
            ..Settings::default()
        };
        save_settings(&dir, &disk).expect("save seed settings");

        // A CLI render-distance override must not leak into the saved file when
        // only the pack selection is persisted.
        let mut runtime = Settings {
            render_distance_chunks: 11,
            texture_pack: Some("/old/pack".into()),
            ..Settings::default()
        };
        let options = ClientOptions {
            texture_pack: Some(PathBuf::from("/new/pack")),
            render_distance_chunks: 11,
            ..ClientOptions::default()
        };
        persist_texture_pack_choice(&options, &mut runtime, &dir);
        assert_eq!(runtime.texture_pack.as_deref(), Some("/new/pack"));
        let loaded = load_settings(&dir);
        assert_eq!(loaded.texture_pack.as_deref(), Some("/new/pack"));
        assert_eq!(loaded.render_distance_chunks, 5);

        // `--no-texture-pack` clears both the runtime and persisted selection.
        let options = ClientOptions {
            no_texture_pack: true,
            render_distance_chunks: 11,
            ..ClientOptions::default()
        };
        persist_texture_pack_choice(&options, &mut runtime, &dir);
        assert_eq!(runtime.texture_pack, None);
        assert_eq!(load_settings(&dir).texture_pack, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ui_font_flags_parse_validate_and_conflict() {
        let path = PathBuf::from("/tmp/fonts/Minecraftia.ttf");
        let options =
            options_from_args(&[("ui-font".to_string(), path.to_string_lossy().into_owned())]).expect("valid font");
        assert_eq!(options.ui_font.as_deref(), Some(path.as_path()));
        assert!(!options.default_ui_font);
        assert!(
            should_show_title(&options),
            "a font selection does not bypass the title"
        );

        let options = options_from_args(&[("default-ui-font".to_string(), String::new())]).expect("valid flag");
        assert!(options.default_ui_font);
        assert_eq!(options.ui_font, None);
        assert!(should_show_title(&options));

        for value in [String::new(), "bad\u{7}path".to_string(), "x".repeat(4097)] {
            let error = options_from_args(&[("ui-font".to_string(), value)]).expect_err("bad path must fail");
            assert!(error.contains("--ui-font"), "{error}");
        }
        let error = options_from_args(&[
            ("ui-font".to_string(), path.to_string_lossy().into_owned()),
            ("default-ui-font".to_string(), String::new()),
        ])
        .expect_err("conflicting font flags");
        assert!(error.contains("--default-ui-font"), "{error}");
    }

    #[test]
    fn front_end_transition_clears_persisted_font_cli_overrides() {
        let mut options = ClientOptions {
            ui_font: Some(PathBuf::from("/tmp/fonts/custom.ttf")),
            ..ClientOptions::default()
        };
        clear_ui_font_cli_overrides(&mut options);
        assert_eq!(options.ui_font, None);
        assert!(!options.default_ui_font);

        let mut options = ClientOptions {
            default_ui_font: true,
            ..ClientOptions::default()
        };
        clear_ui_font_cli_overrides(&mut options);
        assert!(!options.default_ui_font);
    }

    #[test]
    fn cli_ui_font_choice_is_persisted_without_other_cli_overrides() {
        let dir = std::env::temp_dir().join(format!(
            "openblock-client-font-choice-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let disk = Settings {
            render_distance_chunks: 5,
            ui_font_path: Some("/old/Monocraft.ttf".into()),
            ..Settings::default()
        };
        save_settings(&dir, &disk).expect("save seed settings");

        let mut runtime = Settings {
            render_distance_chunks: 11,
            ui_font_path: Some("/old/Monocraft.ttf".into()),
            ..Settings::default()
        };
        let options = ClientOptions {
            ui_font: Some(PathBuf::from("/new/Minecraftia.ttf")),
            render_distance_chunks: 11,
            ..ClientOptions::default()
        };
        persist_ui_font_choice(&options, &mut runtime, &dir);
        assert_eq!(runtime.ui_font_path.as_deref(), Some("/new/Minecraftia.ttf"));
        let loaded = load_settings(&dir);
        assert_eq!(loaded.ui_font_path.as_deref(), Some("/new/Minecraftia.ttf"));
        assert_eq!(loaded.render_distance_chunks, 5);

        // `--default-ui-font` clears both the runtime and persisted selection.
        let options = ClientOptions {
            default_ui_font: true,
            render_distance_chunks: 11,
            ..ClientOptions::default()
        };
        persist_ui_font_choice(&options, &mut runtime, &dir);
        assert_eq!(runtime.ui_font_path, None);
        assert_eq!(load_settings(&dir).ui_font_path, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn game_mode_cli_flags_parse_and_the_last_one_wins() {
        let options = options_from_args(&[("survival".to_string(), String::new())]).expect("survival");
        assert_eq!(options.game_mode, Some(SettingsGameMode::Survival));
        let options = options_from_args(&[("creative".to_string(), String::new())]).expect("creative");
        assert_eq!(options.game_mode, Some(SettingsGameMode::Creative));
        let options = options_from_args(&[
            ("survival".to_string(), String::new()),
            ("creative".to_string(), String::new()),
        ])
        .expect("both flags");
        assert_eq!(options.game_mode, Some(SettingsGameMode::Creative));
        assert_eq!(map_game_mode(SettingsGameMode::Survival), HostGameMode::Survival);
        assert_eq!(map_game_mode(SettingsGameMode::Creative), HostGameMode::Creative);
    }

    #[test]
    fn game_mode_cli_beats_persisted_settings_and_persists_itself() {
        let dir = std::env::temp_dir().join(format!(
            "openblock-client-mode-choice-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let disk = Settings {
            game_mode: SettingsGameMode::Creative,
            render_distance_chunks: 5,
            ..Settings::default()
        };
        save_settings(&dir, &disk).expect("save seed settings");

        // Persisted creative is respected when no CLI mode is given.
        let options = ClientOptions {
            data_dir: Some(dir.clone()),
            ..ClientOptions::default()
        };
        let (settings, _) = effective_settings(&options);
        assert_eq!(settings.game_mode, SettingsGameMode::Creative);

        // `--survival` overrides the persisted mode for this run and is saved
        // without leaking the unrelated render-distance default.
        let options = ClientOptions {
            data_dir: Some(dir.clone()),
            game_mode: Some(SettingsGameMode::Survival),
            ..ClientOptions::default()
        };
        let (mut settings, resolved_dir) = effective_settings(&options);
        assert_eq!(settings.game_mode, SettingsGameMode::Survival);
        assert_eq!(resolved_dir, dir);
        persist_game_mode_choice(&options, &mut settings, &resolved_dir);
        let loaded = load_settings(&dir);
        assert_eq!(loaded.game_mode, SettingsGameMode::Survival);
        assert_eq!(loaded.render_distance_chunks, 5, "unrelated CLI values must not leak");

        // A missing config yields the new default: survival.
        let missing = dir.join("missing");
        save_settings(&missing, &Settings::default()).expect("create dir");
        let options = ClientOptions {
            data_dir: Some(missing.join("not-created")),
            ..ClientOptions::default()
        };
        let (settings, _) = effective_settings(&options);
        assert_eq!(settings.game_mode, SettingsGameMode::Survival);
        let _ = std::fs::remove_dir_all(missing);

        // Even when the resolved mode equals the default, an explicit CLI value
        // is recorded so the choice is visible in config.json.
        let fresh = dir.join("fresh");
        let options = ClientOptions {
            data_dir: Some(fresh.clone()),
            game_mode: Some(SettingsGameMode::Survival),
            ..ClientOptions::default()
        };
        let (mut settings, resolved_dir) = effective_settings(&options);
        persist_game_mode_choice(&options, &mut settings, &resolved_dir);
        assert!(ob_core::config::settings_path(&fresh).exists());
        assert_eq!(load_settings(&fresh).game_mode, SettingsGameMode::Survival);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn screenshot_settings_flag_parses_and_needs_a_screenshot_path() {
        let path = PathBuf::from("/tmp/settings.png");
        let options = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-settings".to_string(), String::new()),
        ])
        .expect("valid settings screenshot");
        assert_eq!(options.screenshot.as_deref(), Some(path.as_path()));
        assert!(options.screenshot_settings);
        assert!(!options.screenshot_dead);

        let error = options_from_args(&[("screenshot-settings".to_string(), String::new())])
            .expect_err("needs a screenshot path");
        assert!(error.contains("--screenshot PATH"));

        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-settings".to_string(), String::new()),
            ("screenshot-dead".to_string(), String::new()),
        ])
        .expect_err("must not combine modes");
        assert!(error.contains("cannot be combined"));

        let error = options_from_args(&[
            ("screenshot-settings".to_string(), String::new()),
            ("screenshot-inventory".to_string(), path.to_string_lossy().into_owned()),
        ])
        .expect_err("must not combine with the inventory screenshot");
        assert!(error.contains("cannot be combined"));
    }

    fn dropped(registry: &Registry, id: u64, name: &str, count: u8) -> ItemEntity {
        let item = registry.item_id(name).expect("item id");
        ItemEntity {
            id,
            pos: WorldVec::new(1.5, 64.0, -2.25),
            velocity: WorldVec::splat(0.0),
            stack: ob_server::ItemStack::new(item, count),
            age: 0.0,
        }
    }

    #[test]
    fn entity_instances_map_block_items_to_their_top_texture() {
        let registry = Registry::base();
        let entity = dropped(&registry, 7, "openblock:planks", 2);
        let instance = entity_instance_for(&entity, &registry, 2.0);
        assert!(instance.textured);
        let planks = registry.id("openblock:planks").expect("planks block");
        assert_eq!(instance.layer, u32::from(registry.block(planks).textures.top.0));
        assert_eq!(instance.tint, [1.0, 1.0, 1.0]);
        assert_eq!(instance.scale, 0.22);
        assert!((instance.position[0] - 1.5).abs() < 1e-6);
        assert!((instance.position[1] - 64.15).abs() < 1e-6);
        assert!((instance.position[2] + 2.25).abs() < 1e-6);
        let expected_yaw = 2.0 * 1.8 + 7.0 * 0.7;
        assert!((instance.yaw - expected_yaw).abs() < 1e-5);

        let table = dropped(&registry, 8, "openblock:crafting_table", 1);
        let instance = entity_instance_for(&table, &registry, 0.0);
        assert_eq!(
            instance.layer,
            u32::from(ob_content::textures::CRAFTING_TABLE_TOP.0),
            "crafting tables must use their distinct top layer"
        );
        assert!(instance.textured);
    }

    #[test]
    fn entity_instances_tint_apple_stick_and_tool_tiers() {
        let registry = Registry::base();
        let instance = |name: &str, id: u64| {
            let entity = dropped(&registry, id, name, 1);
            entity_instance_for(&entity, &registry, 0.0)
        };
        let apple = instance("openblock:apple", 1);
        assert!(!apple.textured);
        assert_eq!(apple.tint, [0.82, 0.14, 0.17]);
        assert_eq!(apple.layer, 0);
        let stick = instance("openblock:stick", 2);
        assert_eq!(stick.tint, [0.61, 0.42, 0.21]);
        let wooden = instance("openblock:wooden_pickaxe", 3);
        assert_eq!(wooden.tint, [0.67, 0.47, 0.25]);
        assert!(!wooden.textured);
        let stone = instance("openblock:stone_axe", 4);
        assert_eq!(stone.tint, [0.54, 0.55, 0.58]);
        assert_eq!(stone.scale, 0.22);
        assert_eq!(stone.layer, 0);
        let iron = instance("openblock:iron_pickaxe", 5);
        assert_eq!(iron.tint, [0.70, 0.73, 0.78]);
        // Every new catalogue item gets a representative non-default tint.
        for (name, expected) in [
            ("openblock:coal", [0.10, 0.10, 0.12]),
            ("openblock:raw_iron", [0.76, 0.66, 0.50]),
            ("openblock:iron_ingot", [0.62, 0.65, 0.69]),
            ("openblock:raw_copper", [0.82, 0.48, 0.24]),
            ("openblock:copper_ingot", [0.79, 0.45, 0.22]),
            ("openblock:iron_chestplate", [0.62, 0.65, 0.69]),
            ("openblock:leather", [0.64, 0.44, 0.23]),
            ("openblock:cooked_beef", [0.45, 0.22, 0.14]),
        ] {
            let item = registry.item_id(name).expect("item");
            assert_eq!(inventory::item_tint_for(&registry, item), Some(expected), "{name}");
            let entity = instance(name, 10);
            assert_eq!(entity.tint, expected, "{name} entity tint");
            assert!(!entity.textured);
        }
    }

    #[test]
    fn entity_instance_batch_preserves_entity_order_and_offsets() {
        let registry = Registry::base();
        let entities = vec![
            dropped(&registry, 10, "openblock:planks", 1),
            dropped(&registry, 11, "openblock:apple", 1),
        ];
        let instances = entity_instances_for(&entities, &registry, 1.0);
        assert_eq!(instances.len(), 2);
        assert!(instances[0].textured);
        assert!(!instances[1].textured);
        assert!((instances[0].yaw - (1.0 * 1.8 + 10.0 * 0.7)).abs() < 1e-5);
        assert!((instances[1].yaw - (1.0 * 1.8 + 11.0 * 0.7)).abs() < 1e-5);
        assert!(entity_instances_for(&[], &registry, 0.0).is_empty());
    }

    #[test]
    fn screenshot_crafting_flag_parses_and_seeds_survival_crafting() {
        let path = PathBuf::from("/tmp/crafting.png");
        let options = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-crafting".to_string(), String::new()),
        ])
        .expect("valid crafting screenshot");
        assert_eq!(options.screenshot.as_deref(), Some(path.as_path()));
        assert!(options.screenshot_crafting);
        assert!(!options.screenshot_settings);
        assert!(!options.screenshot_items);

        let error = options_from_args(&[("screenshot-crafting".to_string(), String::new())])
            .expect_err("needs a screenshot path");
        assert!(error.contains("--screenshot PATH"));

        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-crafting".to_string(), String::new()),
            ("screenshot-dead".to_string(), String::new()),
        ])
        .expect_err("must not combine modes");
        assert!(error.contains("cannot be combined"));

        let dir = std::env::temp_dir().join(format!(
            "openblock-client-crafting-shot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        save_settings(
            &dir,
            &Settings {
                game_mode: SettingsGameMode::Creative,
                ..Settings::default()
            },
        )
        .expect("save seed settings");
        let options = ClientOptions {
            data_dir: Some(dir.clone()),
            screenshot: Some(path),
            screenshot_crafting: true,
            ..ClientOptions::default()
        };
        let (settings, _) = effective_settings(&options);
        assert_eq!(settings.game_mode, SettingsGameMode::Survival);
        assert_eq!(
            load_settings(&dir).game_mode,
            SettingsGameMode::Creative,
            "debug screenshots must not rewrite the saved mode"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_flags_parse_and_mute_does_not_persist() {
        let options = options_from_args(&[
            ("no-audio".to_string(), String::new()),
            ("mute".to_string(), String::new()),
        ])
        .expect("audio flags");
        assert!(options.no_audio);
        assert!(options.mute);

        let dir = std::env::temp_dir().join(format!(
            "openblock-client-audio-flags-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        save_settings(&dir, &Settings::default()).expect("save seed settings");
        let options = ClientOptions {
            data_dir: Some(dir.clone()),
            mute: true,
            ..ClientOptions::default()
        };
        let (settings, _) = effective_settings(&options);
        assert!(!settings.audio_enabled, "mute applies to this run");
        assert!(
            load_settings(&dir).audio_enabled,
            "mute must not rewrite the saved setting"
        );
        let no_audio = ClientOptions {
            data_dir: Some(dir.clone()),
            no_audio: true,
            ..ClientOptions::default()
        };
        let (settings, _) = effective_settings(&no_audio);
        assert!(settings.audio_enabled, "no-audio leaves the persisted setting alone");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_audio_skips_the_engine_and_says_so() {
        let options = ClientOptions {
            no_audio: true,
            ..ClientOptions::default()
        };
        let (engine, status) = open_audio(&options, &Settings::default());
        assert!(engine.is_none());
        assert!(status.contains("--no-audio"), "status was {status}");
    }

    #[test]
    fn screenshot_settings_tab_parses_and_rejects_unknown_names() {
        let path = PathBuf::from("/tmp/audio-settings.png");
        let options = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-settings".to_string(), "audio".to_string()),
        ])
        .expect("audio settings screenshot");
        assert!(options.screenshot_settings);
        assert_eq!(options.screenshot_settings_tab, Some(SettingsTab::Audio));

        let options = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-settings-tab".to_string(), "textures".to_string()),
        ])
        .expect("explicit tab flag");
        assert!(options.screenshot_settings);
        assert_eq!(options.screenshot_settings_tab, Some(SettingsTab::Textures));

        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-settings".to_string(), "nope".to_string()),
        ])
        .expect_err("unknown tab");
        assert!(error.contains("unknown settings tab"));

        let error = options_from_args(&[("screenshot-settings-tab".to_string(), String::new())])
            .expect_err("tab name is required");
        assert!(error.contains("requires a tab name"));

        // `--screenshot-settings=audio` goes through the shared split-on-`=`
        // argument parser as a single token.
        let (flags, free) = ob_core::parse_args(
            ["--screenshot-settings=audio", "--screenshot", "/tmp/alias.png"]
                .into_iter()
                .map(String::from),
        );
        assert!(free.is_empty());
        let options = options_from_args(&flags).expect("equals-form audio screenshot");
        assert_eq!(options.screenshot_settings_tab, Some(SettingsTab::Audio));
        assert_eq!(options.screenshot.as_deref(), Some(Path::new("/tmp/alias.png")));
    }

    #[test]
    fn audio_material_mapping_covers_gameplay_blocks() {
        let registry = Registry::base();
        let material = |name: &str| {
            let block = registry.id(name).expect("block exists");
            audio_material_for(&registry, block).expect("material")
        };
        assert_eq!(material("openblock:grass"), AudioMaterial::Grass);
        assert_eq!(material("openblock:leaves"), AudioMaterial::Grass);
        assert_eq!(material("openblock:dirt"), AudioMaterial::Dirt);
        assert_eq!(material("openblock:stone"), AudioMaterial::Stone);
        assert_eq!(material("openblock:cobblestone"), AudioMaterial::Stone);
        assert_eq!(material("openblock:sand"), AudioMaterial::Sand);
        assert_eq!(material("openblock:snow"), AudioMaterial::Snow);
        assert_eq!(material("openblock:log"), AudioMaterial::Wood);
        assert_eq!(material("openblock:planks"), AudioMaterial::Wood);
        assert_eq!(material("openblock:gravel"), AudioMaterial::Gravel);
        assert_eq!(audio_material_for(&registry, BlockId::AIR), None);
        let water = registry.id("openblock:water").expect("water");
        assert_eq!(audio_material_for(&registry, water), None);
    }

    #[test]
    fn network_and_name_tag_screenshots_are_offscreen_only_modes() {
        for flag in ["screenshot-network", "screenshot-name-tags"] {
            let error = options_from_args(&[(flag.to_string(), String::new())]).expect_err("needs a screenshot path");
            assert!(error.contains("--screenshot PATH"), "{flag}: {error}");
            let options = options_from_args(&[
                ("screenshot".to_string(), "/tmp/net.png".to_string()),
                (flag.to_string(), String::new()),
            ])
            .expect("valid offscreen mode");
            assert!(options.screenshot.is_some());
            assert_eq!(
                usize::from(options.screenshot_network) + usize::from(options.screenshot_name_tags),
                1
            );
        }
        let error = options_from_args(&[
            ("screenshot".to_string(), "/tmp/net.png".to_string()),
            ("screenshot-network".to_string(), String::new()),
            ("screenshot-name-tags".to_string(), String::new()),
        ])
        .expect_err("one mode only");
        assert!(error.contains("cannot be combined"));
    }

    #[test]
    fn net_ui_audit_variants_parse_and_require_a_path() {
        for (name, expected) in [
            ("loading", NetUiKind::Loading),
            ("failed", NetUiKind::Failed),
            ("disconnected", NetUiKind::Disconnected),
            ("roster", NetUiKind::Roster),
        ] {
            let options = options_from_args(&[
                ("screenshot".to_string(), "/tmp/net-ui.png".to_string()),
                ("screenshot-net-ui".to_string(), name.to_string()),
            ])
            .expect("valid net ui mode");
            assert_eq!(options.screenshot_net_ui, Some(expected));
            let error = options_from_args(&[("screenshot-net-ui".to_string(), name.to_string())])
                .expect_err("needs screenshot path");
            assert!(error.contains("--screenshot PATH"));
        }
        let error = options_from_args(&[
            ("screenshot".to_string(), "/tmp/net-ui.png".to_string()),
            ("screenshot-net-ui".to_string(), "nope".to_string()),
        ])
        .expect_err("unknown variant");
        assert!(error.contains("unknown --screenshot-net-ui variant"));
    }

    #[test]
    fn container_demo_variants_parse_and_reject_unknown_values() {
        let options = options_from_args(&[
            ("screenshot".to_string(), "/tmp/furnace.png".to_string()),
            ("screenshot-furnace".to_string(), "empty".to_string()),
        ])
        .expect("empty furnace");
        assert_eq!(options.screenshot_furnace_demo, ContainerDemo::Empty);
        let options = options_from_args(&[
            ("screenshot".to_string(), "/tmp/chest.png".to_string()),
            ("screenshot-chest".to_string(), "full".to_string()),
        ])
        .expect("full chest");
        assert_eq!(options.screenshot_chest_demo, ContainerDemo::Full);
        for flag in ["screenshot-furnace", "screenshot-chest"] {
            let error = options_from_args(&[
                ("screenshot".to_string(), "/tmp/container.png".to_string()),
                (flag.to_string(), "nope".to_string()),
            ])
            .expect_err("unknown variant");
            assert!(error.contains("unknown --") && error.contains("variant"));
        }
    }

    #[test]
    fn screenshot_size_parses_and_clamps_audit_captures() {
        let options = options_from_args(&[
            ("screenshot".to_string(), "/tmp/size.png".to_string()),
            ("screenshot-size".to_string(), "960x540".to_string()),
        ])
        .expect("valid size");
        assert_eq!(screenshot_dimensions(&options), (960, 540));
        let error =
            options_from_args(&[("screenshot-size".to_string(), "960".to_string())]).expect_err("missing height");
        assert!(error.contains("1280x720"));
        let error =
            options_from_args(&[("screenshot-size".to_string(), "100x100".to_string())]).expect_err("too small");
        assert!(error.contains("320-3840"));
    }

    #[test]
    fn ui_scale_flag_accepts_auto_and_supported_percents_only() {
        for (value, expected) in [
            ("auto", Some(0_u16)),
            ("AUTO", Some(0)),
            ("75", Some(75)),
            ("100", Some(100)),
            ("125%", Some(125)),
            ("150", Some(150)),
        ] {
            let options = options_from_args(&[("ui-scale".to_string(), value.to_string())])
                .unwrap_or_else(|error| panic!("{value}: {error}"));
            assert_eq!(options.ui_scale, expected, "{value}");
        }
        for value in ["50", "110", "151", "many"] {
            let error = options_from_args(&[("ui-scale".to_string(), value.to_string())])
                .expect_err("unsupported scale must fail");
            assert!(error.contains("--ui-scale"), "{error}");
        }
        let options = options_from_args(&[
            ("ui-scale".to_string(), "150".to_string()),
            ("render-distance".to_string(), "8".to_string()),
        ])
        .expect("override");
        let (settings, _) = effective_settings(&options);
        assert_eq!(settings.ui_scale_percent, 150);
        // The CLI override must not force a disk write by itself.
        assert!(options.data_dir.is_none());
    }

    #[test]
    fn narrow_item_label_moves_below_the_crosshair_and_clears_the_hud() {
        let screen = [960.0, 540.0];
        let hotbar = UiState::default().hotbar_rects(screen);
        let label_y = selected_name_y(screen, &hotbar);
        let (hearts, hunger) = ui_state::vitals_layout(screen);
        let armor = ui_state::armor_layout(screen);
        let label_bottom = label_y + ob_render::line_height(2.0);
        for rect in hearts.iter().chain(hunger.iter()).chain(armor.iter()) {
            assert!(
                label_bottom <= rect.y || label_y >= rect.y + rect.h,
                "held-item label overlaps a vital row at {screen:?}"
            );
        }
        assert!(
            (260.0..=320.0).contains(&label_y),
            "label stays near the crosshair at 960x540"
        );
        let wide = [1280.0, 720.0];
        let hotbar = UiState::default().hotbar_rects(wide);
        assert_eq!(
            selected_name_y(wide, &hotbar),
            hotbar[0].y - 30.0,
            "wide screens keep the classic label position"
        );
    }

    #[test]
    fn screenshot_ui_demo_flags_parse_and_require_survival() {
        let path = PathBuf::from("/tmp/ui-demo.png");
        for flag in ["screenshot-mobs", "screenshot-furnace", "screenshot-chest"] {
            let options = options_from_args(&[
                ("screenshot".to_string(), path.to_string_lossy().into_owned()),
                (flag.to_string(), String::new()),
            ])
            .expect("valid UI demo screenshot");
            assert!(options.screenshot.is_some());
            match flag {
                "screenshot-mobs" => assert!(options.screenshot_mobs),
                "screenshot-furnace" => assert!(options.screenshot_furnace),
                "screenshot-chest" => assert!(options.screenshot_chest),
                _ => unreachable!(),
            }
            let error = options_from_args(&[
                ("screenshot".to_string(), path.to_string_lossy().into_owned()),
                (flag.to_string(), String::new()),
                ("creative".to_string(), String::new()),
            ])
            .expect_err("UI demo screenshots need survival");
            assert!(error.contains("survival mode"));

            let error =
                options_from_args(&[(flag.to_string(), String::new())]).expect_err("UI demo screenshots need a path");
            assert!(error.contains("--screenshot PATH"));
        }

        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-mobs".to_string(), String::new()),
            ("screenshot-furnace".to_string(), String::new()),
        ])
        .expect_err("UI demo modes cannot be combined");
        assert!(error.contains("cannot be combined"));

        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-chest".to_string(), String::new()),
            ("screenshot-dead".to_string(), String::new()),
        ])
        .expect_err("UI demo modes cannot combine with the death screen");
        assert!(error.contains("cannot be combined"));
    }

    #[test]
    fn screenshot_debug_flags_parse_and_require_survival_screenshots() {
        let path = PathBuf::from("/tmp/survival.png");
        let options = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-dead".to_string(), String::new()),
            ("survival".to_string(), String::new()),
        ])
        .expect("valid dead screenshot");
        assert_eq!(options.screenshot.as_deref(), Some(path.as_path()));
        assert!(options.screenshot_dead);
        assert!(!options.screenshot_items);
        assert_eq!(options.game_mode, Some(SettingsGameMode::Survival));

        let options = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-debug".to_string(), String::new()),
        ])
        .expect("valid debug screenshot");
        assert!(options.screenshot_debug);
        assert!(!options.screenshot_dead);
        let error = options_from_args(&[("screenshot-debug".to_string(), String::new())])
            .expect_err("debug screenshot needs a path");
        assert!(error.contains("--screenshot PATH"));
        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-debug".to_string(), String::new()),
            ("screenshot-dead".to_string(), String::new()),
        ])
        .expect_err("screenshot modes cannot be combined");
        assert!(error.contains("cannot be combined"));
        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-debug".to_string(), String::new()),
            ("creative".to_string(), String::new()),
        ])
        .expect_err("debug screenshot needs survival");
        assert!(error.contains("survival mode"));

        let options = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-items".to_string(), String::new()),
        ])
        .expect("valid items screenshot");
        assert!(options.screenshot_items);
        assert_eq!(options.game_mode, None, "debug flags do not persist a mode");

        let error = options_from_args(&[("screenshot-dead".to_string(), String::new())])
            .expect_err("dead screenshot needs a path");
        assert!(error.contains("--screenshot PATH"));

        let error = options_from_args(&[
            ("screenshot".to_string(), path.to_string_lossy().into_owned()),
            ("screenshot-dead".to_string(), String::new()),
            ("creative".to_string(), String::new()),
        ])
        .expect_err("dead screenshot needs survival");
        assert!(error.contains("survival mode"));

        // Debug flags force survival at runtime, even if persisted settings are
        // creative, without persisting that forced mode themselves.
        let dir = std::env::temp_dir().join(format!(
            "openblock-client-mode-force-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        save_settings(
            &dir,
            &Settings {
                game_mode: SettingsGameMode::Creative,
                ..Settings::default()
            },
        )
        .expect("save seed settings");
        let options = ClientOptions {
            data_dir: Some(dir.clone()),
            screenshot: Some(path),
            screenshot_dead: true,
            ..ClientOptions::default()
        };
        let (settings, _) = effective_settings(&options);
        assert_eq!(settings.game_mode, SettingsGameMode::Survival);

        let options = ClientOptions {
            data_dir: Some(dir.clone()),
            screenshot: Some(PathBuf::from("/tmp/debug.png")),
            screenshot_debug: true,
            ..ClientOptions::default()
        };
        let (settings, _) = effective_settings(&options);
        assert_eq!(settings.game_mode, SettingsGameMode::Survival);
        assert_eq!(
            load_settings(&dir).game_mode,
            SettingsGameMode::Creative,
            "debug screenshots must not rewrite the saved mode"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
