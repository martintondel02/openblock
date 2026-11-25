//! Blocky mob avatars and per-id smooth movement for the client renderer.
//!
//! The host owns mob simulation. This module only turns live [`MobEntity`]
//! state into untextured [`EntityInstance`] cubes: four to eight cubes per
//! species, feet-anchored at `Mob::pos`, rotated around the mob origin by
//! `Mob::yaw`. A red-ish tint marks recently hurt mobs and a small walk bob
//! keeps idle avatars from looking frozen.
//!
//! [`MobRenderCache`] interpolates between the previous and current
//! authoritative tick positions using the frame's tick alpha, keyed by stable
//! mob id. The pure [`mob_instances`] helper used by tests and offscreen
//! screenshots composes avatars directly from a mob slice.

use std::collections::HashMap;

use ob_content::{MobKind, Registry};
use ob_math::WorldVec;
use ob_render::EntityInstance;
use ob_server::MobEntity;

/// How strongly a hurt mob is tinted red (green and blue scale factors).
pub const HURT_TINT: [f32; 3] = [1.0, 0.35, 0.35];
/// Horizontal speed above which a mob is treated as walking.
pub const WALK_SPEED_THRESHOLD: f64 = 0.15;
/// Body bob amplitude in metres.
pub const WALK_BOB: f32 = 0.025;
/// Leg swing amplitude in metres.
pub const LEG_SWING: f32 = 0.07;

/// Palette used by one species' avatar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MobPalette {
    /// Main body colour.
    pub body: [f32; 3],
    /// Head/face colour.
    pub head: [f32; 3],
    /// Accent colour (snout, beak, patch or shirt).
    pub accent: [f32; 3],
    /// Leg/limb colour.
    pub limb: [f32; 3],
}

/// One interpolated mob pose ready for cube composition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MobVisual {
    /// Species.
    pub kind: MobKind,
    /// Interpolated feet centre.
    pub pos: WorldVec,
    /// Facing angle in radians.
    pub yaw: f32,
    /// Whether the hurt tint applies.
    pub hurt: bool,
    /// Walk-cycle phase in radians.
    pub walk_phase: f32,
    /// Whether the avatar should bob/swing.
    pub moving: bool,
    /// Inherited colour variant (`0..=3`).
    pub variant: u8,
    /// Whether the baby half-scale applies.
    pub baby: bool,
}

/// Fixed palette per species.
pub fn palette(kind: MobKind) -> MobPalette {
    match kind {
        MobKind::Cow => MobPalette {
            body: [0.42, 0.27, 0.15],
            head: [0.35, 0.22, 0.13],
            accent: [0.80, 0.65, 0.55],
            limb: [0.24, 0.16, 0.10],
        },
        MobKind::Pig => MobPalette {
            body: [0.93, 0.55, 0.58],
            head: [0.90, 0.50, 0.53],
            accent: [0.80, 0.40, 0.44],
            limb: [0.72, 0.36, 0.40],
        },
        MobKind::Sheep => MobPalette {
            body: [0.91, 0.90, 0.85],
            head: [0.45, 0.37, 0.31],
            accent: [0.97, 0.96, 0.92],
            limb: [0.40, 0.38, 0.36],
        },
        MobKind::Chicken => MobPalette {
            body: [0.95, 0.94, 0.90],
            head: [0.98, 0.97, 0.93],
            accent: [0.94, 0.75, 0.22],
            limb: [0.86, 0.64, 0.18],
        },
        MobKind::Zombie => MobPalette {
            body: [0.22, 0.34, 0.52],
            head: [0.34, 0.53, 0.28],
            accent: [0.80, 0.18, 0.16],
            limb: [0.18, 0.22, 0.34],
        },
        MobKind::Spider => MobPalette {
            body: [0.16, 0.14, 0.13],
            head: [0.22, 0.19, 0.17],
            accent: [0.72, 0.12, 0.10],
            limb: [0.10, 0.09, 0.09],
        },
        MobKind::Skeleton => MobPalette {
            body: [0.80, 0.80, 0.74],
            head: [0.88, 0.88, 0.82],
            accent: [0.55, 0.54, 0.50],
            limb: [0.72, 0.72, 0.66],
        },
        MobKind::Creeper => MobPalette {
            body: [0.27, 0.63, 0.24],
            head: [0.31, 0.70, 0.27],
            accent: [0.08, 0.09, 0.07],
            limb: [0.20, 0.47, 0.18],
        },
    }
}

