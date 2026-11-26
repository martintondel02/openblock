//! Server-authoritative fire: ignition, burn-out, deterministic spread, rain and
//! water extinguishing, entity damage and persistence.
//!
//! Fire is an ordinary block (`openblock:fire`, append-only id 29) plus a small
//! host-owned timer table. The block carries emission 15, a cross cut-out and no
//! collision; the timer table carries the remaining burn ticks that the block
//! palette cannot express (there are no block states in 1.5).
//!
//! # Model and caps
//!
//! * A fire cell burns out after [`FIRE_BURN_TICKS`] (160 ticks, 8 s).
//! * Every [`FIRE_SPREAD_INTERVAL_TICKS`] (40 ticks, four chances per cell) it
//!   attempts to spread to one six-neighbour **flammable** block with a
//!   [`FIRE_SPREAD_PERCENT`] (25%) roll. Spread consumes the flammable block by
//!   replacing it with fire (bounded, so a wooden house can burn down but the
//!   fire never grows faster than one cell per 40 ticks).
//! * At most [`FIRE_MAX_ACTIVE`] (64) fires may exist world-wide and at most
//!   [`FIRE_MAX_PER_CHUNK`] (8) per chunk; further ignitions/spreads fail.
//! * Rain or snow on an open-sky fire, or water in any of its six neighbours,
//!   extinguishes it immediately.
//! * Standing in fire deals [`FIRE_DAMAGE_PER_SECOND`] (1 HP/s) to players and
//!   mobs, applied once per [`FIRE_DAMAGE_INTERVAL_TICKS`] (20) ticks so armor
//!   wears at the documented one point per damage event rather than 20/s.
//! * The fire block emits block light 15 through the normal edit/relight path;
//!   placing or removing it updates stored light and queues a block edit.
//!
//! Rolls are `splitmix64(world_seed ^ FIRE_SPREAD_STREAM ^ position ^ remaining)`
//! and neighbours are tried in ascending `(x, y, z)` order, so spread is
//! independent of map iteration order, save/reload and wall-clock time.
//!
//! # Persistence
//!
//! The block id is saved in `world.obs` like any block; remaining burn ticks are
//! stored in the additive `fire.txt` sidecar:
//!
//! ```text
//! x y z remaining
//! ```
//!
//! Rows are sorted by `(x, y, z)`, missing files load as no fires and malformed
//! rows are skipped with a warning. A saved fire therefore resumes with its exact
//! remaining life and the state hash is save/reload stable. Unlike the 1.3 plan
//! ("fires are transient and cleared on load"), 1.5 deliberately persists burning
//! cells so a reload cannot leave an untimed, permanent fire block behind.

use std::collections::BTreeMap;
use std::path::Path;

use ob_content::{BlockId, Registry};
use ob_math::{Aabb, BlockPos, mix64};
use ob_world::World;

use crate::{Host, read_sidecar};

/// World-directory filename for the fire-timer sidecar.
pub(crate) const FIRES_FILE: &str = "fire.txt";
/// Ticks one fire cell burns before it burns out.
pub const FIRE_BURN_TICKS: u16 = 160;
/// Ticks between spread attempts.
pub const FIRE_SPREAD_INTERVAL_TICKS: u16 = 40;
/// Percentage chance that one spread attempt succeeds.
pub const FIRE_SPREAD_PERCENT: u64 = 25;
/// World-wide cap on active fire cells.
pub const FIRE_MAX_ACTIVE: usize = 64;
/// Per-chunk cap on active fire cells.
pub const FIRE_MAX_PER_CHUNK: usize = 8;
/// Fire damage per second while standing in a fire cell.
pub const FIRE_DAMAGE_PER_SECOND: f32 = 1.0;
/// Ticks between fire damage events (one second).
pub const FIRE_DAMAGE_INTERVAL_TICKS: u64 = 20;
/// Domain separator for fire spread rolls.
pub const FIRE_SPREAD_STREAM: u64 = 0x4649_5245_5F53_5052;

/// Authoritative timer for one burning block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FireCell {
    /// Block position of the fire.
    pub pos: BlockPos,
    /// Ticks left before the fire burns out (`1..=FIRE_BURN_TICKS`).
    pub remaining_ticks: u16,
}

impl FireCell {
    /// Creates a freshly ignited cell with a full burn timer.
    pub fn new(pos: BlockPos) -> Self {
        Self {
            pos,
            remaining_ticks: FIRE_BURN_TICKS,
        }
    }
}

/// Serialises `fire.txt` (`x y z remaining` per line, ascending position).
pub(crate) fn fires_state(fires: &BTreeMap<BlockPos, FireCell>) -> String {
    let mut text = String::new();
    for (pos, cell) in fires {
        text.push_str(&format!("{} {} {} {}\n", pos.x, pos.y, pos.z, cell.remaining_ticks));
    }
    text
}

