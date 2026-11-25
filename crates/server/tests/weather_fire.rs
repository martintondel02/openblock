//! Integration tests for the 1.5 weather, lightning and fire slice.
//!
//! These use the public `ob_server::Host` API with a real temporary world, a
//! fixed seed and loaded chunks. No window, sockets or wall-clock time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ob_content::{BlockId, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_protocol::{PrecipitationKind, WeatherPhase};
use ob_server::{
    FIRE_BURN_TICKS, FIRE_DAMAGE_PER_SECOND, FIRE_MAX_ACTIVE, FIRE_MAX_PER_CHUNK, GameMode, Host, HostConfig,
    HostError, LIGHTNING_DAMAGE, WeatherState,
};
use ob_sim::{MAX_HEALTH, PlayerInput};

const FIRE_Y: i32 = 110;

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
            "openblock-weather-fire-{label}-{}-{nanos}-{unique}",
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
        world_name: "weather-fire".to_string(),
        data_dir: Some(dir.to_path_buf()),
        render_distance_chunks: 2,
        generate_budget: 8,
    }
}

fn converge(host: &mut Host) {
    for _ in 0..200 {
        if host.ensure_streaming(32) == 0 {
            return;
        }
    }
    panic!("streaming did not converge");
}

fn open_host(label: &str, seed: u64) -> (TempDir, Host, HostConfig) {
    let dir = TempDir::new(label);
    let config = config(dir.path(), seed);
    let mut host = Host::open(Arc::new(Registry::base()), config.clone()).expect("open test world");
    converge(&mut host);
    (dir, host, config)
}

/// Builds a 3×3 stone platform at `y` and clears two cells above it.
fn platform(host: &mut Host, x: i32, y: i32, z: i32) -> BlockPos {
    let stone = host.registry().id_or_air("openblock:stone");
    host.ensure_streaming_around(WorldVec::new(f64::from(x), f64::from(y), f64::from(z)), 1, 16);
    for dz in -1..=1 {
        for dx in -1..=1 {
            assert!(host.world_mut().set_block(BlockPos::new(x + dx, y, z + dz), stone));
            assert!(
                host.world_mut()
                    .set_block(BlockPos::new(x + dx, y + 1, z + dz), BlockId::AIR)
            );
            assert!(
                host.world_mut()
                    .set_block(BlockPos::new(x + dx, y + 2, z + dz), BlockId::AIR)
            );
        }
    }
    BlockPos::new(x, y, z)
}

/// Finds a nearby column whose worldgen precipitation is rain in a storm.
fn rain_column(host: &mut Host) -> (i32, i32) {
    host.set_weather_state(WeatherPhase::Precipitating, 10_000, 1_000);
    let spawn = host.spawn();
    let center_x = spawn.x.floor() as i32;
    let center_z = spawn.z.floor() as i32;
    for radius in 0i32..=24 {
        for dz in -radius..=radius {
            for dx in -radius..=radius {
                if radius > 0 && dx.abs() != radius && dz.abs() != radius {
                    continue;
                }
                let x = center_x + dx * 8;
                let z = center_z + dz * 8;
                if host.precipitation_at(x, z) == PrecipitationKind::Rain {
                    return (x, z);
                }
            }
        }
    }
    panic!("no rain column found near the spawn");
}

