//! Windowed client runtime for `--connect HOST:PORT`.
//!
//! Multiplayer deliberately uses a separate [`ApplicationHandler`] instead of
//! forcing the single-player [`Host`](ob_server::Host) into a remote mode. The
//! local Host owns disk persistence, survival state and its own tick; the
//! network client owns a replicated [`World`](ob_world::World), the
//! server-authoritative survival state and replicated entities. Keeping the
//! two runtimes separate means:
//!
//! * no world is opened, saved or autosaved in network mode;
//! * the game loop pauses simulation (and sends no `Move`) while Settings,
//!   inventory, containers or chat are open;
//! * vitals, inventory, equipment, containers, mobs and dropped items are
//!   driven exclusively by protocol v3 messages, never by a local Host.
//!
//! UI drawing that is shared with single-player comes from the crate root and
//! `inventory`/`containers`/`ui_state`; the multiplayer-specific overlays live
//! in [`crate::net_ui`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_audio::{AudioEngine, Material as AudioMaterial, Sfx as AudioSfx};
use ob_content::{BlockId, Registry};
use ob_core::config::{Action, load_settings, save_settings};
use ob_math::{BlockPos, ChunkPos, SectionPos, WorldVec};
use ob_render::{
    BlockOutline, Camera, Environment, LodBatchKey, LodConfig, Renderer, SceneParams, TextureSource, UiFrame,
};
use ob_server::ContainerState;
use ob_sim::{Player, PlayerInput, REACH};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::containers::{self, ClickButton, ContainerKind, ContainerSession, SlotId};
use crate::inventory;
use crate::lod_state::LodState;
use crate::mesh_scheduler::MeshScheduler;
use crate::mining::MiningState;
use crate::net::{ConnectOptions, NetEvent, NetState, NetworkClient};
use crate::net_state;
use crate::net_ui::{self, ConnectPanel};
use crate::ui_scale::{physical_to_ui_point, rescale_cursor, resolved_ui_scale_percent, ui_scale_factor, ui_screen};
use crate::ui_state::{
    BLOCK_NAME_SECONDS, DamageFlash, DeathAction, DebugStats, HOTBAR_SLOTS as UI_HOTBAR_SLOTS,
    INVENTORY_SLOTS as UI_INVENTORY_SLOTS, Screen, SettingsAction, SettingsInfo, SettingsRow, UiState, facing_label,
};
use crate::{ClientOptions, MAX_MESH_SUBMISSIONS_PER_FRAME, MESH_STATS_INTERVAL, MESH_UPLOAD_BUDGET_US};

/// One fixed simulation tick (20 Hz).
const TICK: Duration = Duration::from_millis(50);
/// Cooldown between right-click block placements.
const PLACE_COOLDOWN: Duration = Duration::from_millis(200);
/// Local cooldown between melee attack intents.
const ATTACK_COOLDOWN: Duration = Duration::from_millis(400);
/// Peak camera pitch offset, in radians, after the player takes damage.
const DAMAGE_SHAKE_RADIANS: f32 = 0.05;
/// Seconds the "multiplayer survival" notice stays visible.
const NOTICE_SECONDS: f32 = 12.0;
/// Death-screen note for the network runtime.
const DEATH_NOTE: &str = "The server keeps your inventory and equipment";
/// Crafting limitation shown on the survival inventory footer.
const CRAFTING_HINT: &str = "Crafting is not available in multiplayer yet";

/// Generates a valid random-ish default player name (`player` + four digits).
pub fn default_player_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64 + elapsed.as_secs())
        .unwrap_or(0);
    format!("player{:04}", nanos % 10_000)
}

/// Runs the windowed multiplayer client until the user quits.
pub(crate) fn run(options: ClientOptions) -> Result<(), Box<dyn std::error::Error>> {
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
    let mut app = NetApp::new(options);
    event_loop.run_app(&mut app)?;
    Ok(())
}

pub(crate) struct NetApp {
    options: ClientOptions,
    state: Option<NetGameState>,
}

impl NetApp {
    pub(crate) fn new(options: ClientOptions) -> Self {
        Self { options, state: None }
    }
}