/// Loads `fire.txt`, keeping only rows whose block is still fire.
pub(crate) fn load_fires(world_dir: &Path, world: &World) -> BTreeMap<BlockPos, FireCell> {
    let mut fires = BTreeMap::new();
    let path = world_dir.join(FIRES_FILE);
    let text = match read_sidecar(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return fires,
        Err(error) => {
            log::warn!("could not read {}: {error}; no fire timers restored", path.display());
            return fires;
        }
    };
    let fire = world.registry().id_or_air("openblock:fire");
    if fire == BlockId::AIR {
        return fires;
    }
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(x), Some(y), Some(z), Some(remaining)) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            log::warn!("{}:{}: malformed fire row", path.display(), index + 1);
            continue;
        };
        let (Ok(x), Ok(y), Ok(z)) = (x.parse::<i32>(), y.parse::<i32>(), z.parse::<i32>()) else {
            log::warn!("{}:{}: malformed fire position", path.display(), index + 1);
            continue;
        };
        let pos = BlockPos::new(x, y, z);
        if !pos.in_world() || world.get_block(pos) != fire {
            log::warn!(
                "{}:{}: dropping fire timer without a fire block at {pos:?}",
                path.display(),
                index + 1
            );
            continue;
        }
        let remaining = remaining
            .parse::<u16>()
            .ok()
            .map(|remaining| remaining.clamp(1, FIRE_BURN_TICKS))
            .unwrap_or(FIRE_BURN_TICKS);
        fires.insert(
            pos,
            FireCell {
                pos,
                remaining_ticks: remaining,
            },
        );
    }
    fires
}

/// Deterministic spread roll for one cell.
pub(crate) fn spread_roll_succeeds(seed: u64, pos: BlockPos, remaining_ticks: u16) -> bool {
    let hash = mix64(
        seed ^ FIRE_SPREAD_STREAM
            ^ mix64(pos.x as i64 as u64)
            ^ mix64((pos.y as i64 as u64).wrapping_mul(31))
            ^ mix64((pos.z as i64 as u64).wrapping_mul(131))
            ^ mix64(u64::from(remaining_ticks)),
    );
    hash % 100 < FIRE_SPREAD_PERCENT
}

/// Whether any of the six neighbours is water.
pub(crate) fn water_adjacent(world: &World, registry: &Registry, pos: BlockPos) -> bool {
    let water = registry.id_or_air("openblock:water");
    if water == BlockId::AIR {
        return false;
    }
    [
        pos.offset(-1, 0, 0),
        pos.offset(1, 0, 0),
        pos.offset(0, -1, 0),
        pos.offset(0, 1, 0),
        pos.offset(0, 0, -1),
        pos.offset(0, 0, 1),
    ]
    .into_iter()
    .any(|sample| sample.in_world() && world.get_block(sample) == water)
}

impl Host {
    /// Read-only active fire table.
    pub fn fires(&self) -> &BTreeMap<BlockPos, FireCell> {
        &self.fires
    }

    /// Number of active fire cells.
    pub fn fire_count(&self) -> usize {
        self.fires.len()
    }

    /// The timer for a fire at `pos`, if tracked.
    pub fn fire_cell(&self, pos: BlockPos) -> Option<FireCell> {
        self.fires.get(&pos).copied()
    }

    /// Whether `pos` currently holds a burning fire cell.
    pub fn is_burning(&self, pos: BlockPos) -> bool {
        self.fires.contains_key(&pos)
    }

    /// Places fire at `pos` when the block is air and both caps allow it.
    ///
    /// The block is a normal authoritative edit: light is relit through
    /// `World::set_block` and a `BlockEdit` is queued for the network layer.
    /// Returns `false` for a loaded-chunk/cap/occupancy rejection.
    pub fn ignite_fire(&mut self, pos: BlockPos) -> bool {
        if !self.can_place_fire(pos) {
            return false;
        }
        let fire = self.registry.id_or_air("openblock:fire");
        if !self.world.set_block(pos, fire) {
            return false;
        }
        self.fires.insert(pos, FireCell::new(pos));
        self.record_block_update(pos, fire);
        true
    }

    /// Removes fire at `pos` (block and timer). Returns whether anything burned.
    pub fn extinguish_fire(&mut self, pos: BlockPos) -> bool {
        let fire = self.registry.id_or_air("openblock:fire");
        let had_cell = self.fires.remove(&pos).is_some();
        let had_block = self.world.get_block(pos) == fire;
        if had_block && self.world.set_block(pos, BlockId::AIR) {
            self.record_block_update(pos, BlockId::AIR);
        }
        had_cell || had_block
    }

