//! The game client: window, input, chunk streaming, integrated host and renderer.
//!
//! v0.0.1 is a single-player tech preview: the client owns an [`ob_server::Host`]
//! directly (the same authoritative simulation the dedicated server uses) and
//! renders its world through the wgpu renderer.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ob_content::{BlockId, Registry};
use ob_core::config::{GameMode as SettingsGameMode, Settings, TextureStyle, load_settings, save_settings};
use ob_math::{BlockPos, ChunkPos, SEA_LEVEL, SectionPos, WorldVec};
use ob_render::{
    BlockOutline, Camera, DAY_LENGTH_TICKS, EntityInstance, Environment, OffscreenRenderer, Renderer, SceneParams,
    TextureSource, UiFrame,
};
use ob_server::{GameMode as HostGameMode, Host, HostConfig, ItemEntity};
use ob_sim::{BlockAction, PlayerInput, REACH};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

pub mod crafting;
pub mod inventory;
pub mod mesh_scheduler;
pub mod mining;
pub mod ui_state;

use crafting::{CraftingRow, crafting_rows};
use inventory::inventory_blocks;
use mesh_scheduler::MeshScheduler;
use mining::MiningState;
use ui_state::{
    DamageFlash, DeathAction, DebugColumns, DebugStats, Screen, SettingsAction, SettingsInfo, SettingsRow, UiState,
    facing_label,
};

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
    /// External Minecraft-style resource pack for this run; persisted when given.
    pub texture_pack: Option<std::path::PathBuf>,
    /// Clear the persisted resource pack and use the procedural style.
    pub no_texture_pack: bool,
    /// Time-of-day override in 0..=1 for screenshots (0 midnight, 0.5 noon).
    pub time_of_day: Option<f32>,
    /// Explicit gameplay-mode override for this run (persisted when given).
    pub game_mode: Option<SettingsGameMode>,
    /// Render the death screen in the offscreen screenshot frame.
    pub screenshot_dead: bool,
    /// Render the settings screen in the offscreen screenshot frame.
    pub screenshot_settings: bool,
    /// Render the crafting screen in the offscreen screenshot frame.
    pub screenshot_crafting: bool,
    /// Render the survival HUD with the F3 debug overlay in the screenshot.
    pub screenshot_debug: bool,
    /// Seed the host with sample items and vitals for the offscreen screenshot.
    pub screenshot_items: bool,
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
            time_of_day: None,
            game_mode: None,
            screenshot_dead: false,
            screenshot_settings: false,
            screenshot_crafting: false,
            screenshot_debug: false,
            screenshot_items: false,
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
        let mode = if options.screenshot_settings {
            ScreenshotMode::Settings
        } else if options.screenshot_crafting {
            ScreenshotMode::Crafting
        } else if options.screenshot_dead {
            ScreenshotMode::Dead
        } else if options.screenshot_debug {
            ScreenshotMode::Debug
        } else {
            ScreenshotMode::Hud
        };
        return screenshot_with_mode(&options, path, mode);
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
    /// Modal death screen over the world.
    Dead,
    /// Tabbed settings screen over the world with a deterministic selection.
    Settings,
    /// Crafting screen over the world with a deterministic selection.
    Crafting,
    /// Survival HUD with the F3 debug overlay and deterministic statistics.
    Debug,
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
    let (mut settings, data_dir) = effective_settings(options);
    persist_texture_pack_choice(options, &mut settings, &data_dir);
    persist_game_mode_choice(options, &mut settings, &data_dir);
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
    if options.screenshot_items || matches!(mode, ScreenshotMode::Crafting | ScreenshotMode::Debug) {
        seed_screenshot_items(&mut host, &registry);
    }
    if mode == ScreenshotMode::Crafting {
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

    let (eye, yaw, pitch) = {
        let player = host.player();
        (player.eye(), player.yaw, player.pitch)
    };
    let mut camera = Camera::new(eye, yaw, pitch);
    camera.far = (host.render_distance_chunks() as f32 * 16.0 + 48.0).max(128.0);

    let (mut renderer, texture_source) = create_offscreen_renderer(options, &settings)?;
    log_texture_source(&texture_source);

    // Compose a showcase frame: HUD, inventory or death screen over the world.
    // Screenshot mode is also the headless verification path for the UI
    // renderer, since it needs no window or surface.
    let screen = [SCREENSHOT_WIDTH as f32, SCREENSHOT_HEIGHT as f32];
    let mut ui = UiState::from_settings(settings.clone());
    ui.active_texture_pack = active_texture_pack_label(&texture_source);
    ui.hotbar_selected = host.hotbar_selection();
    if options.screenshot_items || matches!(mode, ScreenshotMode::Crafting | ScreenshotMode::Debug) {
        ui.toasts.push("Picked up Cobblestone x8");
        ui.toasts.push("Picked up Apple x3");
        ui.toasts.push_warning("Inventory full - dropped Log x2");
        ui.block_name_seconds = ui_state::BLOCK_NAME_SECONDS;
    }
    let environment = options.time_of_day.map(Environment::new).unwrap_or_default();
    // The debug screenshot reports real world counters. Render the world once
    // without the overlay to fill `RenderStats` deterministically, then compose
    // the final frame with the HUD and F3 overlay on top.
    let debug_render_stats = if mode == ScreenshotMode::Debug {
        renderer.render_world_scene(
            host.world(),
            &camera,
            &SceneParams {
                environment,
                ui: None,
                outline: None,
            },
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
                ui.inventory.selected = host.hotbar_selection();
                ui.inventory.hovered = Some(host.hotbar_selection());
                inventory::draw_survival(
                    &mut frame,
                    &ui,
                    &registry,
                    host.inventory(),
                    host.hotbar_selection(),
                    host.near_crafting_table(),
                    screen,
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
            // Keep the hotbar visible above the dimmed world, as in gameplay.
            draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
        }
        ScreenshotMode::Dead => {
            ui.open_death();
            draw_vitals_for_mode(&mut frame, &ui, &host, screen);
            draw_hotbar_for_mode(&mut frame, &ui, &registry, &host, screen);
            ui_state::draw_death_screen(&mut frame, &ui, screen);
        }
        ScreenshotMode::Settings => {
            ui.open_settings();
            let info = SettingsInfo {
                health: host.player().health,
                hunger: host.player().hunger,
                texture_source: texture_source.label(),
                texture_resolution: renderer.texture_resolution(),
                mesh_workers: options.mesh_workers,
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
            if let Some(index) = rows.iter().position(|row| row.output_name == "Stone Pickaxe") {
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
            if survival {
                ui_state::draw_toasts(&mut frame, &ui.toasts, screen);
            }
            log::info!(
                "crafting screenshot: {} recipes, near table {}, selected {}, mode {}",
                rows.len(),
                host.near_crafting_table(),
                rows.get(ui.crafting.selected)
                    .map(|row| row.output_name.as_str())
                    .unwrap_or("none"),
                host.game_mode().name(),
            );
        }
        ScreenshotMode::Debug => {
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
                "debug screenshot: {} stats lines, scale {:.1}, columns {:?}, overlay {}x{}",
                stats.left_lines().len(),
                layout.left.scale,
                layout.columns,
                layout.left.panel.w,
                layout.left.panel.h,
            );
        }
    }
    let entities = entity_instances_for(host.item_entities(), &registry, 0.8);
    renderer.set_entities(&entities);
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
        "screenshot written to {} ({}x{}, GPU {}, mode {}, health {:.1}, hunger {:.1})",
        path.display(),
        SCREENSHOT_WIDTH,
        SCREENSHOT_HEIGHT,
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
        cached_sections: host.world().chunk_count(),
        triangles: render_stats.vertices / 3,
        draw_calls: render_stats.draw_calls as usize,
        mesh_workers: options.mesh_workers,
        mesh_in_flight: 0,
        mesh_completed: 0,
        mesh_backlog: 0,
        heap_bytes: host.memory_bytes(),
        ticks: host.tick_count(),
        adapter: renderer.adapter_summary().to_string(),
        window: [SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT],
        fov_degrees: settings.fov_degrees,
        time_of_day: 0.5,
        texture_source: describe_texture_source(texture_source, renderer.texture_resolution()),
        game_mode: host.game_mode().name(),
        health: player.health,
        hunger: player.hunger,
        movement: "walk",
        held,
        held_durability,
        mining_speed,
    }
}

/// Gives the screenshot host a small, deterministic inventory and wounded vitals.
///
/// Tools are worn by breaking a few leaves so the HUD/inventory screenshots
/// show a partially filled durability bar. This uses only the public host API;
/// the leaves are restored to the block that was there before seeding.
fn seed_screenshot_items(host: &mut Host, registry: &Registry) {
    for (name, count) in [
        ("openblock:stone", 8_u8),
        ("openblock:apple", 3),
        ("openblock:log", 2),
        ("openblock:planks", 16),
        ("openblock:stick", 8),
        ("openblock:cobblestone", 16),
        ("openblock:crafting_table", 1),
    ] {
        if let Some(item) = registry.item_id(name) {
            host.give_item(item, count);
        }
    }
    for name in ["openblock:wooden_pickaxe", "openblock:stone_pickaxe"] {
        if let Some(item) = registry.item_id(name) {
            host.give_item(item, 1);
        }
    }
    let slot_of = |host: &Host, name: &str| {
        let item = registry.item_id(name)?;
        host.inventory()
            .iter()
            .position(|stack| stack.is_some_and(|stack| stack.item == item))
    };
    let wood_slot = slot_of(host, "openblock:wooden_pickaxe");
    let stone_slot = slot_of(host, "openblock:stone_pickaxe");
    let leaves = registry.id_or_air("openblock:leaves");
    if leaves != BlockId::AIR {
        if let Some(slot) = wood_slot {
            wear_tool_on_leaves(host, leaves, slot, 7);
        }
        if let Some(slot) = stone_slot {
            wear_tool_on_leaves(host, leaves, slot, 13);
        }
    }
    host.set_hotbar_selection(stone_slot.unwrap_or(0));
    log::info!(
        "screenshot items: selected slot {} held {:?}",
        host.hotbar_selection(),
        host.selected_stack()
    );
    // A few deterministic dropped-item entities in front of the player show
    // the entity renderer in the HUD screenshots.
    let eye = host.player().eye();
    let forward = host.player().forward();
    let base = WorldVec::new(
        eye.x + forward[0] * 3.0,
        host.player().pos.y + 0.2,
        eye.z + forward[2] * 3.0,
    );
    for (index, (name, count)) in [
        ("openblock:planks", 2_u8),
        ("openblock:stick", 1),
        ("openblock:stone_pickaxe", 1),
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

/// Wears one durability point per successful leaf break without leaving a hole.
fn wear_tool_on_leaves(host: &mut Host, leaves: BlockId, slot: usize, breaks: usize) {
    if breaks == 0 {
        return;
    }
    let saved_yaw = host.player().yaw;
    let saved_pitch = host.player().pitch;
    {
        let player = host.player_mut();
        player.yaw = 0.0;
        player.pitch = -1.5533;
    }
    let eye = host.player().eye();
    let forward = host.player().forward();
    let target = host
        .world()
        .raycast(eye, forward, REACH)
        .map(|hit| hit.block)
        .or_else(|| {
            Some(BlockPos::new(
                eye.x.floor() as i32,
                (eye.y - 1.0).floor() as i32,
                eye.z.floor() as i32,
            ))
        });
    if let Some(pos) = target {
        let original = host.world().get_block(pos);
        host.set_hotbar_selection(slot);
        for _ in 0..breaks {
            host.world_mut().set_block(pos, leaves);
            let action = BlockAction {
                break_block: true,
                place_block: false,
                place: BlockId::AIR,
            };
            if !host.apply_action_with_outcome(&action).changed {
                break;
            }
        }
        if original != BlockId::AIR {
            host.world_mut().set_block(pos, original);
        }
    }
    let player = host.player_mut();
    player.yaw = saved_yaw;
    player.pitch = saved_pitch;
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
/// mapping), while non-block items use a flat tint: red apple, wood-brown
/// stick, brown wooden tools and grey stone tools. Position adds the requested
/// 0.15 m bob and the yaw spins deterministically with time and entity id.
pub fn entity_instance_for(entity: &ItemEntity, registry: &Registry, elapsed: f32) -> EntityInstance {
    let position = [entity.pos.x as f32, entity.pos.y as f32 + 0.15, entity.pos.z as f32];
    let yaw = elapsed * 1.8 + entity.id as f32 * 0.7;
    let stack = entity.stack;
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
    if item.0 as usize >= registry.item_len() {
        return [0.80, 0.80, 0.80];
    }
    let item_type = registry.item(item);
    if item_type.id == "openblock:apple" {
        return [0.78, 0.16, 0.12];
    }
    if item_type.id == "openblock:stick" {
        return [0.55, 0.36, 0.18];
    }
    if let Some(spec) = item_type.tool {
        return if spec.tier <= 1 {
            [0.62, 0.43, 0.22]
        } else {
            [0.55, 0.56, 0.58]
        };
    }
    [0.80, 0.80, 0.80]
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
    }
}

/// Draws the selected hotbar slot's display name, if the name timer is active.
fn draw_selected_name(frame: &mut UiFrame, ui: &UiState, registry: &Registry, host: &Host, screen: [f32; 2]) {
    if ui.block_name_seconds <= 0.0 {
        return;
    }
    let label = if host.game_mode() == HostGameMode::Survival {
        host.selected_stack()
            .map(|stack| inventory::display_name(registry.item(stack.item).id))
    } else {
        ui.hotbar_block(registry, ui.hotbar_selected)
            .map(|block| inventory::display_name(block.id))
    };
    if let Some(label) = label {
        let y = ui
            .hotbar_rects(screen)
            .first()
            .map(|slot| slot.y - 30.0)
            .unwrap_or(screen[1] - 70.0);
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
            Screen::Settings => {
                self.ui.close_settings();
                self.capture_pointer();
            }
            Screen::Death => {
                // Focus the buttons instead of opening settings.
                self.ui.focus_death_buttons();
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
        self.ui.open_death();
        self.release_pointer();
        self.damage_flash.observe(self.host.player().health);
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
            let name = inventory::display_name(self.registry.item(stack.item).id);
            self.ui.toasts.push(format!("Picked up {name} x{}", stack.count));
        }
    }

    /// Applies the live side effect of a settings row and persists settings.
    fn apply_settings_row(&mut self, row: SettingsRow) {
        match row {
            SettingsRow::RenderDistance => {
                let requested = self.ui.settings.render_distance_chunks;
                if self.host.set_render_distance_chunks(requested) {
                    self.applied_render_distance = requested;
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
            SettingsRow::ShowHud
            | SettingsRow::DebugOverlay
            | SettingsRow::Fov
            | SettingsRow::MouseSensitivity
            | SettingsRow::MeshWorkers => {}
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
            texture_source: self.texture_source_description(),
            game_mode: self.host.game_mode().name(),
            health: player.health,
            hunger: player.hunger,
            held,
            held_durability,
            mining_speed,
            movement: if player.flying {
                "fly"
            } else if player.sprinting {
                "sprint"
            } else if player.sneaking {
                "sneak"
            } else {
                "walk"
            },
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
        }
    }

    fn update_title(&mut self) {
        let player = self.host.player();
        let survival = self.host.game_mode() == HostGameMode::Survival;
        let block = if survival {
            self.host
                .selected_stack()
                .map(|stack| inventory::display_name(self.registry.item(stack.item).id))
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
            Screen::Settings => " | SETTINGS (Esc to resume)",
            Screen::Death => " | DEAD (Enter respawn)",
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
        if self.host.is_dead() && self.ui.screen != Screen::Death {
            self.begin_death_screen();
        } else if !self.host.is_dead() && self.ui.screen == Screen::Death {
            // A creative-mode switch (or an external respawn) revives the player.
            self.ui.clear_death();
            self.capture_pointer();
        }
        self.accumulator += dt.min(Duration::from_millis(250));
        let mut ticks = 0;
        if self.ui.screen == Screen::Playing && !self.host.is_dead() {
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
        if self.host.game_mode() == HostGameMode::Survival {
            self.damage_flash.observe(self.host.player().health);
        } else {
            self.damage_flash.reset();
        }
        self.damage_flash.tick(dt.as_secs_f32());
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
        // Sprinting slightly widens the FOV; crouching leaves it alone.
        let target_fov = self.ui.settings.fov_degrees * if player.sprinting { 1.10 } else { 1.0 };
        let blend = 1.0 - (-dt.as_secs_f32() * 12.0).exp();
        self.current_fov += (target_fov - self.current_fov) * blend;
        camera.fov_y = self.current_fov.to_radians();
        camera.far = (self.host.render_distance_chunks() as f32 * 16.0 + 48.0).max(128.0);
        self.stream_and_mesh();
        // Keep the window title in sync with the effective render distance.
        if self.applied_render_distance != self.host.render_distance_chunks() {
            self.applied_render_distance = self.host.render_distance_chunks();
        }
        // Day starts at sunrise: tick 0 is 06:00 rather than midnight.
        let environment = Environment::from_ticks(self.host.tick_count() + DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS);
        self.refresh_debug_stats();
        let elapsed = self.started.elapsed().as_secs_f32();
        let entity_instances = entity_instances_for(self.host.item_entities(), &self.registry, elapsed);
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
        // Build the UI for the offscreen target size so the HUD is included.
        let ui_frame = if ui.is_some() {
            self.build_ui_frame_at([SCREENSHOT_WIDTH as f32, SCREENSHOT_HEIGHT as f32])
        } else {
            None
        };
        let elapsed = self.started.elapsed().as_secs_f32();
        let entities = entity_instances_for(self.host.item_entities(), &self.registry, elapsed);
        renderer.set_entities(&entities);
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

    /// Builds the UI frame for the current window size.
    fn build_ui_frame(&self) -> Option<UiFrame> {
        self.build_ui_frame_at(self.window_screen())
    }

    /// Builds the UI frame for an explicit target size (screenshots).
    fn build_ui_frame_at(&self, screen: [f32; 2]) -> Option<UiFrame> {
        let [width, height] = screen;
        let playing = self.ui.screen == Screen::Playing;
        let inventory_open = self.ui.screen == Screen::Inventory;
        let crafting_open = self.ui.screen == Screen::Crafting;
        let death_open = self.ui.screen == Screen::Death;
        let survival = self.host.game_mode() == HostGameMode::Survival;
        let show_hotbar = (playing || inventory_open || death_open) && self.ui.hud_visible;
        let show_crosshair = playing && self.ui.hud_visible && !self.host.is_dead();
        let show_debug = self.ui.debug_overlay;
        let settings_open = self.ui.screen == Screen::Settings;
        if !show_hotbar
            && !show_crosshair
            && !show_debug
            && !settings_open
            && !inventory_open
            && !crafting_open
            && !death_open
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
                    self.host.inventory(),
                    self.host.hotbar_selection(),
                    self.host.near_crafting_table(),
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
            if self.ui.hud_visible {
                ui_state::draw_toasts(&mut frame, &self.ui.toasts, screen);
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
        let survival = self.host.game_mode() == HostGameMode::Survival;
        let outcome = self.host.apply_action_with_outcome(&action);
        if outcome.changed {
            if survival && !outcome.drops.is_empty() {
                // Survival breaks spawn item entities; this is only a subtle
                // acknowledgement, not a pickup (pickup toasts come from the
                // host's drained pickup events).
                for drop in &outcome.drops {
                    let name = inventory::display_name(self.registry.item(drop.item).id);
                    self.ui.toasts.push(format!("Dropped {name} x{}", drop.count));
                }
            }
            if let Some(item) = outcome.tool_broke {
                let name = inventory::display_name(self.registry.item(item).id);
                self.ui.toasts.push_warning(format!("Your {name} broke!"));
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
        if let Some(hit) = self.host.world().raycast(player.eye(), player.forward(), REACH)
            && self.registry.block(self.host.world().get_block(hit.block)).id == "openblock:crafting_table"
        {
            self.open_crafting();
            return;
        }
        if self.host.game_mode() == HostGameMode::Survival {
            let stack = self.host.selected_stack();
            if inventory::should_eat(&self.registry, stack, self.host.player().hunger) {
                let name = stack
                    .map(|stack| inventory::display_name(self.registry.item(stack.item).id))
                    .unwrap_or_else(|| "item".to_string());
                if self.host.eat_selected() {
                    self.ui.toasts.push(format!("Ate {name}"));
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
        let _ = self.host.apply_action_with_outcome(&action);
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
        } else {
            let item_count = self.inventory_item_count();
            self.ui.update_inventory_hover(screen, item_count);
        }
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
        let Some(index) = self.ui.inventory.hovered else {
            return;
        };
        self.ui.inventory.selected = index;
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
    let left_lines = stats.left_lines();
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
    frame.text(
        ui_state::DEBUG_MARGIN + 4.0,
        layout.footer_y,
        ui_state::DEBUG_FOOTER_SCALE,
        [0.80, 0.86, 0.94, 0.9],
        "F3 debug overlay   F1 HUD   F2 screenshot   Esc menu",
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
        [0.03, 0.04, 0.07, 0.76],
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
        let (renderer, texture_source) = match create_window_renderer(Arc::clone(&window), &self.options, &settings) {
            Ok(pair) => pair,
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
        let mut ui = UiState::from_settings(settings);
        ui.active_texture_pack = active_texture_pack_label(&texture_source);
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
                        KeyCode::ShiftRight => state.keys[7] = pressed,
                        KeyCode::ControlLeft => state.keys[6] = pressed,
                        KeyCode::KeyF if pressed && state.ui.screen == Screen::Playing && !state.host.is_dead() => {
                            state.fly_toggle = true;
                        }
                        KeyCode::KeyE if pressed => match state.ui.screen {
                            Screen::Playing if !state.host.is_dead() => state.open_inventory(),
                            Screen::Inventory => state.close_inventory(),
                            Screen::Crafting => state.close_crafting(),
                            Screen::Settings | Screen::Death => {}
                            Screen::Playing => {}
                        },
                        KeyCode::KeyC if pressed => state.toggle_crafting(),
                        KeyCode::Escape if pressed => state.handle_escape(),
                        KeyCode::F1 if pressed && overlay_shortcuts_allowed(state.ui.screen) => {
                            state.ui.hud_visible = !state.ui.hud_visible;
                            state.persist_settings();
                        }
                        KeyCode::F2 if pressed && state.ui.screen == Screen::Playing && !state.host.is_dead() => {
                            state.screenshot_requested = true;
                        }
                        KeyCode::F3 if pressed && overlay_shortcuts_allowed(state.ui.screen) => {
                            state.ui.toggle_debug_overlay();
                            state.persist_settings();
                        }
                        KeyCode::Tab if pressed => match state.ui.screen {
                            Screen::Settings => {
                                if state.keys[5] || state.keys[7] {
                                    state.ui.settings_tab_prev();
                                } else {
                                    state.ui.settings_tab_next();
                                }
                            }
                            Screen::Playing | Screen::Inventory | Screen::Crafting | Screen::Death => {}
                        },
                        KeyCode::ArrowUp if pressed => match state.ui.screen {
                            Screen::Death => state.ui.move_death_selection(-1),
                            Screen::Inventory => state.move_inventory_selection(0, -1),
                            Screen::Settings => state.ui.settings_row_up(),
                            Screen::Crafting => state.move_crafting_selection(-1),
                            Screen::Playing => {}
                        },
                        KeyCode::ArrowDown if pressed => match state.ui.screen {
                            Screen::Death => state.ui.move_death_selection(1),
                            Screen::Inventory => state.move_inventory_selection(0, 1),
                            Screen::Settings => state.ui.settings_row_down(),
                            Screen::Crafting => state.move_crafting_selection(1),
                            Screen::Playing => {}
                        },
                        KeyCode::ArrowLeft if pressed => match state.ui.screen {
                            Screen::Death => state.ui.move_death_selection(-1),
                            Screen::Inventory => state.move_inventory_selection(-1, 0),
                            Screen::Settings => {
                                if let Some(row) = state.ui.settings_adjust(-1) {
                                    state.apply_settings_row(row);
                                }
                            }
                            Screen::Crafting => state.move_crafting_selection(-1),
                            Screen::Playing => {}
                        },
                        KeyCode::ArrowRight if pressed => match state.ui.screen {
                            Screen::Death => state.ui.move_death_selection(1),
                            Screen::Inventory => state.move_inventory_selection(1, 0),
                            Screen::Settings => {
                                if let Some(row) = state.ui.settings_adjust(1) {
                                    state.apply_settings_row(row);
                                }
                            }
                            Screen::Crafting => state.move_crafting_selection(1),
                            Screen::Playing => {}
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
                            Screen::Settings => {
                                if let Some(action) = state.ui.settings_activate()
                                    && state.handle_settings_action(action)
                                {
                                    state.persist_settings();
                                    event_loop.exit();
                                }
                            }
                            Screen::Crafting => state.craft_selected(),
                            Screen::Playing => {}
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
                            if state.ui.screen == Screen::Playing && !state.host.is_dead() =>
                        {
                            if let Some(slot) = digit_slot(code) {
                                state.select_hotbar_slot(slot);
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
                            if let Some(action) = state.ui.settings_click(screen)
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
                    Screen::Settings => {
                        let screen = state.window_screen();
                        state.ui.settings_hover(screen);
                    }
                    Screen::Crafting => state.update_crafting_hover(),
                    Screen::Death => {
                        let screen = state.window_screen();
                        state.ui.update_death_hover(screen);
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
/// requested resource pack cannot be loaded.
fn create_offscreen_renderer(
    options: &ClientOptions,
    settings: &Settings,
) -> Result<(OffscreenRenderer, TextureSource), ob_render::RenderError> {
    let source = resolve_texture_source(options, settings);
    match OffscreenRenderer::new_with_source(SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT, source.clone()) {
        Ok(renderer) => Ok((renderer, source)),
        Err(ob_render::RenderError::Texture(reason)) => {
            if let TextureSource::ResourcePack(path) = &source {
                log::warn!(
                    "could not load texture pack {}: {reason}; falling back to the built-in {} style",
                    path.display(),
                    settings.texture_style.name()
                );
                let style = render_texture_style(settings.texture_style);
                let renderer = OffscreenRenderer::new_with_style(SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT, style)?;
                Ok((renderer, TextureSource::Style(style)))
            } else {
                Err(ob_render::RenderError::Texture(reason))
            }
        }
        Err(err) => Err(err),
    }
}

/// Builds the windowed renderer, falling back to the procedural style when a
/// requested resource pack cannot be loaded.
fn create_window_renderer(
    window: Arc<Window>,
    options: &ClientOptions,
    settings: &Settings,
) -> Result<(Renderer, TextureSource), ob_render::RenderError> {
    let source = resolve_texture_source(options, settings);
    match Renderer::new_with_source(Arc::clone(&window), source.clone()) {
        Ok(renderer) => Ok((renderer, source)),
        Err(ob_render::RenderError::Texture(reason)) => {
            if let TextureSource::ResourcePack(path) = &source {
                log::warn!(
                    "could not load texture pack {}: {reason}; falling back to the built-in {} style",
                    path.display(),
                    settings.texture_style.name()
                );
                let style = render_texture_style(settings.texture_style);
                let renderer = Renderer::new_with_style(window, style)?;
                Ok((renderer, TextureSource::Style(style)))
            } else {
                Err(ob_render::RenderError::Texture(reason))
            }
        }
        Err(err) => Err(err),
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
            "texture-pack" | "resource-pack" => {
                if value.is_empty() {
                    return Err(format!("--{name} requires a path"));
                }
                options.texture_pack = Some(value.into());
            }
            "no-texture-pack" => options.no_texture_pack = true,
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
            "screenshot-dead" => options.screenshot_dead = true,
            "screenshot-settings" => options.screenshot_settings = true,
            "screenshot-crafting" => options.screenshot_crafting = true,
            "screenshot-debug" => options.screenshot_debug = true,
            "screenshot-items" => options.screenshot_items = true,
            "survival" => options.game_mode = Some(SettingsGameMode::Survival),
            "creative" => options.game_mode = Some(SettingsGameMode::Creative),
            "help" | "version" => {}
            other => return Err(format!("unknown option --{other}")),
        }
    }
    if options.screenshot_dead && options.screenshot.is_none() {
        return Err("--screenshot-dead requires --screenshot PATH".into());
    }
    let screenshot_modes = usize::from(options.screenshot_dead)
        + usize::from(options.screenshot_settings)
        + usize::from(options.screenshot_crafting)
        + usize::from(options.screenshot_debug)
        + usize::from(options.screenshot_inventory.is_some());
    if screenshot_modes > 1 {
        return Err("--screenshot screen modes cannot be combined".into());
    }
    if options.screenshot_settings
        && (options.screenshot_dead || options.screenshot_crafting || options.screenshot_inventory.is_some())
    {
        return Err(
            "--screenshot-settings cannot be combined with --screenshot-dead, --screenshot-crafting or --screenshot-inventory"
                .into(),
        );
    }
    if options.screenshot_crafting && (options.screenshot_dead || options.screenshot_inventory.is_some()) {
        return Err("--screenshot-crafting cannot be combined with --screenshot-dead or --screenshot-inventory".into());
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
    if options.screenshot_items && options.screenshot.is_none() && options.screenshot_inventory.is_none() {
        return Err("--screenshot-items requires --screenshot or --screenshot-inventory PATH".into());
    }
    if (options.screenshot_dead || options.screenshot_crafting || options.screenshot_debug || options.screenshot_items)
        && options.game_mode == Some(SettingsGameMode::Creative)
    {
        return Err(
            "--screenshot-dead, --screenshot-crafting, --screenshot-debug and --screenshot-items require survival mode"
                .into(),
        );
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
        assert_eq!(apple.tint, [0.78, 0.16, 0.12]);
        assert_eq!(apple.layer, 0);
        let stick = instance("openblock:stick", 2);
        assert_eq!(stick.tint, [0.55, 0.36, 0.18]);
        let wooden = instance("openblock:wooden_pickaxe", 3);
        assert_eq!(wooden.tint, [0.62, 0.43, 0.22]);
        assert!(!wooden.textured);
        let stone = instance("openblock:stone_axe", 4);
        assert_eq!(stone.tint, [0.55, 0.56, 0.58]);
        assert_eq!(stone.scale, 0.22);
        assert_eq!(stone.layer, 0);
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
