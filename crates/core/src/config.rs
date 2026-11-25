//! User settings: loading, validation and atomic persistence.
//!
//! Settings live in `<data-dir>/config.json`. Unknown fields are preserved on
//! read (ignored) so newer builds can add options without breaking older ones,
//! and every value is clamped to a safe range on load before use.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name of the user settings document.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// Selectable procedural texture-pack style.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextureStyle {
    /// Clean Minecraft-inspired pixel art (default).
    #[default]
    Classic,
    /// Soft hand-painted Hytale-inspired look.
    Painterly,
    /// Detailed, saturated natural textures.
    Lush,
    /// Modern flat, minimal palette.
    Minimal,
}

impl TextureStyle {
    /// All selectable styles, in menu order.
    pub const ALL: [TextureStyle; 4] = [
        TextureStyle::Classic,
        TextureStyle::Painterly,
        TextureStyle::Lush,
        TextureStyle::Minimal,
    ];

    /// Stable settings/CLI name.
    pub fn name(self) -> &'static str {
        match self {
            TextureStyle::Classic => "classic",
            TextureStyle::Painterly => "painterly",
            TextureStyle::Lush => "lush",
            TextureStyle::Minimal => "minimal",
        }
    }

    /// Parses a settings/CLI name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.name() == name)
    }

    /// Next style in the cycle (menu buttons).
    pub fn next(self) -> Self {
        let index = Self::ALL.into_iter().position(|style| style == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Previous style in the cycle.
    pub fn previous(self) -> Self {
        let index = Self::ALL.into_iter().position(|style| style == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Gameplay mode selected by the player.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GameMode {
    /// The real game: health, hunger, fall damage, drops and item consumption.
    #[default]
    Survival,
    /// Sandbox building: full vitals, instant interaction and no item costs.
    Creative,
}

impl GameMode {
    /// All selectable modes, in menu order.
    pub const ALL: [GameMode; 2] = [GameMode::Survival, GameMode::Creative];

    /// Stable settings/CLI name.
    pub fn name(self) -> &'static str {
        match self {
            GameMode::Survival => "survival",
            GameMode::Creative => "creative",
        }
    }

    /// Parses a settings/CLI name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.name() == name)
    }

    /// Next mode in the cycle (settings toggle).
    pub fn next(self) -> Self {
        match self {
            GameMode::Survival => GameMode::Creative,
            GameMode::Creative => GameMode::Survival,
        }
    }
}

/// One rebindable gameplay action.
///
/// The set is deliberately small and stable: the client maps physical keys to
/// these actions, so adding a new action means adding a variant here, a default
/// binding and a label. Persisted names are the lower-case snake-case strings
/// produced by `serde`; [`Action::from_name`] also accepts the display label so
/// hand-edited configuration files stay readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Move forward.
    MoveForward,
    /// Move backward.
    MoveBack,
    /// Strafe left.
    MoveLeft,
    /// Strafe right.
    MoveRight,
    /// Jump or swim upward.
    Jump,
    /// Crouch, sneak or fly down.
    Sneak,
    /// Sprint.
    Sprint,
    /// Toggle flight in single-player creative mode.
    Fly,
    /// Hold to mine or attack.
    Mine,
    /// Place, use, eat or interact.
    Use,
    /// Open or close the inventory.
    Inventory,
    /// Open or close crafting.
    Crafting,
    /// Open multiplayer chat.
    Chat,
    /// Toggle the F3 debug overlay.
    DebugOverlay,
    /// Toggle the HUD.
    ToggleHud,
    /// Save a local screenshot.
    Screenshot,
    /// Open settings or pause.
    Pause,
}

impl Action {
    /// Every action in display order.
    pub const ALL: [Action; 17] = [
        Action::MoveForward,
        Action::MoveBack,
        Action::MoveLeft,
        Action::MoveRight,
        Action::Jump,
        Action::Sneak,
        Action::Sprint,
        Action::Fly,
        Action::Mine,
        Action::Use,
        Action::Inventory,
        Action::Crafting,
        Action::Chat,
        Action::DebugOverlay,
        Action::ToggleHud,
        Action::Screenshot,
        Action::Pause,
    ];

    /// Stable lower-case settings name.
    pub fn name(self) -> &'static str {
        match self {
            Action::MoveForward => "move_forward",
            Action::MoveBack => "move_back",
            Action::MoveLeft => "move_left",
            Action::MoveRight => "move_right",
            Action::Jump => "jump",
            Action::Sneak => "sneak",
            Action::Sprint => "sprint",
            Action::Fly => "fly",
            Action::Mine => "mine",
            Action::Use => "use",
            Action::Inventory => "inventory",
            Action::Crafting => "crafting",
            Action::Chat => "chat",
            Action::DebugOverlay => "debug_overlay",
            Action::ToggleHud => "toggle_hud",
            Action::Screenshot => "screenshot",
            Action::Pause => "pause",
        }
    }

    /// Human-readable label for the settings screen.
    pub fn label(self) -> &'static str {
        match self {
            Action::MoveForward => "Move forward",
            Action::MoveBack => "Move back",
            Action::MoveLeft => "Move left",
            Action::MoveRight => "Move right",
            Action::Jump => "Jump",
            Action::Sneak => "Sneak",
            Action::Sprint => "Sprint",
            Action::Fly => "Fly",
            Action::Mine => "Mine",
            Action::Use => "Use",
            Action::Inventory => "Inventory",
            Action::Crafting => "Crafting",
            Action::Chat => "Chat",
            Action::DebugOverlay => "Debug",
            Action::ToggleHud => "HUD",
            Action::Screenshot => "Screenshot",
            Action::Pause => "Pause",
        }
    }

    /// Parses a stable name or a display label (case-insensitively).
    pub fn from_name(name: &str) -> Option<Self> {
        let normalised = name.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        Self::ALL.into_iter().find(|action| {
            action.name() == normalised || action.label().to_ascii_lowercase().replace([' ', '-'], "_") == normalised
        })
    }

    /// Factory default binding for this action.
    pub fn default_binding(self) -> BindKey {
        match self {
            Action::MoveForward => BindKey::Key(KeyName::KeyW),
            Action::MoveBack => BindKey::Key(KeyName::KeyS),
            Action::MoveLeft => BindKey::Key(KeyName::KeyA),
            Action::MoveRight => BindKey::Key(KeyName::KeyD),
            Action::Jump => BindKey::Key(KeyName::Space),
            Action::Sneak => BindKey::Key(KeyName::ShiftLeft),
            Action::Sprint => BindKey::Key(KeyName::ControlLeft),
            Action::Fly => BindKey::Key(KeyName::KeyF),
            Action::Mine => BindKey::Mouse(MouseButton::Left),
            Action::Use => BindKey::Mouse(MouseButton::Right),
            Action::Inventory => BindKey::Key(KeyName::KeyE),
            Action::Crafting => BindKey::Key(KeyName::KeyC),
            Action::Chat => BindKey::Key(KeyName::KeyT),
            Action::DebugOverlay => BindKey::Key(KeyName::F3),
            Action::ToggleHud => BindKey::Key(KeyName::F1),
            Action::Screenshot => BindKey::Key(KeyName::F2),
            Action::Pause => BindKey::Key(KeyName::Escape),
        }
    }

    /// Whether this action is allowed to use a mouse button.
    pub fn allows_mouse(self) -> bool {
        matches!(self, Action::Mine | Action::Use)
    }
}

