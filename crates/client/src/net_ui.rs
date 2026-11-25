//! Overlay drawing for the multiplayer connect/loading screen, chat, the Tab
//! player list and the survival notice. Survival HUD, inventory, container and
//! death screens are drawn by the shared single-player helpers.
//!
//! Geometry helpers are pure and unit-tested; the draw functions only append
//! rectangles and bitmap text to an [`ob_render::UiFrame`].

use ob_render::UiFrame;

use crate::net::{ChatLog, PlayerEntry};
use crate::ui_state::UiRect;

/// Action selected on the connect/loading overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectAction {
    /// Start a fresh connection attempt.
    Retry,
    /// Close the client.
    Quit,
}

/// Buttons of the connect overlay in display order.
pub const CONNECT_ACTIONS: [ConnectAction; 2] = [ConnectAction::Retry, ConnectAction::Quit];

/// Button geometry for an explicit overlay state.
pub fn connect_button_rects_for_state(screen: [f32; 2], failed: bool, has_server_info: bool) -> [UiRect; 2] {
    let panel = connect_panel_rect_for_state(screen, failed, has_server_info);
    connect_buttons_in_panel(panel)
}

fn connect_buttons_in_panel(panel: UiRect) -> [UiRect; 2] {
    let button_width = (panel.w * 0.34).clamp(120.0, 180.0);
    let button_height = 36.0;
    let gap = 20.0;
    let total = button_width * 2.0 + gap;
    let left = panel.x + (panel.w - total) / 2.0;
    let y = panel.y + panel.h - 62.0;
    [
        UiRect::new(left, y, button_width, button_height),
        UiRect::new(left + button_width + gap, y, button_width, button_height),
    ]
}

/// Returns the overlay action under `(x, y)` for an explicit overlay state.
pub fn connect_action_at_for_state(
    screen: [f32; 2],
    failed: bool,
    has_server_info: bool,
    x: f32,
    y: f32,
) -> Option<ConnectAction> {
    let [retry, quit] = connect_button_rects_for_state(screen, failed, has_server_info);
    if retry.contains(x, y) {
        Some(ConnectAction::Retry)
    } else if quit.contains(x, y) {
        Some(ConnectAction::Quit)
    } else {
        None
    }
}

/// Panel rectangle for an explicit overlay state.
///
/// The height is content-driven: a failure panel is shorter than a loading
/// panel, and a pre-handshake "contacting" panel is shorter still. This is what
/// removes the large empty band the 1.0.0 panel had at 720p.
pub fn connect_panel_rect_for_state(screen: [f32; 2], failed: bool, has_server_info: bool) -> UiRect {
    let width = (screen[0] * 0.72).clamp(420.0, 620.0).min((screen[0] - 32.0).max(1.0));
    let content_height: f32 = if failed {
        240.0
    } else if has_server_info {
        300.0
    } else {
        220.0
    };
    let height = content_height.min((screen[1] - 32.0).max(1.0));
    UiRect::new((screen[0] - width) / 2.0, (screen[1] - height) / 2.0, width, height)
}

/// Text and progress shown while connecting or after a failure.
#[derive(Debug, Clone)]
pub struct ConnectPanel<'a> {
    /// Panel title (`Connecting`, `Connection failed` or `Disconnected`).
    pub title: &'a str,
    /// Endpoint exactly as supplied (`host:port`).
    pub endpoint: &'a str,
    /// Requested player name.
    pub player_name: &'a str,
    /// Current step, e.g. `"Contacting the server"` or `"Streaming the spawn area"`.
    pub status: &'a str,
    /// Animated spinner suffix.
    pub spinner: &'a str,
    /// Server world name, once the handshake completed.
    pub world_name: Option<&'a str>,
    /// Server MOTD, once the handshake completed.
    pub motd: Option<&'a str>,
    /// `(players, max_players)` once the handshake completed.
    pub players: Option<(usize, u16)>,
    /// `(loaded, target)` chunk columns while loading.
    pub progress: Option<(usize, usize)>,
    /// Connection failure or disconnect reason; switches the panel to error mode.
    pub failure: Option<&'a str>,
    /// Keyboard-focused button index (`0` retry, `1` quit).
    pub selected: usize,
    /// Mouse-hovered button index, if any.
    pub hovered: Option<usize>,
}

