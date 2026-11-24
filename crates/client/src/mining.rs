//! Hold-to-mine state machine and HUD progress geometry.
//!
//! The state machine is deliberately GPU-free and time-source agnostic: the
//! caller passes elapsed seconds and the block currently under the crosshair.
//! This keeps the break rules (effective seconds, target changes, one-shot completion)
//! unit-testable without a window.

use ob_math::{BlockPos, WorldVec};
use ob_render::UiFrame;

/// Width of the mining progress bar in UI pixels.
pub const MINING_BAR_WIDTH: f32 = 44.0;
/// Height of the mining progress bar in UI pixels.
pub const MINING_BAR_HEIGHT: f32 = 6.0;
/// Vertical offset of the bar below the crosshair centre.
pub const MINING_BAR_OFFSET_Y: f32 = 16.0;
/// Size of the optional corner ticks around the bar.
pub const MINING_TICK_SIZE: f32 = 3.0;

/// Mutable mining progress for the block currently targeted by the player.
///
/// Progress is `elapsed_seconds / seconds`, where `seconds` comes from the
/// authoritative host (`Host::mining_seconds`) and therefore already includes
/// the selected tool's speed. A target change, a release (via
/// [`MiningState::stop`] or an `update` with a different `current_target`) or a
/// completed break resets the active target. Completion is latched once and
/// returned exactly once from [`MiningState::take_completed`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MiningState {
    target: Option<BlockPos>,
    seconds: f32,
    progress: f32,
    completed: Option<BlockPos>,
}

impl MiningState {
    /// Starts (or restarts) mining `pos`, which takes `seconds` effective
    /// seconds with the currently selected tool.
    ///
    /// `0.0` (and non-finite durations) mark an unbreakable block: it may be
    /// recorded as the active target for an outline, but [`Self::update`] never
    /// advances its progress or reports a completion. A pending completion from
    /// a previous target is deliberately left untouched so it cannot be lost
    /// before [`Self::take_completed`].
    pub fn start(&mut self, pos: BlockPos, seconds: f32) {
        self.target = Some(pos);
        self.seconds = seconds;
        self.progress = 0.0;
    }

    /// Advances mining by `dt` seconds, given the block currently under the ray.
    ///
    /// If `current_target` is not the active target (the crosshair moved, the
    /// block disappeared, or the player looked away), the active target resets
    /// and no progress is accumulated. Once progress reaches 1.0 the break is
    /// latched for [`Self::take_completed`] and the active target is cleared.
    pub fn update(&mut self, dt: f32, current_target: Option<BlockPos>) {
        let Some(target) = self.target else {
            return;
        };
        if current_target != Some(target) {
            self.reset_progress();
            return;
        }
        if !self.seconds.is_finite() || self.seconds <= 0.0 {
            self.progress = 0.0;
            return;
        }
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        self.progress = (self.progress + dt / self.seconds).min(1.0);
        if self.progress >= 1.0 {
            self.completed = Some(target);
            self.reset_progress();
        }
    }

    /// Cancels the active target (mouse released, paused, or screen changed).
    ///
    /// A completion latched by [`Self::update`] is preserved for exactly one
    /// [`Self::take_completed`] call.
    pub fn stop(&mut self) {
        self.reset_progress();
    }

    /// Returns the block that finished breaking, at most once per completion.
    pub fn take_completed(&mut self) -> Option<BlockPos> {
        self.completed.take()
    }

    /// Currently mined block, if any.
    pub fn target(&self) -> Option<BlockPos> {
        self.target
    }

    /// Effective seconds used for the active target, or `0.0` when idle.
    ///
    /// The client compares this with the host's current `mining_seconds` so a
    /// hotbar switch restarts progress instead of keeping a stale timer.
    pub fn duration_seconds(&self) -> f32 {
        self.seconds
    }

    /// Alias used by renderer integrations that need the targeted block.
    pub fn targeted_block(&self) -> Option<BlockPos> {
        self.target
    }

    /// Mining progress in `0.0..=1.0`; always zero when nothing is mined.
    pub fn progress(&self) -> f32 {
        self.progress
    }

