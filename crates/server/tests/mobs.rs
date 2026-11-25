//! End-to-end mob flows through the public host API.
//!
//! These tests use only exported items (`Host`, `MobEntity`, `MobKind`,
//! `AttackOutcome`) so they exercise the same surface the client and network
//! agents will consume. No window, no wall-clock waits and no external network.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ob_content::{BlockId, MobKind, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_server::{GameMode, Host, HostConfig, MAX_PASSIVE_MOBS};
use ob_sim::{MobAi, PlayerInput, REACH};

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
            "openblock-server-mobs-it-{label}-{}-{nanos}-{unique}",
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

fn test_config(data_dir: &Path, seed: u64, render_distance: i32) -> HostConfig {
    HostConfig {
        seed,
        world_name: "test".to_string(),
        data_dir: Some(data_dir.to_path_buf()),
        render_distance_chunks: render_distance,
        generate_budget: 16,
    }
}

fn converge(host: &mut Host) {
    for _ in 0..300 {
        if host.ensure_streaming(128) == 0 {
            return;
        }
    }
    panic!("streaming did not converge");
}

fn grass_platform(host: &mut Host, center_x: i32, center_z: i32, y: i32, radius: i32) {
    let grass = host.registry().id_or_air("openblock:grass");
    for z in center_z - radius..=center_z + radius {
        for x in center_x - radius..=center_x + radius {
            assert!(host.world_mut().set_block(BlockPos::new(x, y, z), grass));
            for dy in 1..=3 {
                assert!(host.world_mut().set_block(BlockPos::new(x, y + dy, z), BlockId::AIR));
            }
        }
    }
}

/// Opens a survival host on a high grass arena and returns its centre.
fn arena(label: &str, seed: u64, render_distance: i32, radius: i32) -> (TempDir, Host, i32, i32, i32) {
    let dir = TempDir::new(label);
    let mut host = Host::open(
        Arc::new(Registry::base()),
        test_config(dir.path(), seed, render_distance),
    )
    .expect("host opens on a fresh world");
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
    converge(&mut host);
    grass_platform(&mut host, center_x, center_z, platform_y, radius);
    converge(&mut host);
    (dir, host, center_x, center_z, platform_y)
}

fn give_and_select(host: &mut Host, name: &str, count: u8) {
    let item = host.registry().item_id(name).expect("item exists");
    assert_eq!(host.give_item(item, count), 0, "inventory must have room");
    let slot = host
        .inventory()
        .iter()
        .position(|slot| matches!(slot, Some(stack) if stack.item == item))
        .expect("item found in inventory");
    host.set_hotbar_selection(slot);
}

#[test]
fn public_mob_combat_ray_drops_and_tool_wear() {
    let (_dir, mut host, center_x, center_z, platform_y) = arena("combat", 61, 2, 12);
    let player = host.player().pos;
    let cow_id = host
        .spawn_mob(
            MobKind::Cow,
            WorldVec::new(player.x + 2.0, platform_y as f64 + 1.0, player.z),
        )
        .expect("cow spawns on the arena");
    assert_eq!(host.mob_count(), 1);
    assert_eq!(host.passive_mob_count(), 1);
    assert_eq!(host.hostile_mob_count(), 0);
    assert_eq!(host.mobs()[0].id, cow_id);
    assert_eq!(host.mobs()[0].mob.kind, MobKind::Cow);

    let origin = host.player().eye();
    let target = host.mob_entity(cow_id).unwrap().mob.pos + WorldVec::new(0.0, 0.65, 0.0);
    let forward = (target - origin).normalize();
    assert_eq!(host.mob_hit_by_ray(origin, forward, REACH), Some(cow_id));
    assert_eq!(host.mob_hit_by_ray(origin, forward, 0.5), None);

    let first = host.attack_mob(cow_id);
    assert!(first.hit);
    assert_eq!(first.damage, 1.0, "an empty hand deals 1 HP");
    assert!(!first.killed);
    for _ in 0..9 {
        // Advance the authoritative clock past the 0.9 charge gate without
        // ticking the fleeing cow out of reach.
        host.set_tick_count(host.tick_count() + 20);
        let outcome = host.attack_mob(cow_id);
        if outcome.killed {
            assert_eq!(outcome.damage, 1.0);
            assert_eq!(outcome.drops.len(), 2);
        }
    }
    assert!(host.mob_entity(cow_id).is_none());
    assert_eq!(host.mob_count(), 0);
    assert_eq!(host.item_entities().len(), 2, "drops become item entities");
    let raw_beef = host.registry().item_id("openblock:raw_beef").unwrap();
    let leather = host.registry().item_id("openblock:leather").unwrap();
    let dropped: Vec<_> = host.item_entities().iter().map(|entity| entity.stack.item).collect();
    assert!(dropped.contains(&raw_beef));
    assert!(dropped.contains(&leather));

    give_and_select(&mut host, "openblock:wooden_sword", 1);
    let zombie_id = host
        .spawn_mob(
            MobKind::Zombie,
            WorldVec::new(center_x as f64 + 2.5, platform_y as f64 + 1.0, center_z as f64 + 2.5),
        )
        .expect("zombie spawns");
    host.set_tick_count(host.tick_count() + 20);
    let hit = host.attack_mob(zombie_id);
    assert!(hit.hit);
    assert_eq!(hit.damage, 4.0, "wooden sword deals 4 HP");
    assert_eq!(host.selected_stack().unwrap().durability, Some(58));
    assert!(matches!(
        host.mob_entity(zombie_id).unwrap().mob.ai,
        MobAi::Chase { .. } | MobAi::Idle
    ));
}