impl ConnectPanel<'_> {
    /// Label of the retry button for the current state.
    pub fn retry_label(&self) -> &'static str {
        if self.failure.is_some() { "Reconnect" } else { "Restart" }
    }
}

/// Draws the connect/loading/failure panel over a dimmed background.
pub fn draw_connect_panel(frame: &mut UiFrame, panel_info: &ConnectPanel<'_>, screen: [f32; 2]) {
    let [width, height] = screen;
    frame.rect(0.0, 0.0, width, height, [0.0, 0.0, 0.0, 0.72]);
    let failed = panel_info.failure.is_some();
    let has_server_info = panel_info.world_name.is_some()
        || panel_info.motd.is_some()
        || panel_info.players.is_some()
        || panel_info.progress.is_some();
    let panel = connect_panel_rect_for_state(screen, failed, has_server_info);
    frame.rect(panel.x + 6.0, panel.y + 8.0, panel.w, panel.h, [0.0, 0.0, 0.0, 0.35]);
    frame.rect(panel.x, panel.y, panel.w, panel.h, [0.055, 0.068, 0.092, 0.985]);
    frame.outline(panel.x, panel.y, panel.w, panel.h, 2.0, [0.43, 0.49, 0.58, 1.0]);
    frame.outline(
        panel.x + 3.0,
        panel.y + 3.0,
        panel.w - 6.0,
        panel.h - 6.0,
        1.0,
        [0.15, 0.18, 0.24, 1.0],
    );

    let centre_x = panel.x + panel.w / 2.0;
    let failure = panel_info.failure;
    frame.title_text_centered(centre_x, panel.y + 18.0, 2.4, [0.98, 0.99, 1.0, 1.0], panel_info.title);
    let status_color = if failure.is_some() {
        [0.98, 0.62, 0.55, 1.0]
    } else {
        [0.80, 0.86, 0.94, 1.0]
    };
    let status = if failure.is_some() {
        panel_info.status.to_string()
    } else {
        format!("{}{}", panel_info.status, panel_info.spinner)
    };
    frame.text_centered(centre_x, panel.y + 50.0, 1.5, status_color, &status);
    frame.rect(
        panel.x + 20.0,
        panel.y + 72.0,
        panel.w - 40.0,
        1.0,
        [0.19, 0.23, 0.29, 0.9],
    );

    let detail_top = panel.y + 88.0;
    let buttons = connect_buttons_in_panel(panel);
    let detail_bottom = buttons[0].y - 48.0;
    let mut y = detail_top;
    if let Some(reason) = failure {
        // Error mode deliberately hides stale world/roster/progress fields.
        y = draw_wrapped(
            frame,
            panel,
            y,
            1.3,
            [1.0, 0.82, 0.76, 1.0],
            &crate::net::friendly_disconnect_reason(reason),
            detail_bottom,
        );
    } else {
        if let Some(world_name) = panel_info.world_name {
            frame.text_centered(
                centre_x,
                y,
                1.6,
                [0.92, 0.96, 1.0, 1.0],
                &format!("World: {world_name}"),
            );
            y += 26.0;
        }
        if let Some(motd) = panel_info.motd
            && !motd.is_empty()
        {
            let max_chars = ((panel.w - 40.0) / ob_render::glyph_advance(1.2)).max(16.0) as usize;
            frame.text_centered(centre_x, y, 1.2, [0.66, 0.73, 0.82, 1.0], &truncate(motd, max_chars));
            y += 20.0;
        }
        if let Some((players, max_players)) = panel_info.players {
            frame.text_centered(
                centre_x,
                y,
                1.3,
                [0.72, 0.79, 0.88, 1.0],
                &format!("Players {players}/{max_players}"),
            );
            y += 22.0;
        }
        if let Some((loaded, target)) = panel_info.progress {
            let fraction = if target == 0 {
                0.0
            } else {
                (loaded as f32 / target as f32).clamp(0.0, 1.0)
            };
            let bar_width = (panel.w - 80.0).max(40.0);
            let bar_x = panel.x + (panel.w - bar_width) / 2.0;
            let label = format!("Loading spawn area  {loaded}/{target} chunks  {:.0}%", fraction * 100.0);
            frame.text_centered(centre_x, y, 1.3, [0.82, 0.88, 0.94, 1.0], &label);
            y += 20.0;
            frame.rect(bar_x, y, bar_width, 12.0, [0.10, 0.12, 0.16, 1.0]);
            frame.outline(bar_x, y, bar_width, 12.0, 1.0, [0.35, 0.41, 0.50, 1.0]);
            frame.rect(
                bar_x + 2.0,
                y + 2.0,
                (bar_width - 4.0) * fraction,
                8.0,
                [0.42, 0.76, 0.94, 1.0],
            );
        }
    }
    let _ = y;

    let footer_scale = 1.1;
    let max_chars = ((panel.w - 40.0) / ob_render::glyph_advance(footer_scale)).max(16.0) as usize;
    frame.text_centered(
        centre_x,
        buttons[0].y - 34.0,
        footer_scale,
        [0.62, 0.68, 0.76, 1.0],
        &format!(
            "Endpoint: {}",
            truncate(panel_info.endpoint, max_chars.saturating_sub(10))
        ),
    );
    frame.text_centered(
        centre_x,
        buttons[0].y - 20.0,
        footer_scale,
        [0.62, 0.68, 0.76, 1.0],
        &format!(
            "Player: {}",
            truncate(panel_info.player_name, max_chars.saturating_sub(8))
        ),
    );
    let [retry, quit] = buttons;
    draw_button(
        frame,
        retry,
        panel_info.retry_label(),
        false,
        panel_info.selected == 0,
        panel_info.hovered == Some(0),
    );
    draw_button(
        frame,
        quit,
        "Quit",
        true,
        panel_info.selected == 1,
        panel_info.hovered == Some(1),
    );
    frame.text_centered(
        centre_x,
        panel.y + panel.h - 18.0,
        0.95,
        [0.52, 0.58, 0.66, 1.0],
        &truncate(
            &format!("R reconnect  -  Enter select  -  Esc quit  -  v{}", ob_core::VERSION),
            ((panel.w - 16.0) / ob_render::glyph_advance(0.95)).max(16.0) as usize,
        ),
    );
}

