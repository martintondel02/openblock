//! Deterministic server-side wheat farming: crop state, hydration and growth.
//!
//! A crop is a host-owned [`CropState`] keyed by its block position. The block
//! itself stays a plain `openblock:wheat_0..3` id, so an old client and the
//! world saver see ordinary blocks; the sidecar adds the age and hydration the
//! block palette cannot express (there are no block states in 1.2).
//!
//! # Timing
//!
//! * One growth stage takes [`CROP_GROWTH_DRY_TICKS`] (2,400 ticks = 2 min) on
//!   dry farmland and [`CROP_GROWTH_WET_TICKS`] (1,200 ticks = 1 min) when
//!   hydrated, exactly as specified by the 1.2 wave plan. A crop therefore
//!   reaches stage 3 in 8/4 minutes; stage 3 does not age further.
//! * Growth pauses when `World::gameplay_light` at the crop is below
//!   [`CROP_MIN_LIGHT`] (9), so a dark cellar farm stays at its current stage.
//! * Hydration is re-evaluated every [`CROP_HYDRATION_INTERVAL_TICKS`] ticks:
//!   water within [`CROP_HYDRATION_RADIUS`] blocks horizontally and ±1 Y. The
//!   farmland block below is swapped between `farmland_dry` and `farmland_wet`
//!   so the visual state follows the logic; the swap is a normal block update.
//!
//! All rolls are deterministic hashes of the world seed and position
//! (`mix64`), never wall-clock time or RNG state, so save/reload and replay
//! produce identical harvests.
//!
//! # Persistence
//!
//! `crops.txt` is a header-free sidecar with one line per crop, sorted by id:
//!
//! ```text
//! id x y z stage age hydrated
//! ```
//!
//! A missing file (a 1.0/1.1 world) loads as an empty crop set. Unknown or
//! malformed rows are skipped with a warning, and the block at a saved position
//! is authoritative for the stage so a hand-edited world cannot leave a crop
//! record pointing at air.

use std::collections::BTreeMap;
use std::path::Path;

use ob_content::{BlockId, ItemId, Registry};
use ob_math::{BlockPos, mix64};
use ob_world::World;

use crate::{MAX_SIDECAR_BYTES, read_sidecar};

/// World-directory filename for the crop sidecar.
pub(crate) const CROPS_FILE: &str = "crops.txt";
/// Ticks for one stage on dry farmland (2,400 = 2 minutes at 20 Hz).
pub const CROP_GROWTH_DRY_TICKS: u32 = 2_400;
/// Ticks for one stage on hydrated farmland (1,200 = 1 minute at 20 Hz).
pub const CROP_GROWTH_WET_TICKS: u32 = 1_200;
/// Minimum gameplay light required to grow.
pub const CROP_MIN_LIGHT: u8 = 9;
/// Horizontal water search radius for hydration, in blocks.
pub const CROP_HYDRATION_RADIUS: i32 = 4;
/// Hydration re-evaluation cadence, in ticks.
pub const CROP_HYDRATION_INTERVAL_TICKS: u64 = 20;
/// Minimum seeds returned by a mature harvest.
pub const CROP_HARVEST_SEEDS_MIN: u8 = 1;
/// Maximum seeds returned by a mature harvest.
pub const CROP_HARVEST_SEEDS_MAX: u8 = 3;
/// One-in-this-many chance that a broken grass block also drops seeds.
pub const GRASS_SEED_DROP_MODULUS: u64 = 8;

/// Authoritative state of one planted wheat crop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CropState {
    /// Stable session id, increasing within a save and used only for ordering.
    pub id: u64,
    /// Block position of the `wheat_*` block.
    pub pos: BlockPos,
    /// Growth stage `0..=3`, matching the block id suffix.
    pub stage: u8,
    /// Ticks accumulated toward the next stage.
    pub age: u32,
    /// Whether the farmland below was hydrated at the last check.
    pub hydrated: bool,
}

impl CropState {
    /// Creates a newly planted crop at stage 0.
    pub fn new(id: u64, pos: BlockPos) -> Self {
        Self {
            id,
            pos,
            stage: 0,
            age: 0,
            hydrated: false,
        }
    }

