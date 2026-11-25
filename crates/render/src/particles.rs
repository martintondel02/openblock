//! Camera-local weather, fire and effect particles.
//!
//! The renderer supports one instanced billboard pass for every particle in a
//! frame. Nothing on the CPU issues a draw call per particle: a
//! [`ParticleSystem`] simulates a bounded `Vec<Particle>`, packs the visible
//! ones into a [`ParticleInstance`] array and the GPU draws six vertices per
//! instance with a procedural quad shape (rain streak, snowflake, smoke puff,
//! ember, debris shard, XP spark or heart).
//!
//! # Design contract
//!
//! * **Bounded:** at most [`MAX_PARTICLES`] particles exist; precipitation has
//!   its own sub-cap ([`MAX_PRECIPITATION`]) so a thunderstorm can never crowd
//!   out explosion or fire feedback.
//! * **Camera-local:** precipitation spawns in a box around the eye and is
//!   recycled as the camera moves. A teleport clears out-of-range drops; a
//!   normal walk keeps them stable because spawn and cull radii overlap.
//! * **Server-authoritative inputs:** the client passes phase intensity,
//!   precipitation type, wind, ground height and sky exposure in
//!   [`WeatherParticles`]. The system never advances weather or lightning
//!   itself.
//! * **Deterministic:** a splitmix64 stream seeded per [`ParticleSystem`] makes
//!   the same camera/weather/time sequence produce byte-identical instances,
//!   which is what the GPU screenshot tests compare.
//! * **Cheap:** all math is `f32` with no allocation after the capacity is
//!   reserved. A full 2,000-drop rain frame costs well under 0.2 ms on the main
//!   thread; the GPU sees one draw call and one small vertex-buffer upload.
//!
//! The public shape is deliberately close to what a gameplay effect needs:
//! `update` for continuous emitters (`weather`, fire, smoke), `spawn_*` for
//! one-shots, and `instances` for the renderer. `ob_client` owns the policy
//! (which fires are near, when a creeper hisses, how bright a flash is); this
//! module only turns it into deterministic pixels.

use bytemuck::{Pod, Zeroable};

/// Hard cap on live particles (all kinds).
pub const MAX_PARTICLES: usize = 4_096;
/// Sub-cap for rain and snow. Leaves room for fire/smoke/debris.
pub const MAX_PRECIPITATION: usize = 2_048;
/// Rain drops at full intensity.
pub const RAIN_PARTICLES: usize = 1_700;
/// Snowflakes at full intensity. Snow reads denser with fewer, larger flakes.
pub const SNOW_PARTICLES: usize = 1_100;
/// Radius around the eye in which precipitation is replenished, in metres.
pub const PRECIPITATION_RADIUS: f32 = 14.0;
/// Horizontal distance at which a weather particle is recycled, in metres.
pub const PRECIPITATION_CULL_RADIUS: f32 = 26.0;
/// Emitted fire effects cap (embers + smoke), independent of weather.
pub const MAX_FIRE_EFFECTS: usize = 384;

/// Precipitation type selected by the server for the player column.
///
/// This mirrors `ob_protocol::PrecipitationKind` without making the renderer
/// depend on the protocol crate. `ob_client` maps one to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Precipitation {
    /// No precipitation (clear weather or a dry biome).
    #[default]
    None,
    /// Liquid rain streaks.
    Rain,
    /// Snowflakes.
    Snow,
}

impl Precipitation {
    /// Stable lower-case name, identical to the protocol tag.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Rain => "rain",
            Self::Snow => "snow",
        }
    }

    /// Parses the stable lower-case name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "none" | "clear" => Some(Self::None),
            "rain" => Some(Self::Rain),
            "snow" => Some(Self::Snow),
            _ => None,
        }
    }
}

/// Per-frame server-authoritative precipitation input for [`ParticleSystem`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherParticles {
    /// Rain / snow / none at the eye column.
    pub precipitation: Precipitation,
    /// Ramp intensity `0..=1`.
    pub intensity: f32,
    /// World-space wind in metres per second (`x`, `z`).
    pub wind: [f32; 2],
    /// Sky exposure `0..=1`: `0` under a roof or underground, `1` in the open.
    pub exposure: f32,
    /// Height of the first ground/roof below the eye, in metres.
    ///
    /// Rain and snow are recycled once they reach it, so drops never visibly
    /// fall through terrain. The client may pass a lower sentinel when it has
    /// no world data; the default is far below the camera.
    pub ground_y: f32,
}

impl Default for WeatherParticles {
    fn default() -> Self {
        Self {
            precipitation: Precipitation::None,
            intensity: 0.0,
            wind: [0.0, 0.0],
            exposure: 1.0,
            ground_y: f32::NEG_INFINITY,
        }
    }
}

impl WeatherParticles {
    /// Clamps every field to a finite, in-range value.
    pub fn sanitised(self) -> Self {
        let finite = |value: f32, fallback: f32| if value.is_finite() { value } else { fallback };
        Self {
            precipitation: self.precipitation,
            intensity: finite(self.intensity, 0.0).clamp(0.0, 1.0),
            wind: [
                finite(self.wind[0], 0.0).clamp(-60.0, 60.0),
                finite(self.wind[1], 0.0).clamp(-60.0, 60.0),
            ],
            exposure: finite(self.exposure, 1.0).clamp(0.0, 1.0),
            ground_y: finite(self.ground_y, f32::NEG_INFINITY),
        }
    }
}

/// Visual kind of one billboard. The fragment shader switches on this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum ParticleKind {
    /// A thin vertical rain streak.
    Rain = 0,
    /// A soft snowflake.
    Snow = 1,
    /// A soft smoke puff.
    Smoke = 2,
    /// A bright rising ember.
    Ember = 3,
    /// A small rotating debris shard.
    Debris = 4,
    /// A green/yellow XP spark.
    Xp = 5,
    /// A pink heart (breeding feedback).
    Heart = 6,
}

impl ParticleKind {
    /// Numeric kind uploaded to the shader.
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// Stable name for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rain => "rain",
            Self::Snow => "snow",
            Self::Smoke => "smoke",
            Self::Ember => "ember",
            Self::Debris => "debris",
            Self::Xp => "xp",
            Self::Heart => "heart",
        }
    }
}

