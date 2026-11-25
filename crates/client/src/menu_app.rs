//! Windowed title/world-selection runtime and offscreen menu screenshots.
//!
//! The front-end owns a renderer and an audio engine but no [`Host`]: worlds are
//! opened only after the player chooses one, and multiplayer hands off to the
//! existing `net_app` runtime so the two network flows stay identical. Settings
//! are the same [`UiState`] screen used in game with `host_optional = true`, so
//! display/audio/texture/control changes work before a world exists.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_audio::AudioEngine;
use ob_core::config::{Settings, TextureStyle, load_settings, remember_recent_server, save_settings};
use ob_math::WorldVec;
use ob_render::{Camera, Environment, Renderer, SceneParams, TextureSource, UiFrame};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::ClientOptions;
use crate::frontend::{
    self, MenuScreen, MultiplayerField, MultiplayerForm, NewWorldField, NewWorldForm, TitleButton, TitleMenu,
    WorldList, continue_target, discover_worlds, is_double_click,
};
use crate::ui_scale::{physical_to_ui_point, rescale_cursor, resolved_ui_scale_percent, ui_scale_factor, ui_screen};
use crate::ui_state::{SettingsAction, SettingsInfo, SettingsRow, SettingsTab, UiState};

/// Result of running the front-end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuOutcome {
    /// Close the process.
    Quit,
    /// Start a single-player world (created or loaded).
    PlayWorld {
        /// World directory name.
        world: String,
        /// Seed used if the world is newly created (ignored for existing saves).
        seed: u64,
    },
    /// Start the network runtime.
    Connect {
        /// Validated `host:port` endpoint.
        address: String,
        /// Validated player name.
        name: String,
    },
}

/// Menu screen selected by the offscreen screenshot flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuScreenshot {
    /// Title with a deterministic Continue target.
    Title,
    /// New-world form.
    NewWorld,
    /// World list with deterministic fake worlds.
    Worlds,
    /// Multiplayer form with deterministic recent servers.
    Multiplayer,
    /// Multiplayer form showing a deterministic validation error.
    MultiplayerError,
    /// Host-optional settings screen as opened from the title.
    Settings,
}

/// Windowed front-end handler. The root event-loop router owns the single
/// winit event loop and swaps this handler for the game or network runtime once
/// the player chooses a world or server.
pub(crate) struct MenuApp {
    options: ClientOptions,
    state: Option<MenuRuntime>,
    startup_outcome: Option<MenuOutcome>,
}

impl MenuApp {
    /// Creates the front-end handler for an existing set of client options.
    pub(crate) fn new(options: ClientOptions) -> Self {
        Self {
            options,
            state: None,
            startup_outcome: None,
        }
    }

    /// Returns the pending choice, if the front-end is finished.
    pub(crate) fn take_outcome(&mut self) -> Option<MenuOutcome> {
        if let Some(outcome) = self.startup_outcome.take() {
            return Some(outcome);
        }
        self.state.as_mut().and_then(|state| state.outcome.take())
    }
}

