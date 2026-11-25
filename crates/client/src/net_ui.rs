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

/// Button geometry for the connect overlay: `[retry, quit]`.
pub fn connect_button_rects(screen: [f32; 2]) -> [UiRect; 2] {
    let panel = connect_panel_rect(screen);
    let button_width = (panel.w * 0.36).clamp(130.0, 210.0);
    let button_height = 42.0;
    let gap = 22.0;
    let total = button_width * 2.0 + gap;
    let left = panel.x + (panel.w - total) / 2.0;
    let y = panel.y + panel.h - button_height - 30.0;
    [
        UiRect::new(left, y, button_width, button_height),
        UiRect::new(left + button_width + gap, y, button_width, button_height),
    ]
}

/// Returns the overlay action under `(x, y)`, if any.
pub fn connect_action_at(screen: [f32; 2], x: f32, y: f32) -> Option<ConnectAction> {
    let [retry, quit] = connect_button_rects(screen);
    if retry.contains(x, y) {
        Some(ConnectAction::Retry)
    } else if quit.contains(x, y) {
        Some(ConnectAction::Quit)
    } else {
        None
    }
}

/// Panel rectangle of the connect overlay.
pub fn connect_panel_rect(screen: [f32; 2]) -> UiRect {
    let width = (screen[0] * 0.72).clamp(380.0, 760.0).min(screen[0] - 32.0);
    let height = (screen[1] * 0.66).clamp(300.0, 520.0).min(screen[1] - 32.0);
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
    /// Current step, e.g. `"Connecting..."` or `"Loading world..."`.
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
}

/// Draws the connect/loading/failure panel over a dimmed background.
pub fn draw_connect_panel(frame: &mut UiFrame, panel_info: &ConnectPanel<'_>, screen: [f32; 2]) {
    let [width, height] = screen;
    frame.rect(0.0, 0.0, width, height, [0.0, 0.0, 0.0, 0.72]);
    let panel = connect_panel_rect(screen);
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
    frame.text_centered(centre_x, panel.y + 26.0, 2.7, [0.98, 0.99, 1.0, 1.0], panel_info.title);
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
    frame.text_centered(centre_x, panel.y + 68.0, 1.6, status_color, &status);

    let mut y = panel.y + 100.0;
    if let Some(reason) = failure {
        y += draw_wrapped(
            frame,
            panel,
            y,
            1.45,
            [1.0, 0.82, 0.76, 1.0],
            &crate::net::friendly_disconnect_reason(reason),
        );
        y += 8.0;
    } else {
        if let Some(world_name) = panel_info.world_name {
            frame.text_centered(
                centre_x,
                y,
                1.7,
                [0.92, 0.96, 1.0, 1.0],
                &format!("World: {world_name}"),
            );
            y += 26.0;
        }
        if let Some(motd) = panel_info.motd
            && !motd.is_empty()
        {
            frame.text_centered(centre_x, y, 1.3, [0.66, 0.73, 0.82, 1.0], motd);
            y += 22.0;
        }
        if let Some((players, max_players)) = panel_info.players {
            frame.text_centered(
                centre_x,
                y,
                1.35,
                [0.72, 0.79, 0.88, 1.0],
                &format!("Players {players}/{max_players}"),
            );
            y += 24.0;
        }
        if let Some((loaded, target)) = panel_info.progress {
            let fraction = if target == 0 {
                0.0
            } else {
                (loaded as f32 / target as f32).clamp(0.0, 1.0)
            };
            let bar_width = panel.w - 110.0;
            let bar_x = panel.x + 55.0;
            frame.text_centered(
                centre_x,
                y,
                1.4,
                [0.82, 0.88, 0.94, 1.0],
                &format!("Loading spawn area  {loaded}/{target} chunks"),
            );
            y += 24.0;
            frame.rect(bar_x, y, bar_width, 12.0, [0.10, 0.12, 0.16, 1.0]);
            frame.outline(bar_x, y, bar_width, 12.0, 1.0, [0.35, 0.41, 0.50, 1.0]);
            frame.rect(
                bar_x + 2.0,
                y + 2.0,
                (bar_width - 4.0) * fraction,
                8.0,
                [0.42, 0.76, 0.94, 1.0],
            );
            y += 30.0;
        }
    }
    let _ = y;

    frame.text_centered(
        centre_x,
        panel.y + panel.h - 96.0,
        1.25,
        [0.58, 0.64, 0.73, 1.0],
        &format!(
            "{}  -  player {}",
            truncate(panel_info.endpoint, 72),
            truncate(panel_info.player_name, 32)
        ),
    );
    let [retry, quit] = connect_button_rects(screen);
    draw_button(
        frame,
        retry,
        if failure.is_some() { "Reconnect" } else { "Restart" },
        false,
    );
    draw_button(frame, quit, "Quit", true);
    frame.text_centered(
        centre_x,
        panel.y + panel.h - 20.0,
        1.1,
        [0.52, 0.58, 0.66, 1.0],
        "R reconnect  -  Esc quit",
    );
}