    /// Whether a block is currently being mined.
    pub fn is_mining(&self) -> bool {
        self.target.is_some()
    }

    /// World-space axis-aligned bounds of the targeted block: `(min, max)`.
    ///
    /// `min` is the block's corner and `max` is `min + (1, 1, 1)`, matching the
    /// inclusive/exclusive convention used by the world and block renderer.
    /// Returns `None` when no block is targeted.
    pub fn outline_box(&self) -> Option<(WorldVec, WorldVec)> {
        self.target.map(|pos| {
            let min = WorldVec::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
            (min, min + WorldVec::splat(1.0))
        })
    }

    fn reset_progress(&mut self) {
        self.target = None;
        self.seconds = 0.0;
        self.progress = 0.0;
    }
}

/// Draws the mining progress bar and its four corner ticks below a crosshair.
///
/// Nothing is drawn outside `0.0..=1.0`, so callers should guard with
/// [`MiningState::is_mining`]. The bar is dark with a warm-coloured fill.
pub fn draw_progress(frame: &mut UiFrame, centre: (f32, f32), progress: f32) {
    if !centre.0.is_finite() || !centre.1.is_finite() || !progress.is_finite() {
        return;
    }
    let progress = progress.clamp(0.0, 1.0);
    let x = centre.0 - MINING_BAR_WIDTH / 2.0;
    let y = centre.1 + MINING_BAR_OFFSET_Y;

    frame.rect(
        x - 1.0,
        y - 1.0,
        MINING_BAR_WIDTH + 2.0,
        MINING_BAR_HEIGHT + 2.0,
        [0.02, 0.02, 0.03, 0.72],
    );
    frame.rect(x, y, MINING_BAR_WIDTH, MINING_BAR_HEIGHT, [0.13, 0.14, 0.17, 0.94]);
    if progress > 0.0 {
        frame.rect(
            x,
            y,
            MINING_BAR_WIDTH * progress,
            MINING_BAR_HEIGHT,
            [0.98, 0.92, 0.55, 1.0],
        );
    }

    let tick = [0.94, 0.96, 1.0, 0.88];
    for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let tick_x = if dx < 0.0 {
            x - MINING_TICK_SIZE - 1.0
        } else {
            x + MINING_BAR_WIDTH + 1.0
        };
        let tick_y = if dy < 0.0 {
            y - MINING_TICK_SIZE
        } else {
            y + MINING_BAR_HEIGHT
        };
        frame.rect(tick_x, tick_y, MINING_TICK_SIZE, MINING_TICK_SIZE, tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(x: i32, y: i32, z: i32) -> BlockPos {
        BlockPos::new(x, y, z)
    }

    #[test]
    fn progress_accumulates_and_completes_exactly_once() {
        let mut mining = MiningState::default();
        let target = pos(1, 2, 3);
        mining.start(target, 2.0);
        assert!(mining.is_mining());
        assert_eq!(mining.target(), Some(target));

        mining.update(0.5, Some(target));
        assert!((mining.progress() - 0.25).abs() < 1e-6);
        assert_eq!(mining.take_completed(), None);

        mining.update(1.0, Some(target));
        assert!((mining.progress() - 0.75).abs() < 1e-6);
        assert_eq!(mining.take_completed(), None);

        mining.update(1.0, Some(target));
        assert!(!mining.is_mining(), "completion clears the active target");
        assert_eq!(mining.progress(), 0.0);
        assert_eq!(mining.take_completed(), Some(target));
        assert_eq!(mining.take_completed(), None, "a completion must fire exactly once");

        // Further updates do nothing until a new target starts.
        mining.update(1.0, Some(target));
        assert_eq!(mining.take_completed(), None);
    }

    #[test]
    fn target_change_resets_progress_without_completing() {
        let mut mining = MiningState::default();
        let first = pos(0, 0, 0);
        let second = pos(1, 0, 0);
        mining.start(first, 1.0);
        mining.update(0.9, Some(first));
        assert!(mining.progress() > 0.8);

        mining.update(0.2, Some(second));
        assert!(!mining.is_mining());
        assert_eq!(mining.progress(), 0.0);
        assert_eq!(mining.take_completed(), None);

        // Releasing the mouse (no current target) also resets.
        mining.start(second, 1.0);
        mining.update(0.5, Some(second));
        mining.update(0.5, None);
        assert!(!mining.is_mining());
        assert_eq!(mining.progress(), 0.0);
        assert_eq!(mining.take_completed(), None);

        // Explicit cancel behaves the same.
        mining.start(first, 1.0);
        mining.update(0.25, Some(first));
        mining.stop();
        assert!(!mining.is_mining());
        assert_eq!(mining.progress(), 0.0);
    }

    #[test]
    fn unbreakable_blocks_never_progress_or_complete() {
        for seconds in [0.0_f32, f32::INFINITY] {
            let mut mining = MiningState::default();
            let target = pos(4, 5, 6);
            mining.start(target, seconds);
            assert!(mining.is_mining());
            assert_eq!(mining.duration_seconds(), seconds);
            for _ in 0..100 {
                mining.update(1.0, Some(target));
                assert_eq!(mining.progress(), 0.0);
            }
            assert_eq!(mining.take_completed(), None);
            assert_eq!(mining.target(), Some(target));
        }
    }

    #[test]
    fn positive_seconds_still_complete_and_expose_the_duration() {
        let mut mining = MiningState::default();
        let target = pos(0, 1, 0);
        mining.start(target, 0.25);
        assert_eq!(mining.duration_seconds(), 0.25);
        mining.update(0.25, Some(target));
        assert_eq!(mining.take_completed(), Some(target));
        assert_eq!(mining.take_completed(), None);
        assert_eq!(mining.duration_seconds(), 0.0);
    }

    #[test]
    fn content_tool_speed_drives_the_timer() {
        let registry = ob_content::Registry::base();
        let stone = registry.id("openblock:stone").expect("stone block");
        let wood_pick = registry.item_id("openblock:wooden_pickaxe").expect("wooden pickaxe");
        let seconds = registry.effective_mining_seconds(stone, Some(wood_pick));
        assert!((seconds - 0.75).abs() < 1e-6);
        let mut mining = MiningState::default();
        let target = pos(1, 2, 3);
        mining.start(target, seconds);
        mining.update(0.375, Some(target));
        assert!((mining.progress() - 0.5).abs() < 1e-6);
        mining.update(0.375, Some(target));
        assert_eq!(mining.take_completed(), Some(target));

        // Hand mining is slower and unbreakable blocks never start progress.
        let by_hand = registry.effective_mining_seconds(stone, None);
        assert!((by_hand - 1.5).abs() < 1e-6);
        let bedrock = registry.id("openblock:bedrock").expect("bedrock block");
        assert_eq!(registry.effective_mining_seconds(bedrock, Some(wood_pick)), 0.0);
    }

    #[test]
    fn outline_box_is_world_space_min_max() {
        let mut mining = MiningState::default();
        assert_eq!(mining.outline_box(), None);
        mining.start(pos(-2, 7, 12), 1.0);
        let (min, max) = mining.outline_box().expect("target outline");
        assert_eq!(min, WorldVec::new(-2.0, 7.0, 12.0));
        assert_eq!(max, WorldVec::new(-1.0, 8.0, 13.0));
        assert_eq!(mining.targeted_block(), Some(pos(-2, 7, 12)));
    }

    #[test]
    fn progress_indicator_draws_a_bar_and_four_corner_ticks() {
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_progress(&mut frame, (640.0, 360.0), 0.5);
        let (vertices, quads) = frame.measure();
        assert_eq!(quads, 7, "background, bar, fill and four ticks");
        assert_eq!(vertices, quads * 6, "each quad is two triangles");
    }

    #[test]
    fn invalid_dt_is_ignored() {
        let mut mining = MiningState::default();
        let target = pos(1, 1, 1);
        mining.start(target, 1.0);
        mining.update(f32::NAN, Some(target));
        assert_eq!(mining.progress(), 0.0);
        mining.update(-1.0, Some(target));
        assert_eq!(mining.progress(), 0.0);
        mining.update(f32::INFINITY, Some(target));
        assert_eq!(mining.progress(), 0.0);
    }
}
