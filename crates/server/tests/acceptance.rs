//! End-to-end 1.0 acceptance sweep of the survival loop.
//!
//! These tests use only the public host/server API (plus test-only world setup
//! for reaching natural blocks) and a deterministic seed. No window, no GPU,
//! no wall-clock sleeps in the single-player scenario.
//!
//! The single-player scenario follows the release checklist:
//! spawn safety and starter resources -> wood -> table -> tools -> stone ->
//! coal/iron -> furnace -> iron gear -> armour -> animals -> food -> fall ->
//! death/respawn -> chest -> save/reload hash.
//!
//! Test scaffolding carve access shafts to buried natural blocks and build a
//! small flat yard near spawn. Every actual break, craft, smelt, equip, eat,
//! attack, chest move and save/reload goes through the production APIs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_content::{ArmorSlot, BlockId, ItemId, MobKind, Registry};
use ob_math::{BlockPos, ChunkPos, SEA_LEVEL, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
use ob_server::{CHEST_SLOTS, ContainerState, FurnaceState, GameMode, Host, HostConfig};
use ob_sim::{BlockAction, PlayerInput, REACH};

/// Deterministic acceptance seed (same seed as the smoke test).
const SEED: u64 = 42;
/// Streaming radius used by the acceptance hosts. Three chunks (48 m) contains
/// the whole 32 m starter scan plus the yard and access shafts.
const RENDER_DISTANCE: i32 = 3;
/// Horizontal radius of the starter-resource scan, in blocks.
const STARTER_RADIUS: i32 = 32;

// ---------------------------------------------------------------------------
// Temporary worlds and small helpers
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "openblock-acceptance-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temporary data directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config_with_seed(data_dir: &Path, seed: u64) -> HostConfig {
    HostConfig {
        seed,
        world_name: "acceptance".to_string(),
        data_dir: Some(data_dir.to_path_buf()),
        render_distance_chunks: RENDER_DISTANCE,
        generate_budget: 64,
    }
}

fn config(data_dir: &Path) -> HostConfig {
    config_with_seed(data_dir, SEED)
}

/// Streams until no chunk in the configured radius is missing.
fn converge(host: &mut Host) {
    for _ in 0..400 {
        if host.ensure_streaming(256) == 0 {
            return;
        }
    }
    panic!("streaming did not converge");
}

fn open_survival_host(label: &str) -> (TempDir, Host) {
    let dir = TempDir::new(label);
    let mut host = Host::open(Arc::new(Registry::base()), config(dir.path())).expect("host opens on a fresh world");
    host.set_game_mode(GameMode::Survival);
    converge(&mut host);
    (dir, host)
}

fn block_id(host: &Host, name: &str) -> BlockId {
    host.registry()
        .id(name)
        .unwrap_or_else(|| panic!("missing block {name}"))
}

fn item_id(host: &Host, name: &str) -> ItemId {
    host.registry()
        .item_id(name)
        .unwrap_or_else(|| panic!("missing item {name}"))
}

fn item_count(host: &Host, item: ItemId) -> u32 {
    host.inventory()
        .iter()
        .flatten()
        .filter(|stack| stack.item == item)
        .map(|stack| u32::from(stack.count))
        .sum()
}

fn find_slot(host: &Host, item: ItemId) -> Option<usize> {
    host.inventory()
        .iter()
        .position(|slot| slot.is_some_and(|stack| stack.item == item))
}

fn select_item(host: &mut Host, item: ItemId) {
    let slot =
        find_slot(host, item).unwrap_or_else(|| panic!("item {} not in inventory", host.registry().item(item).id));
    host.set_hotbar_selection(slot);
}

fn select_empty(host: &mut Host) {
    let slot = host
        .inventory()
        .iter()
        .position(Option::is_none)
        .unwrap_or_else(|| panic!("no empty hotbar slot for an empty-hand action"));
    host.set_hotbar_selection(slot);
}

fn recipe_id(host: &Host, name: &str) -> ob_content::RecipeId {
    host.registry()
        .recipe_id(name)
        .unwrap_or_else(|| panic!("missing recipe {name}"))
}

fn craft(host: &mut Host, name: &str) -> ob_server::CraftOutcome {
    let id = recipe_id(host, name);
    host.craft(id)
}

fn aim_at(host: &mut Host, eye: WorldVec, target: WorldVec) {
    let delta = target - eye;
    assert!(delta.length_squared() > 1e-12, "cannot aim at the camera eye");
    let pitch = (delta.y / delta.length()).clamp(-1.0, 1.0).asin();
    let yaw = (-delta.x).atan2(-delta.z);
    let player = host.player_mut();
    player.yaw = yaw as f32;
    player.pitch = pitch as f32;
}

// ---------------------------------------------------------------------------
// World scan
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StarterScan {
    counts: [u32; 5],
    logs: Vec<BlockPos>,
    stone: Vec<BlockPos>,
    coal: Vec<BlockPos>,
    iron: Vec<BlockPos>,
}

const SCAN_LOG: usize = 0;
const SCAN_LEAVES: usize = 1;
const SCAN_STONE: usize = 2;
const SCAN_COAL: usize = 3;
const SCAN_IRON: usize = 4;
const SCAN_POSITION_CAP: usize = 512;

/// Scans every loaded block within `STARTER_RADIUS` of the spawn column.
fn scan_starter_resources(host: &Host) -> StarterScan {
    let spawn = host.spawn();
    let sx = spawn.x.floor() as i32;
    let sz = spawn.z.floor() as i32;
    let log = block_id(host, "openblock:log");
    let leaves = block_id(host, "openblock:leaves");
    let stone = block_id(host, "openblock:stone");
    let coal = block_id(host, "openblock:coal_ore");
    let iron = block_id(host, "openblock:iron_ore");

    let mut scan = StarterScan::default();
    let mut chunks: Vec<ChunkPos> = host.world().chunks().map(|(pos, _)| *pos).collect();
    chunks.sort_by_key(|pos| (pos.x, pos.z));
    for chunk_pos in chunks {
        let Some(chunk) = host.chunk_column(chunk_pos) else {
            continue;
        };
        let (origin_x, origin_z) = chunk_pos.origin();
        for z in 0..16 {
            let wz = origin_z + z;
            if (wz - sz).abs() > STARTER_RADIUS {
                continue;
            }
            for x in 0..16 {
                let wx = origin_x + x;
                if (wx - sx).abs() > STARTER_RADIUS {
                    continue;
                }
                for y in WORLD_MIN_Y..WORLD_MAX_Y {
                    let pos = BlockPos::new(wx, y, wz);
                    let block = chunk.get_block(pos);
                    let category = if block == log {
                        SCAN_LOG
                    } else if block == leaves {
                        SCAN_LEAVES
                    } else if block == stone {
                        SCAN_STONE
                    } else if block == coal {
                        SCAN_COAL
                    } else if block == iron {
                        SCAN_IRON
                    } else {
                        continue;
                    };
                    scan.counts[category] += 1;
                    let bucket = match category {
                        SCAN_LOG => &mut scan.logs,
                        SCAN_STONE => &mut scan.stone,
                        SCAN_COAL => &mut scan.coal,
                        SCAN_IRON => &mut scan.iron,
                        _ => continue,
                    };
                    if bucket.len() < SCAN_POSITION_CAP {
                        bucket.push(pos);
                    }
                }
            }
        }
    }
    let distance_key = |pos: &BlockPos| {
        let dx = (pos.x - sx) as i64;
        let dz = (pos.z - sz) as i64;
        dx * dx + dz * dz
    };
    scan.logs.sort_by_key(distance_key);
    scan.stone.sort_by_key(distance_key);
    scan.coal.sort_by_key(distance_key);
    scan.iron.sort_by_key(distance_key);
    scan
}