/// One simulated particle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Particle {
    /// Shape family.
    pub kind: ParticleKind,
    /// Centre in world metres.
    pub position: [f32; 3],
    /// Velocity in metres per second.
    pub velocity: [f32; 3],
    /// Linear RGBA base colour. Alpha is scaled by the age fade.
    pub color: [f32; 4],
    /// Half extents of the billboard in metres.
    pub size: [f32; 2],
    /// Billboard rotation in radians.
    pub rotation: f32,
    /// Rotation velocity in radians per second.
    pub spin: f32,
    /// Seconds since spawn.
    pub age: f32,
    /// Total lifetime in seconds.
    pub lifetime: f32,
    /// Linear velocity damping `1/s`.
    pub drag: f32,
    /// Vertical acceleration in m/s² (negative falls).
    pub gravity: f32,
    /// Fractional size growth per second (`0` keeps the spawn size).
    pub size_growth: f32,
    /// Emissive strength passed to the shader (`0` diffuse, `1` glowing).
    pub glow: f32,
}

impl Particle {
    /// Fraction of the lifetime elapsed, clamped to `0..=1`.
    pub fn life_fraction(&self) -> f32 {
        if self.lifetime <= 0.0 {
            1.0
        } else {
            (self.age / self.lifetime).clamp(0.0, 1.0)
        }
    }

    /// Current alpha after the fade-in/fade-out envelope.
    pub fn faded_alpha(&self) -> f32 {
        let t = self.life_fraction();
        let fade_in = (t / 0.08).clamp(0.0, 1.0);
        let fade_out = ((1.0 - t) / 0.35).clamp(0.0, 1.0);
        (self.color[3] * fade_in * fade_out).clamp(0.0, 1.0)
    }

    /// Whether this particle has exceeded its lifetime.
    pub fn expired(&self) -> bool {
        self.age >= self.lifetime
    }
}

/// GPU-ready instance. 48 bytes, one instanced draw for the whole frame.
///
/// `position` is the billboard centre in world space, `size` the half extents
/// along the camera right/up axes, `rotation` rotates the quad, `kind` selects
/// the procedural shape and `glow` raises the emissive contribution.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct ParticleInstance {
    /// Billboard centre.
    pub position: [f32; 3],
    /// Half width and half height in metres.
    pub size: [f32; 2],
    /// Linear RGBA.
    pub color: [f32; 4],
    /// Rotation in radians.
    pub rotation: f32,
    /// [`ParticleKind::code`].
    pub kind: u32,
    /// Emissive strength `0..=1`.
    pub glow: f32,
}

const _: () = assert!(std::mem::size_of::<ParticleInstance>() == 48);
const _: () = assert!(std::mem::align_of::<ParticleInstance>() == 4);

impl ParticleInstance {
    /// Builds an instance from a live particle at its current age.
    pub fn from_particle(particle: &Particle) -> Self {
        let t = particle.life_fraction();
        let alpha = particle.faded_alpha();
        let grow = 1.0 + particle.size_growth * particle.age;
        Self {
            position: particle.position,
            size: [particle.size[0] * grow.max(0.05), particle.size[1] * grow.max(0.05)],
            color: [particle.color[0], particle.color[1], particle.color[2], alpha],
            rotation: particle.rotation,
            kind: particle.kind.code(),
            glow: particle.glow * (1.0 - 0.35 * t),
        }
    }
}

/// Splitmix64 step: the only randomness in the particle system.
#[inline]
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Bounded, deterministic CPU particle simulation.
///
/// One system per client. `update` drives continuous emitters, `spawn_*`
/// methods drive one-shots, and `instances` returns the frame's packed vertex
/// data. The system never allocates after the vector reaches its cap.
#[derive(Debug)]
pub struct ParticleSystem {
    seed: u64,
    rng: u64,
    particles: Vec<Particle>,
    instances: Vec<ParticleInstance>,
    /// Number of frames the simulation has advanced; used by tests.
    updates: u64,
    /// Smoothed sky exposure; rain fades rather than popping when the player
    /// steps under a roof.
    exposure: f32,
    /// Fractional spawn credit for continuous emission.
    precipitation_credit: f32,
    fire_credit: f32,
    smoke_credit: f32,
    fire_glow_credit: f32,
    /// Total weather drops spawned since construction (diagnostics).
    weather_spawned: u64,
}

impl Default for ParticleSystem {
    fn default() -> Self {
        Self::new(0x0B1A_5EED)
    }
}

