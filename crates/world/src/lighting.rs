//! Incremental light propagation and edit relighting.
//!
//! The stored field is the unique fixed point of a monotone max operator:
//! sky light seeds top-down per column, block light seeds from
//! [`ob_content::BlockType::emission`], and both propagate through a 16-bucket
//! 6-neighbour BFS. Sky steps cost `max(1, sky_attenuation)`, RGB block-light
//! steps cost `max(1, attenuation[channel])`, and a write happens only when the
//! new level strictly exceeds the stored one.
//!
//! Generation seeds each column locally ([`crate::ChunkColumn::finalise`]).
//! `reconcile_chunk_borders` then propagates across the 1-block border with
//! every resident neighbour, so a cave entrance or a torch straddling a chunk
//! boundary has no seam. Edits use `relight_edit`, a bounded 31³ box around
//! the changed block, with sky columns recomputed from the stored level just
//! above the box and block light re-seeded from emitters and from the box's
//! outside borders.

use std::collections::BTreeSet;

use ob_content::{BlockId, BlockType};
use ob_math::{BlockPos, ChunkPos, LocalPos, N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MAX_Y, WORLD_MIN_Y};

use crate::light::{unpack_blue, unpack_green, unpack_red, unpack_sky};
use crate::world::World;

/// Edit relight half-extent. A level-14 torch reaches 14 blocks, so a 31³ box
/// around the change contains every cell whose light can change.
pub const RELIGHT_RADIUS: i32 = 15;

/// Inclusive 3D box of block coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LightBounds {
    pub(crate) min: [i32; 3],
    pub(crate) max: [i32; 3],
}

impl LightBounds {
    pub(crate) fn new(center: BlockPos, radius: i32) -> Self {
        Self {
            min: [
                center.x - radius,
                (center.y - radius).max(WORLD_MIN_Y),
                center.z - radius,
            ],
            max: [
                center.x + radius,
                (center.y + radius).min(WORLD_MAX_Y - 1),
                center.z + radius,
            ],
        }
    }

    pub(crate) fn contains(self, pos: BlockPos) -> bool {
        pos.x >= self.min[0]
            && pos.x <= self.max[0]
            && pos.y >= self.min[1]
            && pos.y <= self.max[1]
            && pos.z >= self.min[2]
            && pos.z <= self.max[2]
    }
}

/// Six face neighbours of a block position.
fn neighbours6(pos: BlockPos) -> [BlockPos; 6] {
    [
        BlockPos::new(pos.x - 1, pos.y, pos.z),
        BlockPos::new(pos.x + 1, pos.y, pos.z),
        BlockPos::new(pos.x, pos.y - 1, pos.z),
        BlockPos::new(pos.x, pos.y + 1, pos.z),
        BlockPos::new(pos.x, pos.y, pos.z - 1),
        BlockPos::new(pos.x, pos.y, pos.z + 1),
    ]
}

/// Whether a position can participate in propagation: in the world and in a
/// loaded column.
fn accessible(world: &World, pos: BlockPos) -> bool {
    pos.in_world() && world.chunk(pos.chunk()).is_some()
}

/// Destination-cell sky attenuation, with the mandatory minimum lateral cost.
fn sky_cost(registry: &ob_content::Registry, block: BlockId) -> u8 {
    registry.block(block).sky_attenuation.max(1)
}

/// Destination-cell block-light attenuation for one channel.
fn block_cost(block: &BlockType, channel: usize) -> u8 {
    block.attenuation[channel].max(1)
}

/// Seeds and runs the sky-light max-BFS.
pub(crate) fn propagate_sky(
    world: &mut World,
    seeds: impl IntoIterator<Item = (BlockPos, u8)>,
    bounds: Option<LightBounds>,
    queue: &mut [Vec<(BlockPos, u8)>; 16],
) {
    for (pos, level) in seeds {
        seed_sky(world, pos, level, bounds, queue);
    }
    drain_sky(world, bounds, queue);
}

fn seed_sky(
    world: &mut World,
    pos: BlockPos,
    level: u8,
    bounds: Option<LightBounds>,
    queue: &mut [Vec<(BlockPos, u8)>; 16],
) {
    if level == 0 || !accessible(world, pos) {
        return;
    }
    let inside = bounds.is_none_or(|bounds| bounds.contains(pos));
    if inside {
        let current = world.sky_light(pos);
        if level > current {
            world.set_sky_light(pos, level);
        }
    }
    // Push even when the seed equals its stored value: a source on the bright
    // side of a border must still expand into the darker side.
    queue[level as usize].push((pos, level));
}

fn drain_sky(world: &mut World, bounds: Option<LightBounds>, queue: &mut [Vec<(BlockPos, u8)>; 16]) {
    for level in (1..16u8).rev() {
        while let Some((pos, queued)) = queue[level as usize].pop() {
            if queued < world.sky_light(pos) {
                continue;
            }
            for neighbour in neighbours6(pos) {
                if !accessible(world, neighbour) {
                    continue;
                }
                if let Some(bounds) = bounds
                    && !bounds.contains(neighbour)
                {
                    continue;
                }
                let cost = sky_cost(world.registry(), world.get_block(neighbour));
                let next = queued.saturating_sub(cost);
                if next == 0 || next <= world.sky_light(neighbour) {
                    continue;
                }
                world.set_sky_light(neighbour, next);
                queue[next as usize].push((neighbour, next));
            }
        }
    }
}

/// Seeds and runs the three-channel block-light max-BFS.
pub(crate) fn propagate_block(
    world: &mut World,
    seeds: impl IntoIterator<Item = (BlockPos, [u8; 3])>,
    bounds: Option<LightBounds>,
    queue: &mut [Vec<(BlockPos, [u8; 3])>; 16],
) {
    for (pos, levels) in seeds {
        seed_block(world, pos, levels, bounds, queue);
    }
    drain_block(world, bounds, queue);
}

fn seed_block(
    world: &mut World,
    pos: BlockPos,
    levels: [u8; 3],
    bounds: Option<LightBounds>,
    queue: &mut [Vec<(BlockPos, [u8; 3])>; 16],
) {
    let brightest = levels[0].max(levels[1]).max(levels[2]);
    if brightest == 0 || !accessible(world, pos) {
        return;
    }
    let inside = bounds.is_none_or(|bounds| bounds.contains(pos));
    if inside {
        let sample = world.light_sample(pos);
        let current = [sample.red, sample.green, sample.blue];
        if levels[0] > current[0] || levels[1] > current[1] || levels[2] > current[2] {
            world.set_block_light(pos, levels[0], levels[1], levels[2]);
        }
    }
    // As with sky, an equal seed still has to reach its darker neighbours.
    queue[brightest as usize].push((pos, levels));
}