#[test]
fn weather_state_persists_with_time_fire_and_the_state_hash() {
    let dir = TempDir::new("persist");
    let config = config(dir.path(), 42);
    let registry = Arc::new(Registry::base());
    let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("open host");
    converge(&mut host);
    let spawn = host.spawn();
    let x = spawn.x.floor() as i32 + 4;
    let z = spawn.z.floor() as i32;
    let floor = platform(&mut host, x, FIRE_Y, z);
    let fire_pos = floor.offset(0, 1, 0);
    host.set_tick_count(54_321);
    host.set_weather_state(WeatherPhase::Precipitating, 4_321, 777);
    assert!(host.ignite_fire(fire_pos));
    // Cover the test fire so the persisted storm cannot extinguish it on the
    // first resumed tick; this isolates timer persistence from rain behaviour.
    let stone = host.registry().id_or_air("openblock:stone");
    assert!(host.world_mut().set_block(fire_pos.offset(0, 1, 0), stone));
    let hash = host.state_hash();
    let expected_weather = host.weather().clone();
    host.save().expect("save weather/fire world");
    drop(host);

    // level.txt is additive and carries the authoritative clock.
    let level = std::fs::read_to_string(dir.path().join("worlds/weather-fire/level.txt")).expect("level.txt");
    assert!(level.contains("version=1"));
    assert!(level.contains("time=54321"));
    assert!(level.contains("weather_phase=precipitating"));
    assert!(level.contains("weather_remaining=4321"));
    assert!(level.contains("weather_intensity=777"));

    let mut reopened = Host::open(Arc::clone(&registry), config).expect("reopen host");
    converge(&mut reopened);
    assert_eq!(reopened.tick_count(), 54_321, "level.txt clock overrides player.txt");
    assert_eq!(reopened.weather(), &expected_weather);
    assert!(reopened.is_burning(fire_pos));
    assert_eq!(
        reopened.fire_cell(fire_pos).expect("fire timer").remaining_ticks,
        FIRE_BURN_TICKS
    );
    assert_eq!(
        reopened.registry().id_or_air("openblock:fire"),
        reopened.world().get_block(fire_pos)
    );
    assert_eq!(
        reopened.state_hash(),
        hash,
        "weather/time/fire must round-trip the hash"
    );

    assert!(!reopened.sky_exposed(fire_pos), "roof must keep the fire dry");
    // A tick resumes both clocks and the burn timer.
    reopened.tick(&PlayerInput::default());
    assert_eq!(reopened.weather().remaining_ticks, 4_320);
    assert_eq!(
        reopened.fire_cell(fire_pos).expect("fire survives").remaining_ticks,
        FIRE_BURN_TICKS - 1
    );

    // Weather is part of the deterministic state hash.
    let before = reopened.state_hash();
    reopened.set_weather_state(WeatherPhase::Thunder, 3_000, 900);
    assert_ne!(reopened.state_hash(), before, "weather must change the state hash");
}

#[test]
fn weather_cycle_freezes_and_ramps_toward_the_phase_target() {
    let (_dir, mut host, _) = open_host("cycle", 7);
    host.set_weather_state(WeatherPhase::Thunder, 5_000, 0);
    host.set_weather_cycle(false);
    let frozen = host.weather().clone();
    for _ in 0..100 {
        host.tick(&PlayerInput::default());
    }
    assert_eq!(
        host.weather(),
        &frozen,
        "do_weather_cycle=false must freeze the machine"
    );
    assert!(!host.weather_cycle_enabled());

    host.set_weather_cycle(true);
    host.set_tick_count(0);
    let target = host.weather().target_permille(7);
    assert!((850..=1_000).contains(&target));
    for _ in 0..200 {
        host.tick(&PlayerInput::default());
    }
    assert_eq!(host.weather().intensity_permille, target);
    assert_eq!(host.weather_phase(), WeatherPhase::Thunder);
    assert_eq!(host.weather_factor(), 0.6);
    assert!(host.is_thunder());
    assert!(host.is_precipitating());
    assert!(host.storm_epoch() == 0);
}