impl ParticleSystem {
    /// Creates an empty system with the given deterministic seed.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            rng: seed,
            particles: Vec::with_capacity(MAX_PARTICLES.min(2_048)),
            instances: Vec::with_capacity(MAX_PARTICLES.min(2_048)),
            updates: 0,
            exposure: 1.0,
            precipitation_credit: 0.0,
            fire_credit: 0.0,
            smoke_credit: 0.0,
            fire_glow_credit: 0.0,
            weather_spawned: 0,
        }
    }

    /// Deterministic seed used at construction.
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Number of live particles.
    pub fn len(&self) -> usize {
        self.particles.len()
    }

    /// Whether no particles are live.
    pub fn is_empty(&self) -> bool {
        self.particles.is_empty()
    }

    /// Whether the simulation has reached [`MAX_PARTICLES`].
    pub fn is_full(&self) -> bool {
        self.particles.len() >= MAX_PARTICLES
    }

    /// Number of live particles of one kind.
    pub fn count_kind(&self, kind: ParticleKind) -> usize {
        self.particles.iter().filter(|particle| particle.kind == kind).count()
    }

    /// Number of live weather particles (rain + snow).
    pub fn precipitation_count(&self) -> usize {
        self.particles
            .iter()
            .filter(|particle| matches!(particle.kind, ParticleKind::Rain | ParticleKind::Snow))
            .count()
    }

    /// Updates the simulation by `dt` seconds.
    ///
    /// `dt` is clamped to `0..=0.1` so a stalled frame cannot teleport
    /// particles through walls or spawn a burst. `camera` is the eye position
    /// used for precipitation spawning/recycling.
    pub fn update(&mut self, dt: f32, camera: [f32; 3], weather: WeatherParticles) {
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.1) } else { 0.0 };
        if dt <= 0.0 {
            return;
        }
        self.updates = self.updates.saturating_add(1);
        let weather = weather.sanitised();
        let visible = weather.exposure;
        self.exposure += (visible - self.exposure) * (dt * 8.0).min(1.0);

        self.advance(dt);

        // Entering cover clears the drops immediately: with a roof overhead
        // they would otherwise keep falling inside the room for seconds. The
        // fade only applies while the camera is partially exposed (e.g. under
        // a tree canopy), where a soft stop looks better than a hard pop.
        if weather.exposure <= 0.01 {
            self.particles
                .retain(|particle| !matches!(particle.kind, ParticleKind::Rain | ParticleKind::Snow));
            self.precipitation_credit = 0.0;
            return;
        }

        self.spawn_precipitation(dt, camera, weather);
    }

    /// Pre-fills the current precipitation target in one step.
    ///
    /// Used when a screenshot or benchmark wants a fully dense storm on the
    /// first frame instead of waiting ~0.5 s for the spawn rate to converge.
    /// Purely deterministic and bounded.
    pub fn prefill_weather(&mut self, camera: [f32; 3], weather: WeatherParticles) {
        let weather = weather.sanitised();
        self.exposure = weather.exposure;
        self.particles
            .retain(|particle| !matches!(particle.kind, ParticleKind::Rain | ParticleKind::Snow));
        if weather.exposure <= 0.0 || weather.precipitation == Precipitation::None {
            return;
        }
        let target = precipitation_target(weather);
        for _ in 0..target {
            self.spawn_weather_drop(camera, weather);
        }
    }

    /// Recycles weather particles when the camera teleports.
    ///
    /// A large jump (spawn, respawn, `/tp`) leaves drops behind; they would
    /// otherwise take seconds to leave the cull radius. `previous` is the eye
    /// before the jump. The threshold is deliberately well above a sprint
    /// frame movement.
    pub fn recenter_if_needed(&mut self, previous: [f32; 3], camera: [f32; 3]) {
        if !previous.iter().all(|value| value.is_finite()) {
            self.clear_weather();
            return;
        }
        let dx = camera[0] - previous[0];
        let dy = camera[1] - previous[1];
        let dz = camera[2] - previous[2];
        if dx * dx + dy * dy + dz * dz > 48.0 * 48.0 {
            self.clear_weather();
        }
    }

    /// Removes every rain/snow particle, keeping fire and effect particles.
    pub fn clear_weather(&mut self) {
        self.particles
            .retain(|particle| !matches!(particle.kind, ParticleKind::Rain | ParticleKind::Snow));
        self.precipitation_credit = 0.0;
    }

    /// Removes every particle.
    pub fn clear(&mut self) {
        self.particles.clear();
        self.instances.clear();
        self.precipitation_credit = 0.0;
        self.fire_credit = 0.0;
        self.smoke_credit = 0.0;
        self.fire_glow_credit = 0.0;
    }

    /// Advances positions, ageing and lifetime culling.
    pub fn advance(&mut self, dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let mut index = 0;
        while index < self.particles.len() {
            let keep = {
                let particle = &mut self.particles[index];
                particle.age += dt;
                if particle.expired() {
                    false
                } else {
                    let damping = 1.0 / (1.0 + particle.drag.max(0.0) * dt);
                    particle.velocity[0] *= damping;
                    particle.velocity[1] = (particle.velocity[1] + particle.gravity * dt) * damping;
                    particle.velocity[2] *= damping;
                    particle.position[0] += particle.velocity[0] * dt;
                    particle.position[1] += particle.velocity[1] * dt;
                    particle.position[2] += particle.velocity[2] * dt;
                    particle.rotation += particle.spin * dt;
                    particle.color[0] = particle.color[0].clamp(0.0, 8.0);
                    particle.color[1] = particle.color[1].clamp(0.0, 8.0);
                    particle.color[2] = particle.color[2].clamp(0.0, 8.0);
                    true
                }
            };
            if keep {
                index += 1;
            } else {
                self.particles.swap_remove(index);
            }
        }
    }

    /// Emits embers and smoke from burning blocks.
    ///
    /// `positions` should be the block centres of nearby `openblock:fire`
    /// blocks. The client calls this every frame (or every few frames) with
    /// the fires it found; this method throttles itself to a bounded rate.
    pub fn emit_fire(&mut self, positions: &[[f32; 3]], dt: f32, wind: [f32; 2]) {
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.1) } else { 0.0 };
        if dt <= 0.0 || positions.is_empty() {
            return;
        }
        let wind = [
            if wind[0].is_finite() { wind[0] } else { 0.0 },
            if wind[1].is_finite() { wind[1] } else { 0.0 },
        ];
        let fire_live = self
            .particles
            .iter()
            .filter(|particle| matches!(particle.kind, ParticleKind::Ember | ParticleKind::Smoke))
            .count();
        if fire_live >= MAX_FIRE_EFFECTS {
            return;
        }
        let rate = (positions.len().min(8) as f32 * 10.0).min(60.0);
        self.fire_credit += rate * dt;
        self.smoke_credit += rate * 0.45 * dt;
        self.fire_glow_credit += positions.len().min(6) as f32 * 3.0 * dt;
        let mut budget = ((MAX_FIRE_EFFECTS - fire_live) as f32).min(self.fire_credit.floor()) as usize;
        self.fire_credit = (self.fire_credit - budget as f32).max(0.0);
        let mut smoke_budget = ((MAX_FIRE_EFFECTS - fire_live) / 2).min(self.smoke_credit.floor() as usize);
        self.smoke_credit = (self.smoke_credit - smoke_budget as f32).max(0.0);
        let mut glow_budget = (6_usize).min(self.fire_glow_credit.floor() as usize);
        self.fire_glow_credit = (self.fire_glow_credit - glow_budget as f32).max(0.0);
        while glow_budget > 0 {
            glow_budget -= 1;
            let fire = positions[(self.random_u64() as usize) % positions.len()];
            self.spawn_fire_glow(fire);
        }
        while budget > 0 {
            budget -= 1;
            let fire = positions[(self.random_u64() as usize) % positions.len()];
            self.spawn_fire_ember(fire, wind);
        }
        while smoke_budget > 0 {
            smoke_budget -= 1;
            let fire = positions[(self.random_u64() as usize) % positions.len()];
            self.spawn_fire_smoke(fire, wind);
        }
    }

    /// One fire ember at a block centre.
    pub fn spawn_fire_ember(&mut self, position: [f32; 3], wind: [f32; 2]) {
        if self.is_full() {
            return;
        }
        let jitter = |value: f32| value.clamp(-0.5, 0.5);
        let jx = jitter(self.random_signed());
        let jz = jitter(self.random_signed());
        let lift = self.random_f32();
        let heat = 0.55 + self.random_f32() * 0.45;
        let fire = [position[0] + jx, position[1] + 0.2 + lift * 0.7, position[2] + jz];
        let vx = wind[0] * 0.25 + self.random_signed() * 0.35;
        let vy = 1.2 + self.random_f32() * 1.4;
        let vz = wind[1] * 0.25 + self.random_signed() * 0.35;
        let rotation = self.random_f32() * std::f32::consts::TAU;
        let spin = self.random_signed() * 4.0;
        let lifetime = 0.7 + self.random_f32() * 0.9;
        self.particles.push(Particle {
            kind: ParticleKind::Ember,
            position: fire,
            velocity: [vx, vy, vz],
            color: [1.0, 0.42 + heat * 0.38, 0.08, 0.95],
            size: [0.035, 0.035],
            rotation,
            spin,
            age: 0.0,
            lifetime,
            drag: 0.8,
            gravity: 0.35,
            size_growth: 0.25,
            glow: 1.0,
        });
    }

    /// One soft warm halo at a fire base.
    ///
    /// This is a presentation-only stand-in for the missing point-light pass:
    /// a large, low-alpha billboard adds a warm bloom pool around the block.
    /// It never changes light data or gameplay visibility.
    pub fn spawn_fire_glow(&mut self, position: [f32; 3]) {
        if self.is_full() {
            return;
        }
        let jitter = 0.25;
        let x = position[0] + self.random_signed() * jitter;
        let y = position[1] + 0.15 + self.random_f32() * 0.25;
        let z = position[2] + self.random_signed() * jitter;
        let lifetime = 0.30 + self.random_f32() * 0.12;
        self.particles.push(Particle {
            kind: ParticleKind::Ember,
            position: [x, y, z],
            velocity: [0.0, 0.12, 0.0],
            color: [1.0, 0.34, 0.06, 0.16],
            size: [0.85, 0.55],
            rotation: 0.0,
            spin: 0.0,
            age: 0.0,
            lifetime,
            drag: 1.0,
            gravity: 0.0,
            size_growth: 0.2,
            glow: 0.35,
        });
    }

    /// One smoke puff at a block centre.
    pub fn spawn_fire_smoke(&mut self, position: [f32; 3], wind: [f32; 2]) {
        if self.is_full() {
            return;
        }
        let shade = 0.10 + self.random_f32() * 0.10;
        let px = position[0] + self.random_signed() * 0.35;
        let py = position[1] + 0.7 + self.random_f32() * 0.5;
        let pz = position[2] + self.random_signed() * 0.35;
        let vx = wind[0] * 0.18 + self.random_signed() * 0.18;
        let vy = 0.55 + self.random_f32() * 0.35;
        let vz = wind[1] * 0.18 + self.random_signed() * 0.18;
        let rotation = self.random_f32() * std::f32::consts::TAU;
        let spin = self.random_signed() * 0.6;
        let lifetime = 1.6 + self.random_f32() * 1.4;
        self.particles.push(Particle {
            kind: ParticleKind::Smoke,
            position: [px, py, pz],
            velocity: [vx, vy, vz],
            color: [shade + 0.035, shade + 0.03, shade + 0.028, 0.30],
            size: [0.18, 0.18],
            rotation,
            spin,
            age: 0.0,
            lifetime,
            drag: 0.35,
            gravity: 0.18,
            size_growth: 1.1,
            glow: 0.0,
        });
    }

    /// Spawns the smoke/debris/spark burst of one explosion.
    ///
    /// `power` is the server radius in metres; the particle count scales with
    /// it but is capped so a malicious radius cannot exhaust the pool.
    pub fn spawn_explosion(&mut self, position: [f32; 3], power: f32) {
        if !position.iter().all(|value| value.is_finite()) {
            return;
        }
        let power = if power.is_finite() { power.clamp(0.5, 8.0) } else { 3.0 };
        let smoke = (12.0 + power * 7.0).min(80.0) as usize;
        let debris = (8.0 + power * 4.0).min(48.0) as usize;
        let sparks = (5.0 + power * 2.0).min(24.0) as usize;
        for _ in 0..smoke {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(0.8);
            let speed = 1.5 + self.random_f32() * power * 0.9;
            let rotation = self.random_f32() * std::f32::consts::TAU;
            let spin = self.random_signed() * 0.8;
            let lifetime = 1.2 + self.random_f32() * 1.8;
            self.particles.push(Particle {
                kind: ParticleKind::Smoke,
                position: [
                    position[0] + direction[0] * 0.4,
                    position[1] + 0.3 + direction[1] * 0.4,
                    position[2] + direction[2] * 0.4,
                ],
                velocity: [direction[0] * speed, direction[1] * speed + 0.6, direction[2] * speed],
                color: [0.14, 0.13, 0.12, 0.55],
                size: [0.55, 0.55],
                rotation,
                spin,
                age: 0.0,
                lifetime,
                drag: 1.1,
                gravity: 0.5,
                size_growth: 1.5,
                glow: 0.0,
            });
        }
        for _ in 0..debris {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(1.0);
            let speed = 2.0 + self.random_f32() * power * 1.5;
            let rotation = self.random_f32() * std::f32::consts::TAU;
            let spin = self.random_signed() * 9.0;
            let lifetime = 0.6 + self.random_f32() * 0.8;
            self.particles.push(Particle {
                kind: ParticleKind::Debris,
                position,
                velocity: [
                    direction[0] * speed,
                    direction[1].abs() * speed + 2.0,
                    direction[2] * speed,
                ],
                color: [0.34, 0.30, 0.25, 0.9],
                size: [0.07, 0.07],
                rotation,
                spin,
                age: 0.0,
                lifetime,
                drag: 0.15,
                gravity: -9.8,
                size_growth: 0.0,
                glow: 0.0,
            });
        }
        // A short-lived orange fireball at the centre. It is a handful of big
        // emissive billboards rather than a light source; block-light damage is
        // the server's job and the server owns explosion terrain changes.
        let fireball = (5.0 + power * 2.0).min(18.0) as usize;
        for _ in 0..fireball {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(0.5);
            let offset = 0.15 + self.random_f32() * 0.35;
            let lifetime = 0.22 + self.random_f32() * 0.30;
            let size = 0.18 + power * 0.035;
            let green = 0.48 + self.random_f32() * 0.18;
            self.particles.push(Particle {
                kind: ParticleKind::Ember,
                position: [
                    position[0] + direction[0] * offset,
                    position[1] + 0.5 + direction[1].abs() * offset,
                    position[2] + direction[2] * offset,
                ],
                velocity: [direction[0] * 0.6, 0.5 + direction[1].abs() * 0.8, direction[2] * 0.6],
                color: [1.0, green, 0.10, 0.85],
                size: [size, size],
                rotation: 0.0,
                spin: 0.0,
                age: 0.0,
                lifetime,
                drag: 1.2,
                gravity: 0.4,
                size_growth: 0.6,
                glow: 1.0,
            });
        }
        for _ in 0..sparks {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(1.0);
            let speed = 3.0 + self.random_f32() * power * 2.2;
            let lifetime = 0.35 + self.random_f32() * 0.45;
            self.particles.push(Particle {
                kind: ParticleKind::Ember,
                position,
                velocity: [direction[0] * speed, direction[1].abs() * speed, direction[2] * speed],
                color: [1.0, 0.62, 0.18, 0.95],
                size: [0.055, 0.055],
                rotation: 0.0,
                spin: 0.0,
                age: 0.0,
                lifetime,
                drag: 0.6,
                gravity: -6.0,
                size_growth: 0.0,
                glow: 1.0,
            });
        }
    }

    /// Spawns a short spark/smoke burst at a lightning strike point.
    ///
    /// The bright flash and thunder are presentation state in the client; this
    /// only gives a nearby strike a physical focal point (a few embers and a
    /// wisp of smoke) without pretending to know the server's ignition outcome.
    pub fn spawn_lightning_strike(&mut self, position: [f32; 3]) {
        if !position.iter().all(|value| value.is_finite()) {
            return;
        }
        for _ in 0..14 {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(1.0);
            let speed = 3.0 + self.random_f32() * 6.0;
            let lifetime = 0.20 + self.random_f32() * 0.30;
            self.particles.push(Particle {
                kind: ParticleKind::Ember,
                position: [
                    position[0] + direction[0] * 0.25,
                    position[1] + 0.4 + direction[1].abs() * 0.8,
                    position[2] + direction[2] * 0.25,
                ],
                velocity: [direction[0] * speed, direction[1].abs() * speed, direction[2] * speed],
                color: [0.80, 0.88, 1.0, 0.95],
                size: [0.03, 0.03],
                rotation: 0.0,
                spin: 0.0,
                age: 0.0,
                lifetime,
                drag: 0.4,
                gravity: -4.0,
                size_growth: 0.0,
                glow: 1.0,
            });
        }
        for _ in 0..4 {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(0.4);
            let lifetime = 0.7 + self.random_f32() * 0.8;
            let rise = self.random_f32();
            self.particles.push(Particle {
                kind: ParticleKind::Smoke,
                position: [position[0], position[1] + 0.6, position[2]],
                velocity: [direction[0] * 0.6, 0.8 + rise * 0.6, direction[2] * 0.6],
                color: [0.17, 0.18, 0.20, 0.35],
                size: [0.22, 0.22],
                rotation: 0.0,
                spin: 0.0,
                age: 0.0,
                lifetime,
                drag: 0.8,
                gravity: 0.3,
                size_growth: 1.2,
                glow: 0.0,
            });
        }
    }

    /// Spawns a small green/yellow XP pickup sparkle.
    pub fn spawn_xp_pickup(&mut self, position: [f32; 3], amount: u32) {
        if !position.iter().all(|value| value.is_finite()) {
            return;
        }
        let count = (3 + amount.min(12)) as usize;
        for _ in 0..count {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(1.0);
            let green = self.random_f32() < 0.4;
            let rise = self.random_f32();
            let rotation = self.random_f32() * std::f32::consts::TAU;
            let spin = self.random_signed() * 3.0;
            let lifetime = 0.45 + self.random_f32() * 0.35;
            self.particles.push(Particle {
                kind: ParticleKind::Xp,
                position: [
                    position[0] + direction[0] * 0.25,
                    position[1] + 0.35 + direction[1].abs() * 0.3,
                    position[2] + direction[2] * 0.25,
                ],
                velocity: [direction[0] * 0.8, 1.2 + rise, direction[2] * 0.8],
                color: if green {
                    [0.78, 1.0, 0.28, 0.95]
                } else {
                    [0.95, 0.92, 0.22, 0.95]
                },
                size: [0.045, 0.045],
                rotation,
                spin,
                age: 0.0,
                lifetime,
                drag: 2.0,
                gravity: -0.4,
                size_growth: 0.0,
                glow: 0.8,
            });
        }
    }

    /// Spawns a tiny heart burst (breeding feedback).
    pub fn spawn_hearts(&mut self, position: [f32; 3], count: usize) {
        if !position.iter().all(|value| value.is_finite()) {
            return;
        }
        for _ in 0..count.min(8) {
            if self.is_full() {
                break;
            }
            let direction = self.random_direction(0.6);
            let lift = self.random_f32();
            let rise = self.random_f32();
            let lifetime = 0.8 + self.random_f32() * 0.4;
            self.particles.push(Particle {
                kind: ParticleKind::Heart,
                position: [
                    position[0] + direction[0] * 0.3,
                    position[1] + 0.9 + lift * 0.4,
                    position[2] + direction[2] * 0.3,
                ],
                velocity: [direction[0] * 0.3, 0.65 + rise * 0.4, direction[2] * 0.3],
                color: [1.0, 0.32, 0.46, 0.95],
                size: [0.09, 0.09],
                rotation: 0.0,
                spin: 0.0,
                age: 0.0,
                lifetime,
                drag: 1.0,
                gravity: 0.1,
                size_growth: 0.15,
                glow: 0.2,
            });
        }
    }

    /// Packs every live particle into instanced render data.
    ///
    /// The slice is valid until the next mutable call on the system.
    pub fn instances(&mut self) -> &[ParticleInstance] {
        self.instances.clear();
        self.instances.reserve(self.particles.len());
        for particle in &self.particles {
            self.instances.push(ParticleInstance::from_particle(particle));
        }
        &self.instances
    }

    /// Number of packed instances from the last [`Self::instances`] call.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    /// Number of simulation updates since construction.
    pub const fn updates(&self) -> u64 {
        self.updates
    }

    /// Total weather drops spawned since construction.
    pub const fn weather_spawned(&self) -> u64 {
        self.weather_spawned
    }

    fn spawn_precipitation(&mut self, dt: f32, camera: [f32; 3], weather: WeatherParticles) {
        // Blend the authoritative exposure with the smoothed value so a
        // canopy edge fades the drops instead of popping them.
        let target = (precipitation_target(weather) as f32 * self.exposure.clamp(0.0, 1.0)).round() as usize;
        if target == 0 {
            self.precipitation_credit = 0.0;
            return;
        }
        let mut current = self.precipitation_count();
        if current > target {
            // Intensity dropped: recycle the excess rather than waiting for
            // its natural lifetime, so the rain visibly eases off.
            self.trim_precipitation(target);
            current = target;
        }
        let deficit = target.saturating_sub(current);
        let rate = target as f32 * 1.6 + 40.0;
        self.precipitation_credit = (self.precipitation_credit + rate * dt).min(MAX_PRECIPITATION as f32);
        let spawn_now = (self.precipitation_credit.floor() as usize).min(deficit);
        self.precipitation_credit -= spawn_now as f32;
        for _ in 0..spawn_now {
            self.spawn_weather_drop(camera, weather);
        }
        // Recycle drops that left the local box (fast movement / wind).
        let cull_sq = PRECIPITATION_CULL_RADIUS * PRECIPITATION_CULL_RADIUS;
        let ground = weather.ground_y;
        self.particles.retain(|particle| {
            if !matches!(particle.kind, ParticleKind::Rain | ParticleKind::Snow) {
                return true;
            }
            let dx = particle.position[0] - camera[0];
            let dz = particle.position[2] - camera[2];
            !(dx * dx + dz * dz > cull_sq || particle.position[1] < ground)
        });
    }

    fn spawn_weather_drop(&mut self, camera: [f32; 3], weather: WeatherParticles) {
        if self.particles.len() >= MAX_PARTICLES || self.precipitation_count() >= MAX_PRECIPITATION {
            return;
        }
        // Uniform disc around the eye. New drops fill the whole local volume so
        // walking does not leave a bare trailing edge.
        let angle = self.random_f32() * std::f32::consts::TAU;
        let radius = PRECIPITATION_RADIUS * (0.30 + 0.70 * self.random_f32().sqrt());
        let x = camera[0] + angle.cos() * radius;
        let z = camera[2] + angle.sin() * radius;
        let y = camera[1] + 3.0 + self.random_f32() * 8.0;
        match weather.precipitation {
            Precipitation::Rain => {
                let speed = 13.0 + self.random_f32() * 6.0;
                let vx = weather.wind[0] * 1.4 + self.random_signed() * 0.6;
                let vz = weather.wind[1] * 1.4 + self.random_signed() * 0.6;
                let length = 0.32 + self.random_f32() * 0.22;
                let rotation = self.random_signed() * 0.08;
                self.particles.push(Particle {
                    kind: ParticleKind::Rain,
                    position: [x, y, z],
                    velocity: [vx, -speed, vz],
                    color: [0.52, 0.62, 0.78, 0.55],
                    size: [0.012, length],
                    rotation,
                    spin: 0.0,
                    age: 0.0,
                    lifetime: 2.5,
                    drag: 0.0,
                    gravity: 0.0,
                    size_growth: 0.0,
                    glow: 0.0,
                });
            }
            Precipitation::Snow => {
                let vx = weather.wind[0] * 0.7 + self.random_signed() * 0.35;
                let vy = -1.1 - self.random_f32() * 0.8;
                let vz = weather.wind[1] * 0.7 + self.random_signed() * 0.35;
                let rotation = self.random_f32() * std::f32::consts::TAU;
                let spin = self.random_signed() * 1.4;
                self.particles.push(Particle {
                    kind: ParticleKind::Snow,
                    position: [x, y, z],
                    velocity: [vx, vy, vz],
                    color: [0.94, 0.96, 1.0, 0.9],
                    size: [0.05, 0.05],
                    rotation,
                    spin,
                    age: 0.0,
                    lifetime: 6.0,
                    drag: 0.0,
                    gravity: 0.0,
                    size_growth: 0.0,
                    glow: 0.0,
                });
            }
            Precipitation::None => {}
        }
        self.weather_spawned = self.weather_spawned.saturating_add(1);
    }

    fn trim_precipitation(&mut self, target: usize) {
        let mut excess = self.precipitation_count().saturating_sub(target);
        while excess > 0 {
            if let Some(index) = self
                .particles
                .iter()
                .position(|particle| matches!(particle.kind, ParticleKind::Rain | ParticleKind::Snow))
            {
                self.particles.swap_remove(index);
                excess -= 1;
            } else {
                break;
            }
        }
    }

    fn random_u64(&mut self) -> u64 {
        splitmix64(&mut self.rng)
    }

    fn random_f32(&mut self) -> f32 {
        // 24 significant bits are plenty and never reach exactly 1.0.
        ((self.random_u64() >> 40) as f32) / ((1u64 << 24) as f32)
    }

    fn random_signed(&mut self) -> f32 {
        self.random_f32() * 2.0 - 1.0
    }

    fn random_direction(&mut self, up_bias: f32) -> [f32; 3] {
        loop {
            let x = self.random_signed();
            let y = self.random_signed();
            let z = self.random_signed();
            let length_sq = x * x + y * y + z * z;
            if length_sq > 1e-6 && length_sq <= 1.0 {
                let scale = 1.0 / length_sq.sqrt();
                return [x * scale, (y * scale + up_bias * 0.25).clamp(-1.0, 1.0), z * scale];
            }
        }
    }
}