impl ApplicationHandler for NetApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("OpenBlock {} - Multiplayer", ob_core::VERSION))
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                log::error!("failed to create window: {error}");
                event_loop.exit();
                return;
            }
        };
        let (mut settings, data_dir) = crate::effective_settings(&self.options);
        crate::persist_texture_pack_choice(&self.options, &mut settings, &data_dir);
        crate::persist_ui_font_choice(&self.options, &mut settings, &data_dir);
        let (renderer, texture_source, font_source) =
            match crate::create_window_renderer(Arc::clone(&window), &self.options, &settings, &data_dir) {
                Ok(triple) => triple,
                Err(error) => {
                    log::error!("{error}");
                    log::error!(
                        "OpenBlock requires a working Vulkan driver (Fedora: mesa-vulkan-drivers or the NVIDIA driver)."
                    );
                    event_loop.exit();
                    return;
                }
            };
        crate::log_texture_source(&texture_source);
        log::info!("GPU: {}", renderer.adapter_summary());
        let registry = Arc::new(Registry::base());
        let mesh_workers = self.options.mesh_workers;
        let mesh_scheduler = MeshScheduler::new(mesh_workers, Arc::clone(&registry));
        let window_size = {
            let size = window.inner_size();
            (size.width.max(1) as f32, size.height.max(1) as f32)
        };
        let initial_fov = settings.fov_degrees;
        let mut ui = UiState::from_settings(settings);
        ui.active_texture_pack = crate::active_texture_pack_label(&texture_source);
        ui.active_ui_font_name = renderer.ui_font_name().to_string();
        ui.active_ui_font_source = font_source.display_label();
        ui.ui_scale_percent = resolved_ui_scale_percent(ui.settings.ui_scale_percent, window_size.1);
        ui.screen = Screen::Connecting;
        let applied_render_distance = ui.settings.render_distance_chunks.clamp(
            ob_core::config::MIN_RENDER_DISTANCE,
            ob_core::config::MAX_RENDER_DISTANCE,
        );
        let audio_enabled_before_mute = if self.options.mute {
            Some(load_settings(&data_dir).audio_enabled)
        } else {
            None
        };
        let (audio, audio_status) = crate::open_audio(&self.options, &ui.settings);
        let endpoint = self
            .options
            .connect
            .clone()
            .unwrap_or_else(|| "127.0.0.1:25565".to_string());
        let auto_player_name = self.options.name.is_none();
        let player_name = self.options.name.clone().unwrap_or_else(default_player_name);
        log::info!("multiplayer: connecting to {endpoint} as '{player_name}'");
        let placeholder = Player::new(crate::default_spawn_hint());
        let mut state = NetGameState {
            window,
            renderer,
            texture_source,
            registry,
            mesh_scheduler,
            mesh_stats_timer: Instant::now(),
            mesh_workers,
            keys: [false; 8],
            input_yaw: 0.0,
            input_pitch: 0.0,
            captured: false,
            current_fov: initial_fov,
            accumulator: Duration::ZERO,
            last_frame: Instant::now(),
            prev_eye: placeholder.eye(),
            current_eye: placeholder.eye(),
            frames: 0,
            fps_timer: Instant::now(),
            fps: 0,
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
            lod: LodState::new(LodConfig::for_render_distance(applied_render_distance)),
            rolling_frame_ms: Vec::new(),
            window_size,
            debug_stats: DebugStats::default(),
            damage_flash: DamageFlash::default(),
            camera_shake: 0.0,
            last_camera: Camera::new(placeholder.eye(), 0.0, 0.0),
            audio,
            audio_status,
            audio_enabled_before_mute,
            audio_setting_touched: false,
            placeholder,
            client: None,
            pending_connect: None,
            failure: None,
            endpoint,
            player_name,
            auto_player_name,
            connect_selected: 0,
            connect_hovered: None,
            connect_started: Instant::now(),
            notice_seconds: NOTICE_SECONDS,
            tab_held: false,
            last_place: None,
            attack_ready: None,
            mouse_left: false,
            mining: MiningState::default(),
            container_selected: SlotId::FurnaceInput,
            footstep_distance: 0.0,
            was_in_water: false,
            log_network: true,
        };
        let fullscreen = state.ui.settings.fullscreen;
        state.apply_fullscreen(fullscreen);
        state.fullscreen = state.ui.settings.fullscreen;
        state.start_connect();
        state.update_title();
        self.state = Some(state);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.window.id() != window_id {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                state.shutdown();
                state.persist_settings();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width.max(1), size.height.max(1));
                state.window_size = (size.width.max(1) as f32, size.height.max(1) as f32);
                state.refresh_ui_scale();
            }
            WindowEvent::RedrawRequested => state.frame(),
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let Some(code) = (match event.physical_key {
                    PhysicalKey::Code(code) => Some(code),
                    _ => None,
                }) else {
                    return;
                };
                if state.ui.screen == Screen::Playing && state.chat_active() {
                    if !pressed {
                        return;
                    }
                    match code {
                        KeyCode::Escape => state.close_chat(),
                        KeyCode::Enter => state.submit_chat(),
                        KeyCode::Backspace => {
                            if let Some(client) = &mut state.client {
                                client.chat_input_mut().backspace();
                            }
                        }
                        _ => {
                            if let Some(text) = event.text.as_deref()
                                && let Some(client) = &mut state.client
                            {
                                for character in text.chars() {
                                    client.chat_input_mut().push_char(character);
                                }
                            }
                        }
                    }
                    return;
                }
                if state.ui.screen == Screen::Settings {
                    // Key capture owns every key while the Controls tab is
                    // waiting for a new binding.
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
                                    if let Some(key) = crate::bind_key_for_code(code)
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

                // Gameplay actions resolve through the persisted keybind model.
                if let Some(action) =
                    crate::bind_key_for_code(code).and_then(|key| state.ui.settings.keybinds.action_for(key))
                {
                    if let Some(slot) = crate::movement_slot(action) {
                        state.keys[slot] = pressed;
                    } else if pressed && !event.repeat {
                        match action {
                            Action::Chat => state.open_chat(),
                            Action::Inventory => match state.ui.screen {
                                Screen::Playing if state.can_act() => state.open_inventory(),
                                Screen::Inventory => state.close_inventory(),
                                Screen::Furnace | Screen::Chest => state.close_container_ui(true),
                                Screen::Settings | Screen::Death | Screen::Connecting | Screen::Crafting => {}
                                Screen::Playing => {}
                            },
                            Action::Crafting => {
                                if matches!(state.ui.screen, Screen::Playing | Screen::Inventory) {
                                    state.ui.toasts.push_warning(CRAFTING_HINT);
                                }
                            }
                            Action::DebugOverlay => {
                                if state.ui.screen != Screen::Settings {
                                    state.ui.toggle_debug_overlay();
                                    state.persist_settings();
                                }
                            }
                            Action::ToggleHud => {
                                if state.ui.screen != Screen::Settings {
                                    state.ui.toggle_hud();
                                    state.persist_settings();
                                }
                            }
                            Action::Screenshot => {
                                if state.ui.screen == Screen::Playing {
                                    state
                                        .ui
                                        .toasts
                                        .push("Screenshots are local-mode only in this prototype");
                                }
                            }
                            Action::Pause => {
                                if state.ui.screen == Screen::Connecting {
                                    state.shutdown();
                                    state.persist_settings();
                                    event_loop.exit();
                                    return;
                                }
                                state.handle_escape();
                            }
                            Action::Fly => {
                                if state.ui.screen == Screen::Playing && state.client_playing() {
                                    state
                                        .ui
                                        .toasts
                                        .push_warning("Flight is not available in multiplayer survival");
                                }
                            }
                            Action::Mine | Action::Use => {}
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

                // UI-only keys stay available regardless of bindings.
                match code {
                    KeyCode::Escape if pressed && state.ui.screen != Screen::Playing => {
                        if state.ui.screen == Screen::Connecting {
                            state.shutdown();
                            state.persist_settings();
                            event_loop.exit();
                            return;
                        }
                        state.handle_escape();
                    }
                    KeyCode::KeyR if pressed && !event.repeat && state.ui.screen == Screen::Connecting => {
                        state.retry();
                    }
                    KeyCode::Enter if pressed && !event.repeat && state.ui.screen == Screen::Connecting => {
                        if state.connect_selected == 0 {
                            state.retry();
                        } else {
                            state.shutdown();
                            state.persist_settings();
                            event_loop.exit();
                            return;
                        }
                    }
                    KeyCode::ArrowUp | KeyCode::ArrowDown if pressed && state.ui.screen == Screen::Connecting => {
                        let step = if code == KeyCode::ArrowUp { -1 } else { 1 };
                        state.connect_selected = (state.connect_selected as i32 + step)
                            .rem_euclid(net_ui::CONNECT_ACTIONS.len() as i32)
                            as usize;
                    }
                    KeyCode::Tab => {
                        state.tab_held = pressed;
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
                        if pressed && state.ui.screen == Screen::Playing && state.can_act() =>
                    {
                        if let Some(slot) = digit_slot(code) {
                            state.select_hotbar(slot);
                        }
                    }
                    KeyCode::ArrowUp if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(-1),
                        Screen::Inventory => state.move_inventory_selection(0, -1),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(0, -1),
                        Screen::Playing | Screen::Connecting | Screen::Crafting | Screen::Settings => {}
                    },
                    KeyCode::ArrowDown if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(1),
                        Screen::Inventory => state.move_inventory_selection(0, 1),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(0, 1),
                        Screen::Playing | Screen::Connecting | Screen::Crafting | Screen::Settings => {}
                    },
                    KeyCode::ArrowLeft if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(-1),
                        Screen::Inventory => state.move_inventory_selection(-1, 0),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(-1, 0),
                        Screen::Playing | Screen::Connecting | Screen::Crafting | Screen::Settings => {}
                    },
                    KeyCode::ArrowRight if pressed => match state.ui.screen {
                        Screen::Death => state.ui.move_death_selection(1),
                        Screen::Inventory => state.move_inventory_selection(1, 0),
                        Screen::Furnace | Screen::Chest => state.move_container_selection(1, 0),
                        Screen::Playing | Screen::Connecting | Screen::Crafting | Screen::Settings => {}
                    },
                    KeyCode::Enter if pressed => match state.ui.screen {
                        Screen::Death => {
                            if let Some(action) = state.ui.activate_death() {
                                state.handle_death_action(action);
                            }
                        }
                        Screen::Inventory => {
                            state.assign_inventory_selection();
                        }
                        Screen::Furnace | Screen::Chest => state.activate_container_slot(),
                        Screen::Playing | Screen::Connecting | Screen::Crafting | Screen::Settings => {}
                    },
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
                    if let Some(key) = crate::bind_key_for_mouse(button)
                        && state.ui.apply_bind(key).is_ok()
                    {
                        state.persist_settings();
                    }
                    state.update_title();
                    return;
                }
                match state.ui.screen {
                    Screen::Connecting => {
                        if button != MouseButton::Left {
                            return;
                        }
                        let screen = state.window_screen();
                        match net_ui::connect_action_at_for_state(
                            screen,
                            state.failure.is_some(),
                            state.client.is_some(),
                            state.ui.cursor.0,
                            state.ui.cursor.1,
                        ) {
                            Some(net_ui::ConnectAction::Retry) => {
                                state.connect_selected = 0;
                                state.retry();
                            }
                            Some(net_ui::ConnectAction::Quit) => {
                                state.connect_selected = 1;
                                state.shutdown();
                                state.persist_settings();
                                event_loop.exit();
                            }
                            None => {}
                        }
                        return;
                    }
                    Screen::Settings => {
                        if button != MouseButton::Left {
                            return;
                        }
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
                            event_loop.exit();
                        }
                        return;
                    }
                    Screen::Death => {
                        if button == MouseButton::Left {
                            let screen = state.window_screen();
                            if let Some(action) = state.ui.click_death(screen) {
                                state.handle_death_action(action);
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
                    Screen::Furnace | Screen::Chest => {
                        match button {
                            MouseButton::Left => state.container_click(ClickButton::Left),
                            MouseButton::Right => state.container_click(ClickButton::Right),
                            _ => {}
                        }
                        return;
                    }
                    Screen::Playing if state.chat_active() => return,
                    Screen::Playing => {}
                    Screen::Crafting => return,
                }
                if !state.captured {
                    state.capture_pointer();
                    return;
                }
                if !state.can_act() {
                    return;
                }
                match crate::bind_key_for_mouse(button).and_then(|key| state.ui.settings.keybinds.action_for(key)) {
                    Some(Action::Mine) => state.start_mining(),
                    Some(Action::Use) => state.use_or_place(),
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.ui.cursor = state.ui_point((position.x as f32, position.y as f32));
                match state.ui.screen {
                    Screen::Connecting => {
                        let screen = state.window_screen();
                        let cursor = state.ui.cursor;
                        state.connect_hovered = net_ui::connect_button_rects_for_state(
                            screen,
                            state.failure.is_some(),
                            state.client.is_some(),
                        )
                        .iter()
                        .position(|rect| rect.contains(cursor.0, cursor.1));
                    }
                    Screen::Settings => {
                        let screen = state.window_screen();
                        state.ui.settings_hover(screen);
                    }
                    Screen::Inventory => state.update_inventory_hover(),
                    Screen::Death => {
                        let screen = state.window_screen();
                        state.ui.update_death_hover(screen);
                    }
                    Screen::Playing | Screen::Furnace | Screen::Chest | Screen::Crafting => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 40.0).round() as i32,
                };
                if steps == 0 {
                    return;
                }
                match state.ui.screen {
                    Screen::Playing if !state.chat_active() && state.can_act() => state.cycle_hotbar(steps),
                    Screen::Playing => {}
                    Screen::Inventory => state.scroll_inventory(steps),
                    Screen::Death => state.ui.move_death_selection(-steps.signum()),
                    Screen::Settings => {
                        if let Some(row) = state.ui.settings_adjust(steps.signum()) {
                            state.apply_settings_row(row);
                        }
                    }
                    Screen::Furnace | Screen::Chest | Screen::Crafting | Screen::Connecting => {}
                }
            }
            WindowEvent::Focused(false) => {
                state.keys = [false; 8];
                state.tab_held = false;
                state.cancel_mining();
            }
            WindowEvent::DroppedFile(path) => log::info!("ignoring dropped file: {}", path.display()),
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, event: DeviceEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if !state.captured || state.ui.screen != Screen::Playing || state.chat_active() {
            return;
        }
        if let DeviceEvent::MouseMotion { delta } = event {
            let sensitivity = 0.0022f32 * state.ui.settings.mouse_sensitivity;
            state.input_yaw -= delta.0 as f32 * sensitivity;
            state.input_pitch -= delta.1 as f32 * sensitivity;
            state.input_pitch = state.input_pitch.clamp(-1.5533, 1.5533);
            state.set_look(state.input_yaw, state.input_pitch);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Some(limit) = state.auto_exit
            && state.started.elapsed() >= limit
        {
            state.shutdown();
            state.persist_settings();
            state.log_frame_stats();
            event_loop.exit();
            return;
        }
        state.window.request_redraw();
    }
}

struct NetGameState {
    window: Arc<Window>,
    renderer: Renderer,
    texture_source: TextureSource,
    registry: Arc<Registry>,
    mesh_scheduler: MeshScheduler,
    mesh_stats_timer: Instant,
    mesh_workers: usize,
    keys: [bool; 8],
    input_yaw: f32,
    input_pitch: f32,
    captured: bool,
    current_fov: f32,
    accumulator: Duration,
    last_frame: Instant,
    prev_eye: WorldVec,
    current_eye: WorldVec,
    frames: u32,
    fps_timer: Instant,
    fps: u32,
    meshes: HashMap<SectionPos, usize>,
    /// Distance-LOD state; multiplayer uses the same near/far policy and the
    /// same worker pool as single-player.
    lod: LodState,
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
    damage_flash: DamageFlash,
    camera_shake: f32,
    /// Camera used by the current frame, for projecting name tags.
    last_camera: Camera,
    audio: Option<AudioEngine>,
    audio_status: String,
    audio_enabled_before_mute: Option<bool>,
    audio_setting_touched: bool,
    /// Camera stand-in while connecting or after a failure.
    placeholder: Player,
    client: Option<NetworkClient>,
    pending_connect: Option<Receiver<Result<NetworkClient, String>>>,
    failure: Option<String>,
    endpoint: String,
    player_name: String,
    /// Whether the name was generated (and may be regenerated on retry).
    auto_player_name: bool,
    /// Keyboard focus on the connect overlay (0 reconnect/restart, 1 quit).
    connect_selected: usize,
    /// Hovered connect-overlay button, if any.
    connect_hovered: Option<usize>,
    connect_started: Instant,
    notice_seconds: f32,
    tab_held: bool,
    last_place: Option<Instant>,
    attack_ready: Option<Instant>,
    mouse_left: bool,
    mining: MiningState,
    container_selected: SlotId,
    footstep_distance: f64,
    was_in_water: bool,
    log_network: bool,
}

impl NetGameState {
    fn player(&self) -> &Player {
        self.client.as_ref().map_or(&self.placeholder, |client| client.player())
    }

    fn client_playing(&self) -> bool {
        self.client.as_ref().is_some_and(|client| client.state().is_playing())
    }

    fn client_dead(&self) -> bool {
        self.client.as_ref().is_some_and(NetworkClient::is_dead)
    }

    /// Whether gameplay input may change the world or the player.
    fn can_act(&self) -> bool {
        self.client_playing() && !self.client_dead() && self.failure.is_none()
    }

    fn chat_active(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(|client| client.chat_input().is_active())
    }

    /// Effective GUI-scale percentage for the current physical window.
    fn resolved_ui_percent(&self) -> u16 {
        resolved_ui_scale_percent(self.ui.settings.ui_scale_percent, self.window_size.1)
    }

    fn ui_point(&self, point: (f32, f32)) -> (f32, f32) {
        physical_to_ui_point(
            point,
            ui_scale_factor(self.ui.settings.ui_scale_percent, self.window_size.1),
        )
    }

    fn refresh_ui_scale(&mut self) {
        let old = self.ui.ui_scale_percent;
        let new = self.resolved_ui_percent();
        if new != old {
            self.ui.cursor = rescale_cursor(self.ui.cursor, old, new);
            self.ui.ui_scale_percent = new;
        }
    }

    fn window_screen(&self) -> [f32; 2] {
        ui_screen(
            [self.window_size.0.max(1.0), self.window_size.1.max(1.0)],
            self.ui.settings.ui_scale_percent,
        )
    }

    fn persist_settings(&self) {
        let mut settings = self.ui.settings.clone();
        // Multiplayer does not own the player's single-player game-mode
        // preference, so it must not rewrite it on exit.
        settings.game_mode = load_settings(&self.data_dir).game_mode;
        if let Some(saved) = self.audio_enabled_before_mute
            && !self.audio_setting_touched
        {
            settings.audio_enabled = saved;
        }
        if let Err(error) = save_settings(&self.data_dir, &settings) {
            log::warn!("could not save settings: {error}");
        }
    }

    fn sync_audio_settings(&mut self) {
        if let Some(engine) = &mut self.audio {
            crate::apply_audio_settings(engine, &self.ui.settings);
        }
    }

    fn play_ui_sound(&mut self, confirm: bool) {
        if let Some(engine) = &mut self.audio {
            engine.play_sfx(if confirm {
                AudioSfx::UiConfirm
            } else {
                AudioSfx::UiClick
            });
        }
    }

    /// Plays a block sound with distance attenuation and a yaw-relative pan.
    fn play_block_sfx(&mut self, sfx: AudioSfx, position: BlockPos) {
        if self.audio.is_none() {
            return;
        }
        let player = self.player();
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

    fn set_look(&mut self, yaw: f32, pitch: f32) {
        if let Some(client) = &mut self.client {
            let player = client.player_mut();
            player.yaw = yaw;
            player.pitch = pitch;
        } else {
            self.placeholder.yaw = yaw;
            self.placeholder.pitch = pitch;
        }
    }

    fn handle_escape(&mut self) {
        if self.client_dead() {
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
            Screen::Inventory => self.close_inventory(),
            Screen::Furnace | Screen::Chest => self.close_container_ui(true),
            Screen::Settings => {
                self.ui.resume();
                if self.client_playing() {
                    self.capture_pointer();
                }
            }
            Screen::Death => self.ui.focus_death_buttons(),
            Screen::Crafting | Screen::Connecting => {}
        }
        self.persist_settings();
    }

    fn open_chat(&mut self) {
        let Some(client) = &mut self.client else {
            return;
        };
        if !client.state().is_playing() {
            return;
        }
        client.chat_input_mut().open();
        self.ui.text_input_active = true;
        self.keys = [false; 8];
        self.cancel_mining();
        self.release_pointer();
    }

    fn close_chat(&mut self) {
        if let Some(client) = &mut self.client {
            client.chat_input_mut().cancel();
        }
        self.ui.text_input_active = false;
        if self.ui.screen == Screen::Playing {
            self.capture_pointer();
        }
    }

    fn submit_chat(&mut self) {
        let Some(client) = &mut self.client else {
            return;
        };
        let Some(text) = client.chat_input_mut().submit() else {
            self.ui.text_input_active = false;
            self.capture_pointer();
            return;
        };
        self.ui.text_input_active = false;
        match client.send_chat(&text) {
            Ok(()) => log::debug!("chat sent ({} chars)", text.len()),
            Err(error) => self.ui.toasts.push_warning(error),
        }
        self.capture_pointer();
    }

    fn start_connect(&mut self) {
        self.shutdown_client();
        self.pending_connect = None;
        self.failure = None;
        self.connect_selected = 0;
        self.connect_hovered = None;
        self.ui.clear_death();
        self.damage_flash.reset();
        self.cancel_mining();
        self.ui.screen = Screen::Connecting;
        self.ui.text_input_active = false;
        self.release_pointer();
        self.placeholder = Player::new(crate::default_spawn_hint());
        self.prev_eye = self.placeholder.eye();
        self.current_eye = self.placeholder.eye();
        self.connect_started = Instant::now();
        self.notice_seconds = NOTICE_SECONDS;
        let options = ConnectOptions::new(
            self.endpoint.clone(),
            self.player_name.clone(),
            self.ui.settings.render_distance_chunks.clamp(
                ob_core::config::MIN_RENDER_DISTANCE,
                ob_core::config::MAX_RENDER_DISTANCE,
            ),
            Arc::clone(&self.registry),
        );
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("ob-client-connect".to_string())
            .spawn(move || {
                let result = NetworkClient::connect(options).map_err(|error| error.to_string());
                let _ = sender.send(result);
            });
        match spawned {
            Ok(_) => self.pending_connect = Some(receiver),
            Err(error) => self.failure = Some(format!("could not start the connection worker: {error}")),
        }
        self.update_title();
    }

    fn retry(&mut self) {
        self.clear_world_render();
        let name_conflict = self
            .failure
            .as_deref()
            .is_some_and(|reason| reason.contains("name_taken") || reason.contains("invalid_name"));
        if self.auto_player_name && name_conflict {
            self.player_name = default_player_name();
        }
        self.start_connect();
    }

    fn poll_connection(&mut self) {
        let Some(receiver) = self.pending_connect.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("connection attempt ended unexpectedly".to_string()),
        };
        self.pending_connect = None;
        match result {
            Ok(client) => self.install_client(client),
            Err(reason) => self.fail_connection(reason),
        }
    }

    fn install_client(&mut self, client: NetworkClient) {
        self.clear_world_render();
        self.prev_eye = client.player().eye();
        self.current_eye = self.prev_eye;
        self.input_yaw = client.player().yaw;
        self.input_pitch = client.player().pitch;
        self.failure = None;
        self.ui.screen = Screen::Connecting;
        self.ui.clear_death();
        self.damage_flash.reset();
        self.cancel_mining();
        self.ui.hotbar_selected = client.selected_slot();
        self.applied_render_distance = client.render_radius();
        log::info!(
            "connected to {} as '{}' (id {}) world '{}' seed {}",
            client.endpoint(),
            self.player_name,
            client.local_id(),
            client.server_info().world_name,
            client.server_info().seed
        );
        self.client = Some(client);
        self.log_network = true;
        self.update_title();
    }

    fn fail_connection(&mut self, reason: String) {
        if self.failure.is_none() {
            log::warn!("multiplayer connection closed: {reason}");
            self.failure = Some(reason);
        }
        self.ui.screen = Screen::Connecting;
        self.keys = [false; 8];
        self.cancel_mining();
        self.release_pointer();
        self.update_title();
    }

    fn enter_playing(&mut self) {
        if self.ui.screen != Screen::Connecting || self.failure.is_some() {
            return;
        }
        self.ui.screen = Screen::Playing;
        self.notice_seconds = NOTICE_SECONDS;
        self.accumulator = Duration::ZERO;
        self.keys = [false; 8];
        self.damage_flash.reset();
        self.footstep_distance = 0.0;
        self.was_in_water = self.in_water();
        if self.client_dead() {
            let cause = self
                .client
                .as_ref()
                .and_then(NetworkClient::death_cause)
                .unwrap_or("rejoined while dead")
                .to_string();
            self.begin_death_screen(&cause);
            return;
        }
        self.capture_pointer();
        if let Some(client) = &self.client {
            let info = client.server_info();
            self.ui
                .toasts
                .push_success(format!("Connected to '{}' - multiplayer survival", info.world_name));
        }
        self.update_title();
    }

    fn shutdown_client(&mut self) {
        if let Some(mut client) = self.client.take() {
            client.shutdown();
        }
        self.ui.clear_death();
        self.damage_flash.reset();
        self.cancel_mining();
    }

    fn shutdown(&mut self) {
        self.shutdown_client();
        self.pending_connect = None;
        self.release_pointer();
    }

    fn clear_world_render(&mut self) {
        self.lod.reset(&mut self.renderer, &mut self.mesh_scheduler);
        self.renderer.clear_meshes();
        self.meshes.clear();
        self.mesh_scheduler = MeshScheduler::new(self.mesh_workers, Arc::clone(&self.registry));
    }

    fn handle_net_events(&mut self, events: Vec<NetEvent>) {
        for event in events {
            match event {
                NetEvent::Joined { name, .. } => self.ui.toasts.push(format!("{name} joined")),
                NetEvent::Left { name, .. } => self.ui.toasts.push(format!("{name} left")),
                NetEvent::Chat { .. } => {}
                NetEvent::Disconnected { reason } => self.fail_connection(reason),
                NetEvent::Damage { amount, cause } => {
                    self.damage_flash.trigger();
                    if !self.ui.settings.reduced_motion {
                        self.camera_shake = DAMAGE_SHAKE_RADIANS;
                    }
                    if let Some(engine) = &mut self.audio {
                        engine.play_sfx(AudioSfx::Hurt);
                    }
                    log::debug!("took {amount:.1} damage ({cause})");
                }
                NetEvent::Death { cause } => {
                    // A reconnecting dead player receives `Death` before the
                    // spawn area is resident; keep the loading panel up and let
                    // `enter_playing` open the death screen afterwards.
                    if self.ui.screen != Screen::Connecting {
                        self.begin_death_screen(&cause);
                    }
                }
                NetEvent::Respawned { .. } => {
                    self.end_death_screen();
                }
                NetEvent::AttackResult {
                    hit, damage, killed, ..
                } => {
                    if hit {
                        self.ui.flash_mob_hit();
                        log::debug!("attack hit for {damage:.1}");
                        if killed {
                            self.ui.toasts.push_success("Mob defeated");
                        }
                    } else {
                        self.ui.toasts.push_warning("The attack missed");
                    }
                }
                NetEvent::ActionRejected { reason, hint, .. } => {
                    log::debug!("action rejected: {reason}");
                    self.ui.toasts.push_warning(hint);
                }
                NetEvent::ContainerOpened { .. } => self.open_container_screen(),
                NetEvent::ContainerClosed { window, reason } => {
                    let still_open = self
                        .client
                        .as_ref()
                        .is_some_and(|client| client.containers().window_id() == Some(window));
                    if !still_open {
                        self.close_container_ui(false);
                        if !matches!(reason.as_str(), "closed" | "death" | "disconnect") {
                            self.ui.toasts.push_warning(container_close_hint(&reason));
                        }
                    }
                }
                NetEvent::ContainerCursorDropped => {
                    self.ui
                        .toasts
                        .push_warning("The container changed - held item preview dropped");
                }
            }
        }
    }

    fn begin_death_screen(&mut self, cause: &str) {
        if self.ui.screen == Screen::Death {
            return;
        }
        self.keys = [false; 8];
        self.cancel_mining();
        self.ui.open_death();
        self.ui.death_cause = Some(net_state::friendly_death_cause(cause));
        self.release_pointer();
        self.damage_flash.trigger();
        if let Some(engine) = &mut self.audio {
            engine.play_sfx(AudioSfx::Death);
        }
        self.update_title();
    }

    fn end_death_screen(&mut self) {
        if self.ui.screen != Screen::Death {
            return;
        }
        self.ui.clear_death();
        self.ui.toasts.clear();
        self.keys = [false; 8];
        self.cancel_mining();
        let (yaw, pitch, eye) = {
            let player = self.player();
            (player.yaw, player.pitch, player.eye())
        };
        self.input_yaw = yaw;
        self.input_pitch = pitch;
        self.prev_eye = eye;
        self.current_eye = eye;
        self.accumulator = Duration::ZERO;
        self.damage_flash.reset();
        self.camera_shake = 0.0;
        self.attack_ready = None;
        self.footstep_distance = 0.0;
        self.was_in_water = self.in_water();
        self.ui.toasts.push_success("Respawned at the world spawn");
        self.capture_pointer();
        self.update_title();
    }

    /// Applies a death-screen action. Returns whether the process should exit.
    fn handle_death_action(&mut self, action: DeathAction) -> bool {
        match action {
            DeathAction::Respawn => {
                let sent = self.client.as_mut().is_some_and(NetworkClient::send_respawn);
                if sent {
                    self.ui.toasts.push("Respawning...");
                } else if self
                    .client
                    .as_ref()
                    .is_some_and(|client| client.survival().respawn_requested)
                {
                    self.ui.toasts.push("Waiting for the server to respawn you");
                } else {
                    self.ui.toasts.push_warning("Could not send the respawn request");
                }
                false
            }
            DeathAction::SaveAndQuit => {
                self.shutdown_client();
                self.fail_connection("disconnected by player".to_string());
                false
            }
        }
    }

    fn select_hotbar(&mut self, slot: usize) {
        if !self.can_act() {
            return;
        }
        if let Some(client) = &mut self.client
            && !client.select_slot(slot)
        {
            self.ui.toasts.push_warning("Could not select that slot");
        }
        self.update_title();
    }

    fn cycle_hotbar(&mut self, steps: i32) {
        let current = self
            .client
            .as_ref()
            .map_or(self.ui.hotbar_selected, NetworkClient::selected_slot);
        let slot = (current as i32 - steps).rem_euclid(UI_HOTBAR_SLOTS as i32) as usize;
        self.select_hotbar(slot);
    }

    fn open_inventory(&mut self) {
        if !self.can_act() {
            return;
        }
        self.cancel_mining();
        self.ui.open_inventory();
        if let Some(client) = &self.client {
            self.ui.inventory.selected = client.selected_slot();
            self.ui.hotbar_selected = client.selected_slot();
        }
        self.update_inventory_hover();
        self.release_pointer();
    }

    fn close_inventory(&mut self) {
        self.ui.close_inventory();
        if self.can_act() {
            self.capture_pointer();
        }
    }

    fn update_inventory_hover(&mut self) {
        let screen = self.window_screen();
        self.ui.inventory.hovered = inventory::survival_hit_test(screen, self.ui.cursor.0, self.ui.cursor.1);
        self.ui.inventory.armor_hovered =
            inventory::survival_armor_hit_test(screen, self.ui.cursor.0, self.ui.cursor.1);
    }

    fn move_inventory_selection(&mut self, dx: i32, dy: i32) {
        self.ui.inventory.selected =
            inventory::move_survival_selection(self.ui.inventory.selected, dx, dy, UI_INVENTORY_SLOTS);
    }

    fn scroll_inventory(&mut self, rows: i32) {
        self.ui.inventory.selected =
            inventory::cycle_survival_selection(self.ui.inventory.selected, rows, UI_INVENTORY_SLOTS);
    }

    fn equip_selected_slot(&mut self, slot: usize) {
        let Some(client) = &mut self.client else {
            return;
        };
        let Some(stack) = client.inventory().get(slot).copied().flatten() else {
            return;
        };
        if self.registry.item(stack.item).armor.is_none() {
            return;
        }
        let name = inventory::item_display_name(&self.registry, stack.item);
        if client.equip_inventory_slot(slot) {
            self.ui.toasts.push(format!("Equipping {name}"));
        } else {
            self.ui.toasts.push_warning(format!("Could not equip {name}"));
        }
    }

    /// Click-to-pick/click-to-place on the 36-slot network inventory.
    ///
    /// Moves are intents: the first click only previews a stack under the
    /// pointer, and the second click sends one authoritative `inventory_move`
    /// that the following `inventory_sync` confirms. Shift-click sends a
    /// one-click hotbar <-> main transfer.
    fn inventory_click(&mut self) {
        self.update_inventory_hover();
        let shift = self.keys[5];
        if let Some(armor_slot) = self.ui.inventory.armor_hovered {
            let index = inventory::armor_index(armor_slot);
            if let Some(cursor) = self.ui.inventory.cursor {
                let matching = self
                    .registry
                    .item(cursor.stack.item)
                    .armor
                    .is_some_and(|armor| index == inventory::armor_index(armor.slot));
                if matching {
                    let Some(address) = net_state::equipment_address(index) else {
                        return;
                    };
                    let sent = self
                        .client
                        .as_mut()
                        .is_some_and(|client| client.send_inventory_move(cursor.from as u16, address, 1, None));
                    if sent {
                        self.ui.inventory.clear_cursor();
                        self.ui.toasts.push("Equipping armor".to_string());
                    } else {
                        self.ui.toasts.push_warning("Could not equip that armor");
                    }
                } else {
                    self.ui.inventory.clear_cursor();
                    self.ui.toasts.push_warning("That armor does not fit this slot");
                }
                return;
            }
            let Some(client) = &mut self.client else {
                return;
            };
            let Some(stack) = client.survival().equipment.get(index).copied().flatten() else {
                self.ui.toasts.push(format!(
                    "Empty {} slot",
                    inventory::armor_slot_label(armor_slot).to_lowercase()
                ));
                return;
            };
            let name = inventory::item_display_name(&self.registry, stack.item);
            if client.toggle_equipment(index) {
                self.ui.toasts.push(format!("Unequipped {name}"));
            } else {
                self.ui.toasts.push_warning("Inventory full - cannot unequip");
            }
            return;
        }
        let Some(index) = self.ui.inventory.hovered else {
            self.ui.inventory.clear_cursor();
            return;
        };
        self.ui.inventory.selected = index;
        if shift {
            let stack = self
                .client
                .as_ref()
                .and_then(|client| client.inventory().get(index).copied().flatten());
            let Some(stack) = stack else {
                return;
            };
            if self.registry.item(stack.item).armor.is_some() {
                self.equip_selected_slot(index);
                return;
            }
            let name = inventory::item_display_name(&self.registry, stack.item);
            let sent = self
                .client
                .as_mut()
                .is_some_and(|client| client.quick_transfer_inventory(index));
            if sent {
                self.ui.inventory.clear_cursor();
                self.ui.toasts.push(format!("Moved {name}"));
            } else {
                self.ui.toasts.push_warning("No room in the other row");
            }
            return;
        }
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let inventory = client.inventory();
        if self.ui.inventory.cursor.is_some() {
            let click = inventory::click_cursor_slot(&mut self.ui.inventory, inventory, index);
            match click {
                inventory::CursorClick::Move { from, to, stack } => {
                    self.place_inventory_cursor(inventory::InventoryCursor { from, stack }, to);
                }
                inventory::CursorClick::Cancelled => {}
                inventory::CursorClick::Picked | inventory::CursorClick::Ignored => {}
            }
            return;
        }
        if inventory
            .get(index)
            .copied()
            .flatten()
            .is_some_and(|stack| self.registry.item(stack.item).armor.is_some())
        {
            self.equip_selected_slot(index);
            return;
        }
        if index < UI_HOTBAR_SLOTS {
            self.select_hotbar(index);
            self.close_inventory();
            return;
        }
        let click = inventory::click_cursor_slot(&mut self.ui.inventory, inventory, index);
        debug_assert!(matches!(
            click,
            inventory::CursorClick::Picked | inventory::CursorClick::Ignored
        ));
    }

    fn place_inventory_cursor(&mut self, cursor: inventory::InventoryCursor, to: usize) {
        let name = inventory::item_display_name(&self.registry, cursor.stack.item);
        let sent = self
            .client
            .as_mut()
            .is_some_and(|client| client.send_inventory_move(cursor.from as u16, to as u16, cursor.stack.count, None));
        self.ui.inventory.clear_cursor();
        if sent {
            self.ui.toasts.push(format!("Moved {name}"));
        } else {
            self.ui.toasts.push_warning(format!("Cannot move {name}"));
        }
    }

    fn assign_inventory_selection(&mut self) {
        let slot = self.ui.inventory.selected.min(UI_INVENTORY_SLOTS - 1);
        self.ui.inventory.selected = slot;
        if let Some(cursor) = self.ui.inventory.cursor {
            if cursor.from == slot {
                self.ui.inventory.clear_cursor();
            } else {
                self.place_inventory_cursor(cursor, slot);
            }
            return;
        }
        if self
            .client
            .as_ref()
            .and_then(|client| client.inventory().get(slot).copied().flatten())
            .is_some_and(|stack| self.registry.item(stack.item).armor.is_some())
        {
            self.equip_selected_slot(slot);
            return;
        }
        if slot >= UI_HOTBAR_SLOTS {
            if let Some(stack) = self
                .client
                .as_ref()
                .and_then(|client| client.inventory().get(slot).copied().flatten())
            {
                self.ui.inventory.cursor = Some(inventory::InventoryCursor { from: slot, stack });
            }
            return;
        }
        self.select_hotbar(slot);
        self.close_inventory();
    }

    fn open_container_screen(&mut self) {
        let Some(window) = self.client.as_ref().and_then(|client| client.containers().window()) else {
            return;
        };
        if self.ui.screen == Screen::Death {
            return;
        }
        match window.ui_kind() {
            ContainerKind::Furnace => {
                self.ui.screen = Screen::Furnace;
                self.container_selected = SlotId::FurnaceInput;
            }
            ContainerKind::Chest => {
                self.ui.screen = Screen::Chest;
                self.container_selected = SlotId::Chest(0);
            }
        }
        self.keys = [false; 8];
        self.cancel_mining();
        self.release_pointer();
        self.update_title();
    }

    fn close_container_ui(&mut self, notify: bool) {
        if notify && let Some(client) = &mut self.client {
            let _ = client.send_container_close();
        }
        self.container_selected = SlotId::FurnaceInput;
        if matches!(self.ui.screen, Screen::Furnace | Screen::Chest) {
            self.ui.screen = Screen::Playing;
            if self.can_act() {
                self.capture_pointer();
            }
        }
        self.update_title();
    }

    fn container_layout(&self) -> Option<containers::ContainerLayout> {
        let window = self.client.as_ref()?.containers().window()?;
        Some(containers::container_layout(window.ui_kind(), self.window_screen()))
    }

    fn container_click(&mut self, button: ClickButton) {
        let Some(layout) = self.container_layout() else {
            self.close_container_ui(false);
            return;
        };
        let Some(slot) = layout.hit_test(self.ui.cursor.0, self.ui.cursor.1) else {
            return;
        };
        self.container_selected = slot;
        let Some(client) = &mut self.client else {
            return;
        };
        let outcome = client.container_click(slot, button);
        if let Some(message) = outcome.message {
            self.ui.toasts.push_warning(message);
        } else if outcome.transfer.is_some() {
            self.play_ui_sound(false);
        }
    }

    fn move_container_selection(&mut self, dx: i32, dy: i32) {
        let Some(layout) = self.container_layout() else {
            return;
        };
        self.container_selected = containers::move_selection(&layout, self.container_selected, dx, dy);
    }

    fn activate_container_slot(&mut self) {
        let slot = self.container_selected;
        let Some(client) = &mut self.client else {
            return;
        };
        let outcome = client.container_click(slot, ClickButton::Left);
        if let Some(message) = outcome.message {
            self.ui.toasts.push_warning(message);
        } else if outcome.transfer.is_some() {
            self.play_ui_sound(false);
        }
    }

    fn cancel_mining(&mut self) {
        self.mouse_left = false;
        self.mining.stop();
    }

    fn start_mining(&mut self) {
        if !self.captured || self.ui.screen != Screen::Playing || !self.can_act() {
            return;
        }
        self.mouse_left = true;
        if let Some(id) = self.client.as_ref().and_then(NetworkClient::mob_target) {
            self.mining.stop();
            self.try_attack_mob(id);
        }
    }

    fn try_attack_mob(&mut self, id: u64) {
        if self.attack_ready.is_some_and(|ready| ready.elapsed() < ATTACK_COOLDOWN) {
            return;
        }
        let Some(client) = &mut self.client else {
            return;
        };
        if client.send_attack(Some(id)) {
            self.attack_ready = Some(Instant::now());
        }
    }

    /// Advances hold-to-mine from the frame clock.
    ///
    /// There is no local Host in network mode: the timer uses the content
    /// registry and the authoritative selected stack, and completion sends one
    /// authoritative `block_edit`. The block change is predicted visually; the
    /// inventory count, drops and tool wear arrive only through server sync.
    fn update_mining(&mut self, dt: Duration) {
        if self.ui.screen != Screen::Playing || !self.captured || !self.mouse_left || !self.can_act() {
            self.mining.stop();
            return;
        }
        let Some(client) = &self.client else {
            self.mining.stop();
            return;
        };
        if let Some(id) = client.mob_target() {
            self.mining.stop();
            self.try_attack_mob(id);
            return;
        }
        let player = client.player();
        let current = client
            .world()
            .raycast(player.eye(), player.forward(), REACH)
            .map(|hit| hit.block);
        let breakable_target = current.and_then(|pos| {
            let block_id = client.world().get_block(pos);
            let block = self.registry.block(block_id);
            let seconds = client.mining_seconds(pos);
            (block.breakable() && seconds.is_finite() && seconds > 0.0).then_some((pos, seconds))
        });
        if let Some((pos, seconds)) = breakable_target
            && self.mining.target() == Some(pos)
            && (self.mining.duration_seconds() - seconds).abs() > 1e-6
        {
            self.mining.start(pos, seconds);
        }
        self.mining.update(dt.as_secs_f32(), current);
        if let Some(pos) = self.mining.take_completed() {
            self.break_mined_block(pos);
            return;
        }
        if self.mining.target().is_none()
            && let Some((pos, seconds)) = breakable_target
        {
            self.mining.start(pos, seconds);
        }
    }

    fn break_mined_block(&mut self, pos: BlockPos) {
        let (name, sfx) = {
            let Some(client) = &self.client else {
                return;
            };
            let block = client.world().get_block(pos);
            if block == BlockId::AIR {
                return;
            }
            (
                inventory::display_name(self.registry.block(block).id),
                crate::audio_material_for(&self.registry, block).map(AudioSfx::BlockBreak),
            )
        };
        let Some(client) = &mut self.client else {
            return;
        };
        if client.edit_block(pos, BlockId::AIR) {
            if let Some(sfx) = sfx {
                self.play_block_sfx(sfx, pos);
            }
            self.ui.flash_broken_block(name);
        }
    }

    /// Right-click: container interaction first, then eat, then placement.
    fn use_or_place(&mut self) {
        if !self.captured || self.ui.screen != Screen::Playing || !self.can_act() {
            return;
        }
        let (hit, selected, hunger) = {
            let Some(client) = &self.client else {
                return;
            };
            let player = client.player();
            let hit = client
                .world()
                .raycast(player.eye(), player.forward(), REACH)
                .map(|hit| {
                    let block = client.world().get_block(hit.block);
                    (hit.block, self.registry.block(block).id)
                });
            (hit, client.selected_stack(), client.hunger())
        };
        if let Some((pos, block_name)) = hit {
            match block_name {
                "openblock:crafting_table" => {
                    self.ui.toasts.push_warning(CRAFTING_HINT);
                    return;
                }
                "openblock:furnace" | "openblock:chest" => {
                    if let Some(client) = &mut self.client {
                        let _ = client.use_block(pos);
                    }
                    return;
                }
                _ => {}
            }
        }
        if inventory::should_eat(&self.registry, selected, hunger) {
            let name = selected
                .map(|stack| inventory::item_display_name(&self.registry, stack.item))
                .unwrap_or_else(|| "item".to_string());
            if self.client.as_mut().is_some_and(NetworkClient::send_eat) {
                self.ui.toasts.push(format!("Ate {name}"));
                if let Some(engine) = &mut self.audio {
                    engine.play_sfx(AudioSfx::Eat);
                }
            }
            return;
        }
        if self.last_place.is_some_and(|at| at.elapsed() < PLACE_COOLDOWN) {
            return;
        }
        self.last_place = Some(Instant::now());
        let Some(stack) = selected else {
            self.ui.toasts.push_warning("No item in the selected slot");
            return;
        };
        let Some(block) = self.registry.item(stack.item).block else {
            self.ui.toasts.push_warning("Selected item cannot be placed");
            return;
        };
        let placed = self.client.as_mut().and_then(|client| client.place_block(block));
        if placed.is_some() {
            if let Some(material) = crate::audio_material_for(&self.registry, block)
                && let Some(engine) = &mut self.audio
            {
                engine.play_sfx(AudioSfx::BlockPlace(material));
            }
        } else {
            self.ui.toasts.push_warning("Cannot place there");
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let dt = now - self.last_frame;
        self.last_frame = now;
        if self.started.elapsed() >= Duration::from_secs(2) {
            self.frame_times.push(dt.as_secs_f32() * 1000.0);
            if self.frame_times.len() > 50_000 {
                self.frame_times.drain(..25_000);
            }
        }
        self.poll_connection();
        let mut ready = false;
        let mut disconnected = None;
        let mut events = Vec::new();
        if let Some(client) = &mut self.client {
            events = client.poll();
            // A 512-block join is 4,225 columns. While the interest ring is
            // incomplete, request a large batch each frame; the client's token
            // bucket (128/s, burst 64) bounds the socket and server work. After
            // the ring is complete, drop to the small movement-follow cadence.
            let (loaded, target) = client.loading_progress();
            let budget = if loaded < target { 16 } else { 4 };
            client.maintain_chunks(budget);
            ready = client.state().is_playing();
            if let NetState::Disconnected(reason) = client.state() {
                disconnected = Some(reason.clone());
            }
        }
        self.handle_net_events(events);
        if let Some(reason) = disconnected
            && self.failure.is_none()
        {
            self.fail_connection(reason);
        }
        if ready && self.ui.screen == Screen::Connecting && self.failure.is_none() {
            self.enter_playing();
        }
        if self.ui.screen != Screen::Connecting && self.ui.screen != Screen::Death && self.client_dead() {
            let cause = self
                .client
                .as_ref()
                .and_then(NetworkClient::death_cause)
                .unwrap_or("unknown")
                .to_string();
            self.begin_death_screen(&cause);
        }
        if self.ui.screen == Screen::Death && !self.client_dead() {
            self.end_death_screen();
        }
        // Screens that own the pointer must not keep a stale gameplay hold.
        if !matches!(self.ui.screen, Screen::Playing) && self.mouse_left {
            self.cancel_mining();
        }
        if let Some(client) = &self.client {
            let selected = client.selected_slot();
            if self.ui.hotbar_selected != selected {
                self.ui.hotbar_selected = selected;
                self.ui.block_name_seconds = BLOCK_NAME_SECONDS;
            }
        }
        let playing = self.ui.screen == Screen::Playing && !self.chat_active() && self.can_act();
        self.accumulator += dt.min(Duration::from_millis(250));
        if playing {
            let mut ticks = 0;
            while self.accumulator >= TICK && ticks < 5 {
                self.accumulator -= TICK;
                self.tick();
                ticks += 1;
            }
        } else {
            self.accumulator = Duration::ZERO;
        }
        self.update_mining(dt);
        if let Some(client) = &mut self.client {
            client.tick(dt.as_secs_f32());
        }
        self.notice_seconds = (self.notice_seconds - dt.as_secs_f32()).max(0.0);
        self.ui.tick(dt.as_secs_f32());
        self.damage_flash.tick(dt.as_secs_f32());
        self.camera_shake *= (-dt.as_secs_f32() * 9.0).exp();
        if self.camera_shake < 0.0002 {
            self.camera_shake = 0.0;
        }
        self.update_audio_state(dt.as_secs_f32());
        self.stream_and_mesh();
        self.rolling_frame_ms.push(dt.as_secs_f32() * 1000.0);
        if self.rolling_frame_ms.len() > 120 {
            self.rolling_frame_ms.drain(..60);
        }
        let alpha = (self.accumulator.as_secs_f32() / TICK.as_secs_f32()).clamp(0.0, 1.0);
        let eye = self.prev_eye.lerp(self.current_eye, f64::from(alpha));
        let player = self.player();
        let shake = if !self.ui.settings.reduced_motion && self.camera_shake > 0.0 {
            (self.started.elapsed().as_secs_f32() * 72.0).sin() * self.camera_shake
        } else {
            0.0
        };
        let mut camera = Camera::new(eye, player.yaw, (player.pitch + shake).clamp(-1.5533, 1.5533));
        let target_fov = crate::target_fov(&self.ui.settings, player.sprinting);
        let blend = 1.0 - (-dt.as_secs_f32() * 12.0).exp();
        self.current_fov += (target_fov - self.current_fov) * blend;
        camera.fov_y = self.current_fov.to_radians();
        self.last_camera = camera;
        let radius = self
            .client
            .as_ref()
            .map_or(self.ui.settings.render_distance_chunks, NetworkClient::render_radius);
        camera.far = (radius as f32 * 16.0 + 48.0).max(128.0);
        let outline = self
            .mining
            .targeted_block()
            .map(|block| BlockOutline::for_block([block.x, block.y, block.z]));
        if self.client.as_ref().is_some_and(|client| client.state().is_playing()) {
            let server_tick = self.client.as_ref().map_or(0, NetworkClient::server_tick);
            let environment = Environment::from_ticks(
                server_tick + ob_render::DAY_LENGTH_TICKS / 4,
                ob_render::DAY_LENGTH_TICKS,
            );
            self.log_network_once();
            let now = Instant::now();
            let mut entities = self
                .client
                .as_ref()
                .map(|client| {
                    let states = client.avatar_states(now);
                    crate::net::avatar_instances(&states)
                })
                .unwrap_or_default();
            if let Some(client) = &self.client {
                entities.extend(client.entity_instances(now));
            }
            self.renderer.set_entities(&entities);
            self.refresh_debug_stats();
            let ui_frame = self.build_ui_frame();
            let scene = SceneParams {
                environment,
                ui: ui_frame.as_ref(),
                outline,
                time_seconds: 0.0,
            };
            if let Err(error) = self.renderer.render_scene(&camera, &scene) {
                log::error!("render error: {error}");
            }
        } else {
            self.renderer.set_entities(&[]);
            self.refresh_debug_stats();
            let ui_frame = self.build_ui_frame();
            let environment = Environment::from_ticks(ob_render::DAY_LENGTH_TICKS / 4, ob_render::DAY_LENGTH_TICKS);
            let scene = SceneParams {
                environment,
                ui: ui_frame.as_ref(),
                outline: None,
                time_seconds: 0.0,
            };
            if let Err(error) = self.renderer.render_scene(&camera, &scene) {
                log::error!("render error: {error}");
            }
        }
        if self.applied_render_distance != self.ui.settings.render_distance_chunks {
            self.applied_render_distance = self.ui.settings.render_distance_chunks;
            self.update_title();
        }
        self.frames += 1;
        if self.fps_timer.elapsed() >= Duration::from_secs(1) {
            self.fps = self.frames;
            self.frames = 0;
            self.fps_timer = Instant::now();
            self.update_title();
        }
    }

    /// Logs the server identity exactly once per successful connection.
    fn log_network_once(&mut self) {
        if !self.log_network {
            return;
        }
        self.log_network = false;
        if let Some(client) = &self.client {
            let (mobs, items) = client.entity_counts();
            log::info!(
                "multiplayer survival ready: {} chunks loaded, {} players, {mobs} mobs, {items} items, motd '{}'",
                client.loaded_chunks(),
                client.player_count(),
                client.server_info().motd
            );
        }
    }

    fn tick(&mut self) {
        let mut input = PlayerInput {
            forward: 0.0,
            strafe: 0.0,
            jump: false,
            sprint: false,
            sneak: false,
            fly_toggle: false,
        };
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
        input.sneak = self.keys[5];
        input.sprint = self.keys[6];
        let Some(client) = &mut self.client else {
            return;
        };
        self.prev_eye = client.player().eye();
        let before = client.player().pos;
        let _ = client.tick_player(input);
        let after = client.player().pos;
        let dx = after.x - before.x;
        let dz = after.z - before.z;
        self.footstep_distance += (dx * dx + dz * dz).sqrt();
        self.current_eye = client.player().eye();
    }

    fn stream_and_mesh(&mut self) {
        let Some(client) = self.client.as_mut() else {
            return;
        };
        // Bound replicated-world memory and GPU buffers while flying. The
        // server keeps its own interest set; a returning column is requested
        // again by the tracker.
        for chunk in client.unload_far_chunks(2) {
            let positions: Vec<SectionPos> = self
                .meshes
                .keys()
                .filter(|pos| pos.x == chunk.x && pos.z == chunk.z)
                .copied()
                .collect();
            for pos in positions {
                self.renderer.remove_mesh(pos);
                self.meshes.remove(&pos);
                self.mesh_scheduler.forget_section(pos);
            }
            self.lod.drop_chunk(chunk, &mut self.renderer, &mut self.mesh_scheduler);
        }

        let player_pos = client.player().pos;
        let camera = ChunkPos::new(
            (player_pos.x.floor() as i32).div_euclid(16),
            (player_pos.z.floor() as i32).div_euclid(16),
        );
        let config = LodConfig::for_render_distance(client.render_radius());
        self.lod.maintain(
            client.world_mut(),
            camera,
            config,
            &self.registry,
            &mut self.mesh_scheduler,
            &mut self.renderer,
            &mut self.meshes,
        );

        let dirty = client.world_mut().take_dirty_sections();
        let mut deferred = Vec::new();
        let mut submitted = 0usize;
        for pos in dirty {
            if client.world().section(pos).is_none() {
                self.renderer.remove_mesh(pos);
                self.meshes.remove(&pos);
                self.mesh_scheduler.forget_section(pos);
                continue;
            }
            if self.lod.is_far(pos) {
                let key = LodBatchKey::containing(pos.chunk(), self.lod.config().group_chunks);
                self.lod.invalidate(key, &mut self.renderer, &mut self.mesh_scheduler);
                continue;
            }
            if submitted >= MAX_MESH_SUBMISSIONS_PER_FRAME {
                self.mesh_scheduler.invalidate(pos);
                deferred.push(pos);
                continue;
            }
            if self.mesh_scheduler.submit(client.world(), pos) {
                submitted += 1;
            } else {
                deferred.push(pos);
            }
        }
        for pos in deferred {
            client.world_mut().mark_section_dirty(pos);
        }
        let backlog = client.world().dirty_count();
        let submitted_total = self.mesh_scheduler.submitted_count();
        let completed_total = self.mesh_scheduler.completed_count();
        let dropped_total = self.mesh_scheduler.dropped_count();
        let in_flight = self.mesh_scheduler.in_flight_count();
        // One far batch upload per frame keeps the GPU buffer-creation hitch
        // bounded even in the join burst.
        self.lod
            .drain(client.world(), &mut self.mesh_scheduler, &mut self.renderer);

        let start = Instant::now();
        let mut uploaded = 0usize;
        loop {
            if uploaded > 0 && start.elapsed().as_micros() >= MESH_UPLOAD_BUDGET_US {
                break;
            }
            let Some(result) = self.mesh_scheduler.try_recv() else {
                break;
            };
            let exists = client.world().section(result.pos).is_some();
            if !exists || self.lod.is_far(result.pos) {
                self.renderer.remove_mesh(result.pos);
                self.meshes.remove(&result.pos);
                continue;
            }
            if result.mesh.is_empty() {
                self.renderer.remove_mesh(result.pos);
                self.meshes.remove(&result.pos);
            } else {
                let vertex_count = result.mesh.vertex_count();
                self.renderer.upload_mesh(result.pos, result.mesh);
                self.meshes.insert(result.pos, vertex_count);
            }
            uploaded += 1;
        }
        if self.mesh_stats_timer.elapsed() >= MESH_STATS_INTERVAL {
            log::debug!(
                "net mesh scheduler: submitted {submitted_total} completed {completed_total} dropped {dropped_total} in-flight {in_flight} backlog {backlog}; lod {} batches, {} in flight, {} done, {} dropped",
                self.lod.batches().len(),
                self.mesh_scheduler.lod_in_flight_count(),
                self.mesh_scheduler.lod_completed_count(),
                self.mesh_scheduler.lod_dropped_count(),
            );
            self.mesh_stats_timer = Instant::now();
        }
    }

    fn in_water(&self) -> bool {
        let Some(client) = &self.client else {
            return false;
        };
        let player = client.player();
        let feet = BlockPos::new(
            player.pos.x.floor() as i32,
            player.pos.y.floor() as i32,
            player.pos.z.floor() as i32,
        );
        let waist = BlockPos::new(feet.x, feet.y + 1, feet.z);
        [feet, waist].into_iter().any(|position| {
            let block = client.world().get_block(position);
            self.registry.block(block).fluid
        })
    }

    fn update_audio_state(&mut self, dt: f32) {
        let playing = self.ui.screen == Screen::Playing && !self.chat_active() && !self.client_dead();
        let player = self.player().clone();
        let feet = BlockPos::new(
            player.pos.x.floor() as i32,
            player.pos.y.floor() as i32,
            player.pos.z.floor() as i32,
        );
        let below = BlockPos::new(feet.x, (player.pos.y - 0.1).floor() as i32, feet.z);
        let below_block = self
            .client
            .as_ref()
            .map_or(BlockId::AIR, |client| client.world().get_block(below));
        let material = crate::audio_material_for(&self.registry, below_block).unwrap_or(AudioMaterial::Grass);
        let in_water = self.in_water();
        let distance = std::mem::take(&mut self.footstep_distance) as f32;
        let yaw = player.yaw;
        let listener_eye = player.eye();
        let listener = [listener_eye.x as f32, listener_eye.y as f32, listener_eye.z as f32];
        let on_ground = playing && player.on_ground && !player.flying;
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
        }
        self.was_in_water = in_water;
    }

    fn refresh_debug_stats(&mut self) {
        let player = self.player();
        let eye = player.eye();
        let pos = [eye.x, eye.y, eye.z];
        let chunk = [
            (eye.x.floor() as i32).div_euclid(16),
            (eye.y.floor() as i32).div_euclid(16),
            (eye.z.floor() as i32).div_euclid(16),
        ];
        let selected = self.client.as_ref().and_then(NetworkClient::selected_stack);
        let (target, target_block, seed, loaded, backlog, sections, ticks) = match &self.client {
            Some(client) => {
                let hit = client.world().raycast(eye, player.forward(), 5.0);
                let target = hit.map(|hit| {
                    let id = self.registry.block(client.world().get_block(hit.block)).id.to_string();
                    (id, [hit.block.x, hit.block.y, hit.block.z])
                });
                let target_block = hit.map(|hit| client.world().get_block(hit.block));
                (
                    target,
                    target_block,
                    client.server_info().seed,
                    client.world().chunk_count(),
                    client.world().dirty_count(),
                    client.world().section_count(),
                    client.session_ticks(),
                )
            }
            None => (None, None, 0, 0, 0, 0, 0),
        };
        let held_item = selected
            .map(|stack| inventory::item_display_name(&self.registry, stack.item))
            .unwrap_or_else(|| "empty".to_string());
        let held_durability = selected.and_then(|stack| inventory::stack_durability(&self.registry, stack));
        let held_item_id = selected.map(|stack| stack.item);
        let mining_speed = target_block
            .map(|block| self.registry.mining_speed(block, held_item_id))
            .unwrap_or(1.0);
        let mut rolling = self.rolling_frame_ms.clone();
        rolling.sort_by(f32::total_cmp);
        let percentile = |p: f32| -> f32 {
            if rolling.is_empty() {
                return 0.0;
            }
            let index = ((rolling.len() as f32 - 1.0) * p / 100.0).round() as usize;
            rolling[index.min(rolling.len() - 1)]
        };
        let render_stats = self.renderer.stats();
        let network = self.network_status_line();
        let time_of_day = self
            .client
            .as_ref()
            .map(|client| {
                Environment::from_ticks(
                    client.server_tick() + ob_render::DAY_LENGTH_TICKS / 4,
                    ob_render::DAY_LENGTH_TICKS,
                )
                .time_of_day
            })
            .unwrap_or(0.5);
        let (health, hunger, defense, mobs, passive, hostile, items) = match &self.client {
            Some(client) => (
                client.health(),
                client.hunger(),
                client.armor_defense(),
                client.entities().mob_count(),
                client.entities().passive_mob_count(&self.registry),
                client.entities().hostile_mob_count(&self.registry),
                client.entities().item_count(),
            ),
            None => (20.0, 20.0, 0, 0, 0, 0, 0),
        };
        let network = format!("{network} items {items}");
        self.debug_stats = DebugStats {
            version: ob_core::VERSION.to_string(),
            fps: self.fps,
            frame_p50_ms: percentile(50.0),
            frame_p95_ms: percentile(95.0),
            position: pos,
            chunk,
            facing: facing_label(player.yaw),
            target,
            seed,
            render_distance: self.ui.settings.render_distance_chunks,
            loaded_chunks: loaded,
            drawn_sections: render_stats.sections as usize,
            cached_sections: self.meshes.len(),
            lod_batches: self.lod.batches().len(),
            lod_sections: self.lod.represented_sections() as usize,
            lod_skipped_sections: self.lod.skipped_sections() as usize,
            mesh_bytes: {
                let section_vertices: usize = self.meshes.values().copied().sum();
                (section_vertices as u64 * std::mem::size_of::<ob_render::Vertex>() as u64 + self.lod.vertex_bytes())
                    as usize
                    * 2
            },
            triangles: (render_stats.vertices + render_stats.water_vertices) / 3,
            draw_calls: render_stats.draw_calls as usize,
            mesh_workers: self.mesh_scheduler.workers(),
            mesh_in_flight: self.mesh_scheduler.in_flight_count(),
            mesh_completed: self.mesh_scheduler.completed_count(),
            mesh_backlog: backlog,
            heap_bytes: sections.saturating_mul(8 * 1024),
            ticks,
            adapter: self.renderer.adapter_summary().to_string(),
            window: [self.window_size.0 as u32, self.window_size.1 as u32],
            fov_degrees: self.ui.settings.fov_degrees,
            ui_scale: crate::ui_scale::debug_line(
                self.ui.settings.ui_scale_percent,
                [self.window_size.0, self.window_size.1],
            ),
            time_of_day,
            texture_source: crate::describe_texture_source(&self.texture_source, self.renderer.texture_resolution()),
            ui_font: self.renderer.ui_font_name().to_string(),
            game_mode: "survival",
            health,
            hunger,
            movement: if player.flying {
                "fly"
            } else if player.sprinting {
                "sprint"
            } else if player.sneaking {
                "sneak"
            } else {
                "walk"
            },
            held: held_item,
            held_durability,
            mining_speed,
            audio: self.audio_status.clone(),
            network: Some(network),
            mobs_total: mobs,
            mobs_passive: passive,
            mobs_hostile: hostile,
            day_phase: self.client.as_ref().map_or(0, |client| client.server_tick() % 24_000),
            day: time_of_day > 0.25 && time_of_day < 0.75,
            defense,
            graphics_summary: self.renderer.graphics().summary(),
            shadow_summary: self.renderer.shadow_summary(),
        };
    }

    fn network_status_line(&self) -> String {
        if let Some(reason) = &self.failure {
            return format!(
                "Multiplayer survival | Net: disconnected - {}",
                crate::net::friendly_disconnect_reason(reason)
            );
        }
        let Some(client) = &self.client else {
            return format!("Multiplayer survival | Net: connecting to {}", self.endpoint);
        };
        let ping = client
            .ping_ms()
            .map(|millis| format!("{millis}ms"))
            .unwrap_or_else(|| "--".to_string());
        let max = client.server_info().max_players;
        format!(
            "Multiplayer survival | Net: {} ping {} players {}/{} | mobs {}",
            client.endpoint(),
            ping,
            client.player_count(),
            max,
            client.entities().mob_count(),
        )
    }

    fn build_ui_frame(&self) -> Option<UiFrame> {
        let screen = self.window_screen();
        let mut frame = UiFrame::new(screen[0], screen[1]);
        let playing = self.ui.screen == Screen::Playing;
        let inventory_open = self.ui.screen == Screen::Inventory;
        let container_open = matches!(self.ui.screen, Screen::Furnace | Screen::Chest);
        let death_open = self.ui.screen == Screen::Death;
        let settings_open = self.ui.screen == Screen::Settings;
        let connecting = self.ui.screen == Screen::Connecting;
        let dead = self.client_dead();
        if playing {
            let client = self.client.as_ref();
            if self.ui.hud_visible {
                let centre_x = screen[0] / 2.0;
                let centre_y = screen[1] / 2.0;
                if !dead {
                    let cross = [0.95, 0.97, 1.0, 0.85];
                    frame.rect(centre_x - 1.0, centre_y - 9.0, 2.0, 18.0, cross);
                    frame.rect(centre_x - 9.0, centre_y - 1.0, 18.0, 2.0, cross);
                    if self.mining.is_mining() {
                        crate::mining::draw_progress(&mut frame, (centre_x, centre_y), self.mining.progress());
                    }
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
                if let Some(client) = client {
                    inventory::draw_survival_hotbar(
                        &mut frame,
                        &self.ui,
                        &self.registry,
                        client.inventory(),
                        client.selected_slot(),
                        screen,
                    );
                    if self.ui.block_name_seconds > 0.0
                        && let Some(stack) = client.selected_stack()
                    {
                        let label = inventory::item_display_name(&self.registry, stack.item);
                        let y = crate::selected_name_y(screen, &self.ui.hotbar_rects(screen));
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
                    crate::ui_state::draw_vitals(&mut frame, screen, client.health(), client.hunger());
                    crate::ui_state::draw_armor(&mut frame, screen, client.armor_defense());
                }
                crate::ui_state::draw_toasts(&mut frame, &self.ui.toasts, screen);
                crate::ui_state::draw_damage_flash(&mut frame, screen, self.damage_flash.alpha());
                net_ui::draw_multiplayer_notice(&mut frame, self.notice_seconds, screen);
            }
            if let Some(client) = client {
                let avatars = client.avatar_states(Instant::now());
                let players: Vec<(u64, &str, [f64; 3])> = avatars
                    .iter()
                    .map(|avatar| (avatar.id, avatar.name.as_str(), avatar.pos))
                    .collect();
                let tags = crate::name_tags::project_name_tags(
                    &self.last_camera,
                    screen,
                    &players,
                    crate::name_tags::NAME_TAG_MAX_DISTANCE,
                );
                crate::name_tags::draw_name_tags(&mut frame, &tags, screen);
            }
            if let Some(client) = client {
                net_ui::draw_chat(&mut frame, client.chat_log(), client.chat_input().text(), screen);
                if self.tab_held {
                    net_ui::draw_player_list(
                        &mut frame,
                        &client.player_entries(),
                        client.server_info().max_players,
                        client.ping_ms(),
                        screen,
                    );
                }
            }
            frame.text(
                12.0,
                screen[1] - 26.0,
                1.5,
                [0.85, 0.89, 0.94, 0.85],
                "Multiplayer survival - LMB mine/attack, RMB use/place, E inventory",
            );
        }
        if inventory_open && let Some(client) = &self.client {
            inventory::draw_survival(
                &mut frame,
                &self.ui,
                &self.registry,
                &inventory::SurvivalInventoryView {
                    inventory: client.inventory(),
                    equipment: client.equipment(),
                    defense: client.armor_defense(),
                    selected: client.selected_slot(),
                    near_table: false,
                    craft_hint: Some(CRAFTING_HINT),
                },
                screen,
            );
        }
        if container_open
            && let Some(client) = &self.client
            && let Some(window) = client.containers().window()
        {
            let view = client.container_snapshot();
            if let Some(view) = view {
                let session = ContainerSession {
                    kind: window.ui_kind(),
                    pos: window.pos,
                    cursor: client.containers().cursor().map(|cursor| cursor.stack),
                    selected: self.container_selected,
                };
                match window.ui_kind() {
                    ContainerKind::Furnace => containers::draw_furnace(
                        &mut frame,
                        &self.registry,
                        &session,
                        view.furnace.as_ref(),
                        &view,
                        self.ui.cursor,
                        screen,
                    ),
                    ContainerKind::Chest => {
                        // The view already has the cursor preview subtracted;
                        // drawing from the raw window state would show the held
                        // stack twice.
                        let chest = ContainerState { slots: view.chest };
                        containers::draw_chest(
                            &mut frame,
                            &self.registry,
                            &session,
                            Some(&chest),
                            &view,
                            self.ui.cursor,
                            screen,
                        );
                    }
                }
            }
        }
        if settings_open {
            let info = SettingsInfo {
                health: self.client.as_ref().map_or(20.0, NetworkClient::health),
                hunger: self.client.as_ref().map_or(20.0, NetworkClient::hunger),
                texture_source: self.texture_source.label(),
                texture_resolution: self.renderer.texture_resolution(),
                mesh_workers: self.mesh_scheduler.workers(),
                ui_scale_percent: resolved_ui_scale_percent(self.ui.settings.ui_scale_percent, self.window_size.1),
                audio_status: self.audio_status.clone(),
            };
            crate::ui_state::draw_settings_screen(&mut frame, &self.ui, &info, screen);
        }
        if death_open {
            let awaiting = self
                .client
                .as_ref()
                .is_some_and(|client| client.survival().respawn_requested);
            let note = if awaiting {
                "Waiting for the server to respawn you..."
            } else {
                DEATH_NOTE
            };
            crate::ui_state::draw_death_screen_custom(
                &mut frame,
                &self.ui,
                self.ui.death_cause.as_deref(),
                ["Respawn", "Disconnect"],
                note,
                "Enter/Click select   Up/Down choose   Esc focus buttons   F3 debug",
                screen,
            );
        }
        if connecting {
            let spinner = ".".repeat(1 + (self.connect_started.elapsed().as_secs_f32() * 2.0) as usize % 3);
            let status = if self.failure.is_some() {
                if self.client.is_some() {
                    "The server connection was closed."
                } else {
                    "The connection attempt failed."
                }
            } else if self.client.is_some() {
                "Streaming the spawn area"
            } else {
                "Contacting the server"
            };
            let connection = self.client.as_ref();
            let info = connection.map(NetworkClient::server_info);
            let title = if self.failure.is_some() {
                if self.client.is_some() {
                    "Disconnected"
                } else {
                    "Connection failed"
                }
            } else {
                "Connecting"
            };
            let panel = ConnectPanel {
                title,
                endpoint: &self.endpoint,
                player_name: &self.player_name,
                status,
                spinner: &spinner,
                world_name: info.map(|info| info.world_name.as_str()),
                motd: info.map(|info| info.motd.as_str()),
                players: connection.map(|client| (client.player_count(), client.server_info().max_players)),
                progress: connection.map(NetworkClient::loading_progress),
                failure: self.failure.as_deref(),
                selected: self.connect_selected.min(1),
                hovered: self.connect_hovered,
            };
            net_ui::draw_connect_panel(&mut frame, &panel, screen);
        }
        if self.ui.debug_overlay && self.ui.screen != Screen::Settings {
            crate::draw_debug_overlay(&mut frame, &self.debug_stats, screen);
        }
        Some(frame)
    }

    fn update_title(&mut self) {
        let player = self.player();
        let status = match self.ui.screen {
            Screen::Playing if self.chat_active() => "CHAT (Enter send, Esc cancel)",
            Screen::Playing => {
                if self.captured {
                    "T chat | Tab players | F3 debug"
                } else {
                    "click to capture | T chat | Tab players"
                }
            }
            Screen::Connecting => {
                if self.failure.is_some() {
                    "CONNECTION FAILED (R retry, Esc quit)"
                } else {
                    "CONNECTING..."
                }
            }
            Screen::Settings => "SETTINGS (Esc to resume)",
            Screen::Inventory => "INVENTORY",
            Screen::Crafting => "CRAFTING",
            Screen::Furnace => "FURNACE",
            Screen::Chest => "CHEST",
            Screen::Death => "DEAD",
        };
        let world = self
            .client
            .as_ref()
            .map_or("multiplayer", |client| client.server_info().world_name.as_str());
        self.window.set_title(&format!(
            "OpenBlock {} | {} FPS | {:.0}, {:.0}, {:.0} | {} | multiplayer survival @ {} | {}",
            ob_core::VERSION,
            self.fps,
            player.pos.x,
            player.pos.y,
            player.pos.z,
            world,
            self.endpoint,
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
            "multiplayer frame stats: {} frames in {:.1}s, {:.1} FPS mean, p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
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

    fn apply_settings_row(&mut self, row: SettingsRow) {
        match row {
            SettingsRow::RenderDistance => {
                let requested = self.ui.settings.render_distance_chunks.clamp(
                    ob_core::config::MIN_RENDER_DISTANCE,
                    ob_core::config::MAX_RENDER_DISTANCE,
                );
                if let Some(client) = &mut self.client {
                    client.set_render_radius(requested);
                }
                self.applied_render_distance = requested;
            }
            SettingsRow::Fullscreen => self.apply_fullscreen(self.ui.settings.fullscreen),
            SettingsRow::GameMode => {
                self.ui.toasts.push("The multiplayer server controls the game mode");
            }
            SettingsRow::TextureStyle => {
                let style = crate::render_texture_style(self.ui.settings.texture_style);
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
            SettingsRow::GraphicsPreset
            | SettingsRow::ShadowQuality
            | SettingsRow::Clouds
            | SettingsRow::WaterQuality
            | SettingsRow::CloudCoverage
            | SettingsRow::Bloom
            | SettingsRow::Fxaa => {
                self.renderer
                    .set_graphics(crate::graphics_settings_from_settings(&self.ui.settings));
            }
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
        self.persist_settings();
    }

    fn handle_settings_action(&mut self, action: SettingsAction) -> bool {
        match action {
            SettingsAction::Resume => {
                self.ui.resume();
                if self.client_playing() && !self.client_dead() {
                    self.capture_pointer();
                }
                self.persist_settings();
                false
            }
            SettingsAction::SaveAndQuit => {
                self.shutdown();
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
                let source = crate::reset_ui_font(&mut self.renderer, &mut self.ui.settings);
                self.ui.active_ui_font_name = self.renderer.ui_font_name().to_string();
                self.ui.active_ui_font_source = source.display_label();
                self.persist_settings();
                false
            }
            SettingsAction::ReloadUiFont => {
                let source = crate::reload_ui_font(&mut self.renderer, &self.ui.settings, &self.data_dir);
                self.ui.active_ui_font_name = self.renderer.ui_font_name().to_string();
                self.ui.active_ui_font_source = source.display_label();
                false
            }
        }
    }

    fn set_active_texture_style(&mut self, style: ob_render::TexturePackStyle) {
        let source = TextureSource::Style(style);
        match self.renderer.set_texture_source(source.clone()) {
            Ok(()) => {
                self.texture_source = source;
                self.sync_ui_texture_source();
                log::info!("textures: {}", self.texture_source.label());
            }
            Err(error) => log::warn!("could not switch to texture style {}: {error}", style.name()),
        }
    }

    fn toggle_texture_pack(&mut self) {
        match crate::texture_toggle_target(&self.texture_source, &self.ui.settings) {
            crate::TextureToggle::Noop => {}
            crate::TextureToggle::Disable => {
                let style = crate::render_texture_style(self.ui.settings.texture_style);
                let source = TextureSource::Style(style);
                match self.renderer.set_texture_source(source.clone()) {
                    Ok(()) => {
                        self.texture_source = source;
                        self.ui.settings.texture_pack = None;
                        self.sync_ui_texture_source();
                        log::info!("textures: {}", self.texture_source.label());
                    }
                    Err(error) => log::warn!("could not switch to the built-in texture style: {error}"),
                }
            }
            crate::TextureToggle::Enable(path) => {
                let source = TextureSource::ResourcePack(path.clone());
                match self.renderer.set_texture_source(source.clone()) {
                    Ok(()) => {
                        self.texture_source = source;
                        self.ui.settings.texture_pack = Some(path.to_string_lossy().into_owned());
                        self.sync_ui_texture_source();
                        log::info!("textures: {}", self.texture_source.label());
                    }
                    Err(error) => log::warn!(
                        "could not load texture pack {}: {error}; keeping the built-in style",
                        path.display()
                    ),
                }
            }
        }
    }

    fn sync_ui_texture_source(&mut self) {
        self.ui.active_texture_pack = crate::active_texture_pack_label(&self.texture_source);
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

/// Human-readable server `container_close` reason.
fn container_close_hint(reason: &str) -> &'static str {
    match reason {
        "out_of_range" => "You moved too far from the container",
        "block_removed" => "The container was destroyed",
        "replaced" => "Another container replaced this window",
        "disconnect" => "Disconnected from the server",
        _ => "The container closed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_player_name_is_protocol_valid() {
        let name = default_player_name();
        assert_eq!(name.len(), 10);
        assert!(name.starts_with("player"));
        assert!(ob_protocol::validate_player_name(&name).is_ok());
    }

    #[test]
    fn digits_map_to_hotbar_slots() {
        assert_eq!(digit_slot(KeyCode::Digit1), Some(0));
        assert_eq!(digit_slot(KeyCode::Digit9), Some(8));
        assert_eq!(digit_slot(KeyCode::Digit0), None);
    }

    #[test]
    fn container_close_reasons_have_hints() {
        assert!(container_close_hint("out_of_range").contains("far"));
        assert!(container_close_hint("block_removed").contains("destroyed"));
        assert!(container_close_hint("unknown").contains("closed"));
    }
}