#[test]
fn precipitation_is_per_column_and_clear_weather_has_none() {
    let (_dir, mut host, _) = open_host("precip", 42);
    let spawn = host.spawn();
    let x = spawn.x.floor() as i32 + 8;
    let z = spawn.z.floor() as i32;

    host.set_weather_state(WeatherPhase::Clear, 10_000, 0);
    assert_eq!(host.precipitation_at(x, z), PrecipitationKind::None);
    assert_eq!(host.weather_factor(), 1.0);
    assert!(!host.is_precipitating());

    let (rain_x, rain_z) = rain_column(&mut host);
    assert_eq!(host.precipitation_at(rain_x, rain_z), PrecipitationKind::Rain);
    assert!(host.weather_factor() == 0.8);
    assert_eq!(host.weather_factor(), 0.8);
    assert!(
        matches!(host.biome_name_at(rain_x, rain_z), "plains" | "forest" | "mountains"),
        "rain must come from a warm non-desert biome"
    );
    // Rain reaches only open-sky cells; a sealed cell under a roof is dry.
    let surface = host.world().height_at(rain_x, rain_z).expect("loaded test column");
    let open = BlockPos::new(rain_x, surface + 1, rain_z);
    host.ensure_streaming_around(
        WorldVec::new(f64::from(rain_x), f64::from(surface), f64::from(rain_z)),
        1,
        16,
    );
    assert!(host.sky_exposed(open));
    assert!(host.rain_reaches(open));
    let buried = BlockPos::new(rain_x, surface - 8, rain_z);
    assert!(!host.rain_reaches(buried));

    host.set_weather_state(WeatherPhase::Thunder, 10_000, 900);
    assert_eq!(host.precipitation_at(rain_x, rain_z), PrecipitationKind::Rain);
    assert_eq!(host.weather_factor(), 0.6);
    assert!(host.is_thunder());

    // At noon an open-sky column is dimmer in a storm than in clear weather.
    let probe = BlockPos::new(rain_x, surface + 1, rain_z);
    host.set_tick_count(0);
    let clear = host.world().gameplay_light(probe, host.day_factor(), 1.0);
    let storm = host
        .world()
        .gameplay_light(probe, host.day_factor(), host.weather_factor());
    assert_eq!(clear, 15);
    assert_eq!(storm, 9, "0.6 × 15 = 9");
}

#[test]
fn fire_light_burnout_and_contact_damage() {
    let (_dir, mut host, _) = open_host("fire-burn", 42);
    let spawn = host.spawn();
    let x = spawn.x.floor() as i32 + 3;
    let z = spawn.z.floor() as i32;
    let floor = platform(&mut host, x, FIRE_Y, z);
    let fire_pos = floor.offset(0, 1, 0);
    assert!(host.ignite_fire(fire_pos));
    assert!(host.is_burning(fire_pos));
    assert_eq!(host.fire_count(), 1);
    let fire = host.registry().id_or_air("openblock:fire");
    assert_eq!(host.world().get_block(fire_pos), fire);
    assert_eq!(host.world().block_light(fire_pos), 15, "fire emits full block light");
    assert_eq!(host.world().gameplay_light(fire_pos, 0.0, 0.6), 15);
    assert!(
        host.take_block_updates().contains(&(fire_pos, fire)),
        "ignition must queue a block edit"
    );

    host.tick(&PlayerInput::default());
    assert_eq!(
        host.fire_cell(fire_pos).expect("fire ticks").remaining_ticks,
        FIRE_BURN_TICKS - 1
    );
    for _ in 0..u32::from(FIRE_BURN_TICKS) - 1 {
        host.tick(&PlayerInput::default());
    }
    assert!(!host.is_burning(fire_pos), "fire must burn out after 160 ticks");
    assert_eq!(host.world().get_block(fire_pos), BlockId::AIR);

    // Contact damage: stand in a fire cell in survival.
    let floor2 = platform(&mut host, x + 6, FIRE_Y, z);
    let damage_fire = floor2.offset(0, 1, 0);
    assert!(host.ignite_fire(damage_fire));
    host.set_game_mode(GameMode::Survival);
    host.player_mut().pos = WorldVec::new(
        f64::from(damage_fire.x) + 0.5,
        f64::from(damage_fire.y),
        f64::from(damage_fire.z) + 0.5,
    );
    host.player_mut().velocity = WorldVec::ZERO;
    host.set_tick_count(0);
    for _ in 0..20 {
        host.tick(&PlayerInput::default());
    }
    assert!(
        host.player().health < MAX_HEALTH,
        "standing in fire must deal {} HP/s",
        FIRE_DAMAGE_PER_SECOND
    );
}

