//! Save, sidecar and world-directory robustness tests.
//!
//! The world directory is untrusted input ([17 §3.10]). These tests corrupt the
//! snapshot, poison every sidecar with NaN/out-of-range/unknown values and
//! simulate a failed save, asserting an error or sanitised state but never a
//! panic or a silent large allocation.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use ob_content::Registry;
#[cfg(unix)]
use ob_server::GameMode;
use ob_server::{Host, HostConfig};

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
            "openblock-save-robust-{label}-{}-{nanos}-{unique}",
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

fn config(dir: &TempDir, world_name: &str) -> HostConfig {
    HostConfig {
        seed: 42,
        world_name: world_name.to_string(),
        data_dir: Some(dir.path().to_path_buf()),
        render_distance_chunks: 2,
        generate_budget: 8,
    }
}

fn world_dir(dir: &TempDir, world_name: &str) -> PathBuf {
    dir.path().join("worlds").join(world_name)
}

fn open(registry: &Arc<Registry>, config: HostConfig) -> Result<Host, ob_server::HostError> {
    Host::open(Arc::clone(registry), config)
}

/// Creates a valid world and returns its directory.
fn create_world(dir: &TempDir, registry: &Arc<Registry>, world_name: &str) -> PathBuf {
    let mut host = open(registry, config(dir, world_name)).expect("create world");
    host.save().expect("save initial world");
    drop(host);
    world_dir(dir, world_name)
}

#[test]
fn corrupt_world_snapshots_are_typed_errors() {
    let dir = TempDir::new("corrupt-world");
    let registry = Arc::new(Registry::base());

    let assert_rejected = |world_name: &str, bytes: &[u8]| {
        let path = world_dir(&dir, world_name).join("world.obs");
        std::fs::create_dir_all(path.parent().expect("world dir")).expect("create world dir");
        std::fs::write(&path, bytes).expect("write corrupt save");
        assert!(
            open(&registry, config(&dir, world_name)).is_err(),
            "{world_name} was accepted"
        );
    };

    assert_rejected("not-a-save", b"this is definitely not an OBSV snapshot");
    assert_rejected("truncated-header", b"OBS");
    assert_rejected("empty", b"");

    // A truncated but otherwise valid snapshot.
    let valid = create_world(&dir, &registry, "truncated-valid");
    let bytes = std::fs::read(valid.join("world.obs")).expect("read valid save");
    assert_rejected("truncated-valid-half", &bytes[..bytes.len() / 2]);

    // A single flipped checksum byte.
    let mut bad_crc = bytes.clone();
    *bad_crc.last_mut().expect("checksum") ^= 0xFF;
    assert_rejected("bad-crc", &bad_crc);

    // A newer format version.
    let mut future = bytes.clone();
    future[4..6].copy_from_slice(&999u16.to_le_bytes());
    assert_rejected("future-version", &future);

    // A directory where the file should be.
    let path = world_dir(&dir, "directory").join("world.obs");
    std::fs::create_dir_all(&path).expect("create directory in place of the save");
    assert!(open(&registry, config(&dir, "directory")).is_err());
}

#[cfg(unix)]
#[test]
fn oversized_world_snapshot_is_refused_from_metadata() {
    let dir = TempDir::new("oversized-world");
    let world_name = "oversized";
    let path = world_dir(&dir, world_name).join("world.obs");
    std::fs::create_dir_all(path.parent().expect("world dir")).expect("create world dir");
    let file = std::fs::File::create(&path).expect("create sparse save");
    file.set_len(ob_world::MAX_SAVE_FILE_BYTES + 1)
        .expect("extend sparse save");
    drop(file);

    let started = Instant::now();
    let error = match open(&Arc::new(Registry::base()), config(&dir, world_name)) {
        Ok(_) => panic!("an oversized save must be refused"),
        Err(error) => error,
    };
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the oversized save was read before rejection: {:?}",
        started.elapsed()
    );
    let message = error.to_string();
    assert!(message.contains("limit") || message.contains("too large"), "{message}");
}