fn horizontal_distance(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

// ---------------------------------------------------------------------------
// Test-only world preparation and real player actions
// ---------------------------------------------------------------------------

/// Builds a flat 9x9 yard centred on the spawn column, leaving the centre
/// support block intact so the world spawn stays where `Host::open` found it.
fn build_yard(host: &mut Host) -> (i32, i32, i32) {
    let spawn = host.spawn();
    let sx = spawn.x.floor() as i32;
    let sy = spawn.y.floor() as i32;
    let sz = spawn.z.floor() as i32;
    let base_y = sy - 1;
    let grass = block_id(host, "openblock:grass");
    for dz in -4..=4 {
        for dx in -4..=4 {
            let pos = BlockPos::new(sx + dx, base_y, sz + dz);
            assert!(
                host.world_mut().set_block(pos, grass),
                "yard support is loaded at {pos:?}"
            );
            for dy in 1..=4 {
                assert!(
                    host.world_mut().set_block(pos.offset(0, dy, 0), BlockId::AIR),
                    "yard air is loaded at {pos:?}"
                );
            }
        }
    }
    (sx, sy, sz)
}

/// Places one block item through the real placement action, aiming at the top
/// face of the support block under `target`.
fn place_block_item(host: &mut Host, item: ItemId, target: BlockPos, stand: BlockPos) {
    let support = target.offset(0, -1, 0);
    assert!(
        host.world().is_solid(support),
        "placement support {support:?} must be solid"
    );
    assert_eq!(
        host.world().get_block(target),
        BlockId::AIR,
        "target {target:?} must be air"
    );
    assert_eq!(
        host.world().get_block(target.offset(0, 1, 0)),
        BlockId::AIR,
        "target headroom must be air"
    );

    // Stand at the yard centre, look just below the support's top face.
    {
        let player = host.player_mut();
        player.pos = WorldVec::new(stand.x as f64 + 0.5, stand.y as f64, stand.z as f64 + 0.5);
        player.velocity = WorldVec::ZERO;
        player.on_ground = true;
    }
    let eye = host.player().eye();
    let aim = WorldVec::new(support.x as f64 + 0.5, support.y as f64 + 0.999, support.z as f64 + 0.5);
    aim_at(host, eye, aim);
    let hit = host
        .world()
        .raycast(host.player().eye(), host.player().forward(), REACH)
        .unwrap_or_else(|| panic!("placement ray did not hit the support under {target:?}"));
    assert_eq!(
        hit.block, support,
        "placement ray hit {:?}, expected {support:?}",
        hit.block
    );
    assert_eq!(
        hit.normal,
        BlockPos::new(0, 1, 0),
        "placement ray must hit the top face of {support:?}"
    );

    select_item(host, item);
    let block = host
        .registry()
        .item(item)
        .block
        .unwrap_or_else(|| panic!("item {} is not a block", host.registry().item(item).id));
    let outcome = host.apply_action_with_outcome(&BlockAction {
        break_block: false,
        place_block: true,
        place: block,
    });
    assert!(outcome.changed, "placement of {block:?} was rejected");
    assert_eq!(outcome.placed, Some(block));
    assert_eq!(host.world().get_block(target), block);
}

/// Carves a safe two-block-high camera stand above `target`, mines the target
/// with the selected tool through the real survival action, then waits for the
/// drop to be picked up. Test-only world edits are limited to the access and
/// the floor under the stand; the break itself is authoritative survival.
fn mine_block(host: &mut Host, target: BlockPos) -> ob_server::ActionOutcome {
    let above = target.offset(0, 1, 0);
    let above_two = target.offset(0, 2, 0);
    assert!(host.world_mut().set_block(above, BlockId::AIR), "access cell is loaded");
    assert!(
        host.world_mut().set_block(above_two, BlockId::AIR),
        "access headroom is loaded"
    );

    {
        let player = host.player_mut();
        player.pos = WorldVec::new(target.x as f64 + 0.5, target.y as f64 + 1.0, target.z as f64 + 0.5);
        player.velocity = WorldVec::ZERO;
        player.on_ground = false;
        player.yaw = 0.0;
        player.pitch = -std::f32::consts::FRAC_PI_2;
    }
    let hit = host
        .world()
        .raycast(host.player().eye(), host.player().forward(), REACH)
        .unwrap_or_else(|| panic!("mining ray did not hit {target:?}"));
    assert_eq!(hit.block, target, "mining ray hit {:?}, expected {target:?}", hit.block);
    let block = host.world().get_block(target);
    let outcome = host.apply_action_with_outcome(&BlockAction {
        break_block: true,
        place_block: false,
        place: BlockId::AIR,
    });
    assert!(outcome.changed, "breaking {target:?} was rejected");
    assert_eq!(outcome.broke, Some(block), "wrong block broke at {target:?}");
    assert_eq!(host.world().get_block(target), BlockId::AIR);

    // Stand in the emptied cell to collect the drop without falling.
    let floor = target.offset(0, -1, 0);
    if !host.world().is_solid(floor) {
        let stone = block_id(host, "openblock:stone");
        assert!(host.world_mut().set_block(floor, stone), "floor cell is loaded");
    }
    {
        let player = host.player_mut();
        player.pos = WorldVec::new(target.x as f64 + 0.5, target.y as f64, target.z as f64 + 0.5);
        player.velocity = WorldVec::ZERO;
        player.on_ground = true;
    }
    for _ in 0..80 {
        host.tick(&PlayerInput::default());
        if host.item_entities().is_empty() {
            break;
        }
    }
    outcome
}

/// Teleports the player back to the yard centre between resource trips.
fn go_to_yard(host: &mut Host, center: BlockPos) {
    let player = host.player_mut();
    player.pos = WorldVec::new(center.x as f64 + 0.5, center.y as f64, center.z as f64 + 0.5);
    player.velocity = WorldVec::ZERO;
    player.on_ground = true;
}

/// Mines `count` still-present blocks of `kind` (closest first) and asserts the
/// expected item total rises by exactly one per block.
fn mine_many(host: &mut Host, candidates: &[BlockPos], kind: BlockId, expected_item: ItemId, count: u32) {
    let mut mined = 0u32;
    let before = item_count(host, expected_item);
    for target in candidates {
        if mined == count {
            break;
        }
        if host.world().get_block(*target) != kind {
            continue;
        }
        let at = *target;
        let outcome = mine_block(host, at);
        assert!(
            outcome.drops.iter().any(|drop| drop.item == expected_item),
            "{kind:?} at {at:?} dropped {:?}, expected {expected_item:?}",
            outcome.drops
        );
        mined += 1;
    }
    assert_eq!(mined, count, "not enough reachable {kind:?} blocks");
    assert_eq!(
        item_count(host, expected_item),
        before + count,
        "pickup of {kind:?} drops failed"
    );
}

// ---------------------------------------------------------------------------
// Spawn persistence regression (far candidates and features above the surface)
// ---------------------------------------------------------------------------

#[test]
fn spawn_is_safe_and_stable_across_reopen_for_far_candidates() {
    // Seed 29's old deterministic scan found a tree column at (56, 0): a fresh
    // world spawned inside the trunk (fallback height), while a reload spawned
    // on the canopy. Seeds 3/94 are far-candidate seeds; 42 is the acceptance
    // seed and must keep its familiar spawn.
    for seed in [SEED, 3, 29, 94] {
        let dir = TempDir::new(&format!("spawn-{seed}"));
        let config = config_with_seed(dir.path(), seed);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("fresh spawn host");
        host.set_game_mode(GameMode::Survival);
        let spawn = host.spawn();
        let (sx, sy, sz) = (spawn.x.floor() as i32, spawn.y.floor() as i32, spawn.z.floor() as i32);
        let surface = BlockPos::new(sx, sy - 1, sz);
        assert!(
            host.world().is_solid(surface),
            "seed {seed}: spawn surface is not solid"
        );
        assert_eq!(
            host.world().get_block(BlockPos::new(sx, sy, sz)),
            BlockId::AIR,
            "seed {seed}: spawn feet are occupied"
        );
        assert_eq!(
            host.world().get_block(BlockPos::new(sx, sy + 1, sz)),
            BlockId::AIR,
            "seed {seed}: spawn head is occupied"
        );
        assert_eq!(
            host.world().height_at(sx, sz),
            Some(sy - 1),
            "seed {seed}: a feature sits above the spawn column"
        );
        // Wave-1 worldgen structures are chunk-local; the spawn resolver must
        // not place a player inside one. Four blocks is worldgen's maximum
        // structure half-extent (the mineshaft adit).
        let generator = ob_worldgen::Generator::new(seed);
        if let Some(structure) = generator.structure_in_chunk(BlockPos::new(sx, sy, sz).chunk()) {
            assert!(
                (sx - structure.x).abs() > 4 || (sz - structure.z).abs() > 4,
                "seed {seed}: spawn ({sx},{sz}) is inside {:?} at ({},{})",
                structure.kind,
                structure.x,
                structure.z
            );
        }
        for _ in 0..40 {
            host.tick(&PlayerInput::default());
        }
        assert!(host.player().on_ground, "seed {seed}: spawned player fell");
        assert_eq!(host.player().health, 20.0, "seed {seed}: spawn damaged the player");
        host.damage(100.0);
        assert!(host.is_dead());
        host.respawn();
        assert_eq!(host.spawn(), spawn, "seed {seed}: respawn moved the world spawn");
        assert!(host.player().pos.distance(spawn) < 1e-6);
        host.save().expect("save spawn sidecar");
        drop(host);

        let mut reopened = Host::open(registry, config).expect("reopen with saved spawn");
        converge(&mut reopened);
        assert_eq!(reopened.spawn(), spawn, "seed {seed}: spawn moved after reopen");
        reopened.set_game_mode(GameMode::Survival);
        assert_eq!(reopened.world().height_at(sx, sz), Some(sy - 1));
    }
}

// ---------------------------------------------------------------------------
// Single-player acceptance scenario
// ---------------------------------------------------------------------------

#[test]
fn survival_loop_from_spawn_to_reload_seed_42() {
    let (_dir, mut host) = open_survival_host("loop");
    let spawn = host.spawn();
    let (sx, sy, sz) = (spawn.x.floor() as i32, spawn.y.floor() as i32, spawn.z.floor() as i32);

    // --- Spawn safety -----------------------------------------------------
    let spawn_chunk = BlockPos::new(sx, sy, sz).chunk();
    assert!(host.chunk_column(spawn_chunk).is_some(), "spawn chunk is loaded");
    assert!(sy > SEA_LEVEL, "spawn must be dry land above sea level");
    assert_eq!(
        host.world().height_at(sx, sz),
        Some(sy - 1),
        "spawn feet are one above the surface"
    );
    assert!(
        !host
            .world()
            .block_type(host.world().get_block(BlockPos::new(sx, sy, sz)))
            .solid,
        "spawn feet cell must not be solid"
    );
    assert!(
        !host
            .world()
            .block_type(host.world().get_block(BlockPos::new(sx, sy + 1, sz)))
            .solid,
        "spawn head cell must not be solid"
    );
    assert!(
        host.world().is_solid(BlockPos::new(sx, sy - 1, sz)),
        "spawn must stand on solid ground"
    );
    for depth in 1..=8 {
        assert_ne!(
            host.world().get_block(BlockPos::new(sx, sy - depth, sz)),
            BlockId::AIR,
            "spawn crust is hollow at depth {depth}"
        );
    }
    let start = host.player().pos;
    for _ in 0..40 {
        host.tick(&PlayerInput::default());
    }
    assert!(host.player().on_ground, "spawned player is standing");
    assert!(host.player().pos.distance(start) < 0.01, "spawned player did not fall");
    assert_eq!(host.player().health, 20.0);
    assert!(
        (host.player().hunger - 20.0).abs() < 0.02,
        "idle hunger drain over the two-second settle is bounded (hunger {})",
        host.player().hunger
    );

    // --- Starter resources within 32 m -----------------------------------
    let scan = scan_starter_resources(&host);
    assert!(scan.counts[SCAN_LOG] > 0, "no trees within {STARTER_RADIUS} m of spawn");
    assert!(scan.counts[SCAN_LEAVES] > 0, "the starter tree has no canopy");
    assert!(
        scan.counts[SCAN_STONE] >= 11,
        "not enough stone within {STARTER_RADIUS} m"
    );
    assert!(scan.counts[SCAN_COAL] >= 3, "not enough coal within {STARTER_RADIUS} m");
    assert!(
        scan.counts[SCAN_IRON] >= 11,
        "not enough iron within {STARTER_RADIUS} m"
    );
    assert!(
        horizontal_distance((scan.logs[0].x, scan.logs[0].z), (sx, sz)) <= STARTER_RADIUS,
        "nearest tree is outside the starter radius"
    );
    assert!(
        horizontal_distance((scan.stone[0].x, scan.stone[0].z), (sx, sz)) <= STARTER_RADIUS,
        "nearest stone is outside the starter radius"
    );
    for pos in &scan.coal {
        assert!((5..=96).contains(&pos.y), "coal ore outside its height band: {pos:?}");
    }
    for pos in &scan.iron {
        assert!((5..=64).contains(&pos.y), "iron ore outside its height band: {pos:?}");
    }
    // The nearest tree is a real trunk with a canopy, not a single stray log.
    let nearest_log = scan.logs[0];
    let mut trunk = 0;
    for dy in -6..=6 {
        if host.world().get_block(nearest_log.offset(0, dy, 0)) == block_id(&host, "openblock:log") {
            trunk += 1;
        }
    }
    assert!(trunk >= 3, "starter tree trunk is only {trunk} logs tall");
    let mut canopy = 0;
    for dy in -3..=5 {
        for dz in -3..=3 {
            for dx in -3..=3 {
                if host.world().get_block(nearest_log.offset(dx, dy, dz)) == block_id(&host, "openblock:leaves") {
                    canopy += 1;
                }
            }
        }
    }
    assert!(canopy > 0, "starter tree has no leaves nearby");

    // --- Flat yard for deterministic placement/combat --------------------
    build_yard(&mut host);
    let center = BlockPos::new(sx, sy, sz);
    let table_pos = BlockPos::new(sx + 3, sy, sz);
    let chest_pos = BlockPos::new(sx - 3, sy, sz);
    let furnace_pos = BlockPos::new(sx, sy, sz + 3);

    // --- Punch logs -> planks -> sticks -> table -------------------------
    let log_item = item_id(&host, "openblock:log");
    let mut log_candidates = scan.logs.clone();
    log_candidates.sort_by_key(|pos| -pos.y);
    select_empty(&mut host);
    let log_block = block_id(&host, "openblock:log");
    mine_many(&mut host, &log_candidates, log_block, log_item, 8);
    assert_eq!(item_count(&host, log_item), 8);
    go_to_yard(&mut host, center);

    let planks = item_id(&host, "openblock:planks");
    for _ in 0..8 {
        let outcome = craft(&mut host, "openblock:planks");
        assert!(outcome.crafted, "{}", outcome.message);
    }
    assert_eq!(item_count(&host, planks), 32);
    for _ in 0..3 {
        let outcome = craft(&mut host, "openblock:sticks");
        assert!(outcome.crafted, "{}", outcome.message);
    }
    assert_eq!(item_count(&host, item_id(&host, "openblock:stick")), 12);
    let table = craft(&mut host, "openblock:crafting_table");
    assert!(table.crafted, "{}", table.message);

    // Table recipes are table-gated until the table is placed.
    let wood_pick_recipe = recipe_id(&host, "openblock:wooden_pickaxe");
    assert_eq!(
        host.available_recipes()
            .iter()
            .find(|(id, _)| *id == wood_pick_recipe)
            .map(|(_, n)| *n),
        Some(0),
        "table recipes must be gated before placement"
    );
    let table_item = item_id(&host, "openblock:crafting_table");
    select_item(&mut host, table_item);
    place_block_item(&mut host, table_item, table_pos, center);
    assert!(host.near_crafting_table(), "placed table is not in crafting range");
    assert!(
        host.available_recipes()
            .iter()
            .find(|(id, _)| *id == wood_pick_recipe)
            .map(|(_, n)| *n)
            .unwrap_or(0)
            >= 1,
        "table recipes are still unavailable after placing the table"
    );

    // --- Wooden pickaxe/axe, chest, chest deposit ------------------------
    let wood_pick = craft(&mut host, "openblock:wooden_pickaxe");
    assert!(wood_pick.crafted, "{}", wood_pick.message);
    let wood_axe = craft(&mut host, "openblock:wooden_axe");
    assert!(wood_axe.crafted, "{}", wood_axe.message);
    let chest = craft(&mut host, "openblock:chest");
    assert!(chest.crafted, "{}", chest.message);
    let chest_item = item_id(&host, "openblock:chest");
    select_item(&mut host, chest_item);
    place_block_item(&mut host, chest_item, chest_pos, center);
    assert!(host.chest_at(chest_pos).is_some(), "placed chest has no block entity");
    assert!(host.chest_at(chest_pos).unwrap().is_empty());

    let deposit = |host: &mut Host, item: ItemId, count: u8| {
        let slot = find_slot(host, item).unwrap_or_else(|| panic!("missing deposit item"));
        let stack = host.take_inventory_slot(slot).expect("deposit stack");
        assert_eq!(stack.count, count);
        let empty = find_chest_empty(host, chest_pos).expect("free chest slot");
        assert!(host.put_chest_slot(chest_pos, empty, stack).is_none());
    };
    deposit(&mut host, planks, 8);
    let wood_axe_item = item_id(&host, "openblock:wooden_axe");
    deposit(&mut host, wood_axe_item, 1);
    assert_eq!(host.chest_at(chest_pos).unwrap().contents().len(), 2);

    // --- Stone -> cobblestone and stone pickaxe --------------------------
    let wood_pick_item = item_id(&host, "openblock:wooden_pickaxe");
    select_item(&mut host, wood_pick_item);
    let cobble = item_id(&host, "openblock:cobblestone");
    let stone_block = block_id(&host, "openblock:stone");
    mine_many(&mut host, &scan.stone, stone_block, cobble, 11);
    go_to_yard(&mut host, center);
    let stone_pick = craft(&mut host, "openblock:stone_pickaxe");
    assert!(stone_pick.crafted, "{}", stone_pick.message);

    // --- Coal + iron with the required tiers -----------------------------
    let stone_pick_item = item_id(&host, "openblock:stone_pickaxe");
    select_item(&mut host, stone_pick_item);
    let mut coal_candidates = scan.coal.clone();
    coal_candidates.sort_by_key(|pos| -pos.y);
    let coal_ore = block_id(&host, "openblock:coal_ore");
    let coal = item_id(&host, "openblock:coal");
    mine_many(&mut host, &coal_candidates, coal_ore, coal, 3);
    let mut iron_candidates = scan.iron.clone();
    iron_candidates.sort_by_key(|pos| -pos.y);
    let iron_ore = block_id(&host, "openblock:iron_ore");
    let raw_iron = item_id(&host, "openblock:raw_iron");
    mine_many(&mut host, &iron_candidates, iron_ore, raw_iron, 11);
    go_to_yard(&mut host, center);

    // --- Furnace and smelting -------------------------------------------
    let furnace = craft(&mut host, "openblock:furnace");
    assert!(furnace.crafted, "{}", furnace.message);
    assert_eq!(
        item_count(&host, cobble),
        0,
        "the furnace consumed the last eight cobblestone"
    );
    let furnace_item = item_id(&host, "openblock:furnace");
    select_item(&mut host, furnace_item);
    place_block_item(&mut host, furnace_item, furnace_pos, center);
    assert!(
        host.furnace_at(furnace_pos).is_some(),
        "placed furnace has no block entity"
    );

    let iron_ingot = item_id(&host, "openblock:iron_ingot");
    {
        let slot = find_slot(&host, raw_iron).expect("raw iron stack");
        let stack = host.take_inventory_slot(slot).expect("raw iron");
        assert_eq!(stack.count, 11);
        let mut ten = stack;
        ten.count = 10;
        let mut one = stack;
        one.count = 1;
        assert!(host.insert_furnace_input(furnace_pos, ten).is_none());
        assert!(host.give_stack(one).is_none());
    }
    {
        let slot = find_slot(&host, coal).expect("coal stack");
        let stack = host.take_inventory_slot(slot).expect("coal");
        assert_eq!(stack.count, 3);
        let mut two = stack;
        two.count = 2;
        let mut one = stack;
        one.count = 1;
        assert!(host.insert_furnace_fuel(furnace_pos, two).is_none());
        assert!(host.give_stack(one).is_none());
    }
    for _ in 0..2_200 {
        host.tick(&PlayerInput::default());
    }
    let output = host.take_furnace_output(furnace_pos).expect("smelted iron output");
    assert_eq!(output.item, iron_ingot);
    assert_eq!(output.count, 10);
    assert_eq!(host.give_stack(output), None);
    assert_eq!(item_count(&host, iron_ingot), 10);
    assert!(host.furnace_at(furnace_pos).unwrap().input.is_none());
    assert!(host.furnace_at(furnace_pos).unwrap().output.is_none());

    // --- Iron tools + iron helmet ---------------------------------------
    let iron_pick = craft(&mut host, "openblock:iron_pickaxe");
    assert!(iron_pick.crafted, "{}", iron_pick.message);
    let iron_sword = craft(&mut host, "openblock:iron_sword");
    assert!(iron_sword.crafted, "{}", iron_sword.message);
    let iron_helmet = craft(&mut host, "openblock:iron_helmet");
    assert!(iron_helmet.crafted, "{}", iron_helmet.message);
    assert_eq!(item_count(&host, iron_ingot), 0);
    let helmet_item = item_id(&host, "openblock:iron_helmet");
    let helmet_slot = find_slot(&host, helmet_item).expect("helmet slot");
    assert!(host.equip_armor(helmet_slot), "equipping the iron helmet failed");
    assert_eq!(host.equipment_slot(ArmorSlot::Helmet).unwrap().item, helmet_item);
    assert_eq!(host.armor_defense(), 2);

    // --- Fall damage -----------------------------------------------------
    host.player_mut().pos = WorldVec::new(sx as f64 + 0.5, sy as f64 + 8.0, sz as f64 + 0.5);
    host.player_mut().velocity = WorldVec::ZERO;
    host.player_mut().on_ground = false;
    let health_before_fall = host.player().health;
    for _ in 0..120 {
        host.tick(&PlayerInput::default());
        if host.player().on_ground {
            break;
        }
    }
    assert!(host.player().on_ground, "fall did not land");
    assert!(
        host.player().health < health_before_fall,
        "an eight-metre fall caused no damage"
    );
    assert!(!host.is_dead(), "the acceptance fall must not be lethal");

    // --- Animals: leather + raw beef ------------------------------------
    let iron_sword_item = item_id(&host, "openblock:iron_sword");
    select_item(&mut host, iron_sword_item);
    let cow_positions = [
        WorldVec::new(sx as f64 + 1.5, sy as f64, sz as f64 + 1.5),
        WorldVec::new(sx as f64 - 1.5, sy as f64, sz as f64 + 1.5),
        WorldVec::new(sx as f64 + 1.5, sy as f64, sz as f64 - 1.5),
        WorldVec::new(sx as f64 - 1.5, sy as f64, sz as f64 - 1.5),
    ];
    let mut cows = Vec::new();
    for pos in cow_positions {
        let id = host.spawn_mob(MobKind::Cow, pos).expect("cow spawns on the yard");
        cows.push(id);
    }
    let leather = item_id(&host, "openblock:leather");
    let raw_beef = item_id(&host, "openblock:raw_beef");
    for id in cows {
        let mut killed = false;
        for _ in 0..6 {
            let outcome = host.attack_mob(id);
            if outcome.killed {
                assert_eq!(outcome.drops.len(), 2, "a cow must drop beef and leather");
                killed = true;
                break;
            }
        }
        assert!(killed, "iron sword failed to kill a cow");
    }
    for _ in 0..80 {
        host.tick(&PlayerInput::default());
    }
    assert_eq!(item_count(&host, leather), 4);
    assert_eq!(item_count(&host, raw_beef), 4);

    // --- Cooking and eating ---------------------------------------------
    let slot = find_slot(&host, raw_beef).expect("raw beef");
    let beef = host.take_inventory_slot(slot).expect("raw beef stack");
    assert_eq!(beef.count, 4);
    assert!(host.insert_furnace_input(furnace_pos, beef).is_none());
    for _ in 0..900 {
        host.tick(&PlayerInput::default());
    }
    let cooked = host.take_furnace_output(furnace_pos).expect("cooked beef");
    assert_eq!(cooked.item, item_id(&host, "openblock:cooked_beef"));
    assert_eq!(cooked.count, 4);
    assert!(host.give_stack(cooked).is_none());
    let cooked_beef = item_id(&host, "openblock:cooked_beef");
    host.player_mut().hunger = 10.0;
    select_item(&mut host, cooked_beef);
    assert!(host.eat_selected(), "cooked beef must be edible");
    assert!(
        (host.player().hunger - 18.0).abs() < 1e-4,
        "cooked beef restores 8 hunger"
    );
    assert_eq!(item_count(&host, cooked_beef), 3);

    // --- Leather boots and full equipment -------------------------------
    let boots = craft(&mut host, "openblock:leather_boots");
    assert!(boots.crafted, "{}", boots.message);
    assert_eq!(item_count(&host, leather), 0);
    let boots_item = item_id(&host, "openblock:leather_boots");
    let boots_slot = find_slot(&host, boots_item).expect("boots slot");
    assert!(host.equip_armor(boots_slot), "equipping leather boots failed");
    assert_eq!(host.equipment_slot(ArmorSlot::Boots).unwrap().item, boots_item);
    assert_eq!(host.armor_defense(), 3, "helmet 2 + boots 1");
    assert_eq!(host.equipment().iter().flatten().count(), 2);
    assert!(host.unequip_armor(ArmorSlot::Helmet), "unequip must work");
    let helmet_slot = find_slot(&host, helmet_item).expect("helmet back in inventory");
    assert!(host.equip_armor(helmet_slot), "re-equipping the helmet failed");

    // --- Zombie with the iron sword -------------------------------------
    select_item(&mut host, iron_sword_item);
    let zombie = host
        .spawn_mob(
            MobKind::Zombie,
            WorldVec::new(sx as f64 + 1.5, sy as f64, sz as f64 + 1.5),
        )
        .expect("zombie spawns on the yard");
    let mut zombie_killed = false;
    for _ in 0..8 {
        let outcome = host.attack_mob(zombie);
        if outcome.killed {
            zombie_killed = true;
            break;
        }
    }
    assert!(zombie_killed, "iron sword failed to kill the zombie");

    // --- Chest deposit/withdraw -----------------------------------------
    let sticks = item_id(&host, "openblock:stick");
    {
        let slot = find_slot(&host, sticks).expect("sticks");
        let stack = host.take_inventory_slot(slot).expect("sticks stack");
        assert!(host.put_chest_slot(chest_pos, 2, stack).is_none());
    }
    let withdrawn = host.take_chest_slot(chest_pos, 0).expect("chest planks");
    assert_eq!(withdrawn.item, planks);
    assert_eq!(withdrawn.count, 8);
    assert!(host.give_stack(withdrawn).is_none());
    assert_eq!(host.chest_at(chest_pos).unwrap().contents().len(), 2);

    // Leave a valid mid-smelt state for persistence: one raw iron and one coal.
    {
        let slot = find_slot(&host, raw_iron).expect("spare raw iron");
        let stack = host.take_inventory_slot(slot).expect("spare raw iron stack");
        assert_eq!(stack.count, 1);
        assert!(host.insert_furnace_input(furnace_pos, stack).is_none());
    }
    {
        let slot = find_slot(&host, coal).expect("spare coal");
        let stack = host.take_inventory_slot(slot).expect("spare coal stack");
        assert_eq!(stack.count, 1);
        assert!(host.insert_furnace_fuel(furnace_pos, stack).is_none());
    }

    // --- Death and respawn ----------------------------------------------
    host.damage(100.0);
    assert!(host.is_dead(), "lethal damage must kill");
    assert_eq!(host.player().health, 0.0);
    let inventory_before_death = *host.inventory();
    host.respawn();
    assert!(!host.is_dead(), "respawn must revive");
    assert_eq!(host.player().health, 20.0);
    assert_eq!(host.player().hunger, 20.0);
    assert!(
        host.player().pos.distance(spawn) < 1e-6,
        "respawn returns to the world spawn"
    );
    assert_eq!(*host.inventory(), inventory_before_death, "respawn keeps the inventory");
    assert_eq!(host.equipment().iter().flatten().count(), 2, "respawn keeps armour");

    // --- Save / reload exactness ----------------------------------------
    let expected_hash = host.state_hash();
    let expected_inventory = *host.inventory();
    let expected_equipment = *host.equipment();
    let expected_furnaces: Vec<(BlockPos, FurnaceState)> =
        host.furnaces().map(|(pos, state)| (pos, state.clone())).collect();
    let expected_chests: Vec<(BlockPos, ContainerState)> =
        host.chests().map(|(pos, state)| (pos, state.clone())).collect();
    let expected_player = host.player().pos;
    let expected_ticks = host.tick_count();
    host.save().expect("acceptance save succeeds");
    drop(host);

    let mut reopened =
        Host::open(Arc::new(Registry::base()), config(_dir.path())).expect("reopen saved acceptance world");
    converge(&mut reopened);
    assert_eq!(reopened.tick_count(), expected_ticks);
    assert!(reopened.player().pos.distance(expected_player) < 1e-6);
    assert_eq!(
        *reopened.inventory(),
        expected_inventory,
        "inventory did not round-trip"
    );
    assert_eq!(
        *reopened.equipment(),
        expected_equipment,
        "equipment did not round-trip"
    );
    assert_eq!(
        reopened
            .furnaces()
            .map(|(pos, state)| (pos, state.clone()))
            .collect::<Vec<_>>(),
        expected_furnaces,
        "furnaces did not round-trip"
    );
    assert_eq!(
        reopened
            .chests()
            .map(|(pos, state)| (pos, state.clone()))
            .collect::<Vec<_>>(),
        expected_chests,
        "chests did not round-trip"
    );
    assert_eq!(
        reopened
            .chest_at(chest_pos)
            .map(ContainerState::contents)
            .unwrap()
            .len(),
        2
    );
    assert!(reopened.furnace_at(furnace_pos).unwrap().input.is_some());
    assert_eq!(
        reopened.state_hash(),
        expected_hash,
        "state hash changed across save/reload"
    );
}

// ---------------------------------------------------------------------------
// Small chest helpers kept next to the scenario
// ---------------------------------------------------------------------------

fn find_chest_empty(host: &Host, pos: BlockPos) -> Option<usize> {
    host.chest_at(pos)?.slots.iter().position(Option::is_none)
}

// ---------------------------------------------------------------------------
// Multiplayer survival variant (raw protocol v2 client)
// ---------------------------------------------------------------------------

use ob_math::Aabb;
use ob_protocol::{
    ClientMessage, ContainerKind, EntityKind, ItemStackDto, MAX_FRAME_LEN, PROTOCOL_VERSION, ServerMessage,
    read_message, write_message,
};
use ob_server::{DedicatedServer, NetConfig};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};

