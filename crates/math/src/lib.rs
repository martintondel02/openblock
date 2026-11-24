//! Voxel-aware math: coordinate spaces, AABBs and deterministic RNG.
//!
//! Every coordinate space in OpenBlock has a distinct newtype here. Raw
//! `f64`/`i32` triples are not accepted across crate boundaries; convert
//! explicitly. This is the foundation for the determinism rules in
//! `docs/spec/00-conventions.md`.

use glam::{DVec3, Vec3};

pub use glam::{DVec3 as WorldVec, Mat4, Vec3 as RenderVec, Vec4};

/// Section edge length in blocks (16³ storage unit, DEC-004).
pub const SECTION_SIZE: usize = 16;
/// Horizontal chunk column size in blocks.
pub const CHUNK_SIZE: i32 = 16;
/// Sections per chunk column for the base dimensions (`-64..320`).
pub const N_SECTIONS: usize = 24;
/// Lowest block Y of the base dimensions.
pub const WORLD_MIN_Y: i32 = -64;
/// Exclusive highest block Y of the base dimensions.
pub const WORLD_MAX_Y: i32 = 320;
/// Sea level used by world generation and the renderer.
pub const SEA_LEVEL: i32 = 62;

/// A block position in world space (integer block coordinates).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockPos {
    /// X coordinate (east).
    pub x: i32,
    /// Y coordinate (up).
    pub y: i32,
    /// Z coordinate (south).
    pub z: i32,
}

impl BlockPos {
    /// Creates a block position.
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// The chunk column containing this block.
    pub const fn chunk(self) -> ChunkPos {
        ChunkPos {
            x: self.x.div_euclid(CHUNK_SIZE),
            z: self.z.div_euclid(CHUNK_SIZE),
        }
    }

    /// The section containing this block.
    pub const fn section(self) -> SectionPos {
        SectionPos {
            x: self.x.div_euclid(SECTION_SIZE as i32),
            y: self.y.div_euclid(SECTION_SIZE as i32),
            z: self.z.div_euclid(SECTION_SIZE as i32),
        }
    }

    /// The position inside its section.
    pub const fn local(self) -> LocalPos {
        LocalPos {
            x: self.x.rem_euclid(SECTION_SIZE as i32) as u8,
            y: self.y.rem_euclid(SECTION_SIZE as i32) as u8,
            z: self.z.rem_euclid(SECTION_SIZE as i32) as u8,
        }
    }

    /// Whether the block is inside the base dimension height range.
    pub const fn in_world(self) -> bool {
        self.y >= WORLD_MIN_Y && self.y < WORLD_MAX_Y
    }

    /// Component-wise offset.
    pub const fn offset(self, dx: i32, dy: i32, dz: i32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            z: self.z + dz,
        }
    }

    /// Centre of the block as a world-space vector.
    pub fn center(self) -> DVec3 {
        DVec3::new(self.x as f64 + 0.5, self.y as f64 + 0.5, self.z as f64 + 0.5)
    }
}

/// A chunk column position (16×16 blocks in XZ).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkPos {
    /// Chunk X coordinate.
    pub x: i32,
    /// Chunk Z coordinate.
    pub z: i32,
}

impl ChunkPos {
    /// Creates a chunk position.
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }

    /// World-space XZ origin of the chunk.
    pub const fn origin(self) -> (i32, i32) {
        (self.x * CHUNK_SIZE, self.z * CHUNK_SIZE)
    }

    /// Squared XZ distance to another chunk, in chunks.
    pub fn dist_sq(self, other: ChunkPos) -> i32 {
        let dx = self.x - other.x;
        let dz = self.z - other.z;
        dx * dx + dz * dz
    }
}

/// A 16³ section position (the storage, edit, light and mesh unit).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SectionPos {
    /// Section X coordinate.
    pub x: i32,
    /// Section Y coordinate.
    pub y: i32,
    /// Section Z coordinate.
    pub z: i32,
}

impl SectionPos {
    /// Creates a section position.
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// The chunk column containing this section.
    pub const fn chunk(self) -> ChunkPos {
        ChunkPos { x: self.x, z: self.z }
    }

    /// World-space origin block of the section.
    pub const fn origin(self) -> BlockPos {
        BlockPos {
            x: self.x * SECTION_SIZE as i32,
            y: self.y * SECTION_SIZE as i32,
            z: self.z * SECTION_SIZE as i32,
        }
    }

    /// Index of the section inside its chunk column.
    pub const fn index(self) -> usize {
        (self.y - (WORLD_MIN_Y / SECTION_SIZE as i32)) as usize
    }

    /// Section position from block coordinates.
    pub const fn from_block(pos: BlockPos) -> Self {
        pos.section()
    }
}

/// A position inside a section (0..16 on each axis).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalPos {
    /// Local X (0..16).
    pub x: u8,
    /// Local Y (0..16).
    pub y: u8,
    /// Local Z (0..16).
    pub z: u8,
}

impl LocalPos {
    /// The dense section index of this local position.
    pub const fn index(self) -> usize {
        ((self.y as usize) << 8) | ((self.z as usize) << 4) | (self.x as usize)
    }

    /// Local position from a dense section index.
    pub const fn from_index(index: usize) -> Self {
        Self {
            x: (index & 0xF) as u8,
            y: ((index >> 8) & 0xF) as u8,
            z: ((index >> 4) & 0xF) as u8,
        }
    }
}

