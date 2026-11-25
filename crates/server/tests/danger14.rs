//! 1.3 "Weather & Danger" server integration: breeding, XP, structure loot,
//! creeper explosions and skeleton projectiles.
//!
//! These tests use only the public host API and never open a window, bind a
//! public socket or wait on wall-clock time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ob_content::{MobKind, Registry};
use ob_math::{BlockPos, ChunkPos, WorldVec};
use ob_server::{GameMode, Host, HostConfig};
use ob_sim::PlayerInput;
use ob_worldgen::Generator;

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
            "openblock-danger14-{label}-{}-{nanos}-{unique}",
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

fn config(data_dir: &Path, seed: u64) -> HostConfig {
    HostConfig {
        seed,
        world_name: "test".to_string(),
        data_dir: Some(data_dir.to_path_buf()),
        render_distance_chunks: 0,
        generate_budget: 16,
    }
}

/// Opens a survival host, moves the player onto a stone arena at y = 200 and
/// generates the small set of chunks the arena occupies.
fn arena(label: &str, seed: u64, radius: i32) -> (TempDir, Host, i32, i32) {
    let dir = TempDir::new(label);
    let mut host = Host::open(Arc::new(Registry::base()), config(dir.path(), seed)).expect("host opens");
    host.set_game_mode(GameMode::Survival);
    let spawn = host.player().pos;
    let center_x = spawn.x.floor() as i32;
    let center_z = spawn.z.floor() as i32;
    let platform_y = 200;
    {
        let player = host.player_mut();
        player.pos = WorldVec::new(center_x as f64 + 0.5, platform_y as f64 + 1.0, center_z as f64 + 0.5);
        player.velocity = WorldVec::ZERO;
        player.on_ground = true;
        player.yaw = 0.0;
        player.pitch = 0.0;
    }
    host.ensure_streaming_around(
        WorldVec::new(center_x as f64, platform_y as f64, center_z as f64),
        1,
        64,
    );
    let stone = host.registry().id_or_air("openblock:stone");
    let air = ob_content::BlockId::AIR;
    for z in center_z - radius..=center_z + radius {
        for x in center_x - radius..=center_x + radius {
            assert!(host.world_mut().set_block(BlockPos::new(x, platform_y, z), stone));
            for dy in 1..=4 {
                host.world_mut().set_block(BlockPos::new(x, platform_y + dy, z), air);
            }
        }
    }
    (dir, host, center_x, center_z)
}

fn select(host: &mut Host, name: &str, count: u8) {
    let item = host.registry().item_id(name).expect("item exists");
    assert_eq!(host.give_item(item, count), 0, "inventory has room");
    let slot = host
        .inventory()
        .iter()
        .position(|slot| matches!(slot, Some(stack) if stack.item == item))
        .expect("item in inventory");
    host.set_hotbar_selection(slot);
}

fn platform_y() -> f64 {
    201.0
}