#[test]
fn fire_spreads_to_flammable_neighbours_respects_the_chunk_cap_and_is_extinguished() {
    let (_dir, mut host, _) = open_host("fire-spread", 42);
    let spawn = host.spawn();
    // Centre the fixture inside one chunk so the per-chunk cap is exercised
    // rather than two chunks' independent caps.
    let x = (spawn.x.floor() as i32).div_euclid(16) * 16 + 8;
    let z = (spawn.z.floor() as i32).div_euclid(16) * 16 + 8;
    let floor = platform(&mut host, x, FIRE_Y, z);
    let center = floor.offset(0, 1, 0);
    let planks = host.registry().id_or_air("openblock:planks");
    assert!(host.world_mut().set_block(center.offset(1, 0, 0), planks));
    assert!(host.world_mut().set_block(center.offset(-1, 0, 0), planks));
    assert!(host.world_mut().set_block(center.offset(0, 0, 1), planks));
    assert!(host.world_mut().set_block(center.offset(0, 0, -1), planks));
    assert!(host.ignite_fire(center));
    assert!(
        host.force_spread_fire(center),
        "forced spread must find a flammable neighbour"
    );
    assert_eq!(host.fire_count(), 2);
    let burnt = [
        center.offset(1, 0, 0),
        center.offset(-1, 0, 0),
        center.offset(0, 0, 1),
        center.offset(0, 0, -1),
    ]
    .into_iter()
    .filter(|pos| host.is_burning(*pos))
    .count();
    assert_eq!(burnt, 1, "exactly one neighbour must be consumed by the spread");
    // Spread consumes the wooden block, so no plank remains at the fire cell.
    let spread_pos = [
        center.offset(1, 0, 0),
        center.offset(-1, 0, 0),
        center.offset(0, 0, 1),
        center.offset(0, 0, -1),
    ]
    .into_iter()
    .find(|pos| host.is_burning(*pos))
    .expect("spread target");
    assert_eq!(
        host.world().get_block(spread_pos),
        host.registry().id_or_air("openblock:fire")
    );

    // Per-chunk cap: at most 8 fire cells may burn in one chunk.
    let air_y = FIRE_Y + 2;
    host.ensure_streaming_around(WorldVec::new(f64::from(x), f64::from(air_y), f64::from(z)), 1, 16);
    for dz in -1..=1 {
        for dx in -1..=1 {
            let _ = host.ignite_fire(BlockPos::new(x + dx, air_y, z + dz));
        }
    }
    assert_eq!(host.fire_count(), FIRE_MAX_PER_CHUNK);
    let extra = BlockPos::new(x + 3, air_y, z + 3);
    assert!(!host.ignite_fire(extra), "the ninth fire in one chunk must be rejected");

    // Water in the six-neighbourhood extinguishes immediately.
    let water = host.registry().id_or_air("openblock:water");
    let target = host.fires().keys().next().copied().expect("a burning cell");
    assert!(host.world_mut().set_block(target.offset(1, 0, 0), water));
    host.tick(&PlayerInput::default());
    assert!(!host.is_burning(target), "water adjacency must extinguish");
}

#[test]
fn fire_respects_the_world_wide_cap_across_chunks() {
    let (_dir, mut host, _) = open_host("fire-world-cap", 42);
    let spawn = host.spawn();
    let chunk_x = (spawn.x.floor() as i32).div_euclid(16);
    let chunk_z = (spawn.z.floor() as i32).div_euclid(16);
    let y = FIRE_Y + 2;
    let mut placed = 0usize;
    // Eight loaded chunks × eight per-chunk fires fills the global cap exactly.
    for chunk_dx in -1..=2 {
        for chunk_dz in -1..=0 {
            for slot in 0..FIRE_MAX_PER_CHUNK {
                let x = (chunk_x + chunk_dx) * 16 + 2 + (slot % 4) as i32 * 3;
                let z = (chunk_z + chunk_dz) * 16 + 2 + (slot / 4) as i32 * 4;
                assert!(
                    host.ignite_fire(BlockPos::new(x, y, z)),
                    "fire {placed} in a fresh chunk must be accepted"
                );
                placed += 1;
            }
        }
    }
    assert_eq!(placed, FIRE_MAX_ACTIVE);
    assert_eq!(host.fire_count(), FIRE_MAX_ACTIVE);
    // A ninth chunk has no per-chunk fire, so only the global cap can reject.
    let extra = BlockPos::new((chunk_x + 1) * 16 + 10, y, (chunk_z + 1) * 16 + 10);
    host.ensure_streaming_around(
        WorldVec::new(f64::from(extra.x), f64::from(y), f64::from(extra.z)),
        1,
        64,
    );
    assert!(
        host.world().chunk(extra.chunk()).is_some(),
        "the 65th-candidate chunk must be resident"
    );
    assert!(
        !host.ignite_fire(extra),
        "the 65th fire must be rejected by the world-wide cap"
    );
    assert_eq!(host.fire_count(), FIRE_MAX_ACTIVE);
}