fn drain_block(world: &mut World, bounds: Option<LightBounds>, queue: &mut [Vec<(BlockPos, [u8; 3])>; 16]) {
    for level in (1..16u8).rev() {
        while let Some((pos, levels)) = queue[level as usize].pop() {
            for neighbour in neighbours6(pos) {
                if !accessible(world, neighbour) {
                    continue;
                }
                if let Some(bounds) = bounds
                    && !bounds.contains(neighbour)
                {
                    continue;
                }
                let block = world.registry().block(world.get_block(neighbour));
                let mut next = [0u8; 3];
                let mut brightest = 0u8;
                for channel in 0..3 {
                    next[channel] = levels[channel].saturating_sub(block_cost(block, channel));
                    brightest = brightest.max(next[channel]);
                }
                if brightest == 0 {
                    continue;
                }
                let sample = world.light_sample(neighbour);
                let current = [sample.red, sample.green, sample.blue];
                if next[0] <= current[0] && next[1] <= current[1] && next[2] <= current[2] {
                    continue;
                }
                let merged = [
                    current[0].max(next[0]),
                    current[1].max(next[1]),
                    current[2].max(next[2]),
                ];
                world.set_block_light(neighbour, merged[0], merged[1], merged[2]);
                queue[brightest as usize].push((neighbour, merged));
            }
        }
    }
}

/// Collects cells in a box that are brighter than a 6-neighbour and therefore
/// can spread sky light. Uniform all-15/all-0 sections cannot contain an
/// internal cliff, so they are skipped without touching 4,096 cells.
fn sky_cliff_seeds(world: &World, bounds: LightBounds) -> Vec<(BlockPos, u8)> {
    let mut seeds = Vec::new();
    let min_cx = bounds.min[0].div_euclid(SECTION_SIZE as i32);
    let max_cx = bounds.max[0].div_euclid(SECTION_SIZE as i32);
    let min_cz = bounds.min[2].div_euclid(SECTION_SIZE as i32);
    let max_cz = bounds.max[2].div_euclid(SECTION_SIZE as i32);
    let min_sy = bounds.min[1].div_euclid(SECTION_SIZE as i32);
    let max_sy = bounds.max[1].div_euclid(SECTION_SIZE as i32);
    for cz in min_cz..=max_cz {
        for cx in min_cx..=max_cx {
            for sy in min_sy..=max_sy {
                let Some(section) = world.section(SectionPos::new(cx, sy, cz)) else {
                    continue;
                };
                if let Some(word) = section.light().uniform_value() {
                    let sky = unpack_sky(word);
                    if sky == 0 || sky == 15 {
                        continue;
                    }
                }
                let origin_x = cx * SECTION_SIZE as i32;
                let origin_z = cz * SECTION_SIZE as i32;
                let origin_y = sy * SECTION_SIZE as i32;
                for y in (bounds.min[1].max(origin_y))..=(bounds.max[1].min(origin_y + SECTION_SIZE as i32 - 1)) {
                    for z in (bounds.min[2].max(origin_z))..=(bounds.max[2].min(origin_z + SECTION_SIZE as i32 - 1)) {
                        for x in (bounds.min[0].max(origin_x))..=(bounds.max[0].min(origin_x + SECTION_SIZE as i32 - 1))
                        {
                            let pos = BlockPos::new(x, y, z);
                            let sky = world.sky_light(pos);
                            if sky < 2 {
                                continue;
                            }
                            if neighbours6(pos)
                                .into_iter()
                                .filter(|neighbour| accessible(world, *neighbour) && bounds.contains(*neighbour))
                                .any(|neighbour| world.sky_light(neighbour) + 1 < sky)
                            {
                                seeds.push((pos, sky));
                            }
                        }
                    }
                }
            }
        }
    }
    seeds
}

/// Seeds block light from every emitter inside a box.
///
/// Only sections whose palette contains an emissive block are scanned; almost
/// every generated section is plain stone, air or water.
fn seed_box_block_sources(world: &mut World, bounds: LightBounds) -> Vec<(BlockPos, [u8; 3])> {
    let mut seeds = Vec::new();
    let registry = world.registry().clone();
    let min_cx = bounds.min[0].div_euclid(SECTION_SIZE as i32);
    let max_cx = bounds.max[0].div_euclid(SECTION_SIZE as i32);
    let min_cz = bounds.min[2].div_euclid(SECTION_SIZE as i32);
    let max_cz = bounds.max[2].div_euclid(SECTION_SIZE as i32);
    let min_sy = bounds.min[1].div_euclid(SECTION_SIZE as i32);
    let max_sy = bounds.max[1].div_euclid(SECTION_SIZE as i32);
    for cz in min_cz..=max_cz {
        for cx in min_cx..=max_cx {
            for sy in min_sy..=max_sy {
                let Some(section) = world.section(SectionPos::new(cx, sy, cz)) else {
                    continue;
                };
                if !section.may_have_emitters(&registry) {
                    continue;
                }
                let origin_x = cx * SECTION_SIZE as i32;
                let origin_z = cz * SECTION_SIZE as i32;
                let origin_y = sy * SECTION_SIZE as i32;
                for y in (bounds.min[1].max(origin_y))..=(bounds.max[1].min(origin_y + SECTION_SIZE as i32 - 1)) {
                    for z in (bounds.min[2].max(origin_z))..=(bounds.max[2].min(origin_z + SECTION_SIZE as i32 - 1)) {
                        for x in (bounds.min[0].max(origin_x))..=(bounds.max[0].min(origin_x + SECTION_SIZE as i32 - 1))
                        {
                            let local = LocalPos {
                                x: (x - origin_x) as u8,
                                y: (y - origin_y) as u8,
                                z: (z - origin_z) as u8,
                            };
                            let emission = registry.block(section.get(local)).emission;
                            if emission != [0, 0, 0] {
                                seeds.push((BlockPos::new(x, y, z), emission));
                            }
                        }
                    }
                }
            }
        }
    }
    seeds
}

/// Seeds sky light from the six outside faces of a box.
fn box_border_sky_seeds(world: &World, bounds: LightBounds) -> Vec<(BlockPos, u8)> {
    let mut seeds = Vec::new();
    for x in bounds.min[0]..=bounds.max[0] {
        for z in bounds.min[2]..=bounds.max[2] {
            for y in [bounds.min[1] - 1, bounds.max[1] + 1] {
                let pos = BlockPos::new(x, y, z);
                if accessible(world, pos) {
                    seeds.push((pos, world.sky_light(pos)));
                }
            }
        }
    }
    for z in bounds.min[2]..=bounds.max[2] {
        for y in bounds.min[1]..=bounds.max[1] {
            for x in [bounds.min[0] - 1, bounds.max[0] + 1] {
                let pos = BlockPos::new(x, y, z);
                if accessible(world, pos) {
                    seeds.push((pos, world.sky_light(pos)));
                }
            }
        }
    }
    for x in bounds.min[0]..=bounds.max[0] {
        for y in bounds.min[1]..=bounds.max[1] {
            for z in [bounds.min[2] - 1, bounds.max[2] + 1] {
                let pos = BlockPos::new(x, y, z);
                if accessible(world, pos) {
                    seeds.push((pos, world.sky_light(pos)));
                }
            }
        }
    }
    seeds
}