/// Applies a small deterministic colour-variant shift to a palette.
///
/// Variant 0 is the base colour; 1–3 tint the body/head/limb slightly warmer,
/// cooler or darker so bred offspring are visibly different without new icon
/// or render assets.
fn vary_palette(mut palette: MobPalette, variant: u8) -> MobPalette {
    let shift = match variant % 4 {
        0 => [1.0, 1.0, 1.0],
        1 => [1.08, 0.96, 0.90],
        2 => [0.92, 0.97, 1.08],
        _ => [0.92, 0.92, 0.88],
    };
    for channel in 0..3 {
        palette.body[channel] = (palette.body[channel] * shift[channel]).clamp(0.0, 1.0);
        palette.head[channel] = (palette.head[channel] * shift[channel]).clamp(0.0, 1.0);
        palette.limb[channel] = (palette.limb[channel] * shift[channel]).clamp(0.0, 1.0);
    }
    palette
}

/// Turns every live mob into cubes, using the authoritative positions.
///
/// The composition is deterministic for a given `(mob, elapsed)` pair; walk
/// motion only reads the mob's velocity and stable id so a screenshot or test
/// does not depend on wall-clock time.
pub fn mob_instances(registry: &Registry, mobs: &[MobEntity], elapsed: f32) -> Vec<EntityInstance> {
    let visuals: Vec<MobVisual> = mobs
        .iter()
        .map(|entity| {
            let speed = entity.mob.velocity.x.hypot(entity.mob.velocity.z);
            MobVisual {
                kind: entity.mob.kind,
                pos: entity.mob.pos,
                yaw: entity.mob.yaw,
                hurt: entity.mob.hurt_timer > 0.0,
                walk_phase: elapsed * 8.0 + entity.id as f32 * 0.9,
                moving: speed > WALK_SPEED_THRESHOLD,
                variant: entity.mob.variant.min(3),
                baby: entity.mob.is_baby(),
            }
        })
        .collect();
    mob_instances_from_visuals(registry, &visuals)
}

/// Composes cubes for already-interpolated mob poses.
///
/// Each mob contributes four to eight untextured cubes. Cube positions are
/// offsets around the mob's feet origin rotated by its yaw, so the body faces
/// the movement direction even though the renderer only rotates each cube about
/// its own base centre.
pub fn mob_instances_from_visuals(registry: &Registry, visuals: &[MobVisual]) -> Vec<EntityInstance> {
    let mut instances = Vec::with_capacity(visuals.len() * 8);
    for visual in visuals {
        // Skip unknown kinds defensively; the base registry always has all eight.
        if visual.kind.index() >= registry.mob_len() {
            continue;
        }
        compose_mob(registry, visual, &mut instances);
    }
    instances
}

/// Rotates a local `(x, z)` offset around the mob origin.
///
/// Yaw zero faces `-Z`, matching [`Player::yaw`](ob_sim::Player::yaw), so local
/// forward is negative Z.
pub fn rotate_offset(yaw: f32, x: f32, z: f32) -> (f32, f32) {
    let (sin, cos) = yaw.sin_cos();
    (x * cos + z * sin, -x * sin + z * cos)
}

/// Appends one cube at a local offset, applying yaw, hurt tint and walk bob.
fn push_cube(out: &mut Vec<EntityInstance>, visual: &MobVisual, local: [f32; 3], scale: f32, tint: [f32; 3]) {
    let (x, z) = rotate_offset(visual.yaw, local[0], local[2]);
    let bob = if visual.moving && local[1] > 0.1 {
        visual.walk_phase.sin().abs() * WALK_BOB
    } else {
        0.0
    };
    let tint = if visual.hurt {
        [tint[0], tint[1] * HURT_TINT[1], tint[2] * HURT_TINT[2]]
    } else {
        tint
    };
    out.push(EntityInstance {
        position: [
            visual.pos.x as f32 + x,
            visual.pos.y as f32 + local[1] + bob,
            visual.pos.z as f32 + z,
        ],
        scale,
        yaw: visual.yaw,
        layer: 0,
        tint,
        textured: false,
    });
}