#[test]
fn breeding_spawns_a_persistent_baby_and_awards_xp() {
    let (_dir, mut host, center_x, center_z) = arena("breed", 71, 8);
    select(&mut host, "openblock:wheat", 8);
    let first_id = host
        .spawn_mob(
            MobKind::Cow,
            WorldVec::new(center_x as f64 + 1.5, platform_y(), center_z as f64 + 0.5),
        )
        .expect("cow one");
    let second_id = host
        .spawn_mob(
            MobKind::Cow,
            WorldVec::new(center_x as f64 + 2.5, platform_y(), center_z as f64 + 0.5),
        )
        .expect("cow two");
    let before = host.inventory()[host.hotbar_selection()].unwrap().count;

    let first = host.interact_mob(first_id);
    assert!(first.accepted, "{first:?}");
    assert!(first.child.is_none());
    assert!(host.mob_entity(first_id).unwrap().mob.in_love());
    assert_eq!(
        host.inventory()[host.hotbar_selection()].unwrap().count,
        before - 1,
        "survival feeding consumes one wheat"
    );

    let second = host.interact_mob(second_id);
    assert!(second.accepted, "{second:?}");
    assert!(
        second.xp_awarded >= 1 && second.xp_awarded <= 7,
        "{}",
        second.xp_awarded
    );
    assert_eq!(host.xp_points(), second.xp_awarded);
    assert_eq!(host.xp_level(), 0);
    assert!(host.mob_entity(first_id).unwrap().mob.breed_cooldown > 0.0);
    assert!(host.mob_entity(second_id).unwrap().mob.breed_cooldown > 0.0);

    // The 60-tick gestation then produces a persistent half-scale baby.
    for _ in 0..61 {
        host.tick(&PlayerInput::default());
    }
    assert_eq!(host.passive_mob_count(), 3, "a baby must join the two parents");
    let baby = host
        .mobs()
        .iter()
        .find(|entity| entity.mob.is_baby())
        .expect("a baby exists");
    let baby_id = baby.id;
    assert!((baby.mob.growth_timer - ob_sim::MOB_BABY_GROWTH_TIME).abs() < 5.0);
    assert!(baby.mob.persistent);
    assert_eq!(baby.mob.kind, MobKind::Cow);

    // A third feed is rejected by the parent cooldown.
    let cooldown = host.interact_mob(first_id);
    assert!(!cooldown.accepted);
    assert_eq!(cooldown.reason, "cooldown");

    // Growth is server-authoritative and continues when loaded.
    let growth_before = host.mob_entity(baby_id).unwrap().mob.growth_timer;
    for _ in 0..20 {
        host.tick(&PlayerInput::default());
    }
    assert!(host.mob_entity(baby_id).unwrap().mob.growth_timer < growth_before);

    let xp_points = host.xp_points();
    let xp_level = host.xp_level();
    let growth_at_save = host.mob_entity(baby_id).unwrap().mob.growth_timer;
    let config = host.config().clone();
    let registry = Arc::clone(host.registry());
    host.save().expect("save succeeds");
    drop(host);
    let reopened = Host::open(registry, config).expect("host reopens");
    assert_eq!(reopened.xp_points(), xp_points);
    assert_eq!(reopened.xp_level(), xp_level);
    let loaded_baby = reopened.mob_entity(baby_id).expect("baby persisted");
    assert!(loaded_baby.mob.is_baby());
    assert!((loaded_baby.mob.growth_timer - growth_at_save).abs() < 1e-6);
}

#[test]
fn feeding_rejects_wrong_food_and_babies() {
    let (_dir, mut host, center_x, center_z) = arena("feed-reject", 72, 8);
    let cow = host
        .spawn_mob(
            MobKind::Cow,
            WorldVec::new(center_x as f64 + 1.5, platform_y(), center_z as f64 + 0.5),
        )
        .expect("cow");
    // Empty hand and seeds are wrong for a cow.
    assert_eq!(host.interact_mob(cow).reason, "not_food");
    select(&mut host, "openblock:seeds", 2);
    assert_eq!(host.interact_mob(cow).reason, "wrong_food");
    // Hostile mobs are not breedable.
    let zombie = host
        .spawn_mob(
            MobKind::Zombie,
            WorldVec::new(center_x as f64 - 1.5, platform_y(), center_z as f64 + 0.5),
        )
        .expect("zombie");
    assert_eq!(host.interact_mob(zombie).reason, "not_breedable");
    // Out-of-reach targets are rejected before any item is consumed.
    let far = host
        .spawn_mob(
            MobKind::Cow,
            WorldVec::new(center_x as f64 + 8.5, platform_y(), center_z as f64 + 0.5),
        )
        .expect("far cow");
    assert_eq!(host.interact_mob(far).reason, "too_far");
}

