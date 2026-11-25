//! Public-API integration tests for the 1.2 gameplay slice.
//!
//! These exercise the same `Host` surface a single-player client uses
//! (`use_block_at`, `tick`, `save`, `respawn`) without reaching into private
//! fields. Loopback protocol coverage lives in `net_survival.rs`; unit-level
//! timing coverage lives in `ob_server`'s `crops`/`tests` modules.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ob_content::{BlockId, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_server::{GameMode, Host, HostConfig, SLEEP_FAST_FORWARD_TICKS};
use ob_sim::PlayerInput;

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
            "openblock-gameplay-{label}-{}-{nanos}-{unique}",
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

fn config(dir: &Path, seed: u64) -> HostConfig {
    HostConfig {
        seed,
        world_name: "gameplay".to_string(),
        data_dir: Some(dir.to_path_buf()),
        render_distance_chunks: 2,
        generate_budget: 16,
    }
}

fn converge(host: &mut Host) {
    for _ in 0..200 {
        if host.ensure_streaming(64) == 0 {
            return;
        }
    }
    panic!("streaming did not converge");
}

fn platform(host: &mut Host, x: i32, y: i32, z: i32) -> BlockPos {
    let stone = host.registry().id_or_air("openblock:stone");
    for dz in -1..=1 {
        for dx in -1..=1 {
            assert!(host.world_mut().set_block(BlockPos::new(x + dx, y, z + dz), stone));
            assert!(
                host.world_mut()
                    .set_block(BlockPos::new(x + dx, y + 1, z + dz), BlockId::AIR)
            );
        }
    }
    BlockPos::new(x, y, z)
}

fn select(host: &mut Host, name: &str, count: u8) {
    let item = host.registry().item_id(name).unwrap_or_else(|| panic!("item {name}"));
    if !host.inventory().iter().flatten().any(|stack| stack.item == item) {
        assert_eq!(host.give_item(item, count), 0);
    }
    let slot = host
        .inventory()
        .iter()
        .position(|stack| stack.is_some_and(|stack| stack.item == item))
        .expect("item in inventory");
    host.set_hotbar_selection(slot);
}

#[test]
fn farming_and_torch_round_trip_through_the_public_host() {
    let dir = TempDir::new("farm");
    let mut host = Host::open(Arc::new(Registry::base()), config(dir.path(), 101)).expect("open");
    converge(&mut host);
    host.set_game_mode(GameMode::Survival);
    let spawn = host.spawn();
    let x = spawn.x.floor() as i32 + 2;
    let z = spawn.z.floor() as i32;
    let floor = platform(&mut host, x, 100, z);
    let dirt = host.registry().id_or_air("openblock:dirt");
    assert!(host.world_mut().set_block(floor, dirt));
    let crop_pos = floor.offset(0, 1, 0);
    host.player_mut().pos = WorldVec::new(x as f64 + 0.5, 101.0, z as f64 + 0.5);

    select(&mut host, "openblock:wood_hoe", 1);
    assert!(host.use_block_at(floor).expect("till").changed);
    // Guarantee crop light regardless of any tree canopy above the test column.
    let torch = host.registry().id_or_air("openblock:torch");
    assert!(host.world_mut().set_block(BlockPos::new(x + 1, 101, z), torch));
    select(&mut host, "openblock:seeds", 8);
    assert!(host.use_block_at(floor).expect("plant").changed);
    assert_eq!(host.crop_count(), 1);

    // A torch adjacent to the crop keeps it growing through the night; the
    // level 14 source also gives us a light-hash/migration check.
    let water = host.registry().id_or_air("openblock:water");
    assert!(host.world_mut().set_block(BlockPos::new(x + 3, 100, z), water));
    host.set_tick_count(19);
    host.tick(&PlayerInput::default());
    assert!(host.crops()[&crop_pos].hydrated);
    assert!(host.world().block_light(BlockPos::new(x + 1, 101, z)) >= 14);

    let hash = host.state_hash();
    let light_hash = host.world().light_hash();
    host.save().expect("save");
    drop(host);
    let mut reopened = Host::open(Arc::new(Registry::base()), config(dir.path(), 101)).expect("reopen");
    converge(&mut reopened);
    reopened.set_game_mode(GameMode::Survival);
    assert_eq!(reopened.crop_count(), 1);
    assert_eq!(reopened.state_hash(), hash);
    assert_eq!(reopened.world().light_hash(), light_hash);

    // Mature harvest through the same public use path.
    let wheat_3 = reopened.registry().id_or_air("openblock:wheat_3");
    assert!(reopened.world_mut().set_block(crop_pos, wheat_3));
    let outcome = reopened.use_block_at(crop_pos).expect("harvest");
    let wheat = reopened.registry().item_id("openblock:wheat").expect("wheat");
    assert!(outcome.drops.iter().any(|drop| drop.item == wheat && drop.count == 1));
}