/// Appends four leg cubes with an alternating walk swing.
fn push_quadruped_legs(out: &mut Vec<EntityInstance>, visual: &MobVisual, tint: [f32; 3], scale: f32, x: f32, z: f32) {
    let swing = if visual.moving {
        visual.walk_phase.sin() * LEG_SWING
    } else {
        0.0
    };
    for (index, (side, front)) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)]
        .into_iter()
        .enumerate()
    {
        let phase = if index.is_multiple_of(2) { swing } else { -swing };
        push_cube(out, visual, [side * x, 0.0, front * z + phase], scale, tint);
    }
}

/// Reference collision-box dimensions the hard-coded compositions are tuned
/// for. A content pack may resize a species; [`compose_mob`] then scales the
/// avatar's offsets uniformly so it still matches its MobSpec box.
fn reference_dimensions(kind: MobKind) -> (f32, f32) {
    match kind {
        MobKind::Cow => (0.9, 1.3),
        MobKind::Pig => (0.9, 0.9),
        MobKind::Sheep => (0.9, 1.3),
        MobKind::Chicken => (0.4, 0.7),
        MobKind::Zombie | MobKind::Skeleton | MobKind::Creeper => (0.6, 1.8),
        MobKind::Spider => (1.0, 0.9),
    }
}

/// Species-specific cube composition.
///
/// `EntityInstance::position` is the *base* centre of a cube (the cube extends
/// from `position.y` to `position.y + scale`), so body cubes start just above
/// the legs and heads start at the body top rather than using centre offsets.
/// Cube offsets and edges are then scaled from the reference dimensions to the
/// live [`MobSpec`](ob_content::MobSpec) width/height.
fn compose_mob(registry: &Registry, visual: &MobVisual, out: &mut Vec<EntityInstance>) {
    let start = out.len();
    compose_mob_parts(visual, out);
    let spec = registry.mob(visual.kind);
    let (reference_width, reference_height) = reference_dimensions(visual.kind);
    let sx = (spec.width / reference_width).clamp(0.25, 4.0);
    let sy = (spec.height / reference_height).clamp(0.25, 4.0);
    let uniform = (sx + sy) * 0.5;
    let needs_spec_scale = (sx - 1.0).abs() >= 1e-6 || (sy - 1.0).abs() >= 1e-6;
    if needs_spec_scale {
        for cube in &mut out[start..] {
            cube.position[0] = visual.pos.x as f32 + (cube.position[0] - visual.pos.x as f32) * sx;
            cube.position[1] = visual.pos.y as f32 + (cube.position[1] - visual.pos.y as f32) * sy;
            cube.position[2] = visual.pos.z as f32 + (cube.position[2] - visual.pos.z as f32) * sx;
            cube.scale *= uniform;
        }
    }
    if visual.baby {
        // Babies are half-scale around the same feet anchor. Drops are empty
        // and growth is server-authoritative; this is visual only.
        for cube in &mut out[start..] {
            cube.position[0] = visual.pos.x as f32 + (cube.position[0] - visual.pos.x as f32) * 0.5;
            cube.position[1] = visual.pos.y as f32 + (cube.position[1] - visual.pos.y as f32) * 0.5;
            cube.position[2] = visual.pos.z as f32 + (cube.position[2] - visual.pos.z as f32) * 0.5;
            cube.scale *= 0.5;
        }
    }
}