const NET_WAIT: Duration = Duration::from_secs(10);
const NET_RECV_TIMEOUT: Duration = Duration::from_secs(5);

struct NetServer {
    handle: Option<DedicatedServer>,
    _dir: TempDir,
    registry: Arc<Registry>,
    config: HostConfig,
    spawn: WorldVec,
}

impl NetServer {
    fn start(label: &str) -> Self {
        let dir = TempDir::new(label);
        let registry = Arc::new(Registry::base());
        let config = config(dir.path());
        let host = Host::open(Arc::clone(&registry), config.clone()).expect("open network world");
        let spawn = host.spawn();
        let net_config = NetConfig {
            listen: "127.0.0.1:0".parse::<SocketAddr>().expect("ephemeral loopback"),
            max_players: 4,
            motd: "1.0 acceptance".to_string(),
            interest_radius_chunks: 2,
            chunks_per_tick: 16,
            save_interval_seconds: 3_600,
            whitelist: Vec::new(),
            game_mode: GameMode::Survival,
        };
        let server = DedicatedServer::start(host, net_config).expect("start acceptance server");
        Self {
            handle: Some(server),
            _dir: dir,
            registry,
            config,
            spawn,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.handle.as_ref().expect("running server").local_addr()
    }

    fn server(&self) -> &DedicatedServer {
        self.handle.as_ref().expect("running server")
    }

    fn stop(&mut self) {
        if let Some(server) = self.handle.take() {
            server.stop().expect("stop saves the world");
        }
    }

    fn reopen(&self) -> Host {
        let mut host = Host::open(Arc::clone(&self.registry), self.config.clone()).expect("reopen saved network world");
        for _ in 0..100 {
            if host.ensure_streaming(256) == 0 {
                break;
            }
        }
        host
    }
}

impl Drop for NetServer {
    fn drop(&mut self) {
        if let Some(server) = self.handle.take() {
            let _ = server.stop();
        }
    }
}

struct NetClient {
    stream: TcpStream,
}

impl NetClient {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect to loopback server");
        stream.set_nodelay(true).expect("nodelay");
        stream.set_read_timeout(Some(NET_RECV_TIMEOUT)).expect("read timeout");
        stream.set_write_timeout(Some(NET_RECV_TIMEOUT)).expect("write timeout");
        Self { stream }
    }