/// A keyboard key that can be bound to an action.
///
/// This is intentionally a closed set (rather than a raw winit key code) so a
/// persisted binding can never name an unsupported key. The names map one to
/// one onto the client's physical-key translation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum KeyName {
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Space,
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    CapsLock,
    Minus,
    Equal,
    BracketLeft,
    BracketRight,
    Backslash,
    Semicolon,
    Quote,
    Comma,
    Period,
    Slash,
    Backquote,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadDecimal,
    NumpadEnter,
}

impl KeyName {
    /// Stable settings name.
    pub fn name(self) -> &'static str {
        match self {
            KeyName::KeyA => "key_a",
            KeyName::KeyB => "key_b",
            KeyName::KeyC => "key_c",
            KeyName::KeyD => "key_d",
            KeyName::KeyE => "key_e",
            KeyName::KeyF => "key_f",
            KeyName::KeyG => "key_g",
            KeyName::KeyH => "key_h",
            KeyName::KeyI => "key_i",
            KeyName::KeyJ => "key_j",
            KeyName::KeyK => "key_k",
            KeyName::KeyL => "key_l",
            KeyName::KeyM => "key_m",
            KeyName::KeyN => "key_n",
            KeyName::KeyO => "key_o",
            KeyName::KeyP => "key_p",
            KeyName::KeyQ => "key_q",
            KeyName::KeyR => "key_r",
            KeyName::KeyS => "key_s",
            KeyName::KeyT => "key_t",
            KeyName::KeyU => "key_u",
            KeyName::KeyV => "key_v",
            KeyName::KeyW => "key_w",
            KeyName::KeyX => "key_x",
            KeyName::KeyY => "key_y",
            KeyName::KeyZ => "key_z",
            KeyName::Digit0 => "digit_0",
            KeyName::Digit1 => "digit_1",
            KeyName::Digit2 => "digit_2",
            KeyName::Digit3 => "digit_3",
            KeyName::Digit4 => "digit_4",
            KeyName::Digit5 => "digit_5",
            KeyName::Digit6 => "digit_6",
            KeyName::Digit7 => "digit_7",
            KeyName::Digit8 => "digit_8",
            KeyName::Digit9 => "digit_9",
            KeyName::F1 => "f1",
            KeyName::F2 => "f2",
            KeyName::F3 => "f3",
            KeyName::F4 => "f4",
            KeyName::F5 => "f5",
            KeyName::F6 => "f6",
            KeyName::F7 => "f7",
            KeyName::F8 => "f8",
            KeyName::F9 => "f9",
            KeyName::F10 => "f10",
            KeyName::F11 => "f11",
            KeyName::F12 => "f12",
            KeyName::ArrowUp => "arrow_up",
            KeyName::ArrowDown => "arrow_down",
            KeyName::ArrowLeft => "arrow_left",
            KeyName::ArrowRight => "arrow_right",
            KeyName::Space => "space",
            KeyName::Enter => "enter",
            KeyName::Tab => "tab",
            KeyName::Escape => "escape",
            KeyName::Backspace => "backspace",
            KeyName::Delete => "delete",
            KeyName::Insert => "insert",
            KeyName::Home => "home",
            KeyName::End => "end",
            KeyName::PageUp => "page_up",
            KeyName::PageDown => "page_down",
            KeyName::ShiftLeft => "shift_left",
            KeyName::ShiftRight => "shift_right",
            KeyName::ControlLeft => "control_left",
            KeyName::ControlRight => "control_right",
            KeyName::AltLeft => "alt_left",
            KeyName::AltRight => "alt_right",
            KeyName::CapsLock => "caps_lock",
            KeyName::Minus => "minus",
            KeyName::Equal => "equal",
            KeyName::BracketLeft => "bracket_left",
            KeyName::BracketRight => "bracket_right",
            KeyName::Backslash => "backslash",
            KeyName::Semicolon => "semicolon",
            KeyName::Quote => "quote",
            KeyName::Comma => "comma",
            KeyName::Period => "period",
            KeyName::Slash => "slash",
            KeyName::Backquote => "backquote",
            KeyName::Numpad0 => "numpad_0",
            KeyName::Numpad1 => "numpad_1",
            KeyName::Numpad2 => "numpad_2",
            KeyName::Numpad3 => "numpad_3",
            KeyName::Numpad4 => "numpad_4",
            KeyName::Numpad5 => "numpad_5",
            KeyName::Numpad6 => "numpad_6",
            KeyName::Numpad7 => "numpad_7",
            KeyName::Numpad8 => "numpad_8",
            KeyName::Numpad9 => "numpad_9",
            KeyName::NumpadAdd => "numpad_add",
            KeyName::NumpadSubtract => "numpad_subtract",
            KeyName::NumpadMultiply => "numpad_multiply",
            KeyName::NumpadDivide => "numpad_divide",
            KeyName::NumpadDecimal => "numpad_decimal",
            KeyName::NumpadEnter => "numpad_enter",
        }
    }

    /// Short label shown on the Controls tab.
    pub fn label(self) -> &'static str {
        match self {
            KeyName::KeyA => "A",
            KeyName::KeyB => "B",
            KeyName::KeyC => "C",
            KeyName::KeyD => "D",
            KeyName::KeyE => "E",
            KeyName::KeyF => "F",
            KeyName::KeyG => "G",
            KeyName::KeyH => "H",
            KeyName::KeyI => "I",
            KeyName::KeyJ => "J",
            KeyName::KeyK => "K",
            KeyName::KeyL => "L",
            KeyName::KeyM => "M",
            KeyName::KeyN => "N",
            KeyName::KeyO => "O",
            KeyName::KeyP => "P",
            KeyName::KeyQ => "Q",
            KeyName::KeyR => "R",
            KeyName::KeyS => "S",
            KeyName::KeyT => "T",
            KeyName::KeyU => "U",
            KeyName::KeyV => "V",
            KeyName::KeyW => "W",
            KeyName::KeyX => "X",
            KeyName::KeyY => "Y",
            KeyName::KeyZ => "Z",
            KeyName::Digit0 => "0",
            KeyName::Digit1 => "1",
            KeyName::Digit2 => "2",
            KeyName::Digit3 => "3",
            KeyName::Digit4 => "4",
            KeyName::Digit5 => "5",
            KeyName::Digit6 => "6",
            KeyName::Digit7 => "7",
            KeyName::Digit8 => "8",
            KeyName::Digit9 => "9",
            KeyName::F1 => "F1",
            KeyName::F2 => "F2",
            KeyName::F3 => "F3",
            KeyName::F4 => "F4",
            KeyName::F5 => "F5",
            KeyName::F6 => "F6",
            KeyName::F7 => "F7",
            KeyName::F8 => "F8",
            KeyName::F9 => "F9",
            KeyName::F10 => "F10",
            KeyName::F11 => "F11",
            KeyName::F12 => "F12",
            KeyName::ArrowUp => "Up",
            KeyName::ArrowDown => "Down",
            KeyName::ArrowLeft => "Left",
            KeyName::ArrowRight => "Right",
            KeyName::Space => "Space",
            KeyName::Enter => "Enter",
            KeyName::Tab => "Tab",
            KeyName::Escape => "Escape",
            KeyName::Backspace => "Backspace",
            KeyName::Delete => "Delete",
            KeyName::Insert => "Insert",
            KeyName::Home => "Home",
            KeyName::End => "End",
            KeyName::PageUp => "Page Up",
            KeyName::PageDown => "Page Down",
            KeyName::ShiftLeft => "L Shift",
            KeyName::ShiftRight => "R Shift",
            KeyName::ControlLeft => "L Ctrl",
            KeyName::ControlRight => "R Ctrl",
            KeyName::AltLeft => "L Alt",
            KeyName::AltRight => "R Alt",
            KeyName::CapsLock => "Caps Lock",
            KeyName::Minus => "-",
            KeyName::Equal => "=",
            KeyName::BracketLeft => "[",
            KeyName::BracketRight => "]",
            KeyName::Backslash => "\\",
            KeyName::Semicolon => ";",
            KeyName::Quote => "'",
            KeyName::Comma => ",",
            KeyName::Period => ".",
            KeyName::Slash => "/",
            KeyName::Backquote => "`",
            KeyName::Numpad0 => "Num 0",
            KeyName::Numpad1 => "Num 1",
            KeyName::Numpad2 => "Num 2",
            KeyName::Numpad3 => "Num 3",
            KeyName::Numpad4 => "Num 4",
            KeyName::Numpad5 => "Num 5",
            KeyName::Numpad6 => "Num 6",
            KeyName::Numpad7 => "Num 7",
            KeyName::Numpad8 => "Num 8",
            KeyName::Numpad9 => "Num 9",
            KeyName::NumpadAdd => "Num +",
            KeyName::NumpadSubtract => "Num -",
            KeyName::NumpadMultiply => "Num *",
            KeyName::NumpadDivide => "Num /",
            KeyName::NumpadDecimal => "Num .",
            KeyName::NumpadEnter => "Num Enter",
        }
    }

    /// Parses a stable name or a short label (case-insensitively).
    pub fn from_name(name: &str) -> Option<Self> {
        let candidate = name.trim().to_ascii_lowercase().replace(' ', "_");
        ALL_KEYS.into_iter().find(|key| {
            key.name() == candidate
                || key.name().strip_prefix("key_") == Some(candidate.as_str())
                || key.name().strip_prefix("digit_") == Some(candidate.as_str())
                || key.label().to_ascii_lowercase().replace(' ', "_") == candidate
        })
    }
}