/// Seeds block light from the six outside faces of a box.
fn box_border_block_seeds(world: &World, bounds: LightBounds) -> Vec<(BlockPos, [u8; 3])> {
    let mut seeds = Vec::new();
    let mut push = |pos: BlockPos| {
        if accessible(world, pos) {
            let sample = world.light_sample(pos);
            let levels = [sample.red, sample.green, sample.blue];
            if levels != [0, 0, 0] {
                seeds.push((pos, levels));
            }
        }
    };
    for x in bounds.min[0]..=bounds.max[0] {
        for z in bounds.min[2]..=bounds.max[2] {
            for y in [bounds.min[1] - 1, bounds.max[1] + 1] {
                push(BlockPos::new(x, y, z));
            }
        }
    }
    for z in bounds.min[2]..=bounds.max[2] {
        for y in bounds.min[1]..=bounds.max[1] {
            for x in [bounds.min[0] - 1, bounds.max[0] + 1] {
                push(BlockPos::new(x, y, z));
            }
        }
    }
    for x in bounds.min[0]..=bounds.max[0] {
        for y in bounds.min[1]..=bounds.max[1] {
            for z in [bounds.min[2] - 1, bounds.max[2] + 1] {
                push(BlockPos::new(x, y, z));
            }
        }
    }
    seeds
}

/// Seeds block light from every emitter inside a box and clears the box first.
/// Relights a bounded box around one edit.
///
/// `sky` and `block` select which channels can have changed. A torch or wheat
/// edit touches only block light, so the expensive sky column pass is skipped;
/// placing/removing stone or water touches both. The box is always the 31³
/// [`RELIGHT_RADIUS`] volume specified for 1.2, which contains every cell a
/// level-14 emitter can reach.
pub(crate) fn relight_edit(world: &mut World, center: BlockPos, sky: bool, block: bool) {
    let bounds = LightBounds::new(center, RELIGHT_RADIUS);
    if sky {
        // The 31³ box covers every lateral effect of the edit. A sky-opacity
        // change can also darken or brighten a cave far below the box through
        // the edited column. Detect that by comparing the bottom plane of the
        // box before and after the box pass; if it changed, the volume below
        // the box is rebuilt too. Without this, a sealed shaft would leave the
        // cave below the box stale-bright (max-only propagation cannot remove).
        let old_bottom = bottom_sky_row(world, bounds);
        relight_sky_volume(world, bounds);
        if !old_bottom.is_empty() && bottom_sky_row_changed(world, bounds, &old_bottom) {
            let volume = LightBounds {
                min: [bounds.min[0], WORLD_MIN_Y, bounds.min[2]],
                max: [bounds.max[0], bounds.max[1], bounds.max[2]],
            };
            relight_sky_volume(world, volume);
        }
    }

    if block {
        let mut block_seeds = seed_box_block_sources(world, bounds);
        let mut border_seeds = box_border_block_seeds(world, bounds);
        // A dark box with no emitters and no lit border cannot change. Skipping
        // the block pass makes placing/removing stone in an unlit cave cheap;
        // the torch/emitter case is still detected by `block_seeds`.
        if !block_seeds.is_empty() || !border_seeds.is_empty() || world.box_has_block_light(bounds) {
            block_seeds.append(&mut border_seeds);
            world.clear_light_box(bounds, false, true);
            let mut block_queue: [Vec<(BlockPos, [u8; 3])>; 16] = std::array::from_fn(|_| Vec::new());
            propagate_block(world, block_seeds, Some(bounds), &mut block_queue);
        }
    }

    world.invalidate_light_hash();
}

/// Rebuilds sky light over `bounds` and runs the bounded max-BFS.
///
/// Writing every cell from the stored level just above the bounds (instead of
/// clearing and then adding) removes stale lateral light, which is what makes
/// a sealed shaft or a removed roof actually darken.
fn relight_sky_volume(world: &mut World, bounds: LightBounds) {
    world.rebuild_sky_box(bounds);
    let mut seeds = sky_cliff_seeds(world, bounds);
    seeds.extend(box_border_sky_seeds(world, bounds));
    let mut queue: [Vec<(BlockPos, u8)>; 16] = std::array::from_fn(|_| Vec::new());
    propagate_sky(world, seeds, Some(bounds), &mut queue);
}

/// Snapshot the bottom plane of a sky relight box (empty below world min).
fn bottom_sky_row(world: &World, bounds: LightBounds) -> Vec<u8> {
    if bounds.min[1] <= WORLD_MIN_Y {
        return Vec::new();
    }
    let mut row =
        Vec::with_capacity(((bounds.max[0] - bounds.min[0] + 1) * (bounds.max[2] - bounds.min[2] + 1)) as usize);
    for z in bounds.min[2]..=bounds.max[2] {
        for x in bounds.min[0]..=bounds.max[0] {
            row.push(world.sky_light(BlockPos::new(x, bounds.min[1], z)));
        }
    }
    row
}

/// Whether the bottom plane changed after a box relight.
fn bottom_sky_row_changed(world: &World, bounds: LightBounds, old: &[u8]) -> bool {
    let mut index = 0;
    for z in bounds.min[2]..=bounds.max[2] {
        for x in bounds.min[0]..=bounds.max[0] {
            if world.sky_light(BlockPos::new(x, bounds.min[1], z)) != old[index] {
                return true;
            }
            index += 1;
        }
    }
    false
}