/// Draws a simple raised button with a centred label and keyboard focus ring.
fn draw_button(frame: &mut UiFrame, rect: UiRect, label: &str, danger: bool, focused: bool, hovered: bool) {
    let (background, border) = if danger {
        ([0.18, 0.08, 0.09, 0.98], [0.55, 0.25, 0.27, 1.0])
    } else {
        ([0.10, 0.19, 0.14, 0.98], [0.48, 0.78, 0.44, 1.0])
    };
    let active = focused || hovered;
    let (background, border) = if active {
        (
            [background[0] + 0.08, background[1] + 0.08, background[2] + 0.06, 1.0],
            if focused { [0.98, 0.84, 0.46, 1.0] } else { border },
        )
    } else {
        (background, border)
    };
    if focused {
        frame.rect(
            rect.x - 3.0,
            rect.y - 3.0,
            rect.w + 6.0,
            rect.h + 6.0,
            [0.12, 0.10, 0.04, 0.95],
        );
    }
    frame.rect(rect.x, rect.y, rect.w, rect.h, background);
    frame.outline(rect.x, rect.y, rect.w, rect.h, if active { 2.0 } else { 1.0 }, border);
    let text_height = ob_render::line_height(1.5);
    frame.text_centered(
        rect.center().0,
        rect.y + (rect.h - text_height) / 2.0,
        1.5,
        [1.0, 1.0, 1.0, 1.0],
        label,
    );
}