impl ApplicationHandler for MenuApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("OpenBlock {} - Menu", ob_core::VERSION))
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                log::error!("failed to create window: {error}");
                self.startup_outcome = Some(MenuOutcome::Quit);
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
                    self.startup_outcome = Some(MenuOutcome::Quit);
                    return;
                }
            };
        crate::log_texture_source(&texture_source);
        log::info!("GPU: {}", renderer.adapter_summary());
        let mut ui = UiState::from_settings(settings);
        ui.host_optional = true;
        ui.active_texture_pack = crate::active_texture_pack_label(&texture_source);
        ui.active_ui_font_name = renderer.ui_font_name().to_string();
        ui.active_ui_font_source = font_source.display_label();
        let worlds = discover_worlds(&data_dir);
        let title = TitleMenu::new(continue_target(&data_dir, ui.settings.last_world.as_deref()));
        let audio_enabled_before_mute = if self.options.mute {
            Some(load_settings(&data_dir).audio_enabled)
        } else {
            None
        };
        let (audio, audio_status) = crate::open_audio(&self.options, &ui.settings);
        let window_size = {
            let size = window.inner_size();
            (size.width.max(1) as f32, size.height.max(1) as f32)
        };
        ui.ui_scale_percent = resolved_ui_scale_percent(ui.settings.ui_scale_percent, window_size.1);
        let auto_exit = self
            .options
            .seconds
            .map(|seconds| Duration::from_secs_f32(seconds.max(0.5)));
        let mut state = MenuRuntime {
            window,
            renderer,
            texture_source,
            ui,
            data_dir,
            screen: MenuScreen::Title,
            title,
            new_world: NewWorldForm::default(),
            worlds: WorldList::new(worlds),
            multiplayer: MultiplayerForm::new(),
            audio,
            audio_status,
            audio_enabled_before_mute,
            audio_setting_touched: false,
            fsr_run_override: self.options.fsr.is_some(),
            fsr_setting_touched: false,
            window_size,
            fullscreen: false,
            started: Instant::now(),
            auto_exit,
            last_frame: Instant::now(),
            last_world_click: None,
            frames: 0,
            mesh_workers: self.options.mesh_workers,
            quit_requested: false,
            outcome: None,
        };
        let fullscreen = state.ui.settings.fullscreen;
        state.apply_fullscreen(fullscreen);
        state.update_title();
        log::info!(
            "title screen: {} worlds, last world {:?}, renderer {}",
            state.worlds.worlds.len(),
            state.ui.settings.last_world,
            state.renderer.adapter_summary()
        );
        self.state = Some(state);
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                state.persist_settings();
                state.outcome = Some(MenuOutcome::Quit);
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width.max(1), size.height.max(1));
                state.window_size = (size.width.max(1) as f32, size.height.max(1) as f32);
                state.refresh_ui_scale();
            }
            WindowEvent::RedrawRequested => state.frame(),
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                if state.screen == MenuScreen::Settings {
                    state.handle_settings_key(code, pressed);
                } else {
                    state.handle_menu_key(code, pressed, event.text.as_deref(), event.repeat);
                }
                if pressed {
                    state.update_title();
                }
            }
            WindowEvent::MouseInput {
                state: element, button, ..
            } => {
                if element != ElementState::Pressed {
                    return;
                }
                state.handle_click(button);
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.ui.cursor = state.ui_point((position.x as f32, position.y as f32));
                if state.screen == MenuScreen::Settings {
                    let screen = state.window_screen();
                    state.ui.settings_hover(screen);
                } else if state.screen == MenuScreen::Title {
                    let screen = state.window_screen();
                    state.title.update_hover(screen, state.ui.cursor);
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
                match state.screen {
                    MenuScreen::Settings => {
                        if let Some(row) = state.ui.settings_adjust(steps.signum()) {
                            state.apply_settings_row(row);
                        }
                    }
                    MenuScreen::LoadWorld if state.worlds.confirm_delete.is_none() => {
                        let screen = state.window_screen();
                        let visible = state.worlds.visible_rows(screen);
                        state.worlds.move_selection(-steps.signum(), visible);
                    }
                    MenuScreen::Multiplayer => {
                        if state.multiplayer.field == MultiplayerField::Recents {
                            state
                                .multiplayer
                                .move_recent(-steps.signum(), state.ui.settings.recent_servers.len());
                        }
                    }
                    MenuScreen::Title | MenuScreen::NewWorld | MenuScreen::LoadWorld => {}
                }
            }
            WindowEvent::Focused(false) => {}
            WindowEvent::DroppedFile(path) => log::info!("ignoring dropped file: {}", path.display()),
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, _event: DeviceEvent) {}

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.quit_requested || state.auto_exit.is_some_and(|limit| state.started.elapsed() >= limit) {
            state.persist_settings();
            state.outcome = Some(MenuOutcome::Quit);
            return;
        }
        state.window.request_redraw();
    }
}

struct MenuRuntime {
    window: Arc<Window>,
    renderer: Renderer,
    texture_source: TextureSource,
    ui: UiState,
    data_dir: PathBuf,
    screen: MenuScreen,
    title: TitleMenu,
    new_world: NewWorldForm,
    worlds: WorldList,
    multiplayer: MultiplayerForm,
    audio: Option<AudioEngine>,
    audio_status: String,
    audio_enabled_before_mute: Option<bool>,
    audio_setting_touched: bool,
    /// Whether `--fsr` supplied a run-only override for this launch.
    fsr_run_override: bool,
    /// Whether the user changed the FSR row after a `--fsr` launch.
    fsr_setting_touched: bool,
    window_size: (f32, f32),
    fullscreen: bool,
    started: Instant,
    auto_exit: Option<Duration>,
    last_frame: Instant,
    last_world_click: Option<(SystemTime, usize)>,
    frames: u64,
    mesh_workers: usize,
    quit_requested: bool,
    outcome: Option<MenuOutcome>,
}

impl MenuRuntime {
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

    fn update_title(&mut self) {
        self.window
            .set_title(&format!("OpenBlock {} | {}", ob_core::VERSION, self.screen.label()));
    }

    fn play_ui_sound(&mut self, confirm: bool) {
        if let Some(engine) = &mut self.audio {
            engine.play_sfx(if confirm {
                ob_audio::Sfx::UiConfirm
            } else {
                ob_audio::Sfx::UiClick
            });
        }
    }