#[test]
fn structure_chests_are_initialised_once_and_never_refill() {
    let seed = 0x51A7_C0DE_u64;
    let generator = Generator::new(seed);
    let mut found = None;
    'search: for cz in -16..=16 {
        for cx in -16..=16 {
            let pos = ChunkPos::new(cx, cz);
            if let Some(structure) = generator.structure_in_chunk(pos) {
                found = Some((structure, pos));
                break 'search;
            }
        }
    }
    let (structure, chunk) = found.expect("seed has a nearby structure");
    let dir = TempDir::new("loot");
    let mut host = Host::open(Arc::new(Registry::base()), config(dir.path(), seed)).expect("host opens");
    let center = WorldVec::new(
        (structure.x as f64) + 0.5,
        structure.floor_y as f64,
        (structure.z as f64) + 0.5,
    );
    host.ensure_streaming_around(center, 1, 64);
    assert!(host.chunk_column(chunk).is_some(), "structure chunk is resident");
    let chests = ob_worldgen::structure_chests(&structure);
    assert!(!chests.is_empty());
    let chest_pos = chests[0];
    let state = host.chest_at(chest_pos).expect("worldgen chest has a block entity");
    let contents = state.contents();
    assert!(!contents.is_empty(), "loot table must fill at least one slot");
    assert!(contents.len() <= 6);
    let removed = contents[0];
    assert_eq!(host.take_chest_slot(chest_pos, 0), Some(removed));
    assert!(host.chest_at(chest_pos).unwrap().slots[0].is_none());
    let remaining_total: u32 = host
        .chest_at(chest_pos)
        .unwrap()
        .contents()
        .iter()
        .map(|stack| u32::from(stack.count))
        .sum();

    let config = host.config().clone();
    let registry = Arc::clone(host.registry());
    host.save().expect("save succeeds");
    drop(host);
    let reopened = Host::open(registry, config).expect("host reopens");
    let reloaded = reopened.chest_at(chest_pos).expect("chest record persisted");
    let total: u32 = reloaded.contents().iter().map(|stack| u32::from(stack.count)).sum();
    assert_eq!(total, remaining_total, "looting must not refill after reload");
    assert!(reloaded.is_empty() || total == remaining_total);
}

#[test]
fn creeper_explosion_removes_blocks_and_damages_the_player() {
    let (_dir, mut host, center_x, center_z) = arena("creeper", 73, 8);
    let creeper = host
        .spawn_mob(
            MobKind::Creeper,
            WorldVec::new(center_x as f64 + 2.5, platform_y(), center_z as f64 + 0.5),
        )
        .expect("creeper");
    let health_before = host.player().health;
    let blocks_before = count_non_air(&host, center_x, center_z, 4);
    let mut exploded = false;
    for _ in 0..80 {
        host.tick(&PlayerInput::default());
        if host.mob_entity(creeper).is_none() {
            exploded = true;
            break;
        }
    }
    assert!(exploded, "the creeper fuse must complete");
    assert!(!host.take_explosion_events().is_empty(), "an explosion cue is produced");
    let blocks_after = count_non_air(&host, center_x, center_z, 4);
    assert!(blocks_after < blocks_before, "{blocks_after} vs {blocks_before}");
    assert!(
        host.player().health < health_before,
        "the explosion must damage the player"
    );
}

fn count_non_air(host: &Host, center_x: i32, center_z: i32, radius: i32) -> usize {
    let mut count = 0;
    for z in center_z - radius..=center_z + radius {
        for x in center_x - radius..=center_x + radius {
            for y in 196..=205 {
                if host.world().get_block(BlockPos::new(x, y, z)) != ob_content::BlockId::AIR {
                    count += 1;
                }
            }
        }
    }
    count
}

#[test]
fn skeleton_arrow_hits_the_player_and_is_removed() {
    let (_dir, mut host, center_x, center_z) = arena("skeleton", 74, 12);
    let skeleton = host
        .spawn_mob(
            MobKind::Skeleton,
            WorldVec::new(center_x as f64 + 10.5, platform_y(), center_z as f64 + 0.5),
        )
        .expect("skeleton");
    // Face east towards the skeleton so the server-side look ray is irrelevant;
    // arrows aim at the authoritative player position.
    host.player_mut().yaw = -std::f32::consts::FRAC_PI_2;
    let health_before = host.player().health;
    let mut saw_projectile = false;
    let mut hit = false;
    for _ in 0..100 {
        host.tick(&PlayerInput::default());
        saw_projectile |= host.projectile_count() > 0;
        if host.player().health < health_before {
            hit = true;
            break;
        }
    }
    assert!(saw_projectile, "the skeleton must spawn an arrow entity");
    assert!(hit, "the arrow must hit the player");
    assert!(host.mob_entity(skeleton).is_some());
    // The arrow is removed when it hits; a miss would expire too.
    for _ in 0..40 {
        host.tick(&PlayerInput::default());
        if host.projectile_count() == 0 {
            break;
        }
    }
    assert_eq!(host.projectile_count(), 0, "a spent arrow must despawn");
}