    /// Attempts one spread from `pos` **ignoring the 25% roll** (the cap and
    /// neighbour checks still apply).
    ///
    /// This is the deterministic admin/test seam for the spread path; normal
    /// gameplay uses the roll inside the private fire pass run by
    /// [`Host::tick`](crate::Host::tick).
    pub fn force_spread_fire(&mut self, pos: BlockPos) -> bool {
        self.spread_fire(pos, true)
    }

    /// Whether an ignition at `pos` would pass the occupancy and cap checks.
    fn can_place_fire(&self, pos: BlockPos) -> bool {
        if !pos.in_world() || self.world.chunk(pos.chunk()).is_none() {
            return false;
        }
        let fire = self.registry.id_or_air("openblock:fire");
        if fire == BlockId::AIR || self.world.get_block(pos) != BlockId::AIR {
            return false;
        }
        if self.fires.len() >= FIRE_MAX_ACTIVE {
            return false;
        }
        let chunk = pos.chunk();
        self.fires.keys().filter(|occupied| occupied.chunk() == chunk).count() < FIRE_MAX_PER_CHUNK
    }

    /// Tries to convert one flammable neighbour into fire.
    fn spread_fire(&mut self, pos: BlockPos, force: bool) -> bool {
        let Some(cell) = self.fires.get(&pos).copied() else {
            return false;
        };
        if !force && !spread_roll_succeeds(self.world.seed(), pos, cell.remaining_ticks) {
            return false;
        }
        let fire = self.registry.id_or_air("openblock:fire");
        if fire == BlockId::AIR {
            return false;
        }
        let neighbours = [
            pos.offset(0, 1, 0),
            pos.offset(0, -1, 0),
            pos.offset(-1, 0, 0),
            pos.offset(1, 0, 0),
            pos.offset(0, 0, -1),
            pos.offset(0, 0, 1),
        ];
        let mut candidates = Vec::new();
        for neighbour in neighbours {
            if !neighbour.in_world() || self.world.chunk(neighbour.chunk()).is_none() {
                continue;
            }
            let block = self.world.get_block(neighbour);
            if block != BlockId::AIR && self.registry.block(block).flammable && !self.fires.contains_key(&neighbour) {
                candidates.push(neighbour);
            }
        }
        if candidates.is_empty() {
            return false;
        }
        // The six-neighbour array order must not leak into the roll: sort the
        // candidates so the target selection is a pure function of the set.
        candidates.sort_unstable_by_key(|pos| (pos.x, pos.y, pos.z));
        let index = (mix64(
            self.world.seed()
                ^ FIRE_SPREAD_STREAM
                ^ mix64(cell.remaining_ticks as u64)
                ^ mix64(candidates.len() as u64),
        ) % candidates.len() as u64) as usize;
        let target = candidates[index];
        if !self.can_ignite_spread_target(target) {
            return false;
        }
        if !self.world.set_block(target, fire) {
            return false;
        }
        self.fires.insert(target, FireCell::new(target));
        self.record_block_update(target, fire);
        true
    }

    /// Cap/occupancy check for a spread target (the target still holds the
    /// flammable block, so only the caps can reject it).
    fn can_ignite_spread_target(&self, pos: BlockPos) -> bool {
        if self.fires.len() >= FIRE_MAX_ACTIVE {
            return false;
        }
        let chunk = pos.chunk();
        self.fires.keys().filter(|occupied| occupied.chunk() == chunk).count() < FIRE_MAX_PER_CHUNK
    }

    /// Removes a fire block/timer without re-checking it.
    fn remove_fire(&mut self, pos: BlockPos) {
        self.fires.remove(&pos);
        let fire = self.registry.id_or_air("openblock:fire");
        if self.world.get_block(pos) == fire && self.world.set_block(pos, BlockId::AIR) {
            self.record_block_update(pos, BlockId::AIR);
        }
    }