/// Every bindable key, in a stable order used for validation and tests.
pub const ALL_KEYS: [KeyName; 97] = [
    KeyName::KeyA,
    KeyName::KeyB,
    KeyName::KeyC,
    KeyName::KeyD,
    KeyName::KeyE,
    KeyName::KeyF,
    KeyName::KeyG,
    KeyName::KeyH,
    KeyName::KeyI,
    KeyName::KeyJ,
    KeyName::KeyK,
    KeyName::KeyL,
    KeyName::KeyM,
    KeyName::KeyN,
    KeyName::KeyO,
    KeyName::KeyP,
    KeyName::KeyQ,
    KeyName::KeyR,
    KeyName::KeyS,
    KeyName::KeyT,
    KeyName::KeyU,
    KeyName::KeyV,
    KeyName::KeyW,
    KeyName::KeyX,
    KeyName::KeyY,
    KeyName::KeyZ,
    KeyName::Digit0,
    KeyName::Digit1,
    KeyName::Digit2,
    KeyName::Digit3,
    KeyName::Digit4,
    KeyName::Digit5,
    KeyName::Digit6,
    KeyName::Digit7,
    KeyName::Digit8,
    KeyName::Digit9,
    KeyName::F1,
    KeyName::F2,
    KeyName::F3,
    KeyName::F4,
    KeyName::F5,
    KeyName::F6,
    KeyName::F7,
    KeyName::F8,
    KeyName::F9,
    KeyName::F10,
    KeyName::F11,
    KeyName::F12,
    KeyName::ArrowUp,
    KeyName::ArrowDown,
    KeyName::ArrowLeft,
    KeyName::ArrowRight,
    KeyName::Space,
    KeyName::Enter,
    KeyName::Tab,
    KeyName::Escape,
    KeyName::Backspace,
    KeyName::Delete,
    KeyName::Insert,
    KeyName::Home,
    KeyName::End,
    KeyName::PageUp,
    KeyName::PageDown,
    KeyName::ShiftLeft,
    KeyName::ShiftRight,
    KeyName::ControlLeft,
    KeyName::ControlRight,
    KeyName::AltLeft,
    KeyName::AltRight,
    KeyName::CapsLock,
    KeyName::Minus,
    KeyName::Equal,
    KeyName::BracketLeft,
    KeyName::BracketRight,
    KeyName::Backslash,
    KeyName::Semicolon,
    KeyName::Quote,
    KeyName::Comma,
    KeyName::Period,
    KeyName::Slash,
    KeyName::Backquote,
    KeyName::Numpad0,
    KeyName::Numpad1,
    KeyName::Numpad2,
    KeyName::Numpad3,
    KeyName::Numpad4,
    KeyName::Numpad5,
    KeyName::Numpad6,
    KeyName::Numpad7,
    KeyName::Numpad8,
    KeyName::Numpad9,
    KeyName::NumpadAdd,
    KeyName::NumpadSubtract,
    KeyName::NumpadMultiply,
    KeyName::NumpadDivide,
    KeyName::NumpadDecimal,
    KeyName::NumpadEnter,
];

/// A mouse button that can be bound to Mine or Use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    /// Left mouse button.
    Left,
    /// Right mouse button.
    Right,
    /// Middle mouse button.
    Middle,
}

impl MouseButton {
    /// Stable settings name.
    pub fn name(self) -> &'static str {
        match self {
            MouseButton::Left => "mouse_left",
            MouseButton::Right => "mouse_right",
            MouseButton::Middle => "mouse_middle",
        }
    }

    /// Short label shown on the Controls tab.
    pub fn label(self) -> &'static str {
        match self {
            MouseButton::Left => "Mouse L",
            MouseButton::Right => "Mouse R",
            MouseButton::Middle => "Mouse M",
        }
    }

    /// Parses a stable name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "mouse_left" | "left" => Some(MouseButton::Left),
            "mouse_right" | "right" => Some(MouseButton::Right),
            "mouse_middle" | "middle" => Some(MouseButton::Middle),
            _ => None,
        }
    }
}

/// One key or mouse button bound to an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindKey {
    /// A keyboard key.
    Key(KeyName),
    /// A mouse button (only Mine and Use accept these).
    Mouse(MouseButton),
}