#[cfg(unix)]
#[test]
fn symlink_loops_in_the_world_directory_do_not_escape_or_panic() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new("symlink-loop");
    let registry = Arc::new(Registry::base());
    let world_name = "looped";
    std::fs::create_dir_all(world_dir(&dir, world_name)).expect("create world dir");
    symlink("world.obs", world_dir(&dir, world_name).join("world.obs")).expect("create symlink loop");
    // Host::open either reports an I/O error or replaces the dangling loop with
    // a fresh world; it must never hang, escape the directory or panic.
    match open(&registry, config(&dir, world_name)) {
        Ok(_) => {
            let metadata =
                std::fs::symlink_metadata(world_dir(&dir, world_name).join("world.obs")).expect("world.obs exists");
            assert!(
                metadata.file_type().is_file(),
                "the loop must be replaced by a regular save"
            );
        }
        Err(error) => {
            assert!(
                matches!(
                    error.to_string().as_str(),
                    message if message.contains("invalid") || message.contains("i/o") || message.contains("error")
                ),
                "{error}"
            );
        }
    }
}

#[test]
fn hostile_sidecars_are_sanitized_or_skipped() {
    let dir = TempDir::new("hostile-sidecars");
    let registry = Arc::new(Registry::base());
    let world_name = "hostile";
    let world = create_world(&dir, &registry, world_name);

    std::fs::write(world.join("player.txt"), "1e308 -1e308 1e308 100 100 0 7 NaN NaN 0\n")
        .expect("write player sidecar");
    std::fs::write(
        world.join("inventory.txt"),
        "0 openblock:stone 999999\n\
         1 openblock:wooden_pickaxe 1 999999\n\
         2 openblock:not_a_real_item 5\n\
         999 openblock:dirt 2\n\
         3 openblock:apple -2\n\
         4 openblock:apple 0\n\
         0 openblock:dirt 3\n",
    )
    .expect("write inventory sidecar");
    std::fs::write(
        world.join("equipment.txt"),
        "helmet openblock:iron_helmet 999999\n\
         helmet openblock:iron_helmet 1\n\
         boots openblock:iron_helmet 5\n\
         chestplate openblock:apple 1\n",
    )
    .expect("write equipment sidecar");
    std::fs::write(
        world.join("items.txt"),
        "1 NaN 70 0 0 0 0 openblock:stone 1 - 0\n\
         2 5 70 5 1 2 3 openblock:not_a_real_item 1 - 0\n\
         3 5 70 5 0 0 0 openblock:dirt 0 - 0\n\
         3 6 70 6 0 0 0 openblock:dirt 1 - 0\n\
         4 1e308 1e308 -1e308 0 0 0 openblock:stone 300 - -5\n\
         9223372036854775808 5 70 5 0 0 0 openblock:stone 1 - 0\n\
         18446744073709551615 6 70 6 0 0 0 openblock:dirt 1 - 0\n\
         5 7 70 7 0 0 0 openblock:iron_helmet 1 99999 0\n\
         6 8 70 8 0 0 0 openblock:stone 1 5 0\n",
    )
    .expect("write items sidecar");
    std::fs::write(
        world.join("mobs.txt"),
        "1 openblock:zombie NaN 70 0 0 20 0\n\
         2 openblock:not_a_mob 1 70 1 0 20 0\n\
         3 openblock:cow 5 70 5 0 100 -5\n\
         3 openblock:cow 6 70 6 0 20 0\n\
         4 openblock:zombie 1e308 1e308 1e308 1e30 1e30 -1e30\n\
         9223372036854775808 openblock:cow 7 70 7 0 20 0\n\
         18446744073709551615 openblock:cow 8 70 8 0 20 0\n",
    )
    .expect("write mobs sidecar");
    std::fs::write(
        world.join("block_entities.txt"),
        "furnace 1000 64 1000 65535 0 65535 openblock:not_real,1 - -\n\
         furnace 1000 64 1000 1 1 1 - - -\n\
         furnace 1001 64 1000 10 5 999 openblock:raw_iron,999 openblock:coal,1 openblock:iron_ingot,1\n\
         chest 1002 64 1000 0:openblock:stone,999 99:openblock:dirt,1 1:bogus,1\n\
         not_a_record 1 2 3\n",
    )
    .expect("write block-entity sidecar");
    std::fs::write(
        world.join("network_players.txt"),
        "alice 1e308 NaN -1e308 1e30 1e30 NaN 999 2 999 survival i0:openblock:stone,999 i0:openblock:dirt,5 i99:openblock:stone,1 e0:openblock:iron_helmet,999999\n\
         bob 0 70 0 0 0 20 20 0 0 survival i0:openblock:not_a_real_item,1\n\
         alice 5 70 -5 0 0 12 14 0 4 survival i0:openblock:apple,999\n",
    )
    .expect("write network-player sidecar");
    std::fs::write(world.join("unexpected.bin"), [0xffu8; 64]).expect("write unexpected file");
    std::fs::write(world.join("world.tmp"), b"stale temp file").expect("write stale temp file");

    let mut host = open(&registry, config(&dir, world_name)).expect("hostile sidecars must not prevent opening");

    let player = host.player();
    assert!(player.pos.is_finite());
    assert!(player.pos.x.abs() <= 30_000_000.0);
    assert!(player.pos.z.abs() <= 30_000_000.0);
    assert!(player.pos.y >= f64::from(ob_math::WORLD_MIN_Y) - 32.0);
    assert!(player.pos.y <= f64::from(ob_math::WORLD_MAX_Y) + 256.0);
    assert!(player.yaw.is_finite() && player.yaw.abs() <= std::f32::consts::PI);
    assert!(player.pitch.is_finite() && player.pitch.abs() <= std::f32::consts::FRAC_PI_2);
    assert!(player.health.is_finite() && (0.0..=20.0).contains(&player.health));
    assert!(player.hunger.is_finite() && (0.0..=20.0).contains(&player.hunger));

    let inventory = *host.inventory();
    let stone_slot = inventory[0].expect("dirt duplicate overwrote stone");
    assert_eq!(stone_slot.item, host.registry().item_id("openblock:dirt").unwrap());
    assert_eq!(stone_slot.count, 3);
    let pickaxe = inventory[1].expect("pickaxe");
    assert!((1..=59).contains(&pickaxe.durability.unwrap_or(0)));
    assert!(inventory[2].is_none(), "unknown item must be dropped");
    assert!(inventory[3].is_none(), "negative count must be dropped");
    assert_eq!(
        inventory[4].map(|stack| stack.count),
        Some(1),
        "a zero count clamps to one rather than vanishing"
    );
    assert!(
        inventory[8].is_none(),
        "out-of-range slot 999 is dropped, never clamped onto the last hotbar slot"
    );
    assert!(
        !inventory.iter().flatten().any(|stack| stack.count == 2),
        "the dropped out-of-range dirt must not appear anywhere else"
    );

    let helmet = host.equipment()[0].expect("helmet");
    assert_eq!(helmet.count, 1);
    assert!((1..=165).contains(&helmet.durability.unwrap_or(0)));
    assert!(host.equipment()[1..].iter().all(Option::is_none));

    for entity in host.item_entities() {
        assert!(entity.pos.is_finite());
        assert!(entity.pos.x.abs() <= 30_000_000.0);
        assert!(entity.pos.y >= f64::from(ob_math::WORLD_MIN_Y));
        assert!(entity.pos.y <= f64::from(ob_math::WORLD_MAX_Y));
        assert!(entity.velocity.is_finite());
        assert!(entity.age.is_finite() && (0.0..=300.0).contains(&entity.age));
        assert!(entity.stack.count > 0);
    }
    let mut item_ids: Vec<u64> = host.item_entities().iter().map(|entity| entity.id).collect();
    let item_count = item_ids.len();
    item_ids.sort_unstable();
    item_ids.dedup();
    assert_eq!(item_ids.len(), item_count, "duplicate item ids must be dropped");
    let host_id_max = (1u64 << 63) - 1;
    assert!(
        item_ids.iter().all(|id| *id <= host_id_max),
        "item ids must leave bit 63 for the wire namespace: {item_ids:?}"
    );
    let stone = registry.item_id("openblock:stone").expect("stone item");
    let helmet = registry.item_id("openblock:iron_helmet").expect("helmet item");
    let helmet_max = registry.item(helmet).armor.expect("helmet armor").durability;
    for entity in host.item_entities() {
        if entity.stack.item == stone {
            assert!(
                entity.stack.durability.is_none(),
                "a non-durable item entity must not load a durability"
            );
        }
        if entity.stack.item == helmet {
            let durability = entity.stack.durability.unwrap_or(0);
            assert!(
                (1..=helmet_max).contains(&durability),
                "armor durability {durability} is outside 1..={helmet_max}"
            );
        }
    }
    let spawn_pos = host.spawn();
    let stone_stack = host.make_stack(stone, 1).expect("stone stack");
    let spawned = host.spawn_item_entity(spawn_pos, stone_stack);
    assert!(
        !item_ids.contains(&spawned) && spawned <= host_id_max,
        "a new item entity id {spawned} must not collide with a loaded hostile id"
    );

    for mob in host.mobs() {
        assert!(mob.mob.pos.is_finite());
        assert!(mob.mob.health.is_finite() && mob.mob.health > 0.0);
        assert!(mob.mob.age.is_finite() && mob.mob.age >= 0.0);
    }
    let mut mob_ids: Vec<u64> = host.mobs().iter().map(|mob| mob.id).collect();
    let mob_count = mob_ids.len();
    mob_ids.sort_unstable();
    mob_ids.dedup();
    assert_eq!(mob_ids.len(), mob_count, "duplicate mob ids must be dropped");
    assert!(
        mob_ids.iter().all(|id| *id <= host_id_max),
        "mob ids must stay below the item wire namespace: {mob_ids:?}"
    );

    for (_, furnace) in host.furnaces() {
        assert!(furnace.burn_remaining <= furnace.burn_total);
        assert!(furnace.cook_progress <= ob_content::SMELT_TICKS_PER_ITEM);
        for stack in [furnace.input, furnace.fuel, furnace.output].into_iter().flatten() {
            assert!(stack.count > 0);
            assert!((stack.item.0 as usize) < host.registry().item_len());
        }
    }
    for (_, chest) in host.chests() {
        for stack in chest.slots.iter().flatten() {
            assert!(stack.count > 0);
            assert!((stack.item.0 as usize) < host.registry().item_len());
        }
    }

    let alice = host.saved_network_player("alice").expect("finite alice record wins");
    assert!(alice.name.eq_ignore_ascii_case("alice"));
    assert!(alice.pos.is_finite());
    assert!((alice.pos.x - 5.0).abs() < 0.01);
    assert!((alice.pos.z + 5.0).abs() < 0.01);
    assert!(alice.pos.x.abs() <= 30_000_000.0);
    assert!(alice.pos.y >= -64.0 && alice.pos.y <= 320.0);
    assert_eq!(alice.selected_slot, 4);
    assert!((alice.health - 12.0).abs() < 0.01);
    assert!((alice.hunger - 14.0).abs() < 0.01);
    let apple = alice.inventory[0].expect("apple was clamped into slot 0");
    assert_eq!(apple.item, host.registry().item_id("openblock:apple").unwrap());
    assert!(apple.count > 0 && apple.count <= host.registry().item(apple.item).max_stack);
    let bob = host
        .saved_network_player("bob")
        .expect("a record with an unknown item still loads");
    assert!(bob.inventory.iter().all(Option::is_none));
}