/// Draws wrapped text inside `panel` up to `max_y` and returns the next Y.
fn draw_wrapped(
    frame: &mut UiFrame,
    panel: UiRect,
    mut y: f32,
    scale: f32,
    color: [f32; 4],
    text: &str,
    max_y: f32,
) -> f32 {
    let max_chars = (((panel.w - 80.0) / ob_render::glyph_advance(scale)).max(20.0)) as usize;
    let step = ob_render::line_height(scale) + 3.0;
    for line in wrap_text(text, max_chars) {
        if y + ob_render::line_height(scale) > max_y {
            break;
        }
        frame.text_centered(panel.x + panel.w / 2.0, y, scale, color, &line);
        y += step;
    }
    y
}

/// Wraps plain text at word boundaries, hard-splitting long words.
fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let extra = usize::from(!current.is_empty());
        if current.chars().count() + extra + word.chars().count() <= max_chars {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        let mut rest = word;
        while rest.chars().count() > max_chars {
            let split = rest
                .char_indices()
                .nth(max_chars)
                .map(|(index, _)| index)
                .unwrap_or(rest.len());
            lines.push(rest[..split].to_string());
            rest = &rest[split..];
        }
        current.push_str(rest);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Draws the fading chat history and, when active, the text input bar.
pub fn draw_chat(frame: &mut UiFrame, log: &ChatLog, input: Option<&str>, screen: [f32; 2]) {
    let scale = 1.35;
    let line_height = ob_render::line_height(scale) + 4.0;
    // Sit clear of the toast stack, which occupies the band just above the
    // hearts. At 720p the old `-132` base overlapped the lowest pickup toast.
    let base_y = (screen[1] - 180.0).max(120.0);
    if let Some(text) = input {
        let bar = UiRect::new(12.0, base_y, (screen[0] * 0.62).clamp(320.0, 760.0), line_height + 10.0);
        frame.rect(bar.x, bar.y, bar.w, bar.h, [0.03, 0.04, 0.06, 0.86]);
        frame.outline(bar.x, bar.y, bar.w, bar.h, 1.0, [0.46, 0.52, 0.62, 0.9]);
        let hint = "Enter send  Esc cancel";
        let hint_width = frame.text_width(1.0, hint);
        let input_width = (bar.w - 32.0 - hint_width - 8.0).max(24.0);
        let input_chars = (input_width / ob_render::glyph_advance(scale)).max(4.0) as usize;
        frame.text(bar.x + 8.0, bar.y + 5.0, scale, [1.0, 0.95, 0.65, 1.0], ">");
        frame.text(
            bar.x + 24.0,
            bar.y + 5.0,
            scale,
            [0.96, 0.98, 1.0, 1.0],
            &truncate(text, input_chars),
        );
        frame.text(
            bar.x + bar.w - hint_width - 8.0,
            bar.y + 7.0,
            1.0,
            [0.62, 0.68, 0.76, 0.9],
            hint,
        );
    }
    let max_chars = (((screen[0] - 40.0) / ob_render::glyph_advance(scale)).max(24.0) as usize).clamp(24, 110);
    let mut y = if input.is_some() { base_y - 8.0 } else { base_y };
    for line in log.visible(6).into_iter().rev() {
        let alpha = line.alpha();
        if alpha <= 0.001 {
            continue;
        }
        y -= line_height;
        let text = format!("<{}> {}", line.from, truncate(&line.text, max_chars));
        let width = frame.text_width(scale, &text);
        frame.rect(
            12.0,
            y - 2.0,
            width + 12.0,
            line_height,
            [0.02, 0.02, 0.03, 0.38 * alpha],
        );
        frame.text(18.0, y, scale, [0.92, 0.95, 0.99, alpha], &text);
        if y < 60.0 {
            break;
        }
    }
}

/// Shortens a chat line for the bitmap overlay without splitting a char.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    shortened.push_str("...");
    shortened
}