    fn sync_audio_settings(&mut self) {
        if let Some(engine) = &mut self.audio {
            crate::apply_audio_settings(engine, &self.ui.settings);
        }
    }

    fn persist_settings(&self) {
        let mut settings = self.ui.settings.clone();
        if let Some(saved) = self.audio_enabled_before_mute
            && !self.audio_setting_touched
        {
            settings.audio_enabled = saved;
        }
        crate::restore_fsr_after_run_only_override(
            &mut settings,
            self.fsr_run_override,
            self.fsr_setting_touched,
            &self.data_dir,
        );
        if let Err(error) = save_settings(&self.data_dir, &settings) {
            log::warn!("could not save settings: {error}");
        }
    }

    fn apply_fullscreen(&mut self, fullscreen: bool) {
        if fullscreen == self.fullscreen {
            return;
        }
        self.window.set_fullscreen(if fullscreen {
            Some(Fullscreen::Borderless(None))
        } else {
            None
        });
        self.fullscreen = fullscreen;
    }

    fn open_settings(&mut self) {
        self.screen = MenuScreen::Settings;
        self.ui.screen = crate::ui_state::Screen::Playing;
        self.ui.open_settings();
        self.ui.host_optional = true;
        self.ui.text_input_active = false;
        self.ui.settings_tab = SettingsTab::Game;
    }

    fn close_settings(&mut self) {
        self.ui.close_settings();
        self.ui.host_optional = true;
        self.screen = MenuScreen::Title;
        self.ui.text_input_active = false;
        self.persist_settings();
    }

    fn back_to_title(&mut self) {
        self.ui.cancel_bind();
        self.ui.text_input_active = false;
        self.worlds.cancel_delete();
        self.screen = MenuScreen::Title;
        self.persist_settings();
    }

    fn open_worlds(&mut self) {
        self.worlds.refresh(&self.data_dir);
        self.worlds.error = None;
        self.screen = MenuScreen::LoadWorld;
    }

    fn launch_world(&mut self, world: String, seed: u64) {
        self.ui.settings.last_world = Some(world.clone());
        remember_last_world_choice(&self.data_dir, &world);
        self.persist_settings();
        self.play_ui_sound(true);
        log::info!("front-end: starting world '{world}' with seed {seed}");
        self.outcome = Some(MenuOutcome::PlayWorld { world, seed });
    }

    fn continue_world(&mut self) {
        if let Some(world) = self.title.continue_world.clone() {
            self.launch_world(world, 0);
        }
    }

    fn create_world(&mut self) {
        match self.new_world.validate(&self.data_dir) {
            Ok(request) => self.launch_world(request.name, request.seed),
            Err(error) => {
                self.new_world.error = Some(error);
                self.play_ui_sound(false);
            }
        }
    }

    fn connect(&mut self) {
        match self.multiplayer.validate() {
            Ok(request) => {
                remember_recent_server(&mut self.ui.settings.recent_servers, &request.address, &request.name);
                self.persist_settings();
                self.play_ui_sound(true);
                log::info!("front-end: connecting to {} as '{}'", request.address, request.name);
                self.outcome = Some(MenuOutcome::Connect {
                    address: request.address,
                    name: request.name,
                });
            }
            Err(error) => {
                self.multiplayer.error = Some(error);
                self.play_ui_sound(false);
            }
        }
    }