#[test]
fn sleep_fast_forwards_sets_respawn_and_survives_reload() {
    let dir = TempDir::new("sleep");
    let mut host = Host::open(Arc::new(Registry::base()), config(dir.path(), 102)).expect("open");
    converge(&mut host);
    let spawn = host.spawn();
    let x = spawn.x.floor() as i32 + 2;
    let z = spawn.z.floor() as i32;
    let floor = platform(&mut host, x, 100, z);
    let bed_foot = host.registry().id_or_air("openblock:bed_foot");
    let bed_head = host.registry().id_or_air("openblock:bed_head");
    let foot = floor.offset(0, 1, 0);
    assert!(host.world_mut().set_block(foot, bed_foot));
    assert!(host.world_mut().set_block(foot.offset(0, 0, 1), bed_head));
    host.player_mut().pos = WorldVec::new(x as f64 + 0.5, 102.0, z as f64 + 0.5);
    host.set_tick_count(13_500);
    assert!(host.use_block_at(foot).expect("sleep").changed);
    assert!(host.sleeping());
    for _ in 0..SLEEP_FAST_FORWARD_TICKS {
        host.tick(&PlayerInput::default());
    }
    assert!(!host.sleeping());
    assert_eq!(host.day_phase(), 0);
    let point = host.respawn_point().expect("respawn point");
    assert_eq!(point, WorldVec::new(x as f64 + 0.5, 102.0, z as f64 + 0.5));
    let hash = host.state_hash();
    host.save().expect("save");
    drop(host);
    let mut reopened = Host::open(Arc::new(Registry::base()), config(dir.path(), 102)).expect("reopen");
    converge(&mut reopened);
    assert_eq!(reopened.respawn_point(), Some(point));
    assert_eq!(reopened.state_hash(), hash);

    // Death uses the personal point rather than the world spawn.
    reopened.set_game_mode(GameMode::Survival);
    reopened.player_mut().pos = WorldVec::new(x as f64 + 0.5, 130.0, z as f64 + 0.5);
    reopened.damage(1_000.0);
    assert!(reopened.is_dead());
    reopened.respawn();
    assert_eq!(reopened.player().pos, point);
}