/// Reconciles both sides of a newly inserted chunk with every resident
/// horizontal neighbour. The BFS is bounded to the chunk plus its 15-block
/// neighbourhood, which contains every cell whose light can change.
pub(crate) fn reconcile_chunk_borders(world: &mut World, pos: ChunkPos) {
    let (origin_x, origin_z) = pos.origin();
    let mut seeds_sky = Vec::new();
    let mut seeds_block = Vec::new();
    let mut min_y = WORLD_MAX_Y;
    let mut max_y = WORLD_MIN_Y;
    let mut any = false;

    let section_base_y = WORLD_MIN_Y / SECTION_SIZE as i32;
    for direction in 0..4 {
        let (a_chunk, b_chunk) = match direction {
            0 => (ChunkPos::new(pos.x - 1, pos.z), pos),
            1 => (ChunkPos::new(pos.x + 1, pos.z), pos),
            2 => (ChunkPos::new(pos.x, pos.z - 1), pos),
            _ => (ChunkPos::new(pos.x, pos.z + 1), pos),
        };
        let Some(a_column) = world.chunk(a_chunk) else {
            continue;
        };
        let Some(b_column) = world.chunk(b_chunk) else {
            continue;
        };
        for section_index in 0..N_SECTIONS {
            let sy = section_base_y + section_index as i32;
            let Some(section_a) = a_column.section(SectionPos::new(a_chunk.x, sy, a_chunk.z)) else {
                continue;
            };
            let Some(section_b) = b_column.section(SectionPos::new(b_chunk.x, sy, b_chunk.z)) else {
                continue;
            };
            let uniform_a = section_a.light().uniform_value();
            let uniform_b = section_b.light().uniform_value();
            if let (Some(wa), Some(wb)) = (uniform_a, uniform_b) {
                if wa == wb {
                    continue;
                }
                // Two uniform sections with different words differ along the
                // whole 16-cell edge, not just at one cell: seed every pair so
                // lateral propagation cannot leave most of the border at the
                // stale level. The whole section participates in the bounds
                // because the new light may spread vertically inside it.
                let section_min_y = sy * SECTION_SIZE as i32;
                let section_max_y = section_min_y + SECTION_SIZE as i32 - 1;
                any = true;
                min_y = min_y.min(section_min_y);
                max_y = max_y.max(section_max_y);
                for local_y in 0..SECTION_SIZE as i32 {
                    for i in 0..SECTION_SIZE as i32 {
                        let (a, b) = border_cell(direction, origin_x, origin_z, section_min_y + local_y, i);
                        push_pair_seeds(a, b, wa, wb, &mut seeds_sky, &mut seeds_block);
                    }
                }
                continue;
            }
            for local_y in 0..SECTION_SIZE as i32 {
                for i in 0..SECTION_SIZE as i32 {
                    let (local_a, local_b) = border_local(direction, local_y as u8, i as u8);
                    let wa = section_a.light().get(local_a);
                    let wb = section_b.light().get(local_b);
                    if wa == wb {
                        continue;
                    }
                    let world_y = sy * SECTION_SIZE as i32 + local_y;
                    let (a, b) = border_cell(direction, origin_x, origin_z, world_y, i);
                    any = true;
                    min_y = min_y.min(world_y);
                    max_y = max_y.max(world_y);
                    push_pair_seeds(a, b, wa, wb, &mut seeds_sky, &mut seeds_block);
                }
            }
        }
    }

    if !any {
        return;
    }
    let bounds = LightBounds {
        min: [
            origin_x - RELIGHT_RADIUS,
            (min_y - RELIGHT_RADIUS).max(WORLD_MIN_Y),
            origin_z - RELIGHT_RADIUS,
        ],
        max: [
            origin_x + SECTION_SIZE as i32 + RELIGHT_RADIUS,
            (max_y + RELIGHT_RADIUS).min(WORLD_MAX_Y - 1),
            origin_z + SECTION_SIZE as i32 + RELIGHT_RADIUS,
        ],
    };
    let mut sky_queue: [Vec<(BlockPos, u8)>; 16] = std::array::from_fn(|_| Vec::new());
    propagate_sky(world, seeds_sky, Some(bounds), &mut sky_queue);
    let mut block_queue: [Vec<(BlockPos, [u8; 3])>; 16] = std::array::from_fn(|_| Vec::new());
    propagate_block(world, seeds_block, Some(bounds), &mut block_queue);
    world.invalidate_light_hash();
}

/// Section-local positions of the two cells either side of a chunk border.
fn border_local(direction: usize, y: u8, i: u8) -> (LocalPos, LocalPos) {
    let edge = SECTION_SIZE as u8 - 1;
    match direction {
        0 => (LocalPos { x: edge, y, z: i }, LocalPos { x: 0, y, z: i }),
        1 => (LocalPos { x: 0, y, z: i }, LocalPos { x: edge, y, z: i }),
        2 => (LocalPos { x: i, y, z: edge }, LocalPos { x: i, y, z: 0 }),
        _ => (LocalPos { x: i, y, z: 0 }, LocalPos { x: i, y, z: edge }),
    }
}

/// One pair of adjacent border cells for a chunk direction.
///
/// `direction` is 0 = west, 1 = east, 2 = north, 3 = south; `i` indexes the
/// cells along the border.
fn border_cell(direction: usize, origin_x: i32, origin_z: i32, y: i32, i: i32) -> (BlockPos, BlockPos) {
    let edge = SECTION_SIZE as i32;
    match direction {
        0 => (
            BlockPos::new(origin_x - 1, y, origin_z + i),
            BlockPos::new(origin_x, y, origin_z + i),
        ),
        1 => (
            BlockPos::new(origin_x + edge - 1, y, origin_z + i),
            BlockPos::new(origin_x + edge, y, origin_z + i),
        ),
        2 => (
            BlockPos::new(origin_x + i, y, origin_z - 1),
            BlockPos::new(origin_x + i, y, origin_z),
        ),
        _ => (
            BlockPos::new(origin_x + i, y, origin_z + edge - 1),
            BlockPos::new(origin_x + i, y, origin_z + edge),
        ),
    }
}

/// Pushes every non-zero channel of a differing border pair.
fn push_pair_seeds(
    a: BlockPos,
    b: BlockPos,
    wa: u16,
    wb: u16,
    seeds_sky: &mut Vec<(BlockPos, u8)>,
    seeds_block: &mut Vec<(BlockPos, [u8; 3])>,
) {
    for (pos, word) in [(a, wa), (b, wb)] {
        let sky = unpack_sky(word);
        if sky > 0 {
            seeds_sky.push((pos, sky));
        }
        let levels = [unpack_red(word), unpack_green(word), unpack_blue(word)];
        if levels != [0; 3] {
            seeds_block.push((pos, levels));
        }
    }
}