/// An axis-aligned bounding box in world space (f64 metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    /// Minimum corner.
    pub min: DVec3,
    /// Maximum corner.
    pub max: DVec3,
}

impl Aabb {
    /// Creates an AABB from two corners (order-insensitive).
    pub fn new(a: DVec3, b: DVec3) -> Self {
        Self {
            min: a.min(b),
            max: a.max(b),
        }
    }

    /// Creates an AABB from a centre and a size.
    pub fn from_center_size(center: DVec3, size: DVec3) -> Self {
        let half = size * 0.5;
        Self {
            min: center - half,
            max: center + half,
        }
    }

    /// Creates an AABB around one block.
    pub fn block(pos: BlockPos) -> Self {
        let origin = DVec3::new(pos.x as f64, pos.y as f64, pos.z as f64);
        Self {
            min: origin,
            max: origin + DVec3::splat(1.0),
        }
    }

    /// Whether the two boxes overlap (touching faces do not count as overlap).
    pub fn intersects(&self, other: &Aabb) -> bool {
        self.min.x < other.max.x
            && self.max.x > other.min.x
            && self.min.y < other.max.y
            && self.max.y > other.min.y
            && self.min.z < other.max.z
            && self.max.z > other.min.z
    }

    /// Whether a point is inside the box.
    pub fn contains_point(&self, p: DVec3) -> bool {
        p.x >= self.min.x
            && p.x <= self.max.x
            && p.y >= self.min.y
            && p.y <= self.max.y
            && p.z >= self.min.z
            && p.z <= self.max.z
    }

    /// The box translated by a vector.
    pub fn translated(&self, delta: DVec3) -> Self {
        Self {
            min: self.min + delta,
            max: self.max + delta,
        }
    }

    /// Centre of the box.
    pub fn center(&self) -> DVec3 {
        (self.min + self.max) * 0.5
    }

    /// Size of the box.
    pub fn size(&self) -> DVec3 {
        self.max - self.min
    }
}

/// Deterministic splitmix64-based RNG. Never seeded from wall-clock time.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Creates a generator from a seed.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix64(self.state)
    }

    /// Next 32-bit value.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Next float in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Next float in `[0, 1)` as f64.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform integer in `[0, bound)`.
    pub fn next_below(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as u32
    }
}

/// splitmix64 finaliser: a fast, well-distributed integer hash.
pub const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Derives a deterministic sub-seed from a base seed and coordinates.
pub fn derive_seed(seed: u64, a: i64, b: i64, c: i64) -> u64 {
    mix64(seed ^ mix64(a as u64) ^ mix64((b as u64).wrapping_mul(31)) ^ mix64(c as u64))
}

/// Euclidean division that rounds towards negative infinity for i32.
pub const fn floor_div(a: i32, b: i32) -> i32 {
    a.div_euclid(b)
}

/// A camera-independent forward vector from yaw/pitch (radians).
///
/// Yaw 0 faces `-Z` (north); positive yaw rotates clockwise viewed from above.
pub fn forward_from_angles(yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    Vec3::new(-sy * cp, sp, -cy * cp)
}

/// A right-handed, Y-up perspective projection with a `[0, 1]` depth range
/// (the convention wgpu uses).
pub fn perspective_rh_zo(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fov_y * 0.5).tan();
    Mat4::from_cols(
        glam::Vec4::new(f / aspect, 0.0, 0.0, 0.0),
        glam::Vec4::new(0.0, f, 0.0, 0.0),
        glam::Vec4::new(0.0, 0.0, far / (near - far), -1.0),
        glam::Vec4::new(0.0, 0.0, near * far / (near - far), 0.0),
    )
}

/// A right-handed view matrix looking along a direction.
///
/// `direction` and `up` are normalised internally; `direction` must not be
/// parallel to `up`.
pub fn look_to_rh(eye: Vec3, direction: Vec3, up: Vec3) -> Mat4 {
    let f = direction.normalize();
    let s = f.cross(up).normalize();
    let u = s.cross(f);
    Mat4::from_cols(
        glam::Vec4::new(s.x, u.x, -f.x, 0.0),
        glam::Vec4::new(s.y, u.y, -f.y, 0.0),
        glam::Vec4::new(s.z, u.z, -f.z, 0.0),
        glam::Vec4::new(-s.dot(eye), -u.dot(eye), f.dot(eye), 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_index_round_trips() {
        for y in 0..16u8 {
            for z in 0..16u8 {
                for x in 0..16u8 {
                    let p = LocalPos { x, y, z };
                    assert_eq!(p, LocalPos::from_index(p.index()));
                }
            }
        }
    }

    #[test]
    fn negative_coordinates_floor_correctly() {
        let p = BlockPos::new(-1, -65, -17);
        assert_eq!(p.chunk(), ChunkPos::new(-1, -2));
        assert_eq!(p.section(), SectionPos::new(-1, -5, -2));
        assert_eq!(p.local(), LocalPos { x: 15, y: 15, z: 15 });
    }

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn aabb_intersection() {
        let a = Aabb::block(BlockPos::new(0, 0, 0));
        let b = Aabb::new(DVec3::new(0.5, 0.5, 0.5), DVec3::new(1.5, 1.5, 1.5));
        assert!(a.intersects(&b));
        let c = Aabb::new(DVec3::new(1.0, 0.0, 0.0), DVec3::new(2.0, 1.0, 1.0));
        assert!(!a.intersects(&c));
    }
}