impl BindKey {
    /// Stable settings name.
    pub fn name(self) -> &'static str {
        match self {
            BindKey::Key(key) => key.name(),
            BindKey::Mouse(button) => button.name(),
        }
    }

    /// Short label shown on the Controls tab.
    pub fn label(self) -> &'static str {
        match self {
            BindKey::Key(key) => key.label(),
            BindKey::Mouse(button) => button.label(),
        }
    }

    /// Whether this is a mouse button.
    pub fn is_mouse(self) -> bool {
        matches!(self, BindKey::Mouse(_))
    }

    /// Parses a stable name such as `key_w`, `w`, `space` or `mouse_left`.
    pub fn from_name(name: &str) -> Option<Self> {
        if let Some(button) = MouseButton::from_name(name) {
            return Some(BindKey::Mouse(button));
        }
        KeyName::from_name(name).map(BindKey::Key)
    }
}

impl std::fmt::Display for BindKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Why a binding change was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindError {
    /// The input is already bound to another action.
    Duplicate {
        /// The action that currently owns the input.
        action: Action,
    },
    /// A mouse button was offered for an action that only accepts keys.
    MouseNotAllowed,
    /// The input cannot be represented as a binding.
    Invalid,
    /// A text field (world name, seed, chat) currently owns the keyboard.
    TextInputActive,
}

impl std::fmt::Display for KeybindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeybindError::Duplicate { action } => write!(
                formatter,
                "That input is already bound to '{}'. Choose another input or clear that action first.",
                action.label()
            ),
            KeybindError::MouseNotAllowed => formatter.write_str("Mouse buttons can only be bound to Mine and Use."),
            KeybindError::Invalid => formatter.write_str("That input cannot be bound."),
            KeybindError::TextInputActive => formatter.write_str("Finish typing before rebinding a key."),
        }
    }
}

/// A complete, conflict-free set of action bindings.
///
/// The default map binds every action. A binding may be explicitly cleared with
/// [`Keybinds::clear`] and then serialises as `null`; a missing action in a
/// persisted document falls back to its default. Duplicate and mouse mis-bindings
/// are dropped deterministically on load so a corrupt document cannot make the
/// game unplayable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybinds {
    bindings: std::collections::BTreeMap<Action, Option<BindKey>>,
}

impl Default for Keybinds {
    fn default() -> Self {
        let mut bindings = std::collections::BTreeMap::new();
        for action in Action::ALL {
            bindings.insert(action, Some(action.default_binding()));
        }
        Self { bindings }
    }
}

impl Keybinds {
    /// Binding for one action, or `None` when it was explicitly cleared.
    pub fn binding(&self, action: Action) -> Option<BindKey> {
        self.bindings.get(&action).copied().flatten()
    }

    /// The first action bound to `key`, in [`Action::ALL`] order.
    pub fn action_for(&self, key: BindKey) -> Option<Action> {
        Action::ALL
            .into_iter()
            .find(|action| self.binding(*action) == Some(key))
    }

    /// Every action and its binding, in [`Action::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = (Action, Option<BindKey>)> + '_ {
        Action::ALL.into_iter().map(|action| (action, self.binding(action)))
    }

    /// Binds `key` to `action`, rejecting duplicates and invalid mouse use.
    pub fn rebind(&mut self, action: Action, key: BindKey) -> Result<(), KeybindError> {
        if key.is_mouse() && !action.allows_mouse() {
            return Err(KeybindError::MouseNotAllowed);
        }
        if let Some(existing) = self
            .bindings
            .iter()
            .find(|(other, binding)| **other != action && **binding == Some(key))
            .map(|(other, _)| *other)
        {
            return Err(KeybindError::Duplicate { action: existing });
        }
        self.bindings.insert(action, Some(key));
        Ok(())
    }

    /// Clears the binding for `action`; returns whether anything changed.
    pub fn clear(&mut self, action: Action) -> bool {
        self.bindings.insert(action, None).flatten().is_some()
    }

    /// Restores every factory binding.
    pub fn reset_defaults(&mut self) {
        *self = Self::default();
    }

    /// Whether this is exactly the factory-default set.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// First duplicate conflict, if any.
    pub fn validate(&self) -> Result<(), KeybindError> {
        for (index, first) in Action::ALL.into_iter().enumerate() {
            let Some(key) = self.binding(first) else {
                continue;
            };
            if key.is_mouse() && !first.allows_mouse() {
                return Err(KeybindError::MouseNotAllowed);
            }
            for second in Action::ALL.into_iter().skip(index + 1) {
                if self.binding(second) == Some(key) {
                    return Err(KeybindError::Duplicate { action: second });
                }
            }
        }
        Ok(())
    }

    /// Drops duplicate and disallowed bindings, keeping the first binding in
    /// [`Action::ALL`] order. Returns whether anything changed.
    pub fn sanitise(&mut self) -> bool {
        let before = self.clone();
        let mut used = Vec::new();
        for action in Action::ALL {
            let binding = self.bindings.get(&action).copied().flatten();
            let keep = match binding {
                Some(key) if key.is_mouse() && !action.allows_mouse() => false,
                Some(key) if used.contains(&key) => false,
                _ => true,
            };
            if keep {
                if let Some(key) = binding {
                    used.push(key);
                }
            } else {
                self.bindings.insert(action, None);
            }
        }
        *self != before
    }

    /// Human-readable conflict lines for the settings screen.
    pub fn conflict_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (index, first) in Action::ALL.into_iter().enumerate() {
            let Some(key) = self.binding(first) else {
                continue;
            };
            for second in Action::ALL.into_iter().skip(index + 1) {
                if self.binding(second) == Some(key) {
                    lines.push(format!(
                        "Conflict: '{}' and '{}' both use {}",
                        first.label(),
                        second.label(),
                        key.label()
                    ));
                }
            }
        }
        lines
    }
}

impl Serialize for Keybinds {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(Action::ALL.len()))?;
        for action in Action::ALL {
            map.serialize_entry(action.name(), &self.binding(action).map(BindKey::name))?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Keybinds {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let serde_json::Value::Object(map) = value else {
            return Ok(Self::default());
        };
        let mut bindings = std::collections::BTreeMap::new();
        for action in Action::ALL {
            let binding = match map.get(action.name()) {
                // A missing action in an older document keeps its default; an
                // explicit null means the player cleared it.
                None => Some(action.default_binding()),
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(name)) => {
                    BindKey::from_name(name).or_else(|| Some(action.default_binding()))
                }
                // Unknown value types are tolerated and treated as the default.
                Some(_) => Some(action.default_binding()),
            };
            bindings.insert(action, binding);
        }
        let mut keybinds = Self { bindings };
        keybinds.sanitise();
        Ok(keybinds)
    }
}

/// One remembered multiplayer server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentServer {
    /// `host:port` endpoint.
    pub address: String,
    /// Player name used for the last attempt.
    pub name: String,
}

/// Maximum number of recent multiplayer servers persisted.
pub const MAX_RECENT_SERVERS: usize = 5;

/// Adds or refreshes a recent server, keeping at most [`MAX_RECENT_SERVERS`].
///
/// The most recent address moves to the front; duplicate addresses are removed
/// before truncating so the list cannot grow stale.
pub fn remember_recent_server(list: &mut Vec<RecentServer>, address: &str, name: &str) {
    let address = address.trim();
    if address.is_empty() {
        return;
    }
    let entry = RecentServer {
        address: address.to_string(),
        name: name.trim().to_string(),
    };
    list.retain(|server| server.address != entry.address);
    list.insert(0, entry);
    list.truncate(MAX_RECENT_SERVERS);
}