    fn handle_menu_key(&mut self, code: KeyCode, pressed: bool, text: Option<&str>, repeat: bool) {
        if !pressed {
            return;
        }
        match self.screen {
            MenuScreen::Title => match code {
                KeyCode::ArrowUp => self.title.move_selection(-1),
                KeyCode::ArrowDown => self.title.move_selection(1),
                KeyCode::Enter if !repeat => {
                    if let Some(button) = self.title.activate() {
                        self.activate_title_button(button);
                    }
                }
                KeyCode::Escape => {
                    self.persist_settings();
                    self.outcome = Some(MenuOutcome::Quit);
                }
                _ => {}
            },
            MenuScreen::NewWorld => match code {
                KeyCode::Escape => self.back_to_title(),
                KeyCode::Tab | KeyCode::ArrowDown => self.new_world.next_field(),
                KeyCode::ArrowUp => self.new_world.previous_field(),
                KeyCode::Backspace | KeyCode::Delete => {
                    self.new_world.backspace();
                }
                KeyCode::Enter if !repeat => self.create_world(),
                _ => {
                    if let Some(text) = text {
                        for character in text.chars() {
                            self.new_world.insert_char(character);
                        }
                    }
                }
            },
            MenuScreen::LoadWorld => {
                if let Some(name) = self.worlds.confirm_delete.clone() {
                    match code {
                        KeyCode::Enter | KeyCode::KeyY => match self.worlds.confirm_delete(&self.data_dir) {
                            Ok(()) => {
                                log::info!("front-end: deleted world '{name}'");
                                self.play_ui_sound(true);
                            }
                            Err(error) => {
                                self.worlds.error = Some(error);
                                self.play_ui_sound(false);
                            }
                        },
                        KeyCode::Escape | KeyCode::KeyN => self.worlds.cancel_delete(),
                        _ => {}
                    }
                    return;
                }
                match code {
                    KeyCode::Escape => self.back_to_title(),
                    KeyCode::ArrowUp => {
                        let screen = self.window_screen();
                        self.worlds.move_selection(-1, self.worlds.visible_rows(screen));
                    }
                    KeyCode::ArrowDown => {
                        let screen = self.window_screen();
                        self.worlds.move_selection(1, self.worlds.visible_rows(screen));
                    }
                    KeyCode::Enter if !repeat => {
                        if let Some(world) = self.worlds.selected_world().map(|world| world.name.clone()) {
                            self.launch_world(world, 0);
                        }
                    }
                    KeyCode::Delete => {
                        self.worlds.request_delete();
                    }
                    _ => {}
                }
            }
            MenuScreen::Multiplayer => match code {
                KeyCode::Escape => self.back_to_title(),
                KeyCode::Tab | KeyCode::ArrowDown if self.multiplayer.field == MultiplayerField::Recents => {
                    let count = self.ui.settings.recent_servers.len();
                    if code == KeyCode::Tab {
                        self.multiplayer.next_field(count);
                    } else {
                        self.multiplayer.move_recent(1, count);
                    }
                }
                KeyCode::ArrowUp if self.multiplayer.field == MultiplayerField::Recents => {
                    self.multiplayer.move_recent(-1, self.ui.settings.recent_servers.len());
                }
                KeyCode::Tab => {
                    let count = self.ui.settings.recent_servers.len();
                    self.multiplayer.next_field(count);
                }
                KeyCode::ArrowUp | KeyCode::ArrowDown => {
                    let count = self.ui.settings.recent_servers.len();
                    if code == KeyCode::ArrowUp {
                        self.multiplayer.previous_field(count);
                    } else {
                        self.multiplayer.next_field(count);
                    }
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    self.multiplayer.backspace();
                }
                KeyCode::Enter if !repeat => self.connect(),
                _ => {
                    if let Some(text) = text {
                        for character in text.chars() {
                            self.multiplayer.insert_char(character);
                        }
                    }
                }
            },
            MenuScreen::Settings => self.handle_settings_key(code, true),
        }
    }

    fn activate_title_button(&mut self, button: TitleButton) {
        self.play_ui_sound(false);
        match button {
            TitleButton::Continue => self.continue_world(),
            TitleButton::NewWorld => {
                self.screen = MenuScreen::NewWorld;
                self.ui.text_input_active = true;
                self.new_world.error = None;
            }
            TitleButton::LoadWorld => self.open_worlds(),
            TitleButton::Multiplayer => {
                self.screen = MenuScreen::Multiplayer;
                self.ui.text_input_active = true;
                self.multiplayer.error = None;
            }
            TitleButton::Settings => self.open_settings(),
            TitleButton::Quit => {
                self.persist_settings();
                self.outcome = Some(MenuOutcome::Quit);
            }
        }
    }

    fn handle_settings_key(&mut self, code: KeyCode, pressed: bool) {
        if !pressed {
            return;
        }
        if self.ui.bind_capture.is_some() {
            match code {
                KeyCode::Escape => self.ui.cancel_bind(),
                KeyCode::Backspace | KeyCode::Delete => {
                    if self.ui.clear_binding().is_some() {
                        self.persist_settings();
                    }
                }
                _ => {
                    if let Some(key) = crate::bind_key_for_code(code)
                        && self.ui.apply_bind(key).is_ok()
                    {
                        self.persist_settings();
                    }
                }
            }
            return;
        }
        if matches!(code, KeyCode::Delete | KeyCode::Backspace) && self.ui.clear_binding().is_some() {
            self.persist_settings();
            return;
        }
        match code {
            KeyCode::Escape => self.close_settings(),
            KeyCode::Tab => self.ui.settings_tab_next(),
            KeyCode::ArrowUp => self.ui.settings_row_up(),
            KeyCode::ArrowDown => self.ui.settings_row_down(),
            KeyCode::ArrowLeft => {
                if let Some(row) = self.ui.settings_adjust(-1) {
                    self.apply_settings_row(row);
                }
            }
            KeyCode::ArrowRight => {
                if let Some(row) = self.ui.settings_adjust(1) {
                    self.apply_settings_row(row);
                }
            }
            KeyCode::Enter => {
                if let Some(action) = self.ui.settings_activate() {
                    self.handle_settings_action(action);
                }
            }
            _ => {}
        }
    }