    /// Ticks one stage currently takes for this crop.
    pub fn stage_ticks(&self) -> u32 {
        if self.hydrated {
            CROP_GROWTH_WET_TICKS
        } else {
            CROP_GROWTH_DRY_TICKS
        }
    }
}

/// Returns the `openblock:wheat_<stage>` block id, or air for an invalid stage.
pub(crate) fn wheat_block(registry: &Registry, stage: u8) -> BlockId {
    match stage {
        0 => registry.id_or_air("openblock:wheat_0"),
        1 => registry.id_or_air("openblock:wheat_1"),
        2 => registry.id_or_air("openblock:wheat_2"),
        3 => registry.id_or_air("openblock:wheat_3"),
        _ => BlockId::AIR,
    }
}

/// Returns the stage of a `wheat_*` block, or `None` for any other block.
pub(crate) fn wheat_stage(registry: &Registry, block: BlockId) -> Option<u8> {
    (0..=3u8).find(|stage| block == wheat_block(registry, *stage))
}

/// Returns the `openblock:farmland_dry`/`_wet` block id for a hydration state.
pub(crate) fn farmland_block(registry: &Registry, hydrated: bool) -> BlockId {
    if hydrated {
        registry.id_or_air("openblock:farmland_wet")
    } else {
        registry.id_or_air("openblock:farmland_dry")
    }
}

/// Whether a block is one of the two farmland variants.
pub(crate) fn is_farmland(registry: &Registry, block: BlockId) -> bool {
    block == registry.id_or_air("openblock:farmland_dry") || block == registry.id_or_air("openblock:farmland_wet")
}

/// Whether water exists within the hydration radius and ±1 Y of a crop.
pub(crate) fn water_near(world: &World, registry: &Registry, pos: BlockPos) -> bool {
    let water = registry.id_or_air("openblock:water");
    if water == BlockId::AIR {
        return false;
    }
    for dy in -1..=1 {
        for dz in -CROP_HYDRATION_RADIUS..=CROP_HYDRATION_RADIUS {
            for dx in -CROP_HYDRATION_RADIUS..=CROP_HYDRATION_RADIUS {
                let sample = BlockPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
                if sample.in_world() && world.get_block(sample) == water {
                    return true;
                }
            }
        }
    }
    false
}

/// Advances every crop by one tick.
///
/// Returns the sorted block updates caused by growth or a dry/wet farmland
/// swap. Crops in unloaded columns are frozen (their block cannot be read
/// reliably); a crop whose block is no longer wheat is dropped from the map.
/// The hydration pass runs when `tick_count` is a multiple of
/// [`CROP_HYDRATION_INTERVAL_TICKS`].
pub(crate) fn tick_crops(
    world: &mut World,
    registry: &Registry,
    crops: &mut BTreeMap<BlockPos, CropState>,
    tick_count: u64,
    day_factor: f32,
    weather_factor: f32,
) -> Vec<(BlockPos, BlockId)> {
    let mut updates = Vec::new();
    let check_hydration = tick_count.is_multiple_of(CROP_HYDRATION_INTERVAL_TICKS);
    let mut removed = Vec::new();
    // Iterate in position order for deterministic update ordering.
    for (pos, crop) in crops.iter_mut() {
        if world.chunk(pos.chunk()).is_none() {
            continue;
        }
        let block = world.get_block(*pos);
        let Some(block_stage) = wheat_stage(registry, block) else {
            removed.push(*pos);
            continue;
        };
        if block_stage != crop.stage {
            // The world is authoritative after a direct edit; resync the record
            // so growth continues from the visible stage.
            crop.stage = block_stage;
            crop.age = 0;
        }
        if check_hydration {
            let hydrated = water_near(world, registry, *pos);
            if hydrated != crop.hydrated {
                crop.hydrated = hydrated;
                let below = pos.offset(0, -1, 0);
                let wanted = farmland_block(registry, hydrated);
                if is_farmland(registry, world.get_block(below))
                    && world.get_block(below) != wanted
                    && world.set_block(below, wanted)
                {
                    updates.push((below, wanted));
                }
            }
        }
        if crop.stage >= 3 {
            crop.age = 0;
            continue;
        }
        let light = world.gameplay_light(*pos, day_factor, weather_factor);
        if light < CROP_MIN_LIGHT {
            continue;
        }
        crop.age = crop.age.saturating_add(1);
        if crop.age >= crop.stage_ticks() {
            crop.stage = (crop.stage + 1).min(3);
            crop.age = 0;
            let next = wheat_block(registry, crop.stage);
            if next != BlockId::AIR && world.set_block(*pos, next) {
                updates.push((*pos, next));
            }
        }
    }
    for pos in removed {
        crops.remove(&pos);
    }
    updates
}