    fn send(&mut self, message: ClientMessage) {
        write_message(&mut self.stream, &message).expect("write client frame");
        self.stream.flush().expect("flush client frame");
    }

    fn recv(&mut self) -> ServerMessage {
        read_message(&mut self.stream, MAX_FRAME_LEN).expect("read server frame")
    }

    fn recv_until(&mut self, what: &str, mut predicate: impl FnMut(&ServerMessage) -> bool) -> ServerMessage {
        self.recv_until_in(what, NET_WAIT, |message| predicate(message))
    }

    fn recv_until_in(
        &mut self,
        what: &str,
        timeout: Duration,
        mut predicate: impl FnMut(&ServerMessage) -> bool,
    ) -> ServerMessage {
        let deadline = Instant::now() + timeout;
        loop {
            let message = self.recv();
            if predicate(&message) {
                return message;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
        }
    }
}

struct NetJoined {
    id: u64,
    inventory: ServerMessage,
    vitals: ServerMessage,
}

fn net_join(client: &mut NetClient, name: &str) -> NetJoined {
    client.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: name.to_string(),
    });
    client.recv_until("server hello", |message| matches!(message, ServerMessage::Hello { .. }));
    let join = client.recv_until(
        "own join",
        |message| matches!(message, ServerMessage::Join { name: joined, .. } if joined == name),
    );
    let ServerMessage::Join { id, .. } = join else {
        unreachable!()
    };
    client.recv_until("player list", |message| {
        matches!(message, ServerMessage::PlayerList { .. })
    });
    let mut inventory = None;
    let mut vitals = None;
    let deadline = Instant::now() + NET_WAIT;
    loop {
        let message = client.recv();
        match message {
            ServerMessage::InventorySync { .. } => inventory = Some(message),
            ServerMessage::PlayerVitals { .. } => vitals = Some(message),
            ServerMessage::EntitySnapshot { .. } => break,
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out waiting for initial survival sync");
    }
    NetJoined {
        id,
        inventory: inventory.expect("initial inventory"),
        vitals: vitals.expect("initial vitals"),
    }
}