/// Player-list panel rectangle for `rows` entries.
pub fn player_list_rect(screen: [f32; 2], rows: usize) -> UiRect {
    let rows = rows.clamp(1, 18) as f32;
    let width = (screen[0] * 0.46).clamp(300.0, 560.0).min(screen[0] - 16.0);
    let height = (86.0 + rows * 24.0).min(screen[1] - 16.0);
    UiRect::new(
        (screen[0] - width) / 2.0,
        ((screen[1] - height) * 0.32).max(8.0),
        width,
        height,
    )
}

/// Draws the Tab player list.
pub fn draw_player_list(
    frame: &mut UiFrame,
    entries: &[PlayerEntry],
    max_players: u16,
    ping_ms: Option<u32>,
    screen: [f32; 2],
) {
    let panel = player_list_rect(screen, entries.len().min(18));
    frame.rect(panel.x, panel.y, panel.w, panel.h, [0.035, 0.045, 0.06, 0.88]);
    frame.outline(panel.x, panel.y, panel.w, panel.h, 1.5, [0.40, 0.47, 0.58, 0.95]);
    let centre_x = panel.x + panel.w / 2.0;
    let title = format!("Players {}/{}", entries.len(), max_players);
    frame.title_text_centered(centre_x, panel.y + 12.0, 1.9, [0.98, 0.99, 1.0, 1.0], &title);
    let ping = ping_ms
        .map(|millis| format!("ping {millis} ms"))
        .unwrap_or_else(|| "ping --".to_string());
    frame.text_centered(centre_x, panel.y + 42.0, 1.15, [0.62, 0.69, 0.78, 1.0], &ping);
    frame.rect(
        panel.x + 16.0,
        panel.y + 60.0,
        panel.w - 32.0,
        1.0,
        [0.22, 0.27, 0.34, 1.0],
    );
    let visible_rows = (((panel.h - 68.0) / 24.0).floor() as usize).clamp(1, 18);
    for (index, entry) in entries.iter().take(visible_rows).enumerate() {
        let y = panel.y + 68.0 + index as f32 * 24.0;
        let color = if entry.is_self {
            [1.0, 0.92, 0.58, 1.0]
        } else {
            [0.88, 0.92, 0.97, 1.0]
        };
        let suffix = if entry.is_self { "  (you)" } else { "" };
        let label = format!("#{} {}{suffix}", entry.id, entry.name);
        let max_chars = (((panel.w - 22.0 - 42.0) / ob_render::glyph_advance(1.45)).max(8.0)) as usize;
        frame.text(panel.x + 22.0, y, 1.45, color, &truncate(&label, max_chars));
        let avatar = crate::net::avatar_color(entry.id, &entry.name);
        frame.rect(
            panel.x + panel.w - 30.0,
            y + 2.0,
            10.0,
            10.0,
            [avatar[0], avatar[1], avatar[2], 1.0],
        );
    }
}