/// Target weather particle count for one weather input.
pub fn precipitation_target(weather: WeatherParticles) -> usize {
    let weather = weather.sanitised();
    if weather.precipitation == Precipitation::None || weather.exposure <= 0.0 {
        return 0;
    }
    let base = match weather.precipitation {
        Precipitation::Rain => RAIN_PARTICLES,
        Precipitation::Snow => SNOW_PARTICLES,
        Precipitation::None => return 0,
    };
    ((base as f32 * weather.intensity * weather.exposure).round() as usize).min(MAX_PRECIPITATION)
}

/// Wind vector used for precipitation and fire, in metres per second.
///
/// The server sends only phase/intensity/kind; the client derives a gentle,
/// slowly rotating breeze from the weather epoch so rain reads as wind-blown
/// without becoming a simulation authority. `epoch` may be any monotonic
/// integer; the result is deterministic.
pub fn weather_wind(epoch: u64, intensity: f32, snow: bool) -> [f32; 2] {
    let phase = ((epoch % 360) as f32).to_radians() * 0.35;
    let strength = if snow { 0.9 } else { 2.2 } * intensity.clamp(0.0, 1.0).max(0.2);
    [phase.cos() * strength, phase.sin() * strength]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rain_weather(intensity: f32) -> WeatherParticles {
        WeatherParticles {
            precipitation: Precipitation::Rain,
            intensity,
            wind: [1.0, -0.5],
            exposure: 1.0,
            ground_y: 0.0,
        }
    }

    fn camera() -> [f32; 3] {
        [8.0, 64.0, -3.0]
    }

    #[test]
    fn defaults_are_clear_and_empty() {
        let system = ParticleSystem::default();
        assert!(system.is_empty());
        assert_eq!(system.len(), 0);
        assert_eq!(system.seed(), 0x0B1A_5EED);
        assert_eq!(Precipitation::default(), Precipitation::None);
        assert_eq!(Precipitation::from_name("snow"), Some(Precipitation::Snow));
        assert_eq!(Precipitation::from_name("hail"), None);
        assert_eq!(Precipitation::Rain.name(), "rain");
    }

    #[test]
    fn instance_layout_is_48_bytes_and_pod() {
        assert_eq!(std::mem::size_of::<ParticleInstance>(), 48);
        let instance = ParticleInstance {
            position: [1.0, 2.0, 3.0],
            size: [0.1, 0.2],
            color: [1.0, 0.5, 0.25, 0.75],
            rotation: 0.3,
            kind: ParticleKind::Snow.code(),
            glow: 0.0,
        };
        let bytes = bytemuck::bytes_of(&instance);
        assert_eq!(bytes.len(), 48);
        assert_eq!(ParticleKind::Rain.code(), 0);
        assert_eq!(ParticleKind::Heart.code(), 6);
    }

    #[test]
    fn rain_prefill_follows_the_camera_and_respects_intensity() {
        let mut system = ParticleSystem::new(7);
        system.prefill_weather(camera(), rain_weather(0.5));
        let expected = RAIN_PARTICLES / 2;
        assert!(system.precipitation_count().abs_diff(expected) <= 2);
        let drops: Vec<_> = system
            .particles
            .iter()
            .filter(|particle| particle.kind == ParticleKind::Rain)
            .collect();
        assert!(drops.iter().all(|drop| {
            let dx = drop.position[0] - camera()[0];
            let dz = drop.position[2] - camera()[2];
            dx * dx + dz * dz <= PRECIPITATION_RADIUS * PRECIPITATION_RADIUS + 0.01
        }));
        // Full intensity produces more, and snow produces a different kind.
        let mut full = ParticleSystem::new(7);
        full.prefill_weather(camera(), rain_weather(1.0));
        assert_eq!(full.precipitation_count(), RAIN_PARTICLES);
        let mut snow = ParticleSystem::new(7);
        snow.prefill_weather(
            camera(),
            WeatherParticles {
                precipitation: Precipitation::Snow,
                ..rain_weather(1.0)
            },
        );
        assert_eq!(snow.count_kind(ParticleKind::Snow), SNOW_PARTICLES);
        assert_eq!(snow.count_kind(ParticleKind::Rain), 0);
        assert_eq!(precipitation_target(rain_weather(0.0)), 0);
    }

    #[test]
    fn exposure_zero_clears_drops_and_prevents_spawning() {
        let mut system = ParticleSystem::new(11);
        system.prefill_weather(camera(), rain_weather(1.0));
        assert!(system.precipitation_count() > 0);
        system.update(
            1.0 / 60.0,
            camera(),
            WeatherParticles {
                exposure: 0.0,
                ..rain_weather(1.0)
            },
        );
        assert_eq!(system.precipitation_count(), 0);
        assert!(system.is_empty());
        assert_eq!(
            precipitation_target(WeatherParticles {
                exposure: 0.0,
                ..rain_weather(1.0)
            }),
            0
        );
    }

    #[test]
    fn drops_recycle_below_the_ground_height() {
        let mut system = ParticleSystem::new(3);
        system.prefill_weather(camera(), rain_weather(1.0));
        // Pull one drop below the ground plane and advance enough frames for
        // the recycling pass to run.
        for drop in system
            .particles
            .iter_mut()
            .filter(|particle| particle.kind == ParticleKind::Rain)
            .take(10)
        {
            drop.position[1] = -1.0;
        }
        system.update(1.0 / 60.0, camera(), rain_weather(1.0));
        assert!(
            system
                .particles
                .iter()
                .filter(|particle| particle.kind == ParticleKind::Rain)
                .all(|drop| drop.position[1] >= 0.0)
        );
    }

    #[test]
    fn spawn_is_deterministic_and_bounded() {
        fn run() -> Vec<ParticleInstance> {
            let mut system = ParticleSystem::new(42);
            system.prefill_weather(camera(), rain_weather(1.0));
            system.emit_fire(&[[8.0, 64.0, -3.0], [9.0, 64.0, -3.0]], 1.0 / 60.0, [2.0, 0.0]);
            system.spawn_explosion([10.0, 64.0, -3.0], 3.0);
            system.spawn_xp_pickup([8.5, 64.0, -3.5], 5);
            system.spawn_hearts([9.0, 64.0, -2.0], 3);
            for _ in 0..30 {
                system.update(1.0 / 60.0, camera(), rain_weather(0.8));
            }
            system.instances().to_vec()
        }
        let first = run();
        let second = run();
        assert_eq!(first, second);
        let mut system = ParticleSystem::new(5);
        system.prefill_weather(camera(), rain_weather(1.0));
        for _ in 0..20 {
            system.spawn_explosion([0.0, 64.0, 0.0], 8.0);
        }
        assert!(system.len() <= MAX_PARTICLES);
        assert!(system.precipitation_count() <= MAX_PRECIPITATION);
        assert!(system.instances().len() <= MAX_PARTICLES);
    }

    #[test]
    fn fire_emits_embers_and_smoke_without_unbounded_growth() {
        let mut system = ParticleSystem::new(9);
        let fires = [[0.0, 64.0, 0.0], [2.0, 64.0, 0.0]];
        for _ in 0..600 {
            system.emit_fire(&fires, 1.0 / 60.0, [1.0, 0.0]);
            system.advance(1.0 / 60.0);
        }
        assert!(system.count_kind(ParticleKind::Ember) > 0);
        assert!(system.count_kind(ParticleKind::Smoke) > 0);
        assert!(system.len() <= MAX_PARTICLES);
        assert!(
            system
                .particles
                .iter()
                .filter(|particle| matches!(particle.kind, ParticleKind::Ember | ParticleKind::Smoke))
                .count()
                <= MAX_FIRE_EFFECTS
        );
    }

    #[test]
    fn explosion_burst_has_smoke_debris_and_sparks() {
        let mut system = ParticleSystem::new(1);
        system.spawn_explosion([4.0, 70.0, 4.0], 4.0);
        assert!(system.count_kind(ParticleKind::Smoke) >= 12);
        assert!(system.count_kind(ParticleKind::Debris) >= 8);
        assert!(system.count_kind(ParticleKind::Ember) >= 5);
        assert!(system.len() <= MAX_PARTICLES);
        // Invalid input is ignored instead of producing NaN instances.
        system.spawn_explosion([f32::NAN, 0.0, 0.0], 4.0);
        assert!(
            system
                .particles
                .iter()
                .all(|particle| particle.position.iter().all(|value| value.is_finite()))
        );
    }

    #[test]
    fn recenter_clears_stale_weather_on_teleport() {
        let mut system = ParticleSystem::new(2);
        system.prefill_weather(camera(), rain_weather(1.0));
        assert!(system.precipitation_count() > 0);
        system.recenter_if_needed(camera(), [camera()[0] + 100.0, camera()[1], camera()[2]]);
        assert_eq!(system.precipitation_count(), 0);
        system.prefill_weather(camera(), rain_weather(1.0));
        let count = system.precipitation_count();
        system.recenter_if_needed(camera(), [camera()[0] + 0.5, camera()[1], camera()[2]]);
        assert_eq!(system.precipitation_count(), count);
    }

    #[test]
    fn hostile_inputs_are_sanitised() {
        let weather = WeatherParticles {
            precipitation: Precipitation::Rain,
            intensity: f32::NAN,
            wind: [f32::INFINITY, f32::NAN],
            exposure: f32::NAN,
            ground_y: f32::NAN,
        }
        .sanitised();
        assert_eq!(weather.intensity, 0.0);
        assert_eq!(weather.wind, [0.0, 0.0]);
        assert_eq!(weather.exposure, 1.0);
        assert_eq!(weather.ground_y, f32::NEG_INFINITY);
        let mut system = ParticleSystem::new(1);
        system.update(f32::NAN, camera(), weather);
        system.update(-1.0, camera(), weather);
        system.advance(f32::INFINITY);
        assert!(system.is_empty());
    }

    #[test]
    fn wind_is_deterministic_and_bounded() {
        let first = weather_wind(42, 1.0, false);
        assert_eq!(first, weather_wind(42, 1.0, false));
        assert_ne!(first, weather_wind(43, 1.0, false));
        assert!(first[0].is_finite() && first[1].is_finite());
        assert!(first[0].hypot(first[1]) <= 2.3);
        let snow = weather_wind(42, 1.0, true);
        assert!(snow[0].hypot(snow[1]) < first[0].hypot(first[1]));
    }

    #[test]
    fn particle_fade_and_lifetime() {
        let mut particle = Particle {
            kind: ParticleKind::Smoke,
            position: [0.0; 3],
            velocity: [0.0; 3],
            color: [0.2, 0.2, 0.2, 0.5],
            size: [0.2, 0.2],
            rotation: 0.0,
            spin: 0.0,
            age: 0.0,
            lifetime: 2.0,
            drag: 0.0,
            gravity: 0.0,
            size_growth: 0.0,
            glow: 0.0,
        };
        assert!(!particle.expired());
        assert!(particle.faded_alpha() < 0.01, "fade-in starts at zero");
        particle.age = 1.0;
        assert!((particle.faded_alpha() - 0.5).abs() < 1e-6);
        particle.age = 2.0;
        assert!(particle.expired());
        assert_eq!(particle.life_fraction(), 1.0);
        let instance = ParticleInstance::from_particle(&particle);
        assert_eq!(instance.color[3], 0.0);
    }
}