fn net_inventory(message: &ServerMessage) -> ([Option<ItemStackDto>; 9], u8, [Option<ItemStackDto>; 4]) {
    match message {
        ServerMessage::InventorySync {
            hotbar,
            selected,
            equipment,
        } => (hotbar.clone(), *selected, equipment.clone()),
        _ => panic!("not an inventory sync"),
    }
}

fn net_count(hotbar: &[Option<ItemStackDto>; 9], item: &str) -> u32 {
    hotbar
        .iter()
        .flatten()
        .filter(|stack| stack.item == item)
        .map(|stack| u32::from(stack.count))
        .sum()
}

fn net_slot(hotbar: &[Option<ItemStackDto>; 9], item: &str) -> Option<u8> {
    hotbar
        .iter()
        .position(|stack| stack.as_ref().is_some_and(|stack| stack.item == item))
        .map(|slot| slot as u8)
}

fn net_wait_slot(server: &NetServer, id: u64, item: ItemId) -> u8 {
    let deadline = Instant::now() + NET_WAIT;
    loop {
        if let Some(player) = server.server().server_player(id)
            && let Some(slot) = player
                .inventory
                .iter()
                .position(|stack| stack.is_some_and(|stack| stack.item == item))
        {
            return slot as u8;
        }
        assert!(Instant::now() < deadline, "item never entered the network inventory");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn net_wait_position(server: &NetServer, id: u64, want: WorldVec) {
    let deadline = Instant::now() + NET_WAIT;
    loop {
        if let Some(player) = server.server().server_player(id)
            && player.pos.distance(want) < 0.05
        {
            return;
        }
        assert!(Instant::now() < deadline, "server never accepted the expected position");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Finds an air cell with a solid neighbour and no player overlap.
fn net_place_target(server: &NetServer, player: WorldVec) -> BlockPos {
    let base = BlockPos::new(
        player.x.floor() as i32,
        player.y.floor() as i32,
        player.z.floor() as i32,
    );
    for dy in 0..=2 {
        for dz in -3..=3 {
            for dx in -3..=3 {
                let pos = base.offset(dx, dy, dz);
                if server.server().block_at(pos) != Some(BlockId::AIR) {
                    continue;
                }
                let eye = WorldVec::new(player.x, player.y + 1.62, player.z);
                if pos.center().distance(eye) > 2.5 {
                    continue;
                }
                if Aabb::block(pos).intersects(&Aabb::new(
                    WorldVec::new(player.x - 0.3, player.y, player.z - 0.3),
                    WorldVec::new(player.x + 0.3, player.y + 1.8, player.z + 0.3),
                )) {
                    continue;
                }
                let supported = [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)]
                    .into_iter()
                    .any(|(lx, ly, lz)| {
                        server
                            .server()
                            .block_at(pos.offset(lx, ly, lz))
                            .is_some_and(|block| block != BlockId::AIR)
                    });
                if supported {
                    return pos;
                }
            }
        }
    }
    panic!("no network placement target near {player:?}");
}

fn find_hand_breakable(server: &NetServer, player: WorldVec) -> BlockPos {
    let base = BlockPos::new(
        player.x.floor() as i32,
        player.y.floor() as i32,
        player.z.floor() as i32,
    );
    let grass = server.registry.id("openblock:grass").expect("grass");
    let dirt = server.registry.id("openblock:dirt").expect("dirt");
    for dy in -2..=0 {
        for dz in -3..=3 {
            for dx in -3..=3 {
                let pos = base.offset(dx, dy, dz);
                let Some(block) = server.server().block_at(pos) else {
                    continue;
                };
                if block != grass && block != dirt {
                    continue;
                }
                if server.server().block_at(pos.offset(0, 1, 0)) != Some(BlockId::AIR) {
                    continue;
                }
                let eye = WorldVec::new(player.x, player.y + 1.62, player.z);
                if pos.center().distance(eye) > 3.0 {
                    continue;
                }
                return pos;
            }
        }
    }
    panic!("no hand-breakable surface block near {player:?}");
}

#[test]
fn multiplayer_survival_join_mine_smelt_fight_death_reconnect() {
    let mut server = NetServer::start("multiplayer");
    let mut alice = NetClient::connect(server.addr());
    let joined = net_join(&mut alice, "alice");
    let id = joined.id;
    let (initial_hotbar, _, _) = net_inventory(&joined.inventory);
    assert!(initial_hotbar.iter().all(Option::is_none));
    match joined.vitals {
        ServerMessage::PlayerVitals {
            health,
            hunger,
            armor_defense,
        } => {
            assert_eq!(health, 20.0);
            assert_eq!(hunger, 20.0);
            assert_eq!(armor_defense, 0);
        }
        _ => unreachable!(),
    }

    // Mine one surface block by hand and pick up its drop, then place it.
    let player = server.server().server_player(id).expect("online").pos;
    let dig = find_hand_breakable(&server, player);
    alice.send(ClientMessage::BlockEdit {
        pos: [dig.x, dig.y, dig.z],
        block: 0,
    });
    alice.recv_until("hand break broadcast", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block: 0, player: owner } if *pos == [dig.x, dig.y, dig.z] && *owner == id)
    });
    let pickup = alice.recv_until("dirt pickup", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if net_count(hotbar, "openblock:dirt") == 1)
    });
    let dirt_slot = net_slot(&net_inventory(&pickup).0, "openblock:dirt").expect("dirt slot");
    alice.send(ClientMessage::SelectSlot { slot: dirt_slot });
    alice.recv_until(
        "dirt selected",
        |message| matches!(message, ServerMessage::SelectedSlot { slot } if *slot == dirt_slot),
    );
    let player = server.server().server_player(id).expect("online").pos;
    let place = net_place_target(&server, player);
    let dirt_block = server.registry.id("openblock:dirt").expect("dirt block");
    alice.send(ClientMessage::BlockEdit {
        pos: [place.x, place.y, place.z],
        block: dirt_block.0,
    });
    alice.recv_until("dirt placement", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block, .. } if *pos == [place.x, place.y, place.z] && *block == dirt_block.0)
    });
    alice.recv_until("dirt consumed", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if net_count(hotbar, "openblock:dirt") == 0)
    });

    // Place a furnace, open it, smelt an iron ingot with coal and take it.
    assert!(server.server().give_network_item(id, "openblock:furnace", 1));
    assert!(server.server().give_network_item(id, "openblock:coal", 1));
    assert!(server.server().give_network_item(id, "openblock:raw_iron", 1));
    let furnace_slot = net_wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:furnace").expect("furnace"),
    );
    let coal_slot = net_wait_slot(&server, id, server.registry.item_id("openblock:coal").expect("coal"));
    let iron_slot = net_wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:raw_iron").expect("raw iron"),
    );
    alice.send(ClientMessage::SelectSlot { slot: furnace_slot });
    alice.recv_until(
        "furnace selected",
        |message| matches!(message, ServerMessage::SelectedSlot { slot } if *slot == furnace_slot),
    );
    let player = server.server().server_player(id).expect("online").pos;
    let furnace_pos = net_place_target(&server, player);
    let furnace_block = server.registry.id("openblock:furnace").expect("furnace block");
    alice.send(ClientMessage::BlockEdit {
        pos: [furnace_pos.x, furnace_pos.y, furnace_pos.z],
        block: furnace_block.0,
    });
    alice.recv_until("furnace placed", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block, .. } if *pos == [furnace_pos.x, furnace_pos.y, furnace_pos.z] && *block == furnace_block.0)
    });
    alice.send(ClientMessage::UseBlock {
        pos: [furnace_pos.x, furnace_pos.y, furnace_pos.z],
    });
    alice.recv_until("furnace opened", |message| {
        matches!(
            message,
            ServerMessage::ContainerOpen {
                kind: ContainerKind::Furnace,
                ..
            }
        )
    });
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(iron_slot),
        to: 100,
        count: 1,
        revision: None,
    });
    alice.recv_until("raw iron in input", |message| match message {
        ServerMessage::ContainerUpdate { slots, .. } => slots
            .first()
            .and_then(Option::as_ref)
            .is_some_and(|stack| stack.item == "openblock:raw_iron"),
        _ => false,
    });
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 101,
        count: 1,
        revision: None,
    });
    alice.recv_until("coal in fuel", |message| match message {
        ServerMessage::ContainerUpdate { slots, .. } => slots
            .get(1)
            .and_then(Option::as_ref)
            .is_some_and(|stack| stack.item == "openblock:coal"),
        _ => false,
    });
    alice.recv_until_in("furnace output", Duration::from_secs(20), |message| match message {
        ServerMessage::ContainerUpdate { slots, .. } => slots
            .get(2)
            .and_then(Option::as_ref)
            .is_some_and(|stack| stack.item == "openblock:iron_ingot"),
        _ => false,
    });
    let free_slot = server
        .server()
        .server_player(id)
        .expect("online")
        .inventory
        .iter()
        .position(Option::is_none)
        .expect("free inventory slot") as u16;
    alice.send(ClientMessage::InventoryMove {
        from: 102,
        to: free_slot,
        count: 1,
        revision: None,
    });
    alice.recv_until("iron ingot taken", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if net_count(hotbar, "openblock:iron_ingot") == 1)
    });
    alice.send(ClientMessage::ContainerClose);
    alice.recv_until("furnace closed", |message| {
        matches!(message, ServerMessage::ContainerClose { .. })
    });

    // Kill a cow with a sword and pick up the replicated drops.
    assert!(server.server().give_network_item(id, "openblock:wooden_sword", 1));
    let sword_slot = net_wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:wooden_sword").expect("sword"),
    );
    alice.send(ClientMessage::SelectSlot { slot: sword_slot });
    alice.recv_until(
        "sword selected",
        |message| matches!(message, ServerMessage::SelectedSlot { slot } if *slot == sword_slot),
    );
    let player = server.server().server_player(id).expect("online").pos;
    let mut cow = None;
    for offset in [[1.2, 0.0, 0.0], [-1.2, 0.0, 0.0], [0.0, 0.0, 1.2], [0.0, 0.0, -1.2]] {
        if let Some(spawned) = server.server().spawn_mob(
            MobKind::Cow,
            [player.x + offset[0], player.y + offset[1], player.z + offset[2]],
        ) {
            cow = Some(spawned);
            break;
        }
    }
    let cow = cow.expect("cow spawns next to the network player");
    alice.recv_until(
        "cow spawn",
        |message| matches!(message, ServerMessage::EntitySpawn { id: spawned, .. } if *spawned == cow),
    );
    let mut killed = false;
    for _ in 0..8 {
        alice.send(ClientMessage::Attack { target: Some(cow) });
        let result = alice.recv_until(
            "cow attack",
            |message| matches!(message, ServerMessage::AttackResult { target, .. } if *target == cow),
        );
        if let ServerMessage::AttackResult { killed: dead, hit, .. } = result {
            assert!(hit, "the cow left attack reach");
            killed = dead;
        }
        if killed {
            break;
        }
    }
    assert!(killed, "the sword must kill the cow");
    alice.recv_until("cow drop", |message| match message {
        ServerMessage::EntitySpawn {
            kind: EntityKind::Item { stack },
            ..
        } => stack.item == "openblock:raw_beef" || stack.item == "openblock:leather",
        _ => false,
    });
    let deadline = Instant::now() + NET_WAIT;
    loop {
        let message = alice.recv();
        if let ServerMessage::InventorySync { hotbar, .. } = &message
            && net_count(hotbar, "openblock:raw_beef") >= 1
            && net_count(hotbar, "openblock:leather") >= 1
        {
            break;
        }
        assert!(Instant::now() < deadline, "cow drops were not picked up");
    }

    // Death closes any open window and respawn restores actionability.
    assert!(server.server().set_network_player_vitals(id, 20.0, 20.0));
    assert!(server.server().damage_network_player(id, 100.0));
    alice.recv_until("death", |message| matches!(message, ServerMessage::Death { .. }));
    assert!(server.server().server_player(id).expect("online").dead);
    let inventory_before_respawn = server.server().server_player(id).expect("online").inventory;
    alice.send(ClientMessage::Respawn);
    alice.recv_until("respawn", |message| {
        matches!(message, ServerMessage::Respawn { health, hunger, .. } if (*health - 20.0).abs() < 0.5 && (*hunger - 20.0).abs() < 0.5)
    });
    let respawned = server.server().server_player(id).expect("online");
    assert!(!respawned.dead);
    assert!(respawned.pos.distance(server.spawn) < 0.05);
    assert_eq!(respawned.inventory, inventory_before_respawn);

    // Reconnect restores vitals, position and inventory by name.
    assert!(server.server().set_network_player_vitals(id, 16.0, 12.0));
    let moved = WorldVec::new(server.spawn.x + 2.0, server.spawn.y, server.spawn.z);
    alice.send(ClientMessage::Move {
        pos: [moved.x, moved.y, moved.z],
        yaw: 0.5,
        pitch: -0.25,
    });
    net_wait_position(&server, id, moved);
    alice.send(ClientMessage::Disconnect);
    drop(alice);
    let deadline = Instant::now() + NET_WAIT;
    while server.server().player_count() != 0 {
        assert!(Instant::now() < deadline, "player was not removed after disconnect");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut reconnect = NetClient::connect(server.addr());
    let rejoined = net_join(&mut reconnect, "alice");
    let (hotbar, _, _) = net_inventory(&rejoined.inventory);
    assert_eq!(net_count(&hotbar, "openblock:iron_ingot"), 1, "ingot did not restore");
    assert!(net_count(&hotbar, "openblock:raw_beef") >= 1, "beef did not restore");
    assert!(net_count(&hotbar, "openblock:leather") >= 1, "leather did not restore");
    match rejoined.vitals {
        ServerMessage::PlayerVitals { health, hunger, .. } => {
            assert!((health - 16.0).abs() < 0.5, "health restored as {health}");
            assert!((hunger - 12.0).abs() < 0.5, "hunger restored as {hunger}");
        }
        _ => unreachable!(),
    }
    let restored = server.server().server_player(rejoined.id).expect("online");
    assert!(restored.pos.distance(moved) < 0.05, "position did not restore");
    assert_eq!(
        restored.inventory, inventory_before_respawn,
        "inventory changed across reconnect"
    );
    reconnect.send(ClientMessage::Disconnect);
    drop(reconnect);
    server.stop();

    let host = server.reopen();
    assert!(
        host.furnace_at(furnace_pos).is_some(),
        "furnace block entity did not persist"
    );
    let saved = host.saved_network_player("alice").expect("saved network player");
    assert!((saved.health - 16.0).abs() < 0.5);
    assert!((saved.hunger - 12.0).abs() < 0.5);
    assert!(saved.pos.distance(moved) < 0.05);
    let saved_iron = saved
        .inventory
        .iter()
        .flatten()
        .filter(|stack| stack.item == host.registry().item_id("openblock:iron_ingot").unwrap())
        .map(|stack| stack.count)
        .sum::<u8>();
    assert_eq!(saved_iron, 1);
}