    fn handle_settings_action(&mut self, action: SettingsAction) {
        match action {
            SettingsAction::Resume => self.close_settings(),
            SettingsAction::SaveAndQuit => {
                self.persist_settings();
                self.outcome = Some(MenuOutcome::Quit);
                self.quit_requested = true;
            }
            SettingsAction::Adjust(row) => self.apply_settings_row(row),
            SettingsAction::BeginBind(action) => {
                self.play_ui_sound(false);
                self.ui.begin_bind(action);
            }
            SettingsAction::ResetKeybinds => {
                self.ui.reset_keybinds();
                self.persist_settings();
                self.play_ui_sound(true);
            }
            SettingsAction::ResetUiFont => {
                let source = crate::reset_ui_font(&mut self.renderer, &mut self.ui.settings);
                self.ui.active_ui_font_name = self.renderer.ui_font_name().to_string();
                self.ui.active_ui_font_source = source.display_label();
                self.persist_settings();
                self.play_ui_sound(true);
            }
            SettingsAction::ReloadUiFont => {
                let source = crate::reload_ui_font(&mut self.renderer, &self.ui.settings, &self.data_dir);
                self.ui.active_ui_font_name = self.renderer.ui_font_name().to_string();
                self.ui.active_ui_font_source = source.display_label();
                self.play_ui_sound(true);
            }
        }
    }