/// Deterministic harvest drops for one crop.
///
/// A stage-3 crop yields one wheat plus 1–3 seeds; an immature crop yields one
/// seed. The count is a hash of the world seed and block position, so a
/// save/reload never rerolls a harvest.
pub(crate) fn harvest_drops(registry: &Registry, seed: u64, pos: BlockPos, stage: u8) -> Vec<(ItemId, u8)> {
    let Some(seeds) = registry.item_id("openblock:seeds") else {
        return Vec::new();
    };
    if stage >= 3 {
        let Some(wheat) = registry.item_id("openblock:wheat") else {
            return Vec::new();
        };
        let roll = harvest_roll(seed, pos) % u64::from(CROP_HARVEST_SEEDS_MAX - CROP_HARVEST_SEEDS_MIN + 1);
        let count = CROP_HARVEST_SEEDS_MIN + roll as u8;
        vec![(wheat, 1), (seeds, count)]
    } else {
        vec![(seeds, 1)]
    }
}

/// Whether breaking grass at `pos` also drops one seed (12.5%).
pub(crate) fn grass_seed_drop(seed: u64, pos: BlockPos) -> bool {
    mix64(
        seed ^ (pos.x as i64 as u64).rotate_left(13)
            ^ (pos.y as i64 as u64).rotate_left(29)
            ^ (pos.z as i64 as u64).rotate_left(47)
            ^ 0x9E37_79B9_7F4A_7C15,
    )
    .is_multiple_of(GRASS_SEED_DROP_MODULUS)
}

/// One deterministic harvest roll for a position.
fn harvest_roll(seed: u64, pos: BlockPos) -> u64 {
    mix64(
        seed ^ (pos.x as i64 as u64).rotate_left(7)
            ^ (pos.y as i64 as u64).rotate_left(23)
            ^ (pos.z as i64 as u64).rotate_left(41)
            ^ 0xC0FF_EE00_1234_5678,
    )
}

/// Serializes the crop map to deterministic `crops.txt` rows, sorted by id.
pub(crate) fn crops_state(crops: &BTreeMap<BlockPos, CropState>) -> String {
    let mut sorted: Vec<&CropState> = crops.values().collect();
    sorted.sort_by_key(|crop| crop.id);
    let mut text = String::new();
    for crop in sorted {
        text.push_str(&format!(
            "{} {} {} {} {} {} {}\n",
            crop.id,
            crop.pos.x,
            crop.pos.y,
            crop.pos.z,
            crop.stage,
            crop.age,
            u8::from(crop.hydrated),
        ));
    }
    text
}