// ---------------------------------------------------------------------------
// Edge/fuzz pass: full containers, death and tool break boundaries
// ---------------------------------------------------------------------------

/// Positions the player directly above `target` and aims straight down.
fn aim_down_at(host: &mut Host, target: BlockPos) {
    let above = target.offset(0, 1, 0);
    assert!(host.world_mut().set_block(above, BlockId::AIR));
    assert!(host.world_mut().set_block(target.offset(0, 2, 0), BlockId::AIR));
    let player = host.player_mut();
    player.pos = WorldVec::new(target.x as f64 + 0.5, target.y as f64 + 1.0, target.z as f64 + 0.5);
    player.velocity = WorldVec::ZERO;
    player.on_ground = false;
    player.yaw = 0.0;
    player.pitch = -std::f32::consts::FRAC_PI_2;
    let hit = host
        .world()
        .raycast(host.player().eye(), host.player().forward(), REACH)
        .expect("aim_down_at ray");
    assert_eq!(hit.block, target);
}

#[test]
fn full_inventory_crafting_armor_and_tool_break_edges() {
    let (_dir, mut host) = open_survival_host("full-edges");
    let (sx, sy, sz) = build_yard(&mut host);
    let center = BlockPos::new(sx, sy, sz);
    let stone = item_id(&host, "openblock:stone");
    let planks = item_id(&host, "openblock:planks");
    let log = item_id(&host, "openblock:log");

    // Eight full stone stacks plus one log = a completely full inventory. The
    // planks recipe consumes the log and therefore frees a slot, so it must
    // succeed rather than reporting "inventory full".
    assert_eq!(host.give_item(stone, 255), 0);
    assert_eq!(host.give_item(stone, 255), 0);
    assert_eq!(host.give_item(stone, 2), 0);
    assert_eq!(host.give_item(log, 1), 0);
    assert!(host.inventory().iter().all(Option::is_some));
    let outcome = craft(&mut host, "openblock:planks");
    assert!(outcome.crafted, "{}", outcome.message);
    assert_eq!(item_count(&host, planks), 4);
    assert_eq!(item_count(&host, log), 0);
    assert_eq!(item_count(&host, stone), 512);
    assert!(
        host.inventory().iter().all(Option::is_some),
        "the freed slot holds planks"
    );

    // A full inventory where consuming the ingredient frees no slot must fail
    // without consuming anything.
    let before = *host.inventory();
    let outcome = craft(&mut host, "openblock:sticks");
    assert!(!outcome.crafted);
    assert_eq!(outcome.message, "inventory full");
    assert_eq!(*host.inventory(), before);

    // Armor swap while full: equipping from a slot frees that slot, so the
    // displaced piece always fits; unequipping into a full inventory fails.
    let helmet = item_id(&host, "openblock:iron_helmet");
    let helmet_slot = find_slot(&host, planks).unwrap();
    let replaced = host.set_inventory_slot(helmet_slot, host.make_stack(helmet, 1));
    assert!(replaced.is_some());
    assert!(
        host.equip_armor(helmet_slot),
        "equipping the helmet frees its source slot"
    );
    assert_eq!(host.equipment_slot(ArmorSlot::Helmet).unwrap().item, helmet);
    assert_eq!(host.armor_defense(), 2);
    assert_eq!(host.give_item(stone, 64), 0, "refill the slot the helmet vacated");
    assert!(host.inventory().iter().all(Option::is_some), "inventory is full again");
    assert!(
        !host.equip_armor(find_slot(&host, stone).unwrap()),
        "stone is not armor"
    );
    assert!(
        !host.unequip_armor(ArmorSlot::Helmet),
        "a full inventory rejects unequip"
    );
    assert!(
        host.equipment_slot(ArmorSlot::Helmet).is_some(),
        "failed unequip must not change state"
    );
    assert_eq!(host.inventory().iter().filter(|slot| slot.is_none()).count(), 0);

    // Tool break at one durability with a full inventory: the broken tool slot
    // becomes empty and the resulting drop fits there.
    let pick = item_id(&host, "openblock:wooden_pickaxe");
    let pick_slot = find_slot(&host, stone).unwrap();
    let replaced = host.set_inventory_slot(
        pick_slot,
        host.make_stack(pick, 1).map(|stack| stack.with_durability(1)),
    );
    assert!(replaced.is_some());
    host.set_hotbar_selection(pick_slot);
    let target = BlockPos::new(center.x + 2, sy - 1, center.z + 2);
    aim_down_at(&mut host, target);
    let outcome = host.apply_action_with_outcome(&BlockAction {
        break_block: true,
        place_block: false,
        place: BlockId::AIR,
    });
    assert!(outcome.changed);
    assert_eq!(
        outcome.tool_broke,
        Some(pick),
        "the last durability must break the tool"
    );
    assert_eq!(host.selected_stack(), None);
    // Empty hands still break dirt, so walking into a drop works; the drop is
    // already collected by mine_block-like ticking below via the public host.
    assert_eq!(host.item_entities().len(), 1);
    let dirt = item_id(&host, "openblock:dirt");
    for _ in 0..40 {
        host.tick(&PlayerInput::default());
    }
    assert_eq!(item_count(&host, dirt), 1, "the drop fits the slot the tool vacated");
    assert_eq!(host.item_entities().len(), 0);
}