    fn apply_settings_row(&mut self, row: SettingsRow) {
        match row {
            SettingsRow::Fullscreen => self.apply_fullscreen(self.ui.settings.fullscreen),
            SettingsRow::TextureStyle => {
                let style = crate::render_texture_style(self.ui.settings.texture_style);
                self.set_texture_source(TextureSource::Style(style));
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
            | SettingsRow::Fxaa
            | SettingsRow::Fsr => {
                if row == SettingsRow::Fsr {
                    self.fsr_setting_touched = true;
                }
                self.renderer
                    .set_graphics(crate::graphics_settings_from_settings(&self.ui.settings));
            }
            SettingsRow::RenderDistance
            | SettingsRow::ViewDistance
            | SettingsRow::Fov
            | SettingsRow::MouseSensitivity
            | SettingsRow::ReducedMotion
            | SettingsRow::ShowHud
            | SettingsRow::DebugOverlay
            | SettingsRow::MeshWorkers
            | SettingsRow::UiFont
            | SettingsRow::ResetUiFont
            | SettingsRow::ReloadUiFont
            | SettingsRow::GameMode
            | SettingsRow::Keybind(_)
            | SettingsRow::ResetKeybinds => {}
        }
        self.persist_settings();
    }

    fn set_texture_source(&mut self, source: TextureSource) {
        match self.renderer.set_texture_source(source.clone()) {
            Ok(()) => {
                self.texture_source = source;
                self.ui.active_texture_pack = crate::active_texture_pack_label(&self.texture_source);
                log::info!("textures: {}", self.texture_source.label());
            }
            Err(error) => log::warn!("could not switch texture source: {error}"),
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
                        self.ui.active_texture_pack = None;
                    }
                    Err(error) => log::warn!("could not switch to built-in textures: {error}"),
                }
            }
            crate::TextureToggle::Enable(path) => {
                let source = TextureSource::ResourcePack(path.clone());
                match self.renderer.set_texture_source(source.clone()) {
                    Ok(()) => {
                        self.texture_source = source;
                        self.ui.settings.texture_pack = Some(path.to_string_lossy().into_owned());
                        self.ui.active_texture_pack = crate::active_texture_pack_label(&self.texture_source);
                    }
                    Err(error) => log::warn!("could not load texture pack {}: {error}", path.display()),
                }
            }
        }
    }

    fn handle_click(&mut self, button: MouseButton) {
        match self.screen {
            MenuScreen::Settings => {
                if self.ui.bind_capture.is_some() {
                    if let Some(key) = crate::bind_key_for_mouse(button)
                        && self.ui.apply_bind(key).is_ok()
                    {
                        self.persist_settings();
                    }
                    return;
                }
                if button != MouseButton::Left {
                    return;
                }
                let screen = self.window_screen();
                let action = self.ui.settings_click(screen);
                let interactive = self.ui.settings_hovered_tab.is_some()
                    || self.ui.settings_hovered_row.is_some()
                    || self.ui.settings_hovered_button.is_some();
                if interactive {
                    self.play_ui_sound(matches!(
                        action,
                        Some(SettingsAction::Resume | SettingsAction::SaveAndQuit)
                    ));
                }
                if let Some(action) = action {
                    self.handle_settings_action(action);
                }
            }
            MenuScreen::Title => {
                if button != MouseButton::Left {
                    return;
                }
                let screen = self.window_screen();
                if let Some(chosen) = self.title.update_hover(screen, self.ui.cursor)
                    && let Some(button) = self.title.click(chosen)
                {
                    self.activate_title_button(button);
                }
            }
            MenuScreen::NewWorld => {
                if button != MouseButton::Left {
                    return;
                }
                let screen = self.window_screen();
                let fields = frontend::new_world_field_rects(screen);
                if fields[0].contains(self.ui.cursor.0, self.ui.cursor.1) {
                    self.new_world.field = NewWorldField::Name;
                } else if fields[1].contains(self.ui.cursor.0, self.ui.cursor.1) {
                    self.new_world.field = NewWorldField::Seed;
                } else {
                    let [create, back] = frontend::form_button_rects(screen);
                    if create.contains(self.ui.cursor.0, self.ui.cursor.1) {
                        self.create_world();
                    } else if back.contains(self.ui.cursor.0, self.ui.cursor.1) {
                        self.back_to_title();
                    }
                }
            }
            MenuScreen::LoadWorld => {
                if button != MouseButton::Left {
                    return;
                }
                let screen = self.window_screen();
                if self.worlds.confirm_delete.is_some() {
                    let [delete, cancel] = frontend::delete_confirm_button_rects(screen);
                    if delete.contains(self.ui.cursor.0, self.ui.cursor.1) {
                        match self.worlds.confirm_delete(&self.data_dir) {
                            Ok(()) => self.play_ui_sound(true),
                            Err(error) => {
                                self.worlds.error = Some(error);
                                self.play_ui_sound(false);
                            }
                        }
                    } else if cancel.contains(self.ui.cursor.0, self.ui.cursor.1) {
                        self.worlds.cancel_delete();
                    }
                    return;
                }
                if let Some(index) = self.worlds.select_at(screen, self.ui.cursor) {
                    let now = SystemTime::now();
                    let double = is_double_click(self.last_world_click, now, index);
                    self.last_world_click = Some((now, index));
                    if double && let Some(world) = self.worlds.selected_world().map(|world| world.name.clone()) {
                        self.launch_world(world, 0);
                    }
                    return;
                }
                let [play, delete, back] = frontend::world_list_button_rects(screen);
                if play.contains(self.ui.cursor.0, self.ui.cursor.1) {
                    if let Some(world) = self.worlds.selected_world().map(|world| world.name.clone()) {
                        self.launch_world(world, 0);
                    }
                } else if delete.contains(self.ui.cursor.0, self.ui.cursor.1) {
                    self.worlds.request_delete();
                } else if back.contains(self.ui.cursor.0, self.ui.cursor.1) {
                    self.back_to_title();
                }
            }
            MenuScreen::Multiplayer => {
                if button != MouseButton::Left {
                    return;
                }
                let screen = self.window_screen();
                let fields = frontend::multiplayer_field_rects(screen);
                if fields[0].contains(self.ui.cursor.0, self.ui.cursor.1) {
                    self.multiplayer.field = MultiplayerField::Address;
                } else if fields[1].contains(self.ui.cursor.0, self.ui.cursor.1) {
                    self.multiplayer.field = MultiplayerField::Name;
                } else if let Some(index) = frontend::recent_server_rects(screen)
                    .iter()
                    .position(|rect| rect.contains(self.ui.cursor.0, self.ui.cursor.1))
                {
                    if let Some(recent) = self.ui.settings.recent_servers.get(index).cloned() {
                        self.multiplayer.select_recent(&recent);
                        self.multiplayer.recent_selected = index;
                    }
                } else {
                    let [connect, back] = frontend::form_button_rects(screen);
                    if connect.contains(self.ui.cursor.0, self.ui.cursor.1) {
                        self.connect();
                    } else if back.contains(self.ui.cursor.0, self.ui.cursor.1) {
                        self.back_to_title();
                    }
                }
            }
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let _dt = now - self.last_frame;
        self.last_frame = now;
        let screen = self.window_screen();
        let mut frame = UiFrame::new(screen[0], screen[1]);
        match self.screen {
            MenuScreen::Title => frontend::draw_title_screen(&mut frame, &self.title, self.ui.cursor, screen),
            MenuScreen::NewWorld => {
                frontend::draw_new_world_screen(&mut frame, &self.new_world, self.ui.cursor, screen)
            }
            MenuScreen::LoadWorld => frontend::draw_world_list_screen(&mut frame, &self.worlds, self.ui.cursor, screen),
            MenuScreen::Multiplayer => frontend::draw_multiplayer_screen(
                &mut frame,
                &self.multiplayer,
                &self.ui.settings.recent_servers,
                self.ui.cursor,
                screen,
            ),
            MenuScreen::Settings => {
                let info = SettingsInfo {
                    texture_source: self.texture_source.label(),
                    texture_resolution: self.renderer.texture_resolution(),
                    mesh_workers: self.mesh_workers,
                    ui_scale_percent: resolved_ui_scale_percent(self.ui.settings.ui_scale_percent, self.window_size.1),
                    audio_status: self.audio_status.clone(),
                    fsr_internal: self.renderer.fsr_internal_size().map(|(width, height)| [width, height]),
                    ..SettingsInfo::default()
                };
                crate::ui_state::draw_settings_screen(&mut frame, &self.ui, &info, screen);
            }
        }
        let mut camera = Camera::new(WorldVec::new(0.5, 74.0, 6.0), 0.0, -0.12);
        camera.fov_y = self.ui.settings.fov_degrees.to_radians();
        camera.far = 256.0;
        let environment = Environment::default();
        let scene = SceneParams {
            environment,
            ui: Some(&frame),
            outline: None,
            time_seconds: 0.0,
            ..SceneParams::default()
        };
        if let Err(error) = self.renderer.render_scene(&camera, &scene) {
            log::error!("menu render error: {error}");
        }
        self.frames += 1;
    }
}