/// Loads `crops.txt`; a missing file means no crops (old save).
///
/// Rows are validated against the world: the chunk must be resident, the block
/// must be a `wheat_*` variant and the id must be unique. The visible block is
/// authoritative for the stage, so a stale or hand-edited sidecar cannot make
/// the host grow a crop that is not there.
pub(crate) fn load_crops(world_dir: &Path, registry: &Registry, world: &World) -> BTreeMap<BlockPos, CropState> {
    let path = world_dir.join(CROPS_FILE);
    let text = match read_sidecar(&path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("could not read {}: {error}; starting with no crops", path.display());
            }
            return BTreeMap::new();
        }
    };
    if text.len() as u64 > MAX_SIDECAR_BYTES {
        log::warn!("{} exceeds the sidecar limit; ignoring crops", path.display());
        return BTreeMap::new();
    }
    let mut crops = BTreeMap::new();
    let mut seen_ids = std::collections::BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 7 {
            log::warn!("crops.txt:{line_number}: ignoring malformed crop line");
            continue;
        }
        let parse = |field: &str| field.parse::<i64>().ok();
        let (Some(id), Some(x), Some(y), Some(z)) = (
            fields[0].parse::<u64>().ok(),
            parse(fields[1]),
            parse(fields[2]),
            parse(fields[3]),
        ) else {
            log::warn!("crops.txt:{line_number}: invalid crop fields");
            continue;
        };
        let (Ok(stage), Ok(age), Ok(hydrated)) = (
            fields[4].parse::<u8>(),
            fields[5].parse::<u32>(),
            fields[6].parse::<u8>(),
        ) else {
            log::warn!("crops.txt:{line_number}: invalid crop state");
            continue;
        };
        if id == 0 || !seen_ids.insert(id) {
            log::warn!("crops.txt:{line_number}: invalid or duplicate crop id {id}");
            continue;
        }
        if !(0..=3).contains(&stage) {
            log::warn!("crops.txt:{line_number}: crop stage {stage} out of range");
            continue;
        }
        let (Ok(x), Ok(y), Ok(z)) = (i32::try_from(x), i32::try_from(y), i32::try_from(z)) else {
            log::warn!("crops.txt:{line_number}: crop position out of range");
            continue;
        };
        let pos = BlockPos::new(x, y, z);
        if !pos.in_world() || world.chunk(pos.chunk()).is_none() {
            log::warn!("crops.txt:{line_number}: crop is outside the loaded world");
            continue;
        }
        let block = world.get_block(pos);
        let Some(block_stage) = wheat_stage(registry, block) else {
            log::warn!("crops.txt:{line_number}: no wheat block at the saved position");
            continue;
        };
        crops.insert(
            pos,
            CropState {
                id,
                pos,
                stage: block_stage,
                age,
                hydrated: hydrated != 0,
            },
        );
    }
    crops
}