/// Smallest supported render distance in chunks.
pub const MIN_RENDER_DISTANCE: i32 = 2;
/// Largest render distance offered by the settings screen in v0.0.x.
pub const MAX_RENDER_DISTANCE: i32 = 24;
/// Smallest supported vertical field of view in degrees.
pub const MIN_FOV_DEGREES: f32 = 30.0;
/// Largest supported vertical field of view in degrees.
pub const MAX_FOV_DEGREES: f32 = 110.0;
/// Smallest supported mouse sensitivity multiplier.
pub const MIN_MOUSE_SENSITIVITY: f32 = 0.05;
/// Largest supported mouse sensitivity multiplier.
pub const MAX_MOUSE_SENSITIVITY: f32 = 3.0;
/// Longest accepted persisted resource-pack path, in bytes.
pub const MAX_TEXTURE_PACK_PATH_BYTES: usize = 4096;
/// Lowest volume accepted by the audio settings, as a linear gain.
pub const MIN_AUDIO_VOLUME: f32 = 0.0;
/// Highest volume accepted by the audio settings, as a linear gain.
pub const MAX_AUDIO_VOLUME: f32 = 1.0;

/// Persistent user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Streaming radius in chunks.
    pub render_distance_chunks: i32,
    /// Vertical field of view in degrees.
    pub fov_degrees: f32,
    /// Mouse look multiplier (1.0 = default).
    pub mouse_sensitivity: f32,
    /// Whether the window starts fullscreen.
    pub fullscreen: bool,
    /// Whether the F3 debug overlay is visible.
    pub show_debug_overlay: bool,
    /// Whether the HUD (crosshair, hotbar and vitals) is visible.
    pub show_hud: bool,
    /// Worker threads for chunk meshing; `None` selects the automatic default.
    pub mesh_workers: Option<usize>,
    /// Procedural texture-pack style, used when no external resource pack is
    /// selected and as the fallback when one fails to load.
    pub texture_style: TextureStyle,
    /// Optional path to a Minecraft-style resource pack, persisted after a
    /// successful `--texture-pack` launch.
    pub texture_pack: Option<String>,
    /// Gameplay mode: survival (default) or creative.
    pub game_mode: GameMode,
    /// Whether the audio engine produces sound. Defaults to on.
    pub audio_enabled: bool,
    /// Master output volume in `0.0..=1.0`.
    pub master_volume: f32,
    /// Sound-effect bus volume in `0.0..=1.0`.
    pub sfx_volume: f32,
    /// Music and ambience bus volume in `0.0..=1.0`.
    pub music_volume: f32,
    /// Rebindable keyboard/mouse actions.
    pub keybinds: Keybinds,
    /// Removes camera shake and the sprint FOV pump for motion-sensitive
    /// players. Gameplay is otherwise unchanged.
    pub reduced_motion: bool,
    /// World name opened by the title screen's Continue button.
    pub last_world: Option<String>,
    /// Remembered multiplayer servers, newest first (at most
    /// [`MAX_RECENT_SERVERS`]).
    pub recent_servers: Vec<RecentServer>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_distance_chunks: 6,
            fov_degrees: 70.0,
            mouse_sensitivity: 1.0,
            fullscreen: false,
            show_debug_overlay: false,
            show_hud: true,
            mesh_workers: None,
            texture_style: TextureStyle::Classic,
            texture_pack: None,
            game_mode: GameMode::Survival,
            audio_enabled: true,
            master_volume: 0.8,
            sfx_volume: 1.0,
            music_volume: 0.55,
            keybinds: Keybinds::default(),
            reduced_motion: false,
            last_world: None,
            recent_servers: Vec::new(),
        }
    }
}

impl Settings {
    /// Clamps every value into its supported range, returning whether anything changed.
    pub fn clamp(&mut self) -> bool {
        let before = self.clone();
        self.render_distance_chunks = self
            .render_distance_chunks
            .clamp(MIN_RENDER_DISTANCE, MAX_RENDER_DISTANCE);
        if !self.fov_degrees.is_finite() {
            self.fov_degrees = Settings::default().fov_degrees;
        }
        self.fov_degrees = self.fov_degrees.clamp(MIN_FOV_DEGREES, MAX_FOV_DEGREES);
        if !self.mouse_sensitivity.is_finite() {
            self.mouse_sensitivity = Settings::default().mouse_sensitivity;
        }
        self.mouse_sensitivity = self
            .mouse_sensitivity
            .clamp(MIN_MOUSE_SENSITIVITY, MAX_MOUSE_SENSITIVITY);
        if let Some(workers) = self.mesh_workers {
            self.mesh_workers = Some(workers.clamp(1, 16));
        }
        self.master_volume = clamp_volume(self.master_volume, Settings::default().master_volume);
        self.sfx_volume = clamp_volume(self.sfx_volume, Settings::default().sfx_volume);
        self.music_volume = clamp_volume(self.music_volume, Settings::default().music_volume);
        self.keybinds.sanitise();
        if let Some(world) = self.last_world.take() {
            let world = world.trim();
            let safe = !world.is_empty()
                && world.chars().count() <= 64
                && !world.chars().any(|character| {
                    character.is_control() || matches!(character, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                })
                && world != "."
                && world != "..";
            if safe {
                self.last_world = Some(world.to_string());
            }
        }
        self.recent_servers = sanitise_recent_servers(std::mem::take(&mut self.recent_servers));
        before != *self
    }

    /// Why the persisted resource-pack path cannot be used, if it cannot.
    fn texture_pack_rejection_reason(&self) -> Option<&'static str> {
        let path = self.texture_pack.as_deref()?;
        if path.is_empty() {
            Some("path is empty")
        } else if path.len() > MAX_TEXTURE_PACK_PATH_BYTES {
            Some("path is longer than 4096 bytes")
        } else if path.chars().any(char::is_control) {
            Some("path contains control characters")
        } else {
            None
        }
    }

    /// Clears an unusable resource-pack path, returning the reason it was rejected.
    fn validate_texture_pack(&mut self) -> Option<&'static str> {
        let reason = self.texture_pack_rejection_reason();
        if reason.is_some() {
            self.texture_pack = None;
        }
        reason
    }
}

/// Trims, validates, deduplicates and truncates the recent-server list.
fn sanitise_recent_servers(servers: Vec<RecentServer>) -> Vec<RecentServer> {
    let mut sanitised = Vec::new();
    for server in servers {
        let address = server.address.trim();
        let name = server.name.trim();
        if address.is_empty()
            || address.chars().count() > 255
            || address
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
            || name.chars().count() > 64
            || name.chars().any(char::is_control)
        {
            continue;
        }
        if sanitised
            .iter()
            .any(|existing: &RecentServer| existing.address.eq_ignore_ascii_case(address))
        {
            continue;
        }
        sanitised.push(RecentServer {
            address: address.to_string(),
            name: name.to_string(),
        });
        if sanitised.len() >= MAX_RECENT_SERVERS {
            break;
        }
    }
    sanitised
}