/// Hard-coded per-species cube layout before MobSpec scaling.
fn compose_mob_parts(visual: &MobVisual, out: &mut Vec<EntityInstance>) {
    let palette = vary_palette(palette(visual.kind), visual.variant);
    match visual.kind {
        MobKind::Cow => {
            push_quadruped_legs(out, visual, palette.limb, 0.19, 0.23, 0.24);
            push_cube(out, visual, [0.0, 0.20, 0.02], 0.62, palette.body);
            push_cube(out, visual, [0.0, 0.82, -0.44], 0.38, palette.head);
            push_cube(out, visual, [0.0, 0.85, -0.61], 0.17, palette.accent);
            // White flank patch sits half-embedded in the body side.
            push_cube(out, visual, [0.30, 0.44, 0.08], 0.15, [0.93, 0.91, 0.86]);
        }
        MobKind::Pig => {
            push_quadruped_legs(out, visual, palette.limb, 0.12, 0.16, 0.16);
            push_cube(out, visual, [0.0, 0.13, 0.0], 0.42, palette.body);
            push_cube(out, visual, [0.0, 0.54, -0.32], 0.28, palette.head);
            push_cube(out, visual, [0.0, 0.56, -0.47], 0.14, palette.accent);
        }
        MobKind::Sheep => {
            push_quadruped_legs(out, visual, palette.limb, 0.16, 0.21, 0.25);
            push_cube(out, visual, [0.0, 0.17, 0.0], 0.55, palette.body);
            push_cube(out, visual, [0.0, 0.72, -0.47], 0.30, palette.head);
            push_cube(out, visual, [0.0, 1.02, -0.10], 0.18, palette.accent);
        }
        MobKind::Chicken => {
            let swing = if visual.moving {
                visual.walk_phase.sin() * LEG_SWING * 0.5
            } else {
                0.0
            };
            push_cube(out, visual, [-0.06, 0.0, swing], 0.05, palette.limb);
            push_cube(out, visual, [0.06, 0.0, -swing], 0.05, palette.limb);
            push_cube(out, visual, [0.0, 0.10, 0.0], 0.30, palette.body);
            push_cube(out, visual, [0.0, 0.40, -0.11], 0.18, palette.head);
            push_cube(out, visual, [0.0, 0.42, -0.22], 0.07, palette.accent);
            push_cube(out, visual, [0.0, 0.58, -0.09], 0.06, palette.accent);
            push_cube(out, visual, [0.0, 0.30, 0.15], 0.09, palette.accent);
        }
        MobKind::Zombie => {
            let swing = if visual.moving {
                visual.walk_phase.sin() * LEG_SWING
            } else {
                0.0
            };
            // One tall leg cube per side plus two stacked torso cubes keeps the
            // humanoid silhouette close to the 1.8 m collision height without
            // exceeding the eight-cube avatar budget.
            push_cube(out, visual, [-0.14, 0.0, swing], 0.42, palette.limb);
            push_cube(out, visual, [0.14, 0.0, -swing], 0.42, palette.limb);
            push_cube(out, visual, [0.0, 0.42, 0.0], 0.45, palette.body);
            push_cube(out, visual, [0.0, 0.87, 0.0], 0.42, palette.body);
            push_cube(out, visual, [-0.33, 0.88, 0.0], 0.18, palette.head);
            push_cube(out, visual, [0.33, 0.88, 0.0], 0.18, palette.head);
            push_cube(out, visual, [0.0, 1.29, -0.03], 0.35, palette.head);
        }
        MobKind::Skeleton => {
            let swing = if visual.moving {
                visual.walk_phase.sin() * LEG_SWING
            } else {
                0.0
            };
            push_cube(out, visual, [-0.13, 0.0, swing], 0.40, palette.limb);
            push_cube(out, visual, [0.13, 0.0, -swing], 0.40, palette.limb);
            push_cube(out, visual, [0.0, 0.40, 0.0], 0.44, palette.body);
            push_cube(out, visual, [0.0, 0.84, 0.0], 0.34, palette.accent);
            push_cube(out, visual, [0.0, 1.18, -0.02], 0.34, palette.head);
            push_cube(out, visual, [-0.31, 0.52, -0.20], 0.12, palette.limb);
            push_cube(out, visual, [0.31, 0.52, -0.20], 0.12, palette.limb);
        }
        MobKind::Creeper => {
            let swing = if visual.moving {
                visual.walk_phase.sin() * LEG_SWING * 0.6
            } else {
                0.0
            };
            push_quadruped_legs(out, visual, palette.limb, 0.16, 0.18, 0.20);
            push_cube(out, visual, [0.0, 0.18, 0.0], 0.52, palette.body);
            push_cube(out, visual, [0.0, 0.70, -0.02], 0.38, palette.head);
            push_cube(out, visual, [-0.13, 1.04, -0.21], 0.04, palette.accent);
            push_cube(out, visual, [0.13, 1.04, -0.21], 0.04, palette.accent);
            let _ = swing;
        }
        MobKind::Spider => {
            let swing = if visual.moving {
                visual.walk_phase.sin() * LEG_SWING
            } else {
                0.0
            };
            for (index, side) in [-1.0f32, 1.0].into_iter().enumerate() {
                let phase = if index == 0 { swing } else { -swing };
                push_cube(out, visual, [side * 0.34, 0.0, -0.18 + phase], 0.10, palette.limb);
                push_cube(out, visual, [side * 0.34, 0.0, 0.18 - phase], 0.10, palette.limb);
            }
            push_cube(out, visual, [0.0, 0.14, 0.0], 0.40, palette.body);
            push_cube(out, visual, [0.0, 0.46, -0.28], 0.28, palette.head);
            push_cube(out, visual, [-0.12, 0.54, -0.38], 0.05, palette.accent);
            push_cube(out, visual, [0.12, 0.54, -0.38], 0.05, palette.accent);
        }
    }
}