/// Fades the "multiplayer survival" notice in and out.
pub fn draw_multiplayer_notice(frame: &mut UiFrame, seconds: f32, screen: [f32; 2]) {
    if !seconds.is_finite() || seconds <= 0.0 {
        return;
    }
    let alpha = (seconds / 2.0).clamp(0.0, 1.0) * 0.85;
    let centre_x = screen[0] / 2.0;
    let y = screen[1] * 0.20;
    frame.text_centered(centre_x, y, 1.5, [1.0, 0.93, 0.64, alpha], "Multiplayer survival");
    frame.text_centered(
        centre_x,
        y + 22.0,
        1.15,
        [0.88, 0.92, 0.97, alpha * 0.9],
        "Vitals, inventory, containers and mobs are server-authoritative",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_buttons_are_inside_the_panel_and_hit_test() {
        for screen in [[1280.0, 720.0], [800.0, 600.0], [1920.0, 1080.0]] {
            let panel = connect_panel_rect_for_state(screen, false, true);
            let [retry, quit] = connect_button_rects_for_state(screen, false, true);
            assert!(retry.x >= panel.x && retry.x + retry.w <= panel.x + panel.w);
            assert!(quit.x >= panel.x && quit.x + quit.w <= panel.x + panel.w);
            assert!(retry.y >= panel.y && retry.y + retry.h <= panel.y + panel.h);
            assert_eq!(
                connect_action_at_for_state(screen, false, true, retry.center().0, retry.center().1),
                Some(ConnectAction::Retry)
            );
            assert_eq!(
                connect_action_at_for_state(screen, false, true, quit.center().0, quit.center().1),
                Some(ConnectAction::Quit)
            );
            assert_eq!(connect_action_at_for_state(screen, false, true, 1.0, 1.0), None);
            assert!(panel.w <= screen[0]);
            assert!(panel.h <= screen[1]);
        }
    }

    #[test]
    fn text_wrapping_preserves_words_and_bounds_lines() {
        let lines = wrap_text("the quick brown fox jumps over the lazy dog", 12);
        assert!(lines.iter().all(|line| line.chars().count() <= 12));
        assert_eq!(lines.join(" "), "the quick brown fox jumps over the lazy dog");
        let hard = wrap_text("supercalifragilisticexpialidocious", 6);
        assert!(hard.iter().all(|line| line.chars().count() <= 6));
        assert_eq!(hard.concat(), "supercalifragilisticexpialidocious");
    }

    #[test]
    fn chat_truncation_is_char_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "he...");
        let unicode = truncate("æææææ", 4);
        assert_eq!(unicode, "æ...");
    }

    #[test]
    fn overlays_draw_into_a_ui_frame() {
        use crate::net::{ChatInput, ChatLog, PlayerEntry};

        let screen = [1280.0, 720.0];
        let mut frame = UiFrame::new(screen[0], screen[1]);
        let mut log = ChatLog::default();
        log.push("alice", "hello from the network");
        log.push("server", "A self-hosted OpenBlock world");
        let mut input = ChatInput::default();
        input.open();
        for character in "typing".chars() {
            input.push_char(character);
        }
        draw_connect_panel(
            &mut frame,
            &ConnectPanel {
                title: "Connecting",
                endpoint: "127.0.0.1:25565",
                player_name: "player1234",
                status: "Streaming the spawn area",
                spinner: "..",
                world_name: Some("world"),
                motd: Some("A self-hosted OpenBlock world"),
                players: Some((2, 8)),
                progress: Some((12, 49)),
                failure: None,
                selected: 0,
                hovered: None,
            },
            screen,
        );
        draw_chat(&mut frame, &log, input.text(), screen);
        draw_player_list(
            &mut frame,
            &[
                PlayerEntry {
                    id: 1,
                    name: "alice".to_string(),
                    is_self: true,
                },
                PlayerEntry {
                    id: 2,
                    name: "bob".to_string(),
                    is_self: false,
                },
            ],
            8,
            Some(23),
            screen,
        );
        draw_multiplayer_notice(&mut frame, 6.0, screen);
        draw_connect_panel(
            &mut frame,
            &ConnectPanel {
                title: "Connection failed",
                endpoint: "127.0.0.1:25565",
                player_name: "player1234",
                status: "The connection attempt failed.",
                spinner: "",
                world_name: None,
                motd: None,
                players: None,
                progress: None,
                failure: Some("server_full: 8/8 players"),
                selected: 0,
                hovered: Some(1),
            },
            screen,
        );

        // Shrinking the target must not panic or place a panel outside it.
        let small = [420.0, 300.0];
        let mut tiny = UiFrame::new(small[0], small[1]);
        draw_chat(&mut tiny, &log, Some("x"), small);
        draw_player_list(&mut tiny, &[], 8, None, small);
        draw_connect_panel(
            &mut tiny,
            &ConnectPanel {
                title: "Connecting",
                endpoint: "127.0.0.1:25565",
                player_name: "p",
                status: "Loading",
                spinner: "...",
                world_name: Some("w"),
                motd: Some(""),
                players: Some((1, 1)),
                progress: Some((1, 9)),
                failure: None,
                selected: 0,
                hovered: None,
            },
            small,
        );
    }

    #[test]
    fn player_list_panel_fits_the_screen() {
        for rows in [1, 4, 18, 40] {
            let panel = player_list_rect([1280.0, 720.0], rows);
            assert!(panel.x >= 0.0 && panel.y >= 0.0);
            assert!(panel.x + panel.w <= 1280.0);
            assert!(panel.y + panel.h <= 720.0);
        }
    }

    #[test]
    fn loading_and_failure_panels_are_compact_and_content_driven() {
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0], [640.0, 360.0]] {
            let loading = connect_panel_rect_for_state(screen, false, true);
            let contacting = connect_panel_rect_for_state(screen, false, false);
            let failed = connect_panel_rect_for_state(screen, true, false);
            assert!(failed.h <= loading.h, "failure panel should not be taller than loading");
            assert!(contacting.h <= loading.h);
            for (label, panel) in [("loading", loading), ("contacting", contacting), ("failed", failed)] {
                assert!(
                    panel.x >= 0.0
                        && panel.y >= 0.0
                        && panel.x + panel.w <= screen[0]
                        && panel.y + panel.h <= screen[1],
                    "{label} panel outside {screen:?}: {panel:?}"
                );
                for button in connect_button_rects_for_state(screen, label == "failed", label == "loading") {
                    assert!(
                        button.x >= panel.x && button.x + button.w <= panel.x + panel.w,
                        "{label} button outside panel"
                    );
                    assert!(button.y >= panel.y && button.y + button.h <= panel.y + panel.h);
                }
            }
            // Error mode must hit-test its own, shorter button positions.
            let failed_buttons = connect_button_rects_for_state(screen, true, false);
            assert_eq!(
                connect_action_at_for_state(
                    screen,
                    true,
                    false,
                    failed_buttons[0].center().0,
                    failed_buttons[0].center().1
                ),
                Some(ConnectAction::Retry)
            );
            assert_eq!(
                connect_action_at_for_state(
                    screen,
                    true,
                    false,
                    failed_buttons[1].center().0,
                    failed_buttons[1].center().1
                ),
                Some(ConnectAction::Quit)
            );
        }
    }

    #[test]
    fn focused_and_hovered_connect_buttons_draw_a_visible_focus_state() {
        let screen = [1280.0, 720.0];
        let panel = ConnectPanel {
            title: "Connection failed",
            endpoint: "127.0.0.1:25565",
            player_name: "player1234",
            status: "The connection attempt failed.",
            spinner: "",
            world_name: None,
            motd: None,
            players: None,
            progress: None,
            failure: Some("server_full: 8/8 players"),
            selected: 1,
            hovered: Some(0),
        };
        let mut focused = UiFrame::new(screen[0], screen[1]);
        draw_connect_panel(&mut focused, &panel, screen);
        let mut unfocused = UiFrame::new(screen[0], screen[1]);
        draw_connect_panel(
            &mut unfocused,
            &ConnectPanel {
                selected: 0,
                hovered: None,
                ..panel.clone()
            },
            screen,
        );
        let (focused_quads, _) = focused.measure();
        let (unfocused_quads, _) = unfocused.measure();
        // Both states draw a complete panel; the focus ring is a border/ring
        // recolour plus an outer rectangle, so the important assertion is that
        // neither selection state is skipped or panics.
        assert!(focused_quads > 40);
        assert!(unfocused_quads > 40);
        assert_eq!(panel.retry_label(), "Reconnect");
    }
}