/// Renders one deterministic front-end screenshot without opening a window.
pub fn screenshot_menu(options: &ClientOptions, mode: MenuScreenshot) -> Result<(), Box<dyn std::error::Error>> {
    let path = options
        .screenshot
        .as_ref()
        .ok_or("front-end screenshots require --screenshot PATH")?;
    let deterministic = options.data_dir.is_none();
    let (data_dir, cleanup) = if deterministic {
        (seed_screenshot_data()?, true)
    } else {
        (options.data_dir.clone().unwrap_or_else(ob_core::data_dir), false)
    };
    let result = screenshot_menu_inner(options, mode, path, &data_dir);
    if cleanup {
        let _ = std::fs::remove_dir_all(&data_dir);
    }
    result
}

fn screenshot_menu_inner(
    options: &ClientOptions,
    mode: MenuScreenshot,
    path: &Path,
    data_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut settings = load_settings(data_dir);
    // Menu screenshots are captured with the classic procedural textures unless
    // the caller explicitly selected another style.
    settings.texture_style = options.texture_style.unwrap_or(TextureStyle::Classic);
    if let Some(percent) = options.ui_scale {
        settings.ui_scale_percent = percent;
    }
    settings.clamp();
    let (width, height) = crate::screenshot_dimensions(options);
    let (mut renderer, texture_source, font_source) =
        crate::create_offscreen_renderer(options, &settings, data_dir, None)?;
    let physical = [width as f32, height as f32];
    let screen = ui_screen(physical, settings.ui_scale_percent);
    let effective_ui_scale = resolved_ui_scale_percent(settings.ui_scale_percent, physical[1]);
    let mut ui = UiState::from_settings(settings.clone());
    ui.host_optional = true;
    ui.ui_scale_percent = effective_ui_scale;
    ui.active_ui_font_name = renderer.ui_font_name().to_string();
    ui.active_ui_font_source = font_source.display_label();
    let worlds = discover_worlds(data_dir);
    let mut frame = UiFrame::new(screen[0], screen[1]);
    let mut camera = Camera::new(WorldVec::new(0.5, 74.0, 6.0), 0.0, -0.12);
    camera.fov_y = settings.fov_degrees.to_radians();
    camera.far = 256.0;
    let cursor = (-100.0, -100.0);
    let mut detail = String::new();
    match mode {
        MenuScreenshot::Title => {
            let title = TitleMenu::new(continue_target(data_dir, ui.settings.last_world.as_deref()));
            detail = format!("continue={:?} worlds={}", title.continue_world, worlds.len());
            frontend::draw_title_screen(&mut frame, &title, cursor, screen);
        }
        MenuScreenshot::NewWorld => {
            let form = NewWorldForm {
                name: "Harbor".to_string(),
                ..NewWorldForm::default()
            };
            frontend::draw_new_world_screen(&mut frame, &form, cursor, screen);
        }
        MenuScreenshot::Worlds => {
            let mut list = WorldList::new(worlds.clone());
            list.selected = 0;
            frontend::draw_world_list_screen(&mut frame, &list, cursor, screen);
            detail = format!("{} worlds", list.worlds.len());
        }
        MenuScreenshot::Settings => {
            ui.screen = crate::ui_state::Screen::Playing;
            ui.open_settings();
            ui.host_optional = true;
            ui.settings_tab = SettingsTab::Game;
            ui.settings_selected_row = 0;
            let info = SettingsInfo {
                ui_scale_percent: effective_ui_scale,
                ..SettingsInfo::default()
            };
            crate::ui_state::draw_settings_screen(&mut frame, &ui, &info, screen);
            detail = "host-optional settings".to_string();
        }
        MenuScreenshot::Multiplayer | MenuScreenshot::MultiplayerError => {
            let mut form = MultiplayerForm::new();
            form.field = MultiplayerField::Address;
            form.recent_selected = 0;
            form.name = "player1234".to_string();
            if mode == MenuScreenshot::MultiplayerError {
                form.address = "play.example.com".to_string();
                form.error = Some("Add a port, for example 127.0.0.1:25565.".to_string());
                detail = "multiplayer validation error".to_string();
            } else {
                detail = format!("{} recent servers", ui.settings.recent_servers.len());
            }
            frontend::draw_multiplayer_screen(&mut frame, &form, &ui.settings.recent_servers, cursor, screen);
        }
    }
    crate::log_ui_frame(&format!("{mode:?}"), &frame);
    let environment = Environment::default();
    renderer.render_cached_scene(
        &camera,
        &SceneParams {
            environment,
            ui: Some(&frame),
            outline: None,
            time_seconds: 0.0,
            ..SceneParams::default()
        },
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
    crate::write_png(path, width, height, &rgba)?;
    log::info!(
        "menu screenshot {:?} written to {} ({}x{}, {}px {}, data {}, {detail})",
        mode,
        path.display(),
        width,
        height,
        renderer.texture_resolution(),
        texture_source.label(),
        data_dir.display()
    );
    Ok(())
}

/// Creates a temporary data directory with deterministic worlds, a last-world
/// setting and recent servers for the menu screenshots.
fn seed_screenshot_data() -> Result<PathBuf, std::io::Error> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("openblock-menu-shot-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join("worlds"))?;
    let now = SystemTime::now();
    let worlds = [
        ("creative", 14_336_u64, now - Duration::from_secs(3 * 86_400)),
        ("survival", 1_843_200, now - Duration::from_secs(2 * 3_600)),
        ("harbor", 5_662_310, now - Duration::from_secs(5 * 60)),
        ("far harbor", 2_097_152, now - Duration::from_secs(86_400)),
        (
            "The Long World of Alexandria",
            9_961_472,
            now - Duration::from_secs(6 * 86_400),
        ),
    ];
    for (name, size, modified) in worlds {
        let world = dir.join("worlds").join(name);
        std::fs::create_dir_all(&world)?;
        let manifest = world.join(frontend::WORLD_MANIFEST);
        let file = std::fs::File::create(&manifest)?;
        // Sparse files keep the temporary directory cheap while the world list
        // still displays realistic multi-megabyte sizes.
        file.set_len(size)?;
        file.set_times(std::fs::FileTimes::new().set_modified(modified))?;
    }
    let settings = Settings {
        last_world: Some("harbor".to_string()),
        recent_servers: vec![
            ob_core::config::RecentServer {
                address: "play.example.com:25565".to_string(),
                name: "alice".to_string(),
            },
            ob_core::config::RecentServer {
                address: "127.0.0.1:25565".to_string(),
                name: "builder".to_string(),
            },
            ob_core::config::RecentServer {
                address: "mc.example.net:19132".to_string(),
                name: "explorer".to_string(),
            },
        ],
        ..Settings::default()
    };
    save_settings(&dir, &settings)?;
    Ok(dir)
}

/// Saves only the last-world pointer, preserving every other persisted option.
pub fn remember_last_world_choice(data_dir: &Path, world: &str) {
    let mut disk_settings = load_settings(data_dir);
    if disk_settings.last_world.as_deref() == Some(world) {
        return;
    }
    disk_settings.last_world = Some(world.to_string());
    match save_settings(data_dir, &disk_settings) {
        Ok(()) => log::info!(
            "saved last world '{world}' to {}",
            ob_core::config::settings_path(data_dir).display()
        ),
        Err(error) => log::warn!("could not save last world: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_seed_creates_worlds_and_remembered_settings() {
        let dir = seed_screenshot_data().expect("seed data");
        let worlds = discover_worlds(&dir);
        let names: Vec<&str> = worlds.iter().map(|world| world.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "harbor",
                "survival",
                "far harbor",
                "creative",
                "The Long World of Alexandria"
            ]
        );
        let settings = load_settings(&dir);
        assert_eq!(settings.last_world.as_deref(), Some("harbor"));
        assert_eq!(settings.recent_servers.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remember_last_world_preserves_other_settings() {
        let dir = std::env::temp_dir().join(format!(
            "openblock-menu-last-world-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let settings = Settings {
            render_distance_chunks: 11,
            reduced_motion: true,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        remember_last_world_choice(&dir, "harbor");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.last_world.as_deref(), Some("harbor"));
        assert_eq!(loaded.render_distance_chunks, 11);
        assert!(loaded.reduced_motion);
        let _ = std::fs::remove_dir_all(dir);
    }
}