    /// Advances every fire cell by one tick: extinguishing, burn-out, damage and
    /// spread.
    ///
    /// A timer whose chunk has been streamed out is dropped: the block itself is
    /// gone with the unloaded chunk, so freezing the timer would only create an
    /// orphan row that a later save/reload would discard. The pass uses a copied,
    /// sorted key list, so removals and new spread cells do not affect the order
    /// of the current tick.
    pub(crate) fn tick_fires(&mut self) {
        if self.fires.is_empty() {
            return;
        }
        let fire = self.registry.id_or_air("openblock:fire");
        if fire == BlockId::AIR {
            self.fires.clear();
            return;
        }
        let positions: Vec<BlockPos> = self.fires.keys().copied().collect();
        let mut remove = Vec::new();
        let mut spread = Vec::new();
        for pos in positions {
            let Some(cell) = self.fires.get(&pos).copied() else {
                continue;
            };
            if !pos.in_world() || self.world.chunk(pos.chunk()).is_none() {
                // Streaming unload removes the block itself (unloaded chunk
                // edits are not persisted), so the timer has nothing left to
                // revive. Drop it with the block instead of retaining an orphan
                // timer that would disappear on the next save/reload anyway.
                remove.push(pos);
                continue;
            }
            if self.world.get_block(pos) != fire {
                remove.push(pos);
                continue;
            }
            let precipitation = self.precipitation_at_pos(pos);
            let open_sky = self.sky_exposed(pos);
            if (precipitation.is_some() && open_sky) || water_adjacent(&self.world, &self.registry, pos) {
                remove.push(pos);
                continue;
            }
            if cell.remaining_ticks <= 1 {
                remove.push(pos);
                continue;
            }
            // Spread attempts happen at 160/120/80/40 remaining ticks: four
            // chances per cell, matching the 40-tick cadence from ignition.
            if cell.remaining_ticks.is_multiple_of(FIRE_SPREAD_INTERVAL_TICKS) {
                spread.push(pos);
            }
            let remaining = cell.remaining_ticks - 1;
            if let Some(entry) = self.fires.get_mut(&pos) {
                entry.remaining_ticks = remaining;
            }
        }
        for pos in remove {
            self.remove_fire(pos);
        }
        if self.tick_count.is_multiple_of(FIRE_DAMAGE_INTERVAL_TICKS) {
            self.damage_entities_in_fire();
        }
        for pos in spread {
            self.spread_fire(pos, false);
        }
    }

    /// Applies one second of contact damage to players/mobs whose AABB overlaps
    /// a fire block.
    fn damage_entities_in_fire(&mut self) {
        let positions: Vec<BlockPos> = self.fires.keys().copied().collect();
        if positions.is_empty() {
            return;
        }
        if self.network_players.is_empty() {
            if self.game_mode == crate::GameMode::Survival
                && !self.player.dead
                && positions
                    .iter()
                    .any(|pos| Aabb::block(*pos).intersects(&self.player.aabb()))
            {
                self.damage_with_outcome(FIRE_DAMAGE_PER_SECOND);
            }
        } else {
            let ids: Vec<u64> = self.network_players.keys().copied().collect();
            for id in ids {
                let Some(player) = self.network_players.get(&id) else {
                    continue;
                };
                let aabb = Aabb::new(
                    ob_math::WorldVec::new(player.pos.x - 0.3, player.pos.y, player.pos.z - 0.3),
                    ob_math::WorldVec::new(player.pos.x + 0.3, player.pos.y + 1.8, player.pos.z + 0.3),
                );
                if positions.iter().any(|pos| Aabb::block(*pos).intersects(&aabb)) {
                    self.damage_network_player(id, FIRE_DAMAGE_PER_SECOND, "fire");
                }
            }
        }
        let mut dead = Vec::new();
        for entity in &mut self.mobs {
            let aabb = entity.mob.aabb(&self.registry);
            if positions.iter().any(|pos| Aabb::block(*pos).intersects(&aabb)) {
                entity.mob.health = (entity.mob.health - FIRE_DAMAGE_PER_SECOND).max(0.0);
                entity.mob.hurt_timer = ob_sim::MOB_HURT_FLEE_TIME;
                if entity.mob.health <= 0.0 {
                    dead.push(entity.id);
                }
            }
        }
        if !dead.is_empty() {
            self.mobs.retain(|entity| !dead.contains(&entity.id));
            for id in dead {
                self.network_entity_removals.push(crate::net_players::RemovedEntity {
                    id,
                    item: false,
                    reason: ob_protocol::DespawnReason::Despawned,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_defaults_and_state_round_trip_format_are_stable() {
        let cell = FireCell::new(BlockPos::new(3, 70, -4));
        assert_eq!(cell.remaining_ticks, 160);
        let mut fires = BTreeMap::new();
        fires.insert(cell.pos, cell);
        fires.insert(BlockPos::new(-1, 2, 3), FireCell::new(BlockPos::new(-1, 2, 3)));
        let text = fires_state(&fires);
        assert_eq!(text, "-1 2 3 160\n3 70 -4 160\n");
    }

    #[test]
    fn spread_roll_is_deterministic_and_roughly_a_quarter() {
        let pos = BlockPos::new(10, 64, -3);
        let mut hits = 0;
        for remaining in 1..=10_000u16 {
            if spread_roll_succeeds(42, pos, remaining) {
                hits += 1;
            }
        }
        for remaining in [1u16, 40, 80, 120] {
            assert_eq!(
                spread_roll_succeeds(42, pos, remaining),
                spread_roll_succeeds(42, pos, remaining)
            );
        }
        let share = f64::from(hits) / 10_000.0;
        assert!((0.20..=0.30).contains(&share), "spread share {share}");
    }
}