/// Next id after the largest persisted crop id (at least 1).
pub(crate) fn next_crop_id(crops: &BTreeMap<BlockPos, CropState>) -> u64 {
    crops.values().map(|crop| crop.id).max().unwrap_or(0) + 1
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ob_math::ChunkPos;

    use super::*;

    #[test]
    fn hydration_and_growth_timings_are_exact() {
        let registry = Registry::base();
        let mut world = World::new(Arc::new(Registry::base()), 7);
        // Build one full chunk with a dirt floor at y=64.
        let dirt = registry.id_or_air("openblock:dirt");
        let mut column = ob_world::ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..16 {
            for x in 0..16 {
                column.set_block(BlockPos::new(x, 64, z), dirt);
            }
        }
        world.insert_chunk(column);
        let dry = farmland_block(&registry, false);
        let wet = farmland_block(&registry, true);
        let mut crops = BTreeMap::new();
        let pos = BlockPos::new(4, 65, 4);
        assert!(world.set_block(BlockPos::new(4, 64, 4), dry));
        assert!(world.set_block(pos, wheat_block(&registry, 0)));
        crops.insert(pos, CropState::new(1, pos));

        // Without water: 2,400 ticks per stage.
        for tick in 0..CROP_GROWTH_DRY_TICKS - 1 {
            let updates = tick_crops(&mut world, &registry, &mut crops, u64::from(tick), 1.0, 1.0);
            assert!(updates.is_empty());
        }
        let updates = tick_crops(
            &mut world,
            &registry,
            &mut crops,
            u64::from(CROP_GROWTH_DRY_TICKS - 1),
            1.0,
            1.0,
        );
        assert_eq!(updates, vec![(pos, wheat_block(&registry, 1))]);
        assert_eq!(crops[&pos].stage, 1);

        // Add water and wait one hydration interval, then the wet timing.
        let water = registry.id_or_air("openblock:water");
        assert!(world.set_block(BlockPos::new(7, 65, 4), water));
        let mut tick = u64::from(CROP_GROWTH_DRY_TICKS);
        while !crops[&pos].hydrated {
            tick_crops(&mut world, &registry, &mut crops, tick, 1.0, 1.0);
            tick += 1;
            assert!(tick < 3_000, "hydration must be observed within one interval");
        }
        assert_eq!(world.get_block(BlockPos::new(4, 64, 4)), wet);
        let age_at_hydration = crops[&pos].age;
        for _ in 0..CROP_GROWTH_WET_TICKS - age_at_hydration {
            tick_crops(&mut world, &registry, &mut crops, tick, 1.0, 1.0);
            tick += 1;
        }
        assert_eq!(crops[&pos].stage, 2);
    }

    #[test]
    fn growth_pauses_below_light_nine() {
        let registry = Registry::base();
        let mut world = World::new(Arc::new(Registry::base()), 8);
        let stone = registry.id_or_air("openblock:stone");
        let dirt = registry.id_or_air("openblock:dirt");
        let mut column = ob_world::ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..16 {
            for x in 0..16 {
                column.set_block(BlockPos::new(x, 64, z), dirt);
            }
        }
        // A roof at y=70 seals the crop from the sky.
        for z in 0..16 {
            for x in 0..16 {
                column.set_block(BlockPos::new(x, 70, z), stone);
            }
        }
        world.insert_chunk(column);
        let pos = BlockPos::new(4, 65, 4);
        assert!(world.set_block(pos, wheat_block(&registry, 1)));
        let mut crops = BTreeMap::from([(
            pos,
            CropState {
                id: 1,
                pos,
                stage: 1,
                age: 0,
                hydrated: false,
            },
        )]);
        for tick in 0..10_000 {
            tick_crops(&mut world, &registry, &mut crops, tick, 1.0, 1.0);
        }
        assert_eq!(crops[&pos].stage, 1, "a sealed cellar must pause growth");
        assert_eq!(crops[&pos].age, 0);
        // A torch lifts the light above the threshold.
        let torch = registry.id_or_air("openblock:torch");
        assert!(world.set_block(BlockPos::new(4, 66, 4), torch));
        for tick in 10_000..20_000 {
            tick_crops(&mut world, &registry, &mut crops, tick, 1.0, 1.0);
        }
        assert_eq!(crops[&pos].stage, 3, "torch light must resume growth");
    }

    #[test]
    fn harvest_rolls_are_deterministic_and_bounded() {
        let registry = Registry::base();
        let pos = BlockPos::new(10, 65, -3);
        let first = harvest_drops(&registry, 42, pos, 3);
        assert_eq!(first, harvest_drops(&registry, 42, pos, 3));
        assert_eq!(first[0].1, 1);
        assert!((1..=3).contains(&first[1].1));
        assert_eq!(
            harvest_drops(&registry, 42, pos, 1),
            vec![(registry.item_id("openblock:seeds").expect("seeds"), 1)]
        );
        let mut drops = 0;
        for x in 0..64 {
            if grass_seed_drop(42, BlockPos::new(x, 64, 0)) {
                drops += 1;
            }
        }
        assert!(drops > 0 && drops < 20, "12.5% over 64 samples, got {drops}");
    }

    #[test]
    fn crop_sidecar_round_trips_and_ignores_stale_rows() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("openblock-crops-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let registry = Registry::base();
        let mut world = World::new(Arc::new(Registry::base()), 9);
        let dirt = registry.id_or_air("openblock:dirt");
        let mut column = ob_world::ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..16 {
            for x in 0..16 {
                column.set_block(BlockPos::new(x, 64, z), dirt);
            }
        }
        world.insert_chunk(column);
        let pos = BlockPos::new(2, 65, 2);
        assert!(world.set_block(pos, wheat_block(&registry, 2)));
        let crops = BTreeMap::from([(
            pos,
            CropState {
                id: 4,
                pos,
                stage: 2,
                age: 123,
                hydrated: true,
            },
        )]);
        let text = crops_state(&crops);
        assert_eq!(text, "4 2 65 2 2 123 1\n");
        std::fs::write(dir.join(CROPS_FILE), format!("{text}bad row\n7 0 0 0 3 0 0\n")).expect("write");
        let loaded = load_crops(&dir, &registry, &world);
        assert_eq!(loaded.get(&pos).map(|crop| crop.age), Some(123));
        assert_eq!(loaded.len(), 1, "stale rows must be ignored");
        assert_eq!(next_crop_id(&loaded), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