/// Path of the settings document for a data directory.
pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE_NAME)
}

/// Loads settings from `<data_dir>/config.json`.
///
/// A missing file yields the defaults. A malformed file logs a warning and also
/// yields the defaults, because refusing to start over a corrupt settings file
/// would be hostile; the next save replaces it.
pub fn load_settings(data_dir: &Path) -> Settings {
    let path = settings_path(data_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Settings::default(),
        Err(err) => {
            log::warn!("could not read settings {}: {err}", path.display());
            return Settings::default();
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(mut settings) => {
            if let Some(reason) = settings.validate_texture_pack() {
                log::warn!(
                    "ignoring texture pack path in {}: {reason}; using the procedural texture style",
                    path.display()
                );
            }
            if settings.clamp() {
                log::warn!("settings in {} were out of range and have been clamped", path.display());
            }
            settings
        }
        Err(err) => {
            log::warn!("malformed settings {}: {err}; using defaults", path.display());
            Settings::default()
        }
    }
}

/// Saves settings atomically to `<data_dir>/config.json`.
pub fn save_settings(data_dir: &Path, settings: &Settings) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = settings_path(data_dir);
    let mut settings = sanitised(settings);
    settings.validate_texture_pack();
    settings.clamp();
    let text =
        serde_json::to_string_pretty(&settings).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text.as_bytes())?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

/// Copy of `settings` with non-finite floats replaced by defaults.
fn sanitised(settings: &Settings) -> Settings {
    let mut copy = settings.clone();
    if !copy.fov_degrees.is_finite() {
        copy.fov_degrees = Settings::default().fov_degrees;
    }
    if !copy.mouse_sensitivity.is_finite() {
        copy.mouse_sensitivity = Settings::default().mouse_sensitivity;
    }
    copy.master_volume = clamp_volume(copy.master_volume, Settings::default().master_volume);
    copy.sfx_volume = clamp_volume(copy.sfx_volume, Settings::default().sfx_volume);
    copy.music_volume = clamp_volume(copy.music_volume, Settings::default().music_volume);
    copy
}

