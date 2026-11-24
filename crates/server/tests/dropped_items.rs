//! End-to-end dropped-item flows through the public host API.
//!
//! These tests deliberately use only exported items (`Host`, `ItemEntity`,
//! `ItemStack`) so they exercise the same surface the client uses.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ob_content::{BlockId, ItemId, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_server::{GameMode, Host, HostConfig, ItemStack};
use ob_sim::{BlockAction, PlayerInput};

/// Test-only temporary data directory removed on drop (best effort).
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
            "openblock-server-it-{label}-{}-{nanos}-{unique}",
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

fn test_config(data_dir: &Path, seed: u64) -> HostConfig {
    HostConfig {
        seed,
        world_name: "test".to_string(),
        data_dir: Some(data_dir.to_path_buf()),
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

fn open_survival(data_dir: &Path, seed: u64) -> Host {
    let registry = Arc::new(Registry::base());
    let mut host = Host::open(registry, test_config(data_dir, seed)).expect("host opens on a fresh world");
    host.set_game_mode(GameMode::Survival);
    converge(&mut host);
    host
}

fn break_action() -> BlockAction {
    BlockAction {
        break_block: true,
        place_block: false,
        place: BlockId::AIR,
    }
}

fn item_count(host: &Host, item: ItemId) -> u32 {
    host.inventory()
        .iter()
        .flatten()
        .filter(|stack| stack.item == item)
        .map(|stack| u32::from(stack.count))
        .sum()
}

/// Places a grass block at a high, empty cell in front of the camera, aims at
/// it and returns `(block_pos, centre)` after the break.
///
/// The target is high above the terrain so the platform added afterwards is the
/// only thing near it; the player is moved to `centre` before ticking so the
/// pickup radius is unambiguous.
fn break_high_grass(host: &mut Host) -> (BlockPos, WorldVec) {
    let spawn = host.player().pos;
    let target = BlockPos::new(spawn.x.floor() as i32, 200, spawn.z.floor() as i32 - 2);
    let grass = host.registry().id("openblock:grass").expect("grass block");
    let stone = host.registry().id("openblock:stone").expect("stone block");

    // Clear the ray path and install the break target.
    for z in [target.z, target.z + 1, target.z + 2] {
        assert!(
            host.world_mut()
                .set_block(BlockPos::new(target.x, target.y, z), BlockId::AIR),
            "ray-clearing position must be loaded"
        );
    }
    assert!(host.world_mut().set_block(target, grass), "target must be loaded");
    {
        let player = host.player_mut();
        player.pos = WorldVec::new(target.x as f64 + 0.5, target.y as f64 - 1.12, target.z as f64 + 2.5);
        player.velocity = WorldVec::ZERO;
        player.on_ground = false;
        player.yaw = 0.0;
        player.pitch = 0.0;
    }

    let outcome = host.apply_action_with_outcome(&break_action());
    assert!(outcome.changed, "scripted break failed");
    assert_eq!(outcome.broke, Some(grass));
    assert_eq!(
        outcome.drops,
        vec![ItemStack::new(
            host.registry().item_id("openblock:dirt").expect("dirt item"),
            1
        )],
        "grass drops dirt"
    );
    let centre = WorldVec::new(target.x as f64 + 0.5, target.y as f64 + 0.25, target.z as f64 + 0.5);
    assert_eq!(host.item_entities().len(), 1, "one entity spawned");
    assert_eq!(host.item_entities()[0].pos, centre);
    assert!(
        host.inventory().iter().all(Option::is_none),
        "breaking must not put loot directly into the inventory"
    );

    // Stand on a platform directly under the dropped item so the player is
    // inside the pickup volume while the item falls to its rest height.
    assert!(
        host.world_mut()
            .set_block(BlockPos::new(target.x, target.y - 1, target.z), stone),
        "support block must be loaded"
    );
    {
        let player = host.player_mut();
        player.pos = WorldVec::new(target.x as f64 + 0.5, target.y as f64, target.z as f64 + 0.5);
        player.velocity = WorldVec::ZERO;
        player.on_ground = true;
        player.yaw = 0.0;
        player.pitch = 0.0;
    }
    (target, centre)
}

#[test]
fn scripted_break_is_picked_up_after_ticks() {
    let dir = TempDir::new("break-pickup");
    let mut host = open_survival(dir.path(), 42);
    let (_, centre) = break_high_grass(&mut host);
    assert_eq!(host.item_entities()[0].pos, centre);

    for _ in 0..40 {
        host.tick(&PlayerInput::default());
    }

    let dirt = host.registry().item_id("openblock:dirt").expect("dirt item");
    assert_eq!(item_count(&host, dirt), 1, "the dropped item is collected");
    assert!(host.item_entities().is_empty(), "the collected entity is removed");
    let events = host.take_pickup_events();
    assert_eq!(events.len(), 1, "exactly one pickup event");
    assert_eq!(events[0], ItemStack::new(dirt, 1));
    assert!(host.take_pickup_events().is_empty(), "the event queue drains");
}

#[test]
fn break_then_save_reload_keeps_entity_without_ticking() {
    let dir = TempDir::new("break-persistence");
    let registry = Arc::new(Registry::base());
    let config = test_config(dir.path(), 43);
    let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens on a fresh world");
    host.set_game_mode(GameMode::Survival);
    converge(&mut host);
    let (_, centre) = break_high_grass(&mut host);
    // The scripted break teleports the player one chunk over; stream the new
    // surroundings before saving so the reload's automatic streaming set
    // matches the saved chunk set exactly (the state hash covers loaded
    // chunks).
    converge(&mut host);

    let entities_before = host.item_entities().to_vec();
    let hash_before = host.state_hash();
    assert_eq!(entities_before.len(), 1);
    assert_eq!(entities_before[0].pos, centre);
    assert_eq!(entities_before[0].age, 0.0);
    host.save().expect("save succeeds");

    let items_path = dir.path().join("worlds").join("test").join("items.txt");
    let items_text = std::fs::read_to_string(&items_path).expect("items.txt is written");
    assert_eq!(items_text.lines().count(), 1);
    assert_eq!(items_text.split_whitespace().count(), 11);
    assert!(items_text.contains("openblock:dirt"));
    drop(host);

    let mut reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
    assert_eq!(reopened.item_entities(), entities_before.as_slice());
    assert_eq!(reopened.state_hash(), hash_before, "the entity state must round-trip");
    assert!(reopened.take_pickup_events().is_empty(), "no pickup happened");
}