#[test]
fn death_save_reload_keeps_pending_items_and_dead_state() {
    let (_dir, mut host) = open_survival_host("death-save");
    let spawn = host.spawn();
    let dirt = item_id(&host, "openblock:dirt");
    host.spawn_item_entity(
        WorldVec::new(spawn.x, spawn.y + 0.1, spawn.z),
        host.make_stack(dirt, 2).expect("dirt stack"),
    );
    host.damage(100.0);
    assert!(host.is_dead());
    for _ in 0..30 {
        host.tick(&PlayerInput::default());
    }
    assert_eq!(host.item_entities().len(), 1, "dead players cannot pick items up");
    assert_eq!(item_count(&host, dirt), 0);

    let expected_hash = host.state_hash();
    let expected_items = host.item_entities().to_vec();
    host.save().expect("save a dead player with pending items");
    drop(host);

    let mut reopened = Host::open(Arc::new(Registry::base()), config(_dir.path())).expect("reopen dead world");
    reopened.set_game_mode(GameMode::Survival);
    converge(&mut reopened);
    assert!(reopened.is_dead(), "dead state must round-trip");
    assert_eq!(reopened.player().health, 0.0);
    assert_eq!(reopened.item_entities(), expected_items.as_slice());
    assert_eq!(
        reopened.state_hash(),
        expected_hash,
        "mid-death save hash must round-trip"
    );

    reopened.respawn();
    assert!(!reopened.is_dead());
    assert_eq!(reopened.player().health, 20.0);
    for _ in 0..80 {
        reopened.tick(&PlayerInput::default());
        if reopened.item_entities().is_empty() {
            break;
        }
    }
    assert!(
        reopened.item_entities().is_empty(),
        "respawned player can collect pending drops"
    );
    assert_eq!(item_count(&reopened, dirt), 2);
}