#[test]
fn public_mobs_persist_with_a_stable_state_hash() {
    let (_dir, mut host, _center_x, _center_z, _platform_y) = arena("persist", 62, 2, 12);
    let player = host.player().pos;
    let cow_id = host
        .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 2.0, player.y, player.z))
        .expect("cow spawns");
    let zombie_id = host
        .spawn_mob(MobKind::Zombie, WorldVec::new(player.x - 2.0, player.y, player.z + 1.0))
        .expect("zombie spawns");
    for _ in 0..80 {
        host.tick(&PlayerInput::default());
    }
    converge(&mut host);
    let snapshot: Vec<_> = host
        .mobs()
        .iter()
        .map(|entity| {
            (
                entity.id,
                entity.mob.kind,
                entity.mob.pos,
                entity.mob.yaw,
                entity.mob.health,
                entity.mob.age,
            )
        })
        .collect();
    assert!(snapshot.iter().any(|(id, ..)| *id == cow_id));
    assert!(snapshot.iter().any(|(id, ..)| *id == zombie_id));
    let hash = host.state_hash();
    host.save().expect("save succeeds");

    let path = _dir.path().join("worlds").join("test").join("mobs.txt");
    let text = std::fs::read_to_string(&path).expect("mobs.txt is written");
    assert_eq!(text.lines().count(), host.mob_count());
    for line in text.lines() {
        assert_eq!(
            line.split_whitespace().count(),
            13,
            "id kind x y z yaw health age variant love cooldown growth persistent"
        );
    }

    let config = host.config().clone();
    let registry = Arc::clone(host.registry());
    drop(host);
    let mut reopened = Host::open(registry, config).expect("host reopens");
    converge(&mut reopened);
    let loaded: Vec<_> = reopened
        .mobs()
        .iter()
        .map(|entity| {
            (
                entity.id,
                entity.mob.kind,
                entity.mob.pos,
                entity.mob.yaw,
                entity.mob.health,
                entity.mob.age,
            )
        })
        .collect();
    assert_eq!(loaded, snapshot, "persisted mob fields round-trip exactly");
    assert_eq!(reopened.state_hash(), hash, "mob state hash round-trips");
    assert!(reopened.mobs().iter().all(|entity| entity.mob.ai == MobAi::Idle));
}

#[test]
fn public_zombie_attacks_are_armor_mitigated_and_creative_safe() {
    let (_dir, mut host, _center_x, _center_z, platform_y) = arena("attack", 63, 2, 12);
    give_and_select(&mut host, "openblock:leather_helmet", 1);
    let slot = host.hotbar_selection();
    assert!(host.equip_armor(slot));
    assert_eq!(host.armor_defense(), 1);
    let player = host.player().pos;
    let zombie_id = host
        .spawn_mob(
            MobKind::Zombie,
            WorldVec::new(player.x + 1.0, platform_y as f64 + 1.0, player.z),
        )
        .expect("zombie spawns adjacent");
    let health = host.player().health;
    for _ in 0..12 {
        host.tick(&PlayerInput::default());
        if host.player().health < health {
            break;
        }
    }
    let mitigated = 3.0 * (1.0 - ob_server::armor_damage_reduction(1));
    assert!(
        (health - host.player().health - mitigated).abs() < 1e-4,
        "expected {mitigated} damage, got {}",
        health - host.player().health
    );
    assert_eq!(host.equipment()[0].unwrap().durability, Some(54));
    assert!(host.player().velocity.x < 0.0, "knockback pushes away");
    assert!(host.mob_entity(zombie_id).is_some());

    host.set_game_mode(GameMode::Creative);
    let health = host.player().health;
    for _ in 0..20 {
        host.tick(&PlayerInput::default());
    }
    assert_eq!(host.player().health, health, "creative players are invulnerable");
}

#[test]
fn public_natural_spawning_fills_the_passive_cap() {
    let (_dir, mut host, _center_x, _center_z, _platform_y) = arena("natural", 64, 6, 44);
    let mut observed = std::collections::HashSet::new();
    for _ in 0..600 {
        let before: std::collections::HashSet<u64> = host.mobs().iter().map(|entity| entity.id).collect();
        host.tick(&PlayerInput::default());
        for entity in host.mobs() {
            if before.contains(&entity.id) {
                continue;
            }
            observed.insert(entity.id);
            let spec = host.registry().mob(entity.mob.kind);
            assert!(!spec.hostile, "daylight passive batch spawned a hostile");
            let dx = entity.mob.pos.x - host.player().pos.x;
            let dz = entity.mob.pos.z - host.player().pos.z;
            let distance = (dx * dx + dz * dz).sqrt();
            assert!(
                (12.0..=40.0).contains(&distance),
                "spawn distance {distance} outside 12..40"
            );
            assert!(host.is_day());
        }
    }
    assert_eq!(host.passive_mob_count(), MAX_PASSIVE_MOBS);
    assert_eq!(host.hostile_mob_count(), 0);
    assert_eq!(observed.len(), MAX_PASSIVE_MOBS);
    assert!(host.mobs().windows(2).all(|pair| pair[0].id < pair[1].id));
}