#[test]
fn oversized_sidecar_is_ignored_without_failing_the_open() {
    let dir = TempDir::new("oversized-sidecar");
    let registry = Arc::new(Registry::base());
    let world_name = "sidecar";
    let world = create_world(&dir, &registry, world_name);
    let path = world.join("inventory.txt");
    let file = std::fs::File::create(&path).expect("create oversized sidecar");
    // Larger than the 8 MiB sidecar cap, but cheap on every platform because
    // the file is extended rather than written.
    file.set_len(9 * 1024 * 1024).expect("extend sidecar");
    drop(file);

    let host = open(&registry, config(&dir, world_name)).expect("an oversized sidecar must not abort the load");
    assert!(
        host.inventory().iter().all(Option::is_none),
        "the too-large inventory must be ignored, not partially parsed"
    );
}

#[test]
fn save_target_directory_leaves_previous_files_and_world_intact() {
    let dir = TempDir::new("save-target-directory");
    let registry = Arc::new(Registry::base());
    let world_name = "target-directory";
    let mut host = open(&registry, config(&dir, world_name)).expect("create world");
    host.save().expect("initial save");
    let world = world_dir(&dir, world_name);
    let read = |name: &str| std::fs::read(world.join(name)).expect("read save file");
    let before_world = read("world.obs");
    let before_inventory = read("inventory.txt");

    // Change persisted state, so a committed save must produce different bytes.
    let chunk = host.world().chunks().next().expect("a loaded chunk").0;
    let pos = ob_math::BlockPos::new(chunk.x * 16 + 1, ob_math::WORLD_MAX_Y - 1, chunk.z * 16 + 1);
    let stone = host.registry().id_or_air("openblock:stone");
    assert!(host.world_mut().set_block(pos, stone));
    let dirt = host.registry().item_id("openblock:dirt").expect("dirt");
    assert_eq!(host.give_item(dirt, 3), 0);
    host.player_mut().pos.x += 0.5;

    // A directory where a sidecar target belongs: the save must fail before
    // any target (especially world.obs) is replaced.
    std::fs::remove_file(world.join("player.txt")).expect("remove player sidecar");
    std::fs::create_dir(world.join("player.txt")).expect("replace player sidecar with a directory");
    let error = match host.save() {
        Ok(()) => panic!("saving over a directory target must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("directory"), "{error}");
    assert_eq!(
        read("world.obs"),
        before_world,
        "the world snapshot must stay at the last commit"
    );
    assert_eq!(
        read("inventory.txt"),
        before_inventory,
        "other sidecars must stay at the last commit"
    );
    assert!(
        world.join("player.txt").is_dir(),
        "the failing target must be untouched"
    );

    // Once the path is usable again the staged protocol commits everything.
    std::fs::remove_dir(world.join("player.txt")).expect("remove directory target");
    host.save().expect("save after the target is repaired");
    assert!(world.join("player.txt").is_file());
    assert_ne!(read("world.obs"), before_world, "the changed world must be committed");
    assert_ne!(
        read("inventory.txt"),
        before_inventory,
        "the changed inventory must be committed"
    );
    for entry in std::fs::read_dir(&world).expect("list world directory") {
        let entry = entry.expect("directory entry");
        assert!(
            !entry.file_name().to_string_lossy().ends_with(".tmp"),
            "a successful save must not leave a temporary file: {:?}",
            entry.path()
        );
    }
}

#[cfg(unix)]
#[test]
fn failed_save_keeps_the_previous_snapshot() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("failed-save");
    let registry = Arc::new(Registry::base());
    let world_name = "unwritable";
    let mut host = open(&registry, config(&dir, world_name)).expect("create world");
    host.set_game_mode(GameMode::Survival);
    host.save().expect("initial save");
    let world = world_dir(&dir, world_name);
    let snapshot = std::fs::read(world.join("world.obs")).expect("read initial save");
    let player = std::fs::read(world.join("player.txt")).expect("read initial player");

    std::fs::set_permissions(&world, std::fs::Permissions::from_mode(0o555)).expect("make dir read-only");
    let probe = world.join("write-probe");
    let writable = std::fs::write(&probe, b"x").is_ok();
    let _ = std::fs::remove_file(&probe);
    if writable {
        // Root ignores directory permissions; the failure path is untestable here.
        std::fs::set_permissions(&world, std::fs::Permissions::from_mode(0o755)).expect("restore permissions");
        eprintln!("skipping failed_save_keeps_the_previous_snapshot: directory stays writable");
        return;
    }

    let result = host.save();
    assert!(result.is_err(), "saving into a read-only world must fail");
    std::fs::set_permissions(&world, std::fs::Permissions::from_mode(0o755)).expect("restore permissions");
    assert_eq!(
        std::fs::read(world.join("world.obs")).expect("read save after failure"),
        snapshot,
        "the previous world snapshot must survive a failed save"
    );
    assert_eq!(
        std::fs::read(world.join("player.txt")).expect("read player after failure"),
        player,
        "the previous player sidecar must survive a failed save"
    );
    host.save().expect("saving works again after permissions are restored");
}