/// Full relight for a loaded world: reseed every column, then reconcile every
/// chunk border once. Used by the 1.0/1.1 save migration and by debug tooling.
pub(crate) fn relight_all(world: &mut World) {
    let mut positions: Vec<ChunkPos> = world.chunks().map(|(pos, _)| *pos).collect();
    positions.sort_by_key(|pos| (pos.x, pos.z));
    let registry = world.registry().clone();
    for pos in positions {
        if let Some(column) = world.chunk_mut(pos) {
            column.finalise(&registry);
        }
    }

    let mut sky_seeds = Vec::new();
    let mut block_seeds = Vec::new();
    let mut seen: BTreeSet<(i32, i32)> = BTreeSet::new();
    for (pos, _column) in world.chunks().map(|(pos, column)| (*pos, column)) {
        let (origin_x, origin_z) = pos.origin();
        for (dx, dz) in [(1, 0), (0, 1)] {
            let neighbour = ChunkPos::new(pos.x + dx, pos.z + dz);
            if world.chunk(neighbour).is_none() || !seen.insert((pos.x * 2 + dx, pos.z * 2 + dz)) {
                continue;
            }
            for y in WORLD_MIN_Y..WORLD_MAX_Y {
                for i in 0..SECTION_SIZE as i32 {
                    let (a, b) = if dx == 1 {
                        (
                            BlockPos::new(origin_x + SECTION_SIZE as i32 - 1, y, origin_z + i),
                            BlockPos::new(origin_x + SECTION_SIZE as i32, y, origin_z + i),
                        )
                    } else {
                        (
                            BlockPos::new(origin_x + i, y, origin_z + SECTION_SIZE as i32 - 1),
                            BlockPos::new(origin_x + i, y, origin_z + SECTION_SIZE as i32),
                        )
                    };
                    let wa = world.light_word(a);
                    let wb = world.light_word(b);
                    if wa == wb {
                        continue;
                    }
                    for (pos, word) in [(a, wa), (b, wb)] {
                        let sky = unpack_sky(word);
                        if sky > 0 {
                            sky_seeds.push((pos, sky));
                        }
                        let levels = [unpack_red(word), unpack_green(word), unpack_blue(word)];
                        if levels != [0; 3] {
                            block_seeds.push((pos, levels));
                        }
                    }
                }
            }
        }
    }
    let mut sky_queue: [Vec<(BlockPos, u8)>; 16] = std::array::from_fn(|_| Vec::new());
    propagate_sky(world, sky_seeds, None, &mut sky_queue);
    let mut block_queue: [Vec<(BlockPos, [u8; 3])>; 16] = std::array::from_fn(|_| Vec::new());
    propagate_block(world, block_seeds, None, &mut block_queue);
    world.invalidate_light_hash();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ob_content::{BlockId, Registry};
    use ob_math::{BlockPos, ChunkPos, N_SECTIONS, SectionPos, WORLD_MIN_Y};

    use super::*;
    use crate::ChunkColumn;

    fn registry() -> Arc<Registry> {
        Arc::new(Registry::base())
    }

    fn block(registry: &Registry, name: &str) -> BlockId {
        let id = registry.id(name).expect("base block");
        assert_ne!(id, BlockId::AIR);
        id
    }

    /// One fully solid 16×16 stone plinth from y=50 through y=69 with an air
    /// room carved from (4,64,4)..=(10,66,10). Nothing above the plinth, so the
    /// room is sealed by the roof at y=69.
    fn sealed_room(registry: &Arc<Registry>) -> (World, BlockPos) {
        let stone = block(registry, "openblock:stone");
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for y in 50..70 {
            for z in 0..16 {
                for x in 0..16 {
                    column.set_block(BlockPos::new(x, y, z), stone);
                }
            }
        }
        for y in 64..=66 {
            for z in 4..=10 {
                for x in 4..=10 {
                    column.set_block(BlockPos::new(x, y, z), BlockId::AIR);
                }
            }
        }
        let mut world = World::new(Arc::clone(registry), 1);
        world.insert_chunk(column);
        (world, BlockPos::new(7, 65, 7))
    }

    /// Flat stone floor at y=64 over two chunks with open air above.
    fn flat_world(registry: &Arc<Registry>, chunk_count: i32) -> World {
        let stone = block(registry, "openblock:stone");
        let mut world = World::new(Arc::clone(registry), 2);
        for cx in 0..chunk_count {
            let mut column = ChunkColumn::new(ChunkPos::new(cx, 0));
            for z in 0..16 {
                for x in 0..16 {
                    column.set_block(BlockPos::new(cx * 16 + x, 64, z), stone);
                }
            }
            world.insert_chunk(column);
        }
        world
    }

    /// Solid rock from y=40..=70 with a room at y=50..=53 reached by a
    /// one-block shaft at (8,8) from the sky. The room is below the 31³ relight
    /// box around the shaft opening, so it exercises the deep sky-tail path.
    fn deep_shaft_cave(registry: &Arc<Registry>) -> (World, BlockPos) {
        let stone = block(registry, "openblock:stone");
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for y in 40..=70 {
            for z in 0..16 {
                for x in 0..16 {
                    column.set_block(BlockPos::new(x, y, z), stone);
                }
            }
        }
        for y in 50..=53 {
            for z in 4..=11 {
                for x in 4..=11 {
                    column.set_block(BlockPos::new(x, y, z), BlockId::AIR);
                }
            }
        }
        for y in 54..=70 {
            column.set_block(BlockPos::new(8, y, 8), BlockId::AIR);
        }
        let mut world = World::new(Arc::clone(registry), 9);
        world.insert_chunk(column);
        (world, BlockPos::new(8, 70, 8))
    }

    #[test]
    fn sealed_room_is_dark_and_torch_relights_it() {
        let registry = registry();
        let (mut world, torch_pos) = sealed_room(&registry);
        assert_eq!(world.sky_light(torch_pos), 0, "sealed room has no sky light");
        assert_eq!(world.block_light(torch_pos), 0);

        let torch = block(&registry, "openblock:torch");
        assert!(world.set_block(torch_pos, torch));
        assert_eq!(world.block_light(torch_pos), 14, "torch seeds level 14");
        assert_eq!(world.block_light(BlockPos::new(8, 65, 7)), 13, "one step loses one");
        assert_eq!(
            world.block_light(BlockPos::new(10, 65, 7)),
            11,
            "three steps lose three"
        );
        assert_eq!(
            world.gameplay_light(torch_pos, 0.0, 1.0),
            14,
            "at night a torch is the only light"
        );

        // A wall between the torch and a cell casts a shadow and removing it
        // restores the light; a removal must not leave residual light behind.
        let stone = block(&registry, "openblock:stone");
        for y in 64..=66 {
            for z in 4..=10 {
                assert!(world.set_block(BlockPos::new(8, y, z), stone));
            }
        }
        assert_eq!(
            world.block_light(BlockPos::new(10, 65, 7)),
            0,
            "wall blocks torch light"
        );
        for y in 64..=66 {
            for z in 4..=10 {
                assert!(world.set_block(BlockPos::new(8, y, z), BlockId::AIR));
            }
        }
        assert_eq!(
            world.block_light(BlockPos::new(10, 65, 7)),
            11,
            "removing the wall restores the fixed point"
        );

        // Breaking the torch darkens every cell it lit.
        assert!(world.set_block(torch_pos, BlockId::AIR));
        assert_eq!(world.block_light(torch_pos), 0);
        assert_eq!(world.block_light(BlockPos::new(8, 65, 7)), 0);
        assert_eq!(world.block_light(BlockPos::new(10, 65, 7)), 0);
    }

    #[test]
    fn sky_columns_light_a_shaft_and_shade_under_a_roof() {
        let registry = registry();
        let stone = block(&registry, "openblock:stone");
        let water = block(&registry, "openblock:water");
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..16 {
            for x in 0..16 {
                column.set_block(BlockPos::new(x, 64, z), stone);
                // A roof at y=70 with a hole at x=8,z=8 for the shaft and a
                // second at x=2,z=2 for the water attenuation column.
                if !(x == 8 && z == 8) && !(x == 2 && z == 2) {
                    column.set_block(BlockPos::new(x, 70, z), stone);
                }
            }
        }
        // A three-block water column under the open sky proves the running
        // subtraction (15 -> 14 -> 13 -> 12), not a per-cell reset.
        for y in 65..68 {
            column.set_block(BlockPos::new(2, y, 2), water);
        }
        let mut world = World::new(Arc::clone(&registry), 3);
        world.insert_chunk(column);

        assert_eq!(world.sky_light(BlockPos::new(8, 70, 8)), 15, "hole sees the sky");
        assert_eq!(world.sky_light(BlockPos::new(8, 69, 8)), 15, "shaft carries 15 down");
        assert_eq!(world.sky_light(BlockPos::new(8, 65, 8)), 15);
        assert_eq!(
            world.sky_light(BlockPos::new(8, 50, 8)),
            0,
            "below opaque ground is dark"
        );
        assert_eq!(
            world.sky_light(BlockPos::new(9, 69, 8)),
            14,
            "light spreads one step sideways"
        );
        assert_eq!(world.sky_light(BlockPos::new(10, 69, 8)), 13);
        assert_eq!(
            world.sky_light(BlockPos::new(0, 50, 0)),
            0,
            "below opaque ground stays dark"
        );

        assert_eq!(
            world.sky_light(BlockPos::new(2, 67, 2)),
            14,
            "first water cell loses one"
        );
        assert_eq!(world.sky_light(BlockPos::new(2, 66, 2)), 13);
        assert_eq!(world.sky_light(BlockPos::new(2, 65, 2)), 12);
    }

    #[test]
    fn block_light_attenuates_through_water_and_crosses_chunk_borders() {
        let registry = registry();
        let water = block(&registry, "openblock:water");
        let torch = block(&registry, "openblock:torch");
        let mut world = flat_world(&registry, 2);
        for x in 9..=11 {
            assert!(world.set_block(BlockPos::new(x, 65, 8), water));
        }
        assert!(world.set_block(BlockPos::new(8, 65, 8), torch));
        assert_eq!(world.block_light(BlockPos::new(8, 65, 8)), 14);
        assert_eq!(world.block_light(BlockPos::new(9, 65, 8)), 13, "one water cell");
        assert_eq!(world.block_light(BlockPos::new(10, 65, 8)), 12);
        assert_eq!(world.block_light(BlockPos::new(11, 65, 8)), 11);
        assert_eq!(world.block_light(BlockPos::new(12, 65, 8)), 10);
        assert_eq!(
            world.block_light(BlockPos::new(21, 65, 8)),
            1,
            "torch reaches 13 blocks"
        );
        assert_eq!(
            world.block_light(BlockPos::new(22, 65, 8)),
            0,
            "level 14 reaches 14 cells"
        );
        // The same field must be smooth across the x=15/16 chunk border.
        assert_eq!(world.block_light(BlockPos::new(15, 65, 8)), 7);
        assert_eq!(world.block_light(BlockPos::new(16, 65, 8)), 6);
    }

    #[test]
    fn torch_light_propagates_when_a_neighbour_chunk_streams_in() {
        let registry = registry();
        let torch = block(&registry, "openblock:torch");
        let mut world = flat_world(&registry, 1);
        // Torch on the last column of chunk 0: chunk 1 is not resident yet.
        assert!(world.set_block(BlockPos::new(15, 65, 8), torch));
        assert_eq!(world.block_light(BlockPos::new(15, 65, 8)), 14);

        let mut column = ChunkColumn::new(ChunkPos::new(1, 0));
        let stone = block(&registry, "openblock:stone");
        for z in 0..16 {
            for x in 0..16 {
                column.set_block(BlockPos::new(16 + x, 64, z), stone);
            }
        }
        world.insert_chunk(column);
        assert_eq!(
            world.block_light(BlockPos::new(16, 65, 8)),
            13,
            "border reconciliation lights the streamed-in chunk"
        );
        assert_eq!(world.block_light(BlockPos::new(18, 65, 8)), 11);
    }

    #[test]
    fn sky_light_crosses_a_chunk_border_without_a_seam() {
        let registry = registry();
        let stone = block(&registry, "openblock:stone");
        let mut world = World::new(Arc::clone(&registry), 4);
        // Chunk 0: roof over the entire column except the last row x=15.
        let mut west = ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..16 {
            for x in 0..16 {
                if x != 15 {
                    west.set_block(BlockPos::new(x, 70, z), stone);
                }
            }
        }
        world.insert_chunk(west);
        // Chunk 1 is open sky; the shaft at x=15 spreads into the roofed side.
        let mut east = ChunkColumn::new(ChunkPos::new(1, 0));
        for z in 0..16 {
            for x in 0..16 {
                if x != 0 {
                    east.set_block(BlockPos::new(16 + x, 70, z), stone);
                }
            }
        }
        world.insert_chunk(east);
        assert_eq!(world.sky_light(BlockPos::new(15, 69, 8)), 15);
        assert_eq!(world.sky_light(BlockPos::new(14, 69, 8)), 14);
        assert_eq!(world.sky_light(BlockPos::new(16, 69, 8)), 15);
        assert_eq!(world.sky_light(BlockPos::new(17, 69, 8)), 14);
    }

    #[test]
    fn relight_box_reaches_the_same_fixed_point_as_a_full_relight() {
        let registry = registry();
        let (mut world, room) = sealed_room(&registry);
        let torch = block(&registry, "openblock:torch");
        let stone = block(&registry, "openblock:stone");
        // A sequence of edits whose removal paths must converge.
        assert!(world.set_block(room, torch));
        assert!(world.set_block(BlockPos::new(5, 65, 5), torch));
        assert!(world.set_block(BlockPos::new(8, 65, 7), stone));
        assert!(world.set_block(BlockPos::new(5, 65, 5), BlockId::AIR));
        assert!(world.set_block(BlockPos::new(7, 65, 7), stone));
        assert!(world.set_block(BlockPos::new(7, 65, 7), torch));
        let incremental = world.light_hash();
        world.relight_all();
        let full = world.light_hash();
        assert_eq!(incremental, full, "incremental light must equal a from-scratch relight");
    }

    #[test]
    fn gameplay_light_applies_day_and_weather_without_mutating_stored_light() {
        let registry = registry();
        let (mut world, room) = sealed_room(&registry);
        let before = world.light_hash();
        assert_eq!(world.gameplay_light(BlockPos::new(2, 62, 2), 1.0, 1.0), 0);
        assert_eq!(world.gameplay_light(BlockPos::new(2, 71, 2), 1.0, 1.0), 15);
        assert_eq!(
            world.gameplay_light(BlockPos::new(2, 71, 2), 0.5, 1.0),
            7,
            "half daylight floors to 7"
        );
        assert_eq!(
            world.gameplay_light(BlockPos::new(2, 71, 2), 0.8, 0.8),
            9,
            "15 * 0.64 = 9.6 floors to 9"
        );
        assert_eq!(
            world.gameplay_light(BlockPos::new(2, 71, 2), 0.0, 1.0),
            0,
            "night is dark"
        );
        let torch = block(&registry, "openblock:torch");
        assert!(world.set_block(room, torch));
        assert_eq!(world.gameplay_light(room, 0.0, 0.6), 14, "torches ignore weather");
        assert_eq!(world.light_hash(), world.light_hash());
        assert_ne!(world.light_hash(), before);
    }

    #[test]
    fn light_hash_is_cached_and_save_round_trip_stable() {
        let registry = registry();
        let (mut world, room) = sealed_room(&registry);
        let torch = block(&registry, "openblock:torch");
        assert!(world.set_block(room, torch));
        let hash = world.light_hash();
        assert_eq!(hash, world.light_hash(), "cached checksum is stable");
        assert!(world.dense_light_section_count() > 0);

        let bytes = crate::save::to_bytes(&world);
        let loaded = crate::save::from_bytes(Arc::clone(&registry), &bytes).expect("save loads");
        assert_eq!(loaded.light_hash(), hash, "save/load preserves every light word");
        assert_eq!(loaded.light_sample(room), world.light_sample(room));
    }

    /// Manual performance probe; run with
    /// `./dev-cargo test -p ob_world --lib -- --ignored --nocapture light_performance_probe`.
    #[test]
    #[ignore = "performance probe; prints timing instead of asserting"]
    fn light_performance_probe() {
        use std::time::Instant;

        let registry = registry();
        let stone = block(&registry, "openblock:stone");
        let torch = block(&registry, "openblock:torch");
        let (mut world, room) = sealed_room(&registry);

        // One column with a realistic solid/air split and a carved room.
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for y in 0..=64 {
            for z in 0..16 {
                for x in 0..16 {
                    column.set_block(BlockPos::new(x, y, z), stone);
                }
            }
        }
        for y in 50..=56 {
            for z in 4..=11 {
                for x in 4..=11 {
                    column.set_block(BlockPos::new(x, y, z), BlockId::AIR);
                }
            }
        }
        let iterations = 32;
        let old_start = Instant::now();
        for _ in 0..iterations {
            // The 1.1 path was a per-section direct-sky seed.
            let mut copy = column.clone();
            for index in 0..N_SECTIONS {
                let y = WORLD_MIN_Y / 16 + index as i32;
                if let Some(section) = copy.section_mut(SectionPos::new(0, y, 0)) {
                    section.seed_sky_light(&registry);
                }
            }
        }
        let old = old_start.elapsed().as_secs_f64() / f64::from(iterations);

        let new_start = Instant::now();
        for _ in 0..iterations {
            let mut copy = column.clone();
            copy.finalise(&registry);
        }
        let new = new_start.elapsed().as_secs_f64() / f64::from(iterations);
        eprintln!(
            "column light seeding: old={:.3} ms, new={:.3} ms, delta={:.3} ms/chunk",
            old * 1e3,
            new * 1e3,
            (new - old) * 1e3
        );

        // Edit/relight timing: alternating torch place/remove.
        let mut samples = Vec::new();
        for index in 0..100 {
            let block_id = if index % 2 == 0 { torch } else { BlockId::AIR };
            let start = Instant::now();
            assert!(world.set_block(room, block_id));
            samples.push(start.elapsed().as_secs_f64() * 1e3);
        }
        samples.sort_by(f64::total_cmp);
        eprintln!(
            "relight torch edit: p50={:.3} ms, p99={:.3} ms, max={:.3} ms",
            samples[samples.len() / 2],
            samples[samples.len() * 99 / 100],
            samples[samples.len() - 1]
        );

        let mut cave_samples = Vec::new();
        let wall = BlockPos::new(8, 65, 7);
        for index in 0..100 {
            let block_id = if index % 2 == 0 { BlockId::AIR } else { stone };
            let start = Instant::now();
            assert!(world.set_block(wall, block_id));
            cave_samples.push(start.elapsed().as_secs_f64() * 1e3);
        }
        cave_samples.sort_by(f64::total_cmp);
        eprintln!(
            "relight dark-room stone edit (sky skipped): p50={:.3} ms, p99={:.3} ms, max={:.3} ms",
            cave_samples[cave_samples.len() / 2],
            cave_samples[cave_samples.len() * 99 / 100],
            cave_samples[cave_samples.len() - 1]
        );

        let mut sky_samples = Vec::new();
        // A real sky edit: an open-air roof block above the sealed plinth.
        let roof = BlockPos::new(2, 71, 2);
        for index in 0..100 {
            let block_id = if index % 2 == 0 { stone } else { BlockId::AIR };
            let start = Instant::now();
            assert!(world.set_block(roof, block_id));
            sky_samples.push(start.elapsed().as_secs_f64() * 1e3);
        }
        sky_samples.sort_by(f64::total_cmp);
        eprintln!(
            "relight open-sky roof edit: p50={:.3} ms, p99={:.3} ms, max={:.3} ms",
            sky_samples[sky_samples.len() / 2],
            sky_samples[sky_samples.len() * 99 / 100],
            sky_samples[sky_samples.len() - 1]
        );

        let (mut cave, seal) = deep_shaft_cave(&registry);
        let mut deep_samples = Vec::new();
        for index in 0..40 {
            let block_id = if index % 2 == 0 { stone } else { BlockId::AIR };
            let start = Instant::now();
            assert!(cave.set_block(seal, block_id));
            deep_samples.push(start.elapsed().as_secs_f64() * 1e3);
        }
        deep_samples.sort_by(f64::total_cmp);
        eprintln!(
            "relight deep-shaft seal/open (volume tail): p50={:.3} ms, p99={:.3} ms, max={:.3} ms",
            deep_samples[deep_samples.len() / 2],
            deep_samples[deep_samples.len() * 99 / 100],
            deep_samples[deep_samples.len() - 1]
        );

        // Streaming insertion of a 65×65-chunk flat world (radius 32) without
        // worldgen cost: this measures finalise + insert + border reconcile.
        let streaming_registry = Arc::new(Registry::base());
        let mut stream = World::new(Arc::clone(&streaming_registry), 5);
        let start = Instant::now();
        for cz in -32..=32 {
            for cx in -32..=32 {
                let mut column = ChunkColumn::new(ChunkPos::new(cx, cz));
                for z in 0..16 {
                    for x in 0..16 {
                        column.set_block(BlockPos::new(cx * 16 + x, 64, cz * 16 + z), stone);
                    }
                }
                stream.insert_chunk(column);
            }
        }
        let elapsed = start.elapsed();
        eprintln!(
            "streamed {} flat chunks in {:.3} s ({:.3} ms/chunk), light hash {:016x}, heap {} MiB",
            stream.chunk_count(),
            elapsed.as_secs_f64(),
            elapsed.as_secs_f64() * 1e3 / stream.chunk_count() as f64,
            stream.light_hash(),
            stream.heap_bytes() / (1024 * 1024)
        );

        // Baseline without cross-chunk reconciliation: same construction and
        // finalise, raw insertion. The delta is the added border work.
        let mut raw = World::new(Arc::clone(&streaming_registry), 5);
        let raw_start = Instant::now();
        for cz in -32..=32 {
            for cx in -32..=32 {
                let mut column = ChunkColumn::new(ChunkPos::new(cx, cz));
                for z in 0..16 {
                    for x in 0..16 {
                        column.set_block(BlockPos::new(cx * 16 + x, 64, cz * 16 + z), stone);
                    }
                }
                column.finalise(&streaming_registry);
                raw.insert_loaded_chunk(column);
            }
        }
        eprintln!(
            "streamed {} flat chunks without border reconciliation in {:.3} s ({:.3} ms/chunk)",
            raw.chunk_count(),
            raw_start.elapsed().as_secs_f64(),
            raw_start.elapsed().as_secs_f64() * 1e3 / raw.chunk_count() as f64,
        );
    }

    /// A shaft at x=8,z=8 opens from the sky into a room far below the 31³
    /// relight box. Sealing the shaft must darken the whole room, not only the
    /// column directly under it, and opening it again must restore the light.
    #[test]
    fn sealing_a_deep_shaft_darkens_the_room_below_the_relight_box() {
        let registry = registry();
        let stone = block(&registry, "openblock:stone");
        let (mut world, seal) = deep_shaft_cave(&registry);

        let lit = BlockPos::new(4, 52, 4);
        assert!(world.sky_light(lit) > 0, "the shaft lights the room before sealing");
        assert!(world.sky_light(BlockPos::new(8, 52, 8)) > 0);

        assert!(world.set_block(seal, stone));
        for y in 50..=53 {
            for z in 4..=11 {
                for x in 4..=11 {
                    let pos = BlockPos::new(x, y, z);
                    assert_eq!(
                        world.sky_light(pos),
                        0,
                        "sealing the shaft must darken {pos:?} below the relight box"
                    );
                }
            }
        }

        // Removing the seal restores the original sky field in the room.
        assert!(world.set_block(seal, BlockId::AIR));
        assert_eq!(
            world.sky_light(BlockPos::new(4, 52, 4)),
            7,
            "the room is relit from the restored shaft"
        );
        assert!(world.sky_light(BlockPos::new(8, 52, 8)) > 7);
    }

    /// Two uniform sections with different words differ along the entire
    /// 16-cell border. The fast path must seed every pair, not one corner.
    #[test]
    fn uniform_border_fast_path_seeds_the_whole_edge() {
        let registry = registry();
        let mut west_sections: Vec<crate::section::Section> =
            (0..N_SECTIONS).map(|_| crate::section::Section::empty()).collect();
        let mut east_sections = west_sections.clone();
        let sky_index = ((0 - WORLD_MIN_Y) / SECTION_SIZE as i32) as usize;
        *west_sections[sky_index].light_mut() = crate::light::Light::from_uniform(crate::light::pack(15, 0, 0, 0));
        *east_sections[sky_index].light_mut() = crate::light::Light::from_uniform(crate::light::pack(5, 0, 0, 0));
        let block_index = ((16 - WORLD_MIN_Y) / SECTION_SIZE as i32) as usize;
        *west_sections[block_index].light_mut() = crate::light::Light::from_uniform(crate::light::pack(0, 10, 10, 10));
        *east_sections[block_index].light_mut() = crate::light::Light::from_uniform(crate::light::pack(0, 5, 5, 5));
        let mut world = World::new(Arc::clone(&registry), 10);
        world.insert_loaded_chunk(ChunkColumn::from_sections(ChunkPos::new(0, 0), west_sections));
        world.insert_loaded_chunk(ChunkColumn::from_sections(ChunkPos::new(1, 0), east_sections));

        reconcile_chunk_borders(&mut world, ChunkPos::new(1, 0));

        for y in 0..SECTION_SIZE as i32 {
            for z in 0..SECTION_SIZE as i32 {
                let sky = world.sky_light(BlockPos::new(16, y, z));
                assert_eq!(sky, 14, "sky edge cell (16,{y},{z})");
                let block_light = world.light_sample(BlockPos::new(16, 16 + y, z)).block_level();
                assert_eq!(block_light, 9, "block-light edge cell (16,{},{z})", 16 + y);
            }
        }
    }

    /// A light change in one section changes meshes through the snapshot
    /// border, so the changed section must be in the dirty set even when the
    /// edit does not touch its border.
    #[test]
    fn light_changes_mark_affected_sections_dirty() {
        let registry = registry();
        let torch = block(&registry, "openblock:torch");
        let mut world = flat_world(&registry, 2);
        world.take_dirty_sections();
        // Local x=14 is not on a section border, but a level-14 torch still
        // reaches x=16 in the neighbouring column.
        assert!(world.set_block(BlockPos::new(14, 65, 8), torch));
        let dirty = world.take_dirty_sections();
        let neighbour = SectionPos::new(1, 4, 0);
        assert!(
            dirty.contains(&neighbour),
            "the neighbour section lit through the snapshot border must be remeshed: {dirty:?}"
        );
        assert!(world.block_light(BlockPos::new(16, 65, 8)) > 0);
    }

    #[test]
    fn roof_edits_darken_and_restore_the_column_below() {
        let registry = registry();
        let stone = block(&registry, "openblock:stone");
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..16 {
            for x in 0..16 {
                column.set_block(BlockPos::new(x, 64, z), stone);
            }
        }
        let mut world = World::new(Arc::clone(&registry), 8);
        world.insert_chunk(column);
        let roof = BlockPos::new(8, 70, 8);
        assert_eq!(world.sky_light(BlockPos::new(8, 65, 8)), 15);
        assert!(world.set_block(roof, stone));
        assert_eq!(world.sky_light(roof), 0, "the new roof blocks direct sky");
        let below = world.sky_light(BlockPos::new(8, 65, 8));
        assert!(below < 15, "the column below loses direct sky, got {below}");
        assert_eq!(
            world.sky_light(BlockPos::new(9, 65, 8)),
            15,
            "the open neighbour column keeps full sky"
        );
        assert!(world.set_block(roof, BlockId::AIR));
        assert_eq!(world.sky_light(roof), 15, "removing the roof restores sky");
        assert_eq!(world.sky_light(BlockPos::new(8, 65, 8)), 15);
    }
}