/// Builds small cubes for host-simulated arrows in single-player mode.
///
/// Mirrors the network client's arrow rendering so skeleton combat looks the
/// same in both client paths without touching the render crate.
pub fn projectile_instances(projectiles: &[ob_server::ProjectileEntity]) -> Vec<EntityInstance> {
    let mut instances = Vec::with_capacity(projectiles.len() * 2);
    for entity in projectiles {
        let pos = entity.projectile.pos;
        instances.push(EntityInstance {
            position: [pos.x as f32, pos.y as f32, pos.z as f32],
            scale: 0.14,
            yaw: 0.0,
            layer: 0,
            tint: [0.82, 0.78, 0.66],
            textured: false,
        });
        instances.push(EntityInstance {
            position: [pos.x as f32, pos.y as f32 + 0.12, pos.z as f32],
            scale: 0.09,
            yaw: 0.0,
            layer: 0,
            tint: [0.42, 0.39, 0.33],
            textured: false,
        });
    }
    instances
}

/// Per-mob interpolation state keyed by stable session id.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CachedMob {
    kind: MobKind,
    previous: WorldVec,
    current: WorldVec,
    yaw: f32,
    hurt: bool,
    walk_phase: f32,
    moving: bool,
    variant: u8,
    baby: bool,
}

/// Smooths authoritative mob positions between fixed ticks.
///
/// Call [`MobRenderCache::sync`] once per frame after the host ticks, then
/// [`MobRenderCache::instances`] with the frame's interpolation alpha
/// (`accumulator / TICK`). Entries for despawned ids are dropped immediately.
#[derive(Debug, Default, Clone)]
pub struct MobRenderCache {
    entries: HashMap<u64, CachedMob>,
}