/// Clamps a volume to `0.0..=1.0`, replacing non-finite values with `fallback`.
fn clamp_volume(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(MIN_AUDIO_VOLUME, MAX_AUDIO_VOLUME)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openblock-core-config-{}-{label}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(load_settings(&dir), Settings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_round_trip() {
        let dir = temp_dir("roundtrip");
        let settings = Settings {
            render_distance_chunks: 12,
            fov_degrees: 90.0,
            mouse_sensitivity: 1.5,
            fullscreen: true,
            show_debug_overlay: true,
            mesh_workers: Some(6),
            texture_pack: Some("packs/rotrBLOCKS V88".to_string()),
            game_mode: GameMode::Creative,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert_eq!(load_settings(&dir), settings);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn texture_pack_is_optional_and_round_trips() {
        let dir = temp_dir("pack-roundtrip");
        let settings = Settings {
            texture_pack: Some("/tmp/packs/example".to_string()),
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let text = fs::read_to_string(settings_path(&dir)).expect("read");
        assert!(text.contains("\"texture_pack\": \"/tmp/packs/example\""));
        assert_eq!(load_settings(&dir).texture_pack.as_deref(), Some("/tmp/packs/example"));

        let settings = Settings {
            texture_pack: None,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert_eq!(load_settings(&dir).texture_pack, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_persisted_texture_pack_is_read_as_none() {
        let dir = temp_dir("pack-empty");
        fs::write(settings_path(&dir), br#"{"texture_pack": ""}"#).expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.texture_pack, None);
        assert_eq!(
            settings.render_distance_chunks,
            Settings::default().render_distance_chunks
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_persisted_texture_pack_is_rejected() {
        let dir = temp_dir("pack-invalid");
        for (label, value) in [
            ("nul", r#""bad\u0000path""#.to_string()),
            ("newline", r#""bad\npath""#.to_string()),
            ("long", format!("\"{}\"", "a".repeat(MAX_TEXTURE_PACK_PATH_BYTES + 1))),
        ] {
            fs::write(
                settings_path(&dir),
                format!(r#"{{"render_distance_chunks": 8, "texture_pack": {value}}}"#),
            )
            .expect("write");
            let settings = load_settings(&dir);
            assert_eq!(settings.texture_pack, None, "{label} path must be rejected");
            // The rest of the settings document still loads.
            assert_eq!(settings.render_distance_chunks, 8, "{label}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn longest_accepted_texture_pack_path_is_kept() {
        let dir = temp_dir("pack-longest");
        let path = "b".repeat(MAX_TEXTURE_PACK_PATH_BYTES);
        fs::write(
            settings_path(&dir),
            serde_json::json!({ "texture_pack": path }).to_string(),
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.texture_pack.as_deref(), Some(path.as_str()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_file_yields_defaults() {
        let dir = temp_dir("malformed");
        fs::write(settings_path(&dir), b"{ not json").expect("write");
        assert_eq!(load_settings(&dir), Settings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let dir = temp_dir("clamp");
        fs::write(
            settings_path(&dir),
            br#"{"render_distance_chunks": 999, "fov_degrees": -10.0, "mouse_sensitivity": 50.0, "mesh_workers": 900}"#,
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.render_distance_chunks, MAX_RENDER_DISTANCE);
        assert_eq!(settings.fov_degrees, MIN_FOV_DEGREES);
        assert_eq!(settings.mouse_sensitivity, MAX_MOUSE_SENSITIVITY);
        assert_eq!(settings.mesh_workers, Some(16));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let dir = temp_dir("unknown");
        fs::write(
            settings_path(&dir),
            br#"{"render_distance_chunks": 8, "future_option": {"nested": [1, 2, 3]}}"#,
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.render_distance_chunks, 8);
        assert_eq!(settings.fov_degrees, Settings::default().fov_degrees);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_sanitises_non_finite_values() {
        let dir = temp_dir("nan");
        let settings = Settings {
            fov_degrees: f32::NAN,
            mouse_sensitivity: f32::INFINITY,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.fov_degrees, Settings::default().fov_degrees);
        assert_eq!(loaded.mouse_sensitivity, Settings::default().mouse_sensitivity);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn texture_style_round_trips_and_cycles() {
        for style in TextureStyle::ALL {
            assert_eq!(TextureStyle::from_name(style.name()), Some(style));
            assert_eq!(style.next().previous(), style);
        }
        assert_eq!(TextureStyle::Classic.next(), TextureStyle::Painterly);
        assert_eq!(TextureStyle::Minimal.next(), TextureStyle::Classic);
        assert_eq!(TextureStyle::from_name("nope"), None);
        let dir = temp_dir("style");
        let settings = Settings {
            texture_style: TextureStyle::Lush,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.texture_style, TextureStyle::Lush);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn game_mode_defaults_to_survival_and_names_round_trip() {
        assert_eq!(GameMode::default(), GameMode::Survival);
        assert_eq!(Settings::default().game_mode, GameMode::Survival);
        assert_eq!(GameMode::ALL, [GameMode::Survival, GameMode::Creative]);
        for mode in GameMode::ALL {
            assert_eq!(GameMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(GameMode::from_name("nope"), None);
        assert_eq!(GameMode::Survival.next(), GameMode::Creative);
        assert_eq!(GameMode::Creative.next(), GameMode::Survival);
    }

    #[test]
    fn game_mode_round_trips_and_missing_field_defaults_to_survival() {
        let dir = temp_dir("game-mode");
        for mode in GameMode::ALL {
            let settings = Settings {
                game_mode: mode,
                ..Settings::default()
            };
            save_settings(&dir, &settings).expect("save");
            let text = fs::read_to_string(settings_path(&dir)).expect("read");
            assert!(
                text.contains(&format!("\"game_mode\": \"{}\"", mode.name())),
                "settings must use the lowercase mode name:\n{text}"
            );
            assert_eq!(load_settings(&dir).game_mode, mode);
        }

        fs::write(settings_path(&dir), br#"{"render_distance_chunks": 8}"#).expect("write");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.game_mode, GameMode::Survival);
        assert_eq!(loaded.render_distance_chunks, 8);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_defaults_are_sensible_and_old_configs_load_them() {
        let defaults = Settings::default();
        assert!(defaults.audio_enabled);
        assert_eq!(defaults.master_volume, 0.8);
        assert_eq!(defaults.sfx_volume, 1.0);
        assert_eq!(defaults.music_volume, 0.55);

        let dir = temp_dir("audio-old-config");
        fs::write(settings_path(&dir), br#"{"render_distance_chunks": 8}"#).expect("write");
        let loaded = load_settings(&dir);
        assert!(loaded.audio_enabled);
        assert_eq!(loaded.master_volume, defaults.master_volume);
        assert_eq!(loaded.sfx_volume, defaults.sfx_volume);
        assert_eq!(loaded.music_volume, defaults.music_volume);
        assert_eq!(loaded.render_distance_chunks, 8);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_settings_round_trip() {
        let dir = temp_dir("audio-roundtrip");
        let settings = Settings {
            audio_enabled: false,
            master_volume: 0.35,
            sfx_volume: 0.65,
            music_volume: 0.2,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert_eq!(load_settings(&dir), settings);
        let text = fs::read_to_string(settings_path(&dir)).expect("read");
        for needle in [
            "\"audio_enabled\": false",
            "\"master_volume\": 0.35",
            "\"sfx_volume\": 0.65",
            "\"music_volume\": 0.2",
        ] {
            assert!(text.contains(needle), "missing {needle} in:\n{text}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_volumes_are_clamped_and_non_finite_values_recover() {
        let dir = temp_dir("audio-clamp");
        fs::write(
            settings_path(&dir),
            br#"{"master_volume": 9.0, "sfx_volume": -3.0, "music_volume": "not a number"}"#,
        )
        .expect("write");
        // A malformed JSON type makes the whole document fall back to defaults,
        // which is also a valid safe recovery. Test finite out-of-range values
        // separately below.
        assert_eq!(load_settings(&dir), Settings::default());

        fs::write(
            settings_path(&dir),
            br#"{"master_volume": 9.0, "sfx_volume": -3.0, "music_volume": 2.0}"#,
        )
        .expect("write");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.master_volume, MAX_AUDIO_VOLUME);
        assert_eq!(loaded.sfx_volume, MIN_AUDIO_VOLUME);
        assert_eq!(loaded.music_volume, MAX_AUDIO_VOLUME);

        let mut settings = Settings {
            master_volume: f32::NAN,
            sfx_volume: f32::INFINITY,
            music_volume: f32::NEG_INFINITY,
            ..Settings::default()
        };
        assert!(settings.clamp());
        assert_eq!(settings.master_volume, Settings::default().master_volume);
        assert_eq!(settings.sfx_volume, Settings::default().sfx_volume);
        assert_eq!(settings.music_volume, Settings::default().music_volume);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_sanitises_non_finite_audio_volumes() {
        let dir = temp_dir("audio-nan");
        let settings = Settings {
            master_volume: f32::NAN,
            sfx_volume: f32::NEG_INFINITY,
            music_volume: f32::INFINITY,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.master_volume, Settings::default().master_volume);
        assert_eq!(loaded.sfx_volume, Settings::default().sfx_volume);
        assert_eq!(loaded.music_volume, Settings::default().music_volume);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_path_is_inside_the_data_dir() {
        let dir = Path::new("/tmp/example");
        assert_eq!(settings_path(dir), PathBuf::from("/tmp/example/config.json"));
    }

    #[test]
    fn keybind_defaults_cover_every_action_and_match_the_documented_controls() {
        let keybinds = Keybinds::default();
        for action in Action::ALL {
            let binding = keybinds.binding(action).expect("default action is bound");
            assert_eq!(binding, action.default_binding());
            assert_eq!(keybinds.action_for(binding), Some(action));
        }
        assert_eq!(keybinds.binding(Action::MoveForward), Some(BindKey::Key(KeyName::KeyW)));
        assert_eq!(keybinds.binding(Action::Jump), Some(BindKey::Key(KeyName::Space)));
        assert_eq!(keybinds.binding(Action::Mine), Some(BindKey::Mouse(MouseButton::Left)));
        assert_eq!(keybinds.binding(Action::Use), Some(BindKey::Mouse(MouseButton::Right)));
        assert_eq!(keybinds.binding(Action::Pause), Some(BindKey::Key(KeyName::Escape)));
        assert!(keybinds.validate().is_ok());
        assert!(keybinds.is_default());
    }

    #[test]
    fn rebinding_rejects_duplicates_and_mouse_buttons_for_non_mouse_actions() {
        let mut keybinds = Keybinds::default();
        assert_eq!(
            keybinds.rebind(Action::Jump, BindKey::Key(KeyName::KeyW)),
            Err(KeybindError::Duplicate {
                action: Action::MoveForward
            })
        );
        assert_eq!(
            keybinds.rebind(Action::Jump, BindKey::Mouse(MouseButton::Left)),
            Err(KeybindError::MouseNotAllowed)
        );
        assert_eq!(keybinds.rebind(Action::Jump, BindKey::Key(KeyName::KeyQ)), Ok(()));
        assert_eq!(keybinds.binding(Action::Jump), Some(BindKey::Key(KeyName::KeyQ)));
        assert_eq!(keybinds.action_for(BindKey::Key(KeyName::Space)), None);
        assert!(keybinds.conflict_lines().is_empty());
    }

    #[test]
    fn rebinding_frees_the_old_key_and_keeps_the_set_conflict_free() {
        let mut keybinds = Keybinds::default();
        keybinds.rebind(Action::Jump, BindKey::Key(KeyName::KeyQ)).unwrap();
        keybinds
            .rebind(Action::MoveForward, BindKey::Key(KeyName::Space))
            .unwrap();
        assert_eq!(
            keybinds.binding(Action::MoveForward),
            Some(BindKey::Key(KeyName::Space))
        );
        assert_eq!(keybinds.binding(Action::Jump), Some(BindKey::Key(KeyName::KeyQ)));
        assert!(keybinds.validate().is_ok());
        assert!(!keybinds.is_default());
    }

    #[test]
    fn clearing_and_resetting_bindings_round_trips() {
        let mut keybinds = Keybinds::default();
        assert!(keybinds.clear(Action::Jump));
        assert!(!keybinds.clear(Action::Jump), "clearing twice changes nothing");
        assert_eq!(keybinds.binding(Action::Jump), None);
        keybinds.rebind(Action::Jump, BindKey::Key(KeyName::KeyQ)).unwrap();
        keybinds.reset_defaults();
        assert!(keybinds.is_default());
        assert!(keybinds.validate().is_ok());
    }

    #[test]
    fn keybinds_serialise_as_readable_names_and_load_unknown_keys_tolerantly() {
        let json = serde_json::to_string(&Settings::default()).expect("serialise settings");
        assert!(json.contains("\"move_forward\":\"key_w\""), "{json}");
        assert!(json.contains("\"mine\":\"mouse_left\""), "{json}");
        assert!(json.contains("\"jump\":\"space\""), "{json}");

        let dir = temp_dir("keybinds-tolerance");
        fs::write(
            settings_path(&dir),
            br#"{
                "keybinds": {
                    "jump": "key_q",
                    "future_action": "key_z",
                    "mine": "mouse_left",
                    "use": "mouse_right",
                    "move_back": "not_a_real_key",
                    "sprint": null
                }
            }"#,
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(
            settings.keybinds.binding(Action::Jump),
            Some(BindKey::Key(KeyName::KeyQ))
        );
        assert_eq!(
            settings.keybinds.binding(Action::MoveBack),
            Some(BindKey::Key(KeyName::KeyS)),
            "invalid persisted names fall back to the factory default"
        );
        assert_eq!(settings.keybinds.binding(Action::Sprint), None);
        assert_eq!(
            settings.keybinds.binding(Action::MoveForward),
            Some(BindKey::Key(KeyName::KeyW)),
            "actions missing from an old document keep their defaults"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_persisted_bindings_are_dropped_deterministically() {
        let dir = temp_dir("keybinds-duplicates");
        fs::write(
            settings_path(&dir),
            br#"{"keybinds": {"jump": "key_w", "move_back": "key_w", "sprint": "mouse_left"}}"#,
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(
            settings.keybinds.binding(Action::MoveForward),
            Some(BindKey::Key(KeyName::KeyW))
        );
        assert_eq!(settings.keybinds.binding(Action::Jump), None);
        assert_eq!(settings.keybinds.binding(Action::MoveBack), None);
        assert_eq!(settings.keybinds.binding(Action::Sprint), None);
        assert!(settings.keybinds.validate().is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn keybinds_round_trip_through_settings() {
        let dir = temp_dir("keybinds-roundtrip");
        let mut settings = Settings::default();
        settings
            .keybinds
            .rebind(Action::Jump, BindKey::Key(KeyName::KeyQ))
            .unwrap();
        settings.keybinds.clear(Action::Chat);
        save_settings(&dir, &settings).expect("save");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.keybinds, settings.keybinds);
        assert_eq!(loaded.keybinds.binding(Action::Jump), Some(BindKey::Key(KeyName::KeyQ)));
        assert_eq!(loaded.keybinds.binding(Action::Chat), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn key_names_and_mouse_buttons_round_trip_every_variant() {
        for key in ALL_KEYS {
            assert_eq!(KeyName::from_name(key.name()), Some(key), "{}", key.name());
            assert_eq!(KeyName::from_name(key.label()), Some(key), "{}", key.label());
            assert_eq!(BindKey::from_name(key.name()), Some(BindKey::Key(key)));
        }
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            assert_eq!(MouseButton::from_name(button.name()), Some(button));
            assert_eq!(BindKey::from_name(button.name()), Some(BindKey::Mouse(button)));
        }
        assert_eq!(BindKey::from_name("not_a_key"), None);
    }

    #[test]
    fn action_names_and_labels_round_trip() {
        for action in Action::ALL {
            assert_eq!(Action::from_name(action.name()), Some(action));
            assert_eq!(Action::from_name(action.label()), Some(action));
            assert_eq!(Action::from_name(&action.name().replace('_', "-")), Some(action));
        }
        assert_eq!(Action::from_name("nope"), None);
    }

    #[test]
    fn show_hud_round_trips_and_old_configs_default_on() {
        assert!(Settings::default().show_hud);
        let dir = temp_dir("show-hud");
        let settings = Settings {
            show_hud: false,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert!(!load_settings(&dir).show_hud);
        fs::write(settings_path(&dir), br#"{"render_distance_chunks": 8}"#).expect("write");
        assert!(load_settings(&dir).show_hud);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reduced_motion_round_trips_and_defaults_to_off() {
        assert!(!Settings::default().reduced_motion);
        let dir = temp_dir("reduced-motion");
        let settings = Settings {
            reduced_motion: true,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert!(load_settings(&dir).reduced_motion);
        fs::write(settings_path(&dir), br#"{"render_distance_chunks": 8}"#).expect("write");
        assert!(!load_settings(&dir).reduced_motion);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn last_world_round_trips_and_unsafe_names_are_cleared() {
        let dir = temp_dir("last-world");
        let settings = Settings {
            last_world: Some("survival ridge".to_string()),
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert_eq!(load_settings(&dir).last_world.as_deref(), Some("survival ridge"));

        fs::write(settings_path(&dir), br#"{"last_world": "../escape"}"#).expect("write");
        assert_eq!(load_settings(&dir).last_world, None);
        fs::write(settings_path(&dir), br#"{"last_world": ""}"#).expect("write");
        assert_eq!(load_settings(&dir).last_world, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn recent_servers_are_trimmed_deduped_and_capped() {
        let mut list = Vec::new();
        for index in 0..8 {
            remember_recent_server(&mut list, &format!("127.0.0.1:{}", 25565 + index), "alice");
        }
        assert_eq!(list.len(), MAX_RECENT_SERVERS);
        assert_eq!(list[0].address, "127.0.0.1:25572");
        remember_recent_server(&mut list, "127.0.0.1:25570", "bob");
        assert_eq!(list[0].address, "127.0.0.1:25570");
        assert_eq!(list[0].name, "bob");
        assert_eq!(
            list.iter().filter(|server| server.address == "127.0.0.1:25570").count(),
            1
        );

        let dir = temp_dir("recent-servers");
        let settings = Settings {
            recent_servers: vec![
                RecentServer {
                    address: " 127.0.0.1:25565 ".to_string(),
                    name: " alice ".to_string(),
                },
                RecentServer {
                    address: " ".to_string(),
                    name: "nobody".to_string(),
                },
                RecentServer {
                    address: "127.0.0.1:25565".to_string(),
                    name: "duplicate".to_string(),
                },
            ],
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let loaded = load_settings(&dir);
        assert_eq!(
            loaded.recent_servers,
            vec![RecentServer {
                address: "127.0.0.1:25565".to_string(),
                name: "alice".to_string(),
            }]
        );
        let _ = fs::remove_dir_all(dir);
    }
}