#[test]
fn rain_extinguishes_exposed_fire_and_lightning_damages_and_ignites() {
    let (_dir, mut host, _) = open_host("rain-lightning", 42);
    let (rain_x, rain_z) = rain_column(&mut host);
    let floor = platform(&mut host, rain_x, FIRE_Y, rain_z);
    let fire_pos = floor.offset(0, 1, 0);
    assert!(host.ignite_fire(fire_pos));
    host.set_tick_count(0);
    host.tick(&PlayerInput::default());
    assert!(
        !host.is_burning(fire_pos),
        "rain on an open-sky fire must extinguish it"
    );
    assert_eq!(host.world().get_block(fire_pos), BlockId::AIR);

    // Lightning damage/ignition on a dry platform (clear weather).
    host.set_weather_state(WeatherPhase::Clear, 10_000, 0);
    let (x, z) = (rain_x + 6, rain_z);
    let floor = platform(&mut host, x, FIRE_Y, z);
    host.set_game_mode(GameMode::Survival);
    host.player_mut().pos = WorldVec::new(
        f64::from(floor.x) + 0.5,
        f64::from(floor.y) + 1.0,
        f64::from(floor.z) + 0.5,
    );
    host.player_mut().velocity = WorldVec::ZERO;
    host.set_tick_count(0);
    let health_before = host.player().health;
    let event = host.force_lightning_at(floor).expect("lightning on a solid surface");
    assert!(event.flash);
    assert_eq!(event.block, floor);
    assert!(event.ignited);
    assert_eq!(
        host.world().get_block(floor.offset(0, 1, 0)),
        host.registry().id_or_air("openblock:fire")
    );
    assert!(
        host.player().health < health_before,
        "a direct strike must deal up to {} HP",
        LIGHTNING_DAMAGE
    );
    assert_eq!(host.take_lightning_events(), vec![event]);

    // Falloff: a player 4 m away takes no damage from the next strike.
    host.extinguish_fire(floor.offset(0, 1, 0));
    host.player_mut().pos = WorldVec::new(
        f64::from(floor.x) + 4.5,
        f64::from(floor.y) + 1.0,
        f64::from(floor.z) + 0.5,
    );
    let health_before = host.player().health;
    host.force_lightning_at(floor).expect("second strike");
    assert_eq!(host.player().health, health_before, "damage ends at 3 m");
}

#[test]
fn automatic_lightning_is_deterministic_between_hosts() {
    let (_dir_a, mut first, _) = open_host("lightning-a", 42);
    let (_dir_b, mut second, _) = open_host("lightning-b", 42);
    for host in [&mut first, &mut second] {
        host.set_weather_state(WeatherPhase::Thunder, 50_000, 1_000);
        host.set_weather_cycle(false);
        host.set_tick_count(0);
    }
    let mut first_events = Vec::new();
    let mut second_events = Vec::new();
    for tick in 1..=24_000u64 {
        first.set_tick_count(tick);
        second.set_tick_count(tick);
        first.tick_weather();
        second.tick_weather();
        first_events.append(&mut first.take_lightning_events());
        second_events.append(&mut second.take_lightning_events());
    }
    assert!(
        !first_events.is_empty(),
        "24,000 full-intensity thunder ticks must strike"
    );
    assert_eq!(
        first_events, second_events,
        "lightning must be deterministic for one seed"
    );
    assert!(
        first_events.len() < 40,
        "the rate must stay near one strike per ~2,400 ticks, saw {}",
        first_events.len()
    );
}

#[test]
fn weather_state_constructor_and_constants_are_exposed() {
    let state = WeatherState::new(42);
    assert_eq!(state.phase, WeatherPhase::Clear);
    assert_eq!(state.epoch, 0);
    assert_eq!(state.intensity(), 0.0);
    assert!(
        HostError::SaveTimeout(std::time::Duration::from_secs(1))
            .to_string()
            .contains("save")
    );
    let registry = Registry::base();
    assert!(registry.id("openblock:fire").is_some());
}