impl MobRenderCache {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked mobs.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no mobs are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Updates the cache from the authoritative population.
    ///
    /// `dt` advances the walk phase; pass `0.0` while the simulation is paused.
    pub fn sync(&mut self, mobs: &[MobEntity], dt: f32) {
        let mut alive = Vec::with_capacity(mobs.len());
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        for entity in mobs {
            let id = entity.id;
            alive.push(id);
            let pos = entity.mob.pos;
            let speed = entity.mob.velocity.x.hypot(entity.mob.velocity.z);
            let moving = speed > WALK_SPEED_THRESHOLD;
            match self.entries.get_mut(&id) {
                Some(cached) => {
                    if cached.current != pos {
                        cached.previous = cached.current;
                        cached.current = pos;
                    }
                    cached.kind = entity.mob.kind;
                    cached.yaw = entity.mob.yaw;
                    cached.hurt = entity.mob.hurt_timer > 0.0;
                    cached.moving = moving;
                    cached.variant = entity.mob.variant.min(3);
                    cached.baby = entity.mob.is_baby();
                    if moving {
                        cached.walk_phase = (cached.walk_phase + dt * speed as f32 * 5.0) % std::f32::consts::TAU;
                    } else {
                        cached.walk_phase = 0.0;
                    }
                }
                None => {
                    self.entries.insert(
                        id,
                        CachedMob {
                            kind: entity.mob.kind,
                            previous: pos,
                            current: pos,
                            yaw: entity.mob.yaw,
                            hurt: entity.mob.hurt_timer > 0.0,
                            walk_phase: 0.0,
                            moving,
                            variant: entity.mob.variant.min(3),
                            baby: entity.mob.is_baby(),
                        },
                    );
                }
            }
        }
        self.entries.retain(|id, _| alive.contains(id));
    }

    /// Interpolated poses, ordered by mob id for deterministic draw order.
    pub fn visuals(&self, alpha: f32) -> Vec<MobVisual> {
        let alpha = if alpha.is_finite() { alpha.clamp(0.0, 1.0) } else { 1.0 };
        let mut ids: Vec<u64> = self.entries.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| {
                let cached = self.entries.get(&id)?;
                Some(MobVisual {
                    kind: cached.kind,
                    pos: cached.previous.lerp(cached.current, f64::from(alpha)),
                    yaw: cached.yaw,
                    hurt: cached.hurt,
                    walk_phase: cached.walk_phase,
                    moving: cached.moving,
                    variant: cached.variant,
                    baby: cached.baby,
                })
            })
            .collect()
    }
}