/// Draws a simple raised button with a centred label.
fn draw_button(frame: &mut UiFrame, rect: UiRect, label: &str, danger: bool) {
    let (background, border) = if danger {
        ([0.18, 0.08, 0.09, 0.98], [0.55, 0.25, 0.27, 1.0])
    } else {
        ([0.10, 0.19, 0.14, 0.98], [0.48, 0.78, 0.44, 1.0])
    };
    frame.rect(rect.x, rect.y, rect.w, rect.h, background);
    frame.outline(rect.x, rect.y, rect.w, rect.h, 2.0, border);
    frame.text_centered(rect.center().0, rect.y + 13.0, 1.6, [1.0, 1.0, 1.0, 1.0], label);
}

/// Draws wrapped text inside `panel` and returns the used height.
fn draw_wrapped(frame: &mut UiFrame, panel: UiRect, mut y: f32, scale: f32, color: [f32; 4], text: &str) -> f32 {
    let max_chars = ((panel.w - 80.0) / (ob_render::line_height(scale) * 0.55)).max(24.0) as usize;
    let start = y;
    for line in wrap_text(text, max_chars) {
        frame.text_centered(panel.x + panel.w / 2.0, y, scale, color, &line);
        y += ob_render::line_height(scale) + 4.0;
    }
    y - start
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
        frame.text(bar.x + 8.0, bar.y + 5.0, scale, [1.0, 0.95, 0.65, 1.0], ">");
        frame.text(bar.x + 24.0, bar.y + 5.0, scale, [0.96, 0.98, 1.0, 1.0], text);
        frame.text(
            bar.x + bar.w - frame.text_width(1.0, "Enter send  Esc cancel") - 8.0,
            bar.y + 7.0,
            1.0,
            [0.62, 0.68, 0.76, 0.9],
            "Enter send  Esc cancel",
        );
    }
    let max_chars = (((screen[0] - 40.0) / (line_height * 0.55)).max(24.0) as usize).clamp(24, 110);
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
    frame.text_centered(centre_x, panel.y + 12.0, 1.9, [0.98, 0.99, 1.0, 1.0], &title);
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
        frame.text(
            panel.x + 22.0,
            y,
            1.45,
            color,
            &format!("#{} {}{suffix}", entry.id, entry.name),
        );
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
            let panel = connect_panel_rect(screen);
            let [retry, quit] = connect_button_rects(screen);
            assert!(retry.x >= panel.x && retry.x + retry.w <= panel.x + panel.w);
            assert!(quit.x >= panel.x && quit.x + quit.w <= panel.x + panel.w);
            assert!(retry.y >= panel.y && retry.y + retry.h <= panel.y + panel.h);
            assert_eq!(
                connect_action_at(screen, retry.center().0, retry.center().1),
                Some(ConnectAction::Retry)
            );
            assert_eq!(
                connect_action_at(screen, quit.center().0, quit.center().1),
                Some(ConnectAction::Quit)
            );
            assert_eq!(connect_action_at(screen, 1.0, 1.0), None);
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
}