#[test]
fn full_chest_break_drops_every_slot() {
    let (_dir, mut host) = open_survival_host("full-chest");
    let (sx, sy, sz) = build_yard(&mut host);
    let center = BlockPos::new(sx, sy, sz);
    let chest_pos = BlockPos::new(sx - 3, sy, sz);
    // This edge test focuses on the full chest/break path; grant the chest
    // block directly rather than repeating the whole table/planks chain.
    let chest_item = item_id(&host, "openblock:chest");
    assert_eq!(host.give_item(chest_item, 1), 0);
    select_item(&mut host, chest_item);
    place_block_item(&mut host, chest_item, chest_pos, center);
    let stone = item_id(&host, "openblock:stone");
    for slot in 0..CHEST_SLOTS {
        let stack = host.make_stack(stone, 64).expect("stone stack");
        assert!(host.put_chest_slot(chest_pos, slot, stack).is_none());
    }
    assert_eq!(host.chest_at(chest_pos).unwrap().contents().len(), CHEST_SLOTS);
    let outcome = mine_block(&mut host, chest_pos);
    assert_eq!(outcome.drops.len(), CHEST_SLOTS + 1, "27 stacks plus the chest item");
    assert_eq!(
        outcome.drops.iter().filter(|stack| stack.item == stone).count(),
        CHEST_SLOTS
    );
    assert!(
        host.chest_at(chest_pos).is_none(),
        "broken chest record must be removed"
    );
    assert_eq!(host.world().get_block(chest_pos), BlockId::AIR);
    // The player's nine-slot inventory can only accept part of the 27 stacks,
    // but at least one chest-content stack must have been collected; the rest
    // remain as item entities.
    let stone_count = item_count(&host, stone);
    assert!(stone_count > 0, "at least some chest contents were collected");
    assert!(host.item_entities().iter().any(|entity| entity.stack.item == stone));
}

#[test]
fn dead_survival_player_cannot_break_or_place() {
    let (_dir, mut host) = open_survival_host("dead-actions");
    let (sx, sy, sz) = build_yard(&mut host);
    let target = BlockPos::new(sx + 2, sy - 1, sz + 2);
    aim_down_at(&mut host, target);
    host.player_mut().dead = true;
    host.player_mut().health = 0.0;
    let block_before = host.world().get_block(target);
    let outcome = host.apply_action_with_outcome(&BlockAction {
        break_block: true,
        place_block: false,
        place: BlockId::AIR,
    });
    assert!(!outcome.changed, "a dead player must not break blocks");
    assert_eq!(host.world().get_block(target), block_before);

    // Placement is rejected too. Stand at the yard centre and aim at a distant
    // support's top face so the placement cell is not under the player (which
    // would make the placement fail for overlap even without a death guard).
    let dirt = item_id(&host, "openblock:dirt");
    assert_eq!(host.give_item(dirt, 1), 0);
    select_item(&mut host, dirt);
    let support = BlockPos::new(sx - 3, sy - 1, sz);
    assert!(host.world().is_solid(support));
    let placement_target = support.offset(0, 1, 0);
    assert_eq!(host.world().get_block(placement_target), BlockId::AIR);
    {
        let player = host.player_mut();
        player.pos = WorldVec::new(sx as f64 + 0.5, sy as f64, sz as f64 + 0.5);
        player.velocity = WorldVec::ZERO;
        player.on_ground = true;
    }
    let eye = host.player().eye();
    let aim = WorldVec::new(support.x as f64 + 0.5, support.y as f64 + 0.999, support.z as f64 + 0.5);
    aim_at(&mut host, eye, aim);
    let hit = host
        .world()
        .raycast(host.player().eye(), host.player().forward(), REACH)
        .expect("dead placement ray");
    assert_eq!(hit.block, support);
    let outcome = host.apply_action_with_outcome(&BlockAction {
        break_block: false,
        place_block: true,
        place: block_id(&host, "openblock:dirt"),
    });
    assert!(!outcome.changed, "a dead player must not place blocks");
    assert_eq!(host.world().get_block(placement_target), BlockId::AIR);
}

#[test]
fn malformed_spawn_sidecar_is_recomputed_safely() {
    let dir = TempDir::new("bad-spawn");
    let config = config(dir.path());
    let registry = Arc::new(Registry::base());
    let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("fresh world");
    let good = host.spawn();
    host.save().expect("write the initial spawn sidecar");
    drop(host);

    let spawn_file = dir.path().join("worlds").join("acceptance").join("spawn.txt");
    std::fs::write(&spawn_file, "1e308 -1e308 1e308\n").expect("write a hostile spawn file");
    let mut reopened = Host::open(registry, config).expect("malformed spawn must not fail the open");
    reopened.set_game_mode(GameMode::Survival);
    let spawn = reopened.spawn();
    let (sx, sy, sz) = (spawn.x.floor() as i32, spawn.y.floor() as i32, spawn.z.floor() as i32);
    assert!(spawn.x.is_finite() && spawn.y.is_finite() && spawn.z.is_finite());
    assert_eq!(spawn, good, "invalid spawn text should resolve the same safe column");
    assert!(reopened.world().is_solid(BlockPos::new(sx, sy - 1, sz)));
    assert_eq!(reopened.world().get_block(BlockPos::new(sx, sy, sz)), BlockId::AIR);
    assert_eq!(reopened.world().get_block(BlockPos::new(sx, sy + 1, sz)), BlockId::AIR);
}