impl MobRenderCache {
    /// Interpolated cubes for every tracked mob.
    pub fn instances(&self, registry: &Registry, alpha: f32) -> Vec<EntityInstance> {
        mob_instances_from_visuals(registry, &self.visuals(alpha))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::Registry;
    use ob_math::WorldVec;
    use ob_server::MobEntity;
    use ob_sim::Mob;

    fn registry() -> Registry {
        Registry::base()
    }

    fn entity(registry: &Registry, id: u64, kind: MobKind, pos: WorldVec) -> MobEntity {
        MobEntity::new(id, Mob::new(registry.mob(kind), pos))
    }

    fn instances_for(registry: &Registry, kind: MobKind, pos: WorldVec, elapsed: f32) -> Vec<EntityInstance> {
        let mobs = [entity(registry, 1, kind, pos)];
        mob_instances(registry, &mobs, elapsed)
    }

    #[test]
    fn every_species_has_four_to_eight_feet_anchored_cubes() {
        let registry = registry();
        for kind in MobKind::ALL {
            let pos = WorldVec::new(3.5, 64.0, -2.25);
            let instances = instances_for(&registry, kind, pos, 1.0);
            assert!(
                (4..=8).contains(&instances.len()),
                "{kind:?} produced {} cubes",
                instances.len()
            );
            assert!(instances.iter().all(|cube| !cube.textured));
            assert!(instances.iter().all(|cube| cube.yaw == 0.0));
            for cube in &instances {
                assert!(
                    cube.position[1] >= pos.y as f32 - 1e-4,
                    "{kind:?} cube sinks below the feet anchor"
                );
                assert!(cube.scale > 0.0);
            }
            let lowest = instances
                .iter()
                .map(|cube| cube.position[1])
                .fold(f32::INFINITY, f32::min);
            assert!(
                (lowest - pos.y as f32).abs() < 1e-4,
                "{kind:?} must have at least one cube exactly at the feet"
            );
        }
    }

    #[test]
    fn yaw_rotates_the_composite_around_the_mob_origin() {
        let registry = registry();
        let pos = WorldVec::new(10.0, 64.0, 10.0);
        let north = instances_for(&registry, MobKind::Zombie, pos, 0.0);
        // Yaw zero faces -Z: the head is in front (negative Z) of the feet.
        let north_head = north
            .iter()
            .max_by(|a, b| a.position[1].total_cmp(&b.position[1]))
            .unwrap();
        assert!(north_head.position[2] < pos.z as f32);

        let mut mobs = [entity(&registry, 2, MobKind::Zombie, pos)];
        mobs[0].mob.yaw = std::f32::consts::FRAC_PI_2;
        let east = mob_instances(&registry, &mobs, 0.0);
        let east_head = east
            .iter()
            .max_by(|a, b| a.position[1].total_cmp(&b.position[1]))
            .unwrap();
        // Yaw +90° faces -X, so the head rotates to negative X.
        assert!(east_head.position[0] < pos.x as f32);
        assert!((east_head.position[2] - pos.z as f32).abs() < 1e-4);
        assert!(
            east.iter()
                .all(|cube| (cube.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5)
        );
    }

    #[test]
    fn species_palettes_are_recognisable() {
        let registry = registry();
        let pos = WorldVec::new(0.0, 64.0, 0.0);
        let cow = instances_for(&registry, MobKind::Cow, pos, 0.0);
        assert!(cow.iter().any(|cube| cube.tint.iter().all(|channel| *channel > 0.85)));
        assert!(cow.iter().any(|cube| {
            let [r, g, b] = cube.tint;
            r > g && g > b && r < 0.6
        }));

        let pig = instances_for(&registry, MobKind::Pig, pos, 0.0);
        let pink = pig.iter().max_by(|a, b| a.tint[0].total_cmp(&b.tint[0])).unwrap();
        assert!(pink.tint[0] > 0.8 && pink.tint[0] > pink.tint[1] && pink.tint[0] > pink.tint[2]);

        let sheep = instances_for(&registry, MobKind::Sheep, pos, 0.0);
        assert!(sheep.iter().any(|cube| cube.tint[0] > 0.9 && cube.tint[1] > 0.88));
        assert!(sheep.iter().any(|cube| {
            let [r, g, b] = cube.tint;
            r < 0.6 && g < 0.5 && b < 0.45
        }));

        let chicken = instances_for(&registry, MobKind::Chicken, pos, 0.0);
        assert!(chicken.iter().any(|cube| {
            let [r, g, b] = cube.tint;
            r > 0.85 && g > 0.6 && b < 0.35
        }));

        let zombie = instances_for(&registry, MobKind::Zombie, pos, 0.0);
        assert!(zombie.iter().any(|cube| {
            let [r, g, b] = cube.tint;
            g > r && g > b && r < 0.5
        }));
    }

    #[test]
    fn babies_are_half_scale_and_variants_tint_differently() {
        let registry = registry();
        let pos = WorldVec::new(4.0, 64.0, 4.0);
        let mut mobs = [entity(&registry, 21, MobKind::Cow, pos)];
        let adult = mob_instances(&registry, &mobs, 0.0);
        mobs[0].mob.growth_timer = 100.0;
        let baby = mob_instances(&registry, &mobs, 0.0);
        assert_eq!(adult.len(), baby.len());
        for (adult_cube, baby_cube) in adult.iter().zip(&baby) {
            assert!(baby_cube.scale < adult_cube.scale);
            assert!(baby_cube.position[1] >= pos.y as f32 - 1e-4);
            assert!(
                baby_cube.position[1] <= adult_cube.position[1] + 1e-4,
                "a baby must not be taller than its adult"
            );
        }
        let plain = instances_for(&registry, MobKind::Sheep, pos, 0.0);
        let mut variant_mobs = [entity(&registry, 22, MobKind::Sheep, pos)];
        variant_mobs[0].mob.variant = 2;
        let varied = mob_instances(&registry, &variant_mobs, 0.0);
        assert_ne!(plain[0].tint, varied[0].tint, "variant must change the palette");
    }

    #[test]
    fn projectile_instances_render_as_small_arrow_cubes() {
        let projectiles = [ob_server::ProjectileEntity {
            id: 1,
            projectile: ob_sim::Projectile::new(
                ob_content::ProjectileKind::Arrow,
                WorldVec::new(1.0, 2.0, 3.0),
                WorldVec::new(0.0, 0.0, 1.0),
                3.0,
            ),
            target_player: None,
        }];
        let cubes = projectile_instances(&projectiles);
        assert_eq!(cubes.len(), 2);
        assert!(cubes.iter().all(|cube| cube.scale < 0.2 && !cube.textured));
        assert!(projectile_instances(&[]).is_empty());
    }

    #[test]
    fn hurt_tint_reddens_the_same_avatar() {
        let registry = registry();
        let pos = WorldVec::new(5.0, 64.0, 5.0);
        let healthy = instances_for(&registry, MobKind::Cow, pos, 0.4);
        let mut mobs = [entity(&registry, 7, MobKind::Cow, pos)];
        mobs[0].mob.hurt_timer = 2.0;
        let hurt = mob_instances(&registry, &mobs, 0.4);
        assert_eq!(healthy.len(), hurt.len());
        for (normal, hurt) in healthy.iter().zip(&hurt) {
            assert_eq!(normal.position, hurt.position);
            assert_eq!(normal.tint[0], hurt.tint[0]);
            assert!(hurt.tint[1] < normal.tint[1]);
            assert!(hurt.tint[2] < normal.tint[2]);
        }
    }

    #[test]
    fn walking_changes_the_pose_but_idle_does_not() {
        let registry = registry();
        let pos = WorldVec::new(1.0, 64.0, 1.0);
        let idle = instances_for(&registry, MobKind::Pig, pos, 0.0);
        let idle_later = instances_for(&registry, MobKind::Pig, pos, 2.0);
        assert_eq!(idle, idle_later);

        let mut mobs = [entity(&registry, 3, MobKind::Pig, pos)];
        mobs[0].mob.velocity = WorldVec::new(1.2, 0.0, 0.0);
        let walking = mob_instances(&registry, &mobs, 0.3);
        let walking_later = mob_instances(&registry, &mobs, 0.9);
        assert_ne!(walking, walking_later);
    }

    #[test]
    fn render_cache_interpolates_positions_and_drops_dead_mobs() {
        let registry = registry();
        let pos = WorldVec::new(0.0, 64.0, 0.0);
        let mut cache = MobRenderCache::new();
        let mut mobs = [entity(&registry, 11, MobKind::Cow, pos)];
        cache.sync(&mobs, 0.05);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.visuals(0.0)[0].pos, pos);
        assert_eq!(cache.visuals(1.0)[0].pos, pos);

        mobs[0].mob.pos = WorldVec::new(2.0, 64.0, 0.0);
        cache.sync(&mobs, 0.05);
        assert_eq!(cache.visuals(0.0)[0].pos, pos);
        assert_eq!(cache.visuals(1.0)[0].pos, WorldVec::new(2.0, 64.0, 0.0));
        let mid = cache.visuals(0.5)[0].pos;
        assert!((mid.x - 1.0).abs() < 1e-9);

        let cubes = cache.instances(&registry, 0.5);
        assert!(cubes.len() >= 4);
        cache.sync(&[], 0.05);
        assert!(cache.is_empty());
        assert!(cache.instances(&registry, 0.5).is_empty());
    }

    #[test]
    fn reference_dimensions_match_the_base_mob_specs() {
        let registry = registry();
        for kind in MobKind::ALL {
            let spec = registry.mob(kind);
            assert_eq!(
                (spec.width, spec.height),
                reference_dimensions(kind),
                "{kind:?} avatar scaling would drift from its collision box"
            );
        }
    }

    #[test]
    fn rotate_offset_matches_the_yaw_convention() {
        let (x, z) = rotate_offset(0.0, 0.0, -1.0);
        assert!(x.abs() < 1e-6 && (z + 1.0).abs() < 1e-6, "yaw zero faces -Z");
        let (x, z) = rotate_offset(std::f32::consts::FRAC_PI_2, 0.0, -1.0);
        assert!((x + 1.0).abs() < 1e-6 && z.abs() < 1e-6, "yaw +90° faces -X");
    }
}