#[test]
fn gameplay_light_rule_suppresses_hostiles_and_dirty_sections_are_unique() {
    let dir = TempDir::new("light");
    let mut host = Host::open(Arc::new(Registry::base()), config(dir.path(), 103)).expect("open");
    converge(&mut host);
    let spawn = host.spawn();
    let x = spawn.x.floor() as i32;
    let z = spawn.z.floor() as i32;
    let stone = host.registry().id_or_air("openblock:stone");
    for dz in -10..=10 {
        for dx in -10..=10 {
            assert!(host.world_mut().set_block(BlockPos::new(x + dx, 100, z + dz), stone));
            assert!(host.world_mut().set_block(BlockPos::new(x + dx, 105, z + dz), stone));
        }
    }
    for dy in 101..=104 {
        for d in -10..=10 {
            assert!(host.world_mut().set_block(BlockPos::new(x + d, dy, z - 10), stone));
            assert!(host.world_mut().set_block(BlockPos::new(x + d, dy, z + 10), stone));
            assert!(host.world_mut().set_block(BlockPos::new(x - 10, dy, z + d), stone));
            assert!(host.world_mut().set_block(BlockPos::new(x + 10, dy, z + d), stone));
        }
    }
    let torch = host.registry().id_or_air("openblock:torch");
    let torch_pos = BlockPos::new(x, 101, z);
    let dark = BlockPos::new(x + 9, 101, z + 9);
    assert!(host.world_mut().set_block(torch_pos, torch));
    for phase in [12_600, 13_500, 15_000, 18_000, 22_000] {
        host.set_tick_count(phase);
        assert!(!host.hostile_spawn_light_ok(torch_pos));
        assert!(host.hostile_spawn_light_ok(dark));
    }
    assert!(host.world_mut().set_block(torch_pos, BlockId::AIR));
    host.set_tick_count(15_000);
    assert!(host.hostile_spawn_light_ok(torch_pos));

    let _ = host.take_dirty_sections();
    let chunk = BlockPos::new(x, 0, z).chunk();
    let near_border = BlockPos::new(chunk.x * 16 + 14, 110, chunk.z * 16 + 8);
    assert!(host.world_mut().set_block(near_border, torch));
    let dirty = host.take_dirty_sections();
    let unique: std::collections::HashSet<_> = dirty.iter().copied().collect();
    assert_eq!(unique.len(), dirty.len());
    assert!(dirty.iter().any(|section| section.x == chunk.x + 1));
}
#[test]
fn use_block_rejections_name_the_failed_rule() {
    let dir = TempDir::new("reasons");
    let mut host = Host::open(Arc::new(Registry::base()), config(dir.path(), 104)).expect("open");
    converge(&mut host);
    host.set_game_mode(GameMode::Survival);
    let spawn = host.spawn();
    let x = spawn.x.floor() as i32 + 2;
    let z = spawn.z.floor() as i32;
    let floor = platform(&mut host, x, 100, z);
    let dirt = host.registry().id_or_air("openblock:dirt");
    assert!(host.world_mut().set_block(floor, dirt));
    host.player_mut().pos = WorldVec::new(x as f64 + 0.5, 101.0, z as f64 + 0.5);

    // Empty hand on grass/dirt.
    assert_eq!(host.use_block_at(floor).expect_err("no hoe"), "needs_hoe");

    select(&mut host, "openblock:wood_hoe", 1);
    assert!(host.use_block_at(floor).expect("till").changed);
    // Hoe selected but no seeds.
    assert_eq!(host.use_block_at(floor).expect_err("no seeds"), "needs_seeds");

    select(&mut host, "openblock:seeds", 8);
    let stone = host.registry().id_or_air("openblock:stone");
    assert!(host.world_mut().set_block(floor.offset(0, 1, 0), stone));
    assert_eq!(host.use_block_at(floor).expect_err("occupied"), "occupied");
    assert!(host.world_mut().set_block(floor.offset(0, 1, 0), BlockId::AIR));

    // Torch and far-away interactions are rejected without changing anything.
    let torch = host.registry().id_or_air("openblock:torch");
    assert!(host.world_mut().set_block(floor.offset(0, 0, 1), stone));
    let torch_pos = floor.offset(0, 1, 1);
    assert!(host.world_mut().set_block(torch_pos, torch));
    assert_eq!(host.use_block_at(torch_pos).expect_err("torch"), "not_interactable");
    host.player_mut().pos = WorldVec::new(x as f64 + 20.5, 101.0, z as f64 + 0.5);
    assert_eq!(host.use_block_at(floor).expect_err("far"), "out_of_reach");
}
