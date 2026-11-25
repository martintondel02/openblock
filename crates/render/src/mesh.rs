//! CPU voxel meshing: greedy per-face rectangles with baked ambient occlusion.
//!
//! For each of the six face directions the mesher walks the sixteen slices
//! along that face's normal. A slice builds a 16×16 visibility mask; every
//! visible cell carries a merge key of `(texture layer, face brightness, four
//! corner AO factors)`. Equal keys are expanded into maximal rectangles
//! (column first, then row) and each rectangle becomes one quad whose UVs
//! cover the rectangle in block units, so a 16×16 texture tiles correctly
//! without an atlas.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles

use ob_content::{BlockId, BlockType, Registry};
use ob_math::{BlockPos, SECTION_SIZE, SectionPos};
use ob_world::World;

use crate::{MeshData, Vertex};

/// Number of blocks along one section axis.
const SECTION_EDGE: usize = SECTION_SIZE;

/// Edge length of an immutable section snapshot: the 16³ section plus one
/// block of border on every side.
pub const SNAPSHOT_SIZE: usize = 18;

// The literal above is part of the snapshot contract; keep it tied to the
// section size if the 16³ storage unit ever changes.
const _: () = assert!(SNAPSHOT_SIZE == SECTION_EDGE + 2);

/// Offset from a section-local coordinate to an index inside a snapshot.
const SNAPSHOT_BORDER: i32 = 1;

/// Highest section-local coordinate stored in a snapshot (the +1 border).
const SNAPSHOT_MAX_LOCAL: i32 = SECTION_EDGE as i32;

/// An immutable copy of a section plus its one-block border.
///
/// Worker threads can mesh a snapshot without touching the live [`World`],
/// which keeps meshing independent of edits and streaming on the main thread.
/// The box is indexed `(y * 18 + z) * 18 + x` with a `+1` border offset on every
/// axis, so section-local coordinates `-1..=16` are addressable.
#[derive(Clone, Debug)]
pub struct SectionSnapshot {
    /// Position of the captured section.
    pub pos: SectionPos,
    /// `SNAPSHOT_SIZE³` block ids in `(y, z, x)` order.
    blocks: Box<[BlockId]>,
}

impl SectionSnapshot {
    /// Captures a section and its border from the world.
    ///
    /// Positions outside a loaded column or outside the world height read as
    /// air, matching [`World::get_block`].
    pub fn capture(world: &World, pos: SectionPos) -> Self {
        let mut blocks = vec![BlockId::AIR; SNAPSHOT_SIZE * SNAPSHOT_SIZE * SNAPSHOT_SIZE].into_boxed_slice();
        let origin = pos.origin();
        let mut index = 0;
        for y in -SNAPSHOT_BORDER..=SNAPSHOT_MAX_LOCAL {
            for z in -SNAPSHOT_BORDER..=SNAPSHOT_MAX_LOCAL {
                for x in -SNAPSHOT_BORDER..=SNAPSHOT_MAX_LOCAL {
                    blocks[index] = world.get_block(origin.offset(x, y, z));
                    index += 1;
                }
            }
        }
        Self { pos, blocks }
    }

    /// Reads a block at section-local coordinates in `-1..=16`.
    ///
    /// Coordinates outside the stored border read as air.
    pub fn block(&self, x: i32, y: i32, z: i32) -> BlockId {
        let inside = |value: i32| (-SNAPSHOT_BORDER..=SNAPSHOT_MAX_LOCAL).contains(&value);
        if !inside(x) || !inside(y) || !inside(z) {
            return BlockId::AIR;
        }
        let x = (x + SNAPSHOT_BORDER) as usize;
        let y = (y + SNAPSHOT_BORDER) as usize;
        let z = (z + SNAPSHOT_BORDER) as usize;
        self.blocks[(y * SNAPSHOT_SIZE + z) * SNAPSHOT_SIZE + x]
    }

    /// Builds the greedy mesh for this snapshot using registry block data.
    ///
    /// Opaque and fluid faces are emitted into separate vertex lists:
    /// [`MeshData::vertices`] and [`MeshData::water_vertices`]. The split lets
    /// the renderer draw water in a translucent pass after the opaque world
    /// without changing greedy merging, AO or baked light for either list.
    pub fn mesh(&self, registry: &Registry) -> MeshData {
        let mut mesh = MeshData::default();
        for face in FACES {
            mesh_face(&mut mesh, self, registry, face);
        }
        mesh
    }

    /// Whether the block at section-local coordinates is opaque.
    fn is_opaque(&self, registry: &Registry, x: i32, y: i32, z: i32) -> bool {
        registry.block(self.block(x, y, z)).opaque
    }
}

/// Ambient-occlusion brightness indexed by the number of occluding neighbours
/// around a vertex corner (0..=3).
const AO_FACTORS: [f32; 4] = [1.0, 0.78, 0.60, 0.42];

/// Vertex order producing the two counter-clockwise triangles of a quad.
const TRIANGLE_INDICES: [usize; 6] = [0, 1, 2, 0, 2, 3];

/// Description of one cube face.
///
/// `u` and `v` are the in-plane axes with `u × v == normal`, so the quad
/// corners `base`, `base + u`, `base + u + v`, `base + v` are counter-clockwise
/// when viewed from outside the block.
#[derive(Debug, Clone, Copy)]
struct Face {
    /// Outward integer normal.
    normal: [i32; 3],
    /// First in-plane axis.
    u: [i32; 3],
    /// Second in-plane axis.
    v: [i32; 3],
    /// Directional shading multiplier.
    brightness: f32,
    /// Face selector for [`ob_content::BlockTextures::for_face`]: 0 = +Y,
    /// 1 = -Y, 2..=5 = sides.
    texture_face: u8,
}

/// Faces in +X, -X, +Y, -Y, +Z, -Z order.
const FACES: [Face; 6] = [
    Face {
        normal: [1, 0, 0],
        u: [0, 0, -1],
        v: [0, 1, 0],
        brightness: 0.72,
        texture_face: 2,
    },
    Face {
        normal: [-1, 0, 0],
        u: [0, 0, 1],
        v: [0, 1, 0],
        brightness: 0.72,
        texture_face: 3,
    },
    Face {
        normal: [0, 1, 0],
        u: [1, 0, 0],
        v: [0, 0, -1],
        brightness: 1.0,
        texture_face: 0,
    },
    Face {
        normal: [0, -1, 0],
        u: [1, 0, 0],
        v: [0, 0, 1],
        brightness: 0.45,
        texture_face: 1,
    },
    Face {
        normal: [0, 0, 1],
        u: [1, 0, 0],
        v: [0, 1, 0],
        brightness: 0.85,
        texture_face: 4,
    },
    Face {
        normal: [0, 0, -1],
        u: [-1, 0, 0],
        v: [0, 1, 0],
        brightness: 0.85,
        texture_face: 5,
    },
];

/// Face axes resolved to indices and signs, plus the plane offset.
#[derive(Debug, Clone, Copy)]
struct FaceAxes {
    normal_axis: usize,
    u_axis: usize,
    v_axis: usize,
    u_sign: i32,
    v_sign: i32,
    plane_offset: f32,
    texture_face: u8,
}

impl Face {
    /// Resolves the face's integer vectors once per direction.
    fn axes(self) -> FaceAxes {
        let normal_axis = axis_index(self.normal);
        let u_axis = axis_index(self.u);
        let v_axis = axis_index(self.v);
        FaceAxes {
            normal_axis,
            u_axis,
            v_axis,
            u_sign: self.u[u_axis],
            v_sign: self.v[v_axis],
            plane_offset: if self.normal[normal_axis] > 0 { 1.0 } else { 0.0 },
            texture_face: self.texture_face,
        }
    }
}

/// Merge key shared by equal cells of one slice.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MergeKey {
    layer: u32,
    brightness: f32,
    ao: [f32; 4],
    /// Fluid faces are merged among themselves and emitted into the water list.
    water: bool,
}

/// One merged rectangle in slice coordinates.
#[derive(Debug, Clone, Copy)]
struct Rect {
    u: usize,
    v: usize,
    width: usize,
    height: usize,
}

/// Meshing statistics for one section, used by tests and profiling.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MesherStats {
    /// Visible opaque faces emitted after greedy merging (quads, not triangles).
    pub faces: u32,
    /// Opaque vertices emitted (six per quad).
    pub vertices: u32,
    /// Visible fluid/water faces emitted after greedy merging.
    pub water_faces: u32,
    /// Water vertices emitted (six per quad).
    pub water_vertices: u32,
}

/// Builds the mesh for one section, reading neighbouring sections through the world.
pub fn mesh_section(world: &World, pos: SectionPos) -> MeshData {
    mesh_section_with_stats(world, pos).0
}

/// Returns the greedy mesh and its statistics in one pass.
pub fn mesh_section_with_stats(world: &World, pos: SectionPos) -> (MeshData, MesherStats) {
    let snapshot = SectionSnapshot::capture(world, pos);
    let mesh = snapshot.mesh(world.registry());
    let vertices = mesh.vertices.len() as u32;
    let water_vertices = mesh.water_vertices.len() as u32;
    let stats = MesherStats {
        faces: vertices / 6,
        vertices,
        water_faces: water_vertices / 6,
        water_vertices,
    };
    (mesh, stats)
}

/// Meshes every slice of one face direction into merged quads.
fn mesh_face(out: &mut MeshData, snapshot: &SectionSnapshot, registry: &Registry, face: Face) {
    let axes = face.axes();
    for slice in 0..SECTION_EDGE {
        let mut mask: [[Option<MergeKey>; SECTION_EDGE]; SECTION_EDGE] = [[None; SECTION_EDGE]; SECTION_EDGE];
        // `mask` is indexed `[u][v]`; iterate rows and cells so the loop
        // variables are not also used as slice indices.
        for (u, row) in mask.iter_mut().enumerate() {
            for (v, cell) in row.iter_mut().enumerate() {
                let mut local = [0i32; 3];
                local[axes.normal_axis] = slice as i32;
                local[axes.u_axis] = u as i32;
                local[axes.v_axis] = v as i32;
                let block_id = snapshot.block(local[0], local[1], local[2]);
                if block_id == BlockId::AIR {
                    continue;
                }
                let block = registry.block(block_id);
                let neighbour = snapshot.block(
                    local[0] + face.normal[0],
                    local[1] + face.normal[1],
                    local[2] + face.normal[2],
                );
                if !face_visible(block, registry.block(neighbour)) {
                    continue;
                }
                let ao = face_ambient_occlusion(snapshot, registry, local, &face);
                *cell = Some(MergeKey {
                    layer: u32::from(block.textures.for_face(axes.texture_face).0),
                    brightness: face.brightness,
                    ao,
                    water: block.fluid,
                });
            }
        }
        emit_greedy(out, snapshot.pos.origin(), &axes, slice, &mask);
    }
}

/// Expands equal merge keys into maximal rectangles and emits one quad each.
fn emit_greedy(
    out: &mut MeshData,
    origin: BlockPos,
    axes: &FaceAxes,
    slice: usize,
    mask: &[[Option<MergeKey>; SECTION_EDGE]; SECTION_EDGE],
) {
    let mut consumed = [[false; SECTION_EDGE]; SECTION_EDGE];
    for v in 0..SECTION_EDGE {
        for u in 0..SECTION_EDGE {
            if consumed[u][v] {
                continue;
            }
            let Some(key) = mask[u][v] else {
                continue;
            };

            // Column first: extend along U while keys match.
            let mut width = 1;
            while u + width < SECTION_EDGE && !consumed[u + width][v] && mask[u + width][v] == Some(key) {
                width += 1;
            }

            // Then row: extend along V while the whole run still matches.
            let mut height = 1;
            'expand_v: while v + height < SECTION_EDGE {
                for offset in 0..width {
                    if consumed[u + offset][v + height] || mask[u + offset][v + height] != Some(key) {
                        break 'expand_v;
                    }
                }
                height += 1;
            }

            for row in 0..height {
                for column in 0..width {
                    consumed[u + column][v + row] = true;
                }
            }

            emit_quad(out, origin, axes, slice, Rect { u, v, width, height }, key);
        }
    }
}

/// Subtle per-quad brightness variation in `0.94..=1.04`.
///
/// Large merged surfaces (grass fields, stone floors, tree canopies) would
/// otherwise read as one flat colour. Hashing the quad's world anchor and
/// texture layer keeps the variation deterministic and, because it is applied
/// after greedy merging, it does not reduce merging or affect the merge key.
fn quad_tint(anchor: [f32; 3], layer: u32) -> f32 {
    let x = anchor[0].round() as i64 as u64;
    let y = anchor[1].round() as i64 as u64;
    let z = anchor[2].round() as i64 as u64;
    let mut seed = x
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(y.rotate_left(21))
        .wrapping_add(z.rotate_left(42))
        .wrapping_add(u64::from(layer) << 17);
    seed = ob_math::mix64(seed);
    0.94 + 0.10 * ((seed >> 40) as f32 / (1u32 << 24) as f32)
}

/// Emits the two CCW triangles of one merged rectangle.
fn emit_quad(out: &mut MeshData, origin: BlockPos, axes: &FaceAxes, slice: usize, rect: Rect, key: MergeKey) {
    // The rectangle covers the half-open block interval `rect.u..rect.u + width`
    // (and likewise in V). Walk the corners in the actual U/V direction so the
    // winding stays counter-clockwise from outside regardless of axis signs.
    let u0 = if axes.u_sign > 0 {
        rect.u as f32
    } else {
        (rect.u + rect.width) as f32
    };
    let u1 = if axes.u_sign > 0 {
        (rect.u + rect.width) as f32
    } else {
        rect.u as f32
    };
    let v0 = if axes.v_sign > 0 {
        rect.v as f32
    } else {
        (rect.v + rect.height) as f32
    };
    let v1 = if axes.v_sign > 0 {
        (rect.v + rect.height) as f32
    } else {
        rect.v as f32
    };

    let plane = slice as f32 + axes.plane_offset;
    let corner = |along_u: f32, along_v: f32| {
        let mut position = [0.0f32; 3];
        position[axes.normal_axis] = plane;
        position[axes.u_axis] = along_u;
        position[axes.v_axis] = along_v;
        [
            origin.x as f32 + position[0],
            origin.y as f32 + position[1],
            origin.z as f32 + position[2],
        ]
    };
    let positions = [corner(u0, v0), corner(u1, v0), corner(u1, v1), corner(u0, v1)];
    let width = rect.width as f32;
    let height = rect.height as f32;
    // Texture row 0 is the top of the image, while the first quad corner sits
    // at the start of the V axis. Flip V so an upright side texture (the grass
    // band on `grass_side`, bark stripes on `log_side`) is not upside down.
    let uvs = [[0.0, height], [width, height], [width, 0.0], [0.0, 0.0]];

    let tint = quad_tint(positions[0], key.layer);
    // Water and other fluids go to their own list so the opaque pass never
    // draws a translucent surface. The merge key already separates the two
    // lists, so a rectangle can never span both.
    let vertices = if key.water {
        &mut out.water_vertices
    } else {
        &mut out.vertices
    };
    for index in TRIANGLE_INDICES {
        vertices.push(Vertex {
            position: positions[index],
            uv: uvs[index],
            layer: key.layer,
            light: key.brightness * key.ao[index] * tint,
        });
    }
}

/// Whether `neighbour` leaves the face of `current` visible.
///
/// Opaque blocks hide every face; a non-opaque block hides its shared face
/// against an identical block (water against water, leaves against leaves).
/// A fluid/air or fluid/opaque boundary follows the normal rules, so the water
/// surface stays visible.
fn face_visible(current: &BlockType, neighbour: &BlockType) -> bool {
    if neighbour.opaque {
        return false;
    }
    current.id != neighbour.id
}

/// Ambient-occlusion factor for each of a face's four corners.
///
/// Corner order is `(u0,v0), (u1,v0), (u1,v1), (u0,v1)`. The three blocks
/// sharing a corner in the layer in front of the face (two sides and the
/// diagonal) are counted with [`SectionSnapshot::is_opaque`].
fn face_ambient_occlusion(snapshot: &SectionSnapshot, registry: &Registry, local: [i32; 3], face: &Face) -> [f32; 4] {
    const CORNERS: [(i32, i32); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];
    CORNERS.map(|(su, sv)| {
        let side_u = if su == 0 { -1 } else { 1 };
        let side_v = if sv == 0 { -1 } else { 1 };
        let layer = face.normal;
        let mut occluders = 0u8;
        if snapshot.is_opaque(
            registry,
            local[0] + layer[0] + side_u * face.u[0],
            local[1] + layer[1] + side_u * face.u[1],
            local[2] + layer[2] + side_u * face.u[2],
        ) {
            occluders += 1;
        }
        if snapshot.is_opaque(
            registry,
            local[0] + layer[0] + side_v * face.v[0],
            local[1] + layer[1] + side_v * face.v[1],
            local[2] + layer[2] + side_v * face.v[2],
        ) {
            occluders += 1;
        }
        if snapshot.is_opaque(
            registry,
            local[0] + layer[0] + side_u * face.u[0] + side_v * face.v[0],
            local[1] + layer[1] + side_u * face.u[1] + side_v * face.v[1],
            local[2] + layer[2] + side_u * face.u[2] + side_v * face.v[2],
        ) {
            occluders += 1;
        }
        AO_FACTORS[occluders as usize]
    })
}

/// Index of the non-zero component of an axis-aligned integer vector.
fn axis_index(vector: [i32; 3]) -> usize {
    if vector[0] != 0 {
        0
    } else if vector[1] != 0 {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ob_content::{BlockId, Registry, textures};
    use ob_math::{BlockPos, ChunkPos, WORLD_MAX_Y, WORLD_MIN_Y};
    use ob_world::ChunkColumn;

    use super::*;

    /// Registry shared by all test worlds.
    fn registry() -> Arc<Registry> {
        Arc::new(Registry::base())
    }

    /// Block id of a base block by name.
    fn block(world: &World, name: &str) -> BlockId {
        world.registry().id_or_air(name)
    }

    /// One world with a single loaded chunk column.
    fn world_with_chunk(chunk: ChunkPos) -> World {
        let mut world = World::new(registry(), 0);
        world.insert_chunk(ChunkColumn::new(chunk));
        world
    }

    /// Section used by the single-section tests.
    fn section() -> SectionPos {
        SectionPos::new(0, 0, 0)
    }

    /// Vertices of every quad lying in the given horizontal plane.
    fn quads_at_y(mesh: &MeshData, y: f32) -> Vec<&[Vertex]> {
        mesh.vertices
            .as_chunks::<6>()
            .0
            .iter()
            .filter(|quad| quad.iter().all(|vertex| (vertex.position[1] - y).abs() < 1e-5))
            .map(|quad| &quad[..])
            .collect()
    }

    /// Texture layer of the single quad lying on an axis-aligned cube plane.
    fn layer_on_plane(mesh: &MeshData, axis: usize, value: f32) -> u32 {
        let quads: Vec<&[Vertex]> = mesh
            .vertices
            .as_chunks::<6>()
            .0
            .iter()
            .filter(|quad| quad.iter().all(|vertex| (vertex.position[axis] - value).abs() < 1e-5))
            .map(|quad| &quad[..])
            .collect();
        assert_eq!(
            quads.len(),
            1,
            "expected exactly one quad on axis {axis} at {value}, found {}",
            quads.len()
        );
        let layer = quads[0][0].layer;
        assert!(
            quads[0].iter().all(|vertex| vertex.layer == layer),
            "one quad must use one texture layer"
        );
        layer
    }

    #[test]
    fn single_textured_block_emits_six_quads() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(0, 0, 0), stone));

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 6);
        assert_eq!(stats.vertices, 36);
        assert_eq!(mesh.vertices.len(), 36);
        assert_eq!(mesh.triangle_count(), 12);
        assert_eq!(mesh_section(&world, section()).vertices.len(), mesh.vertices.len());

        // FACES order is +X, -X, +Y, -Y, +Z, -Z; all AO factors are 1.0 for an
        // isolated block, so each quad's light is the face brightness times the
        // deterministic per-quad tint.
        let expected_lights = [0.72, 0.72, 1.0, 0.45, 0.85, 0.85];
        for (quad, expected) in mesh.vertices.as_chunks::<6>().0.iter().zip(expected_lights) {
            let tint = quad_tint(quad[0].position, u32::from(textures::STONE.0));
            assert!((0.94..=1.04).contains(&tint), "tint {tint} out of range");
            for vertex in quad {
                assert_eq!(vertex.layer, u32::from(textures::STONE.0));
                assert!(
                    (vertex.light - expected * tint).abs() < 1e-5,
                    "face brightness {} should be {expected} x {tint}",
                    vertex.light,
                );
            }
        }
    }

    #[test]
    fn fronted_blocks_use_the_front_layer_only_on_the_positive_z_face() {
        let cases = [
            ("openblock:furnace", Some(textures::FURNACE_FRONT)),
            ("openblock:chest", Some(textures::CHEST_FRONT)),
            ("openblock:crafting_table", None),
        ];
        for (name, expected_front) in cases {
            let mut world = world_with_chunk(ChunkPos::new(0, 0));
            let id = block(&world, name);
            assert!(world.set_block(BlockPos::new(0, 0, 0), id));
            let faces = world.registry().block(id).textures;
            let (mesh, stats) = mesh_section_with_stats(&world, section());
            assert_eq!(stats.faces, 6, "{name} must emit six faces");

            // +Z is the interim front face and selects `front` (or `side` when
            // the block declares no front texture).
            assert_eq!(
                layer_on_plane(&mesh, 2, 1.0),
                u32::from(faces.front_texture().0),
                "{name} +Z must use the front layer"
            );
            if let Some(front) = expected_front {
                assert_eq!(layer_on_plane(&mesh, 2, 1.0), u32::from(front.0));
            }
            // The other three side faces keep the ordinary side layer.
            for (axis, value) in [(0, 1.0), (0, 0.0), (2, 0.0)] {
                assert_eq!(
                    layer_on_plane(&mesh, axis, value),
                    u32::from(faces.side.0),
                    "{name} side face on axis {axis} at {value} must use the side layer"
                );
            }
            // Top and bottom keep their dedicated layers.
            assert_eq!(layer_on_plane(&mesh, 1, 1.0), u32::from(faces.top.0));
            assert_eq!(layer_on_plane(&mesh, 1, 0.0), u32::from(faces.bottom.0));
        }
    }

    #[test]
    fn two_adjacent_stone_blocks_cover_ten_unit_faces_in_six_quads() {
        // The old per-face mesher emitted one quad per exposed face (10). A
        // correct greedy mesher merges the coplanar runs of two touching
        // identical blocks, covering the same 10 unit faces with 6 quads:
        // merged top, bottom, front and back plus the two end caps.
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(0, 0, 0), stone));
        assert!(world.set_block(BlockPos::new(1, 0, 0), stone));

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 6);
        assert_eq!(stats.vertices, 36);
        assert_eq!(mesh.triangle_count(), 12);

        // Sum the merged rectangles' UV extents: that is the number of unit
        // block faces the six quads cover.
        let unit_faces: f32 = mesh
            .vertices
            .as_chunks::<6>()
            .0
            .iter()
            .map(|quad| {
                let min_u = quad.iter().map(|vertex| vertex.uv[0]).fold(f32::MAX, f32::min);
                let max_u = quad.iter().map(|vertex| vertex.uv[0]).fold(f32::MIN, f32::max);
                let min_v = quad.iter().map(|vertex| vertex.uv[1]).fold(f32::MAX, f32::min);
                let max_v = quad.iter().map(|vertex| vertex.uv[1]).fold(f32::MIN, f32::max);
                (max_u - min_u) * (max_v - min_v)
            })
            .sum();
        assert!((unit_faces - 10.0).abs() < 1e-5, "covered {unit_faces} unit faces");

        let top = quads_at_y(&mesh, 1.0);
        assert_eq!(top.len(), 1, "two coplanar top faces should merge");
        let uvs: Vec<[f32; 2]> = top[0].iter().map(|vertex| vertex.uv).collect();
        assert_eq!(uvs.iter().map(|uv| uv[0]).fold(f32::MIN, f32::max), 2.0);
        assert_eq!(uvs.iter().map(|uv| uv[1]).fold(f32::MIN, f32::max), 1.0);
    }

    #[test]
    fn fully_enclosed_solid_section_emits_no_quads() {
        // A solid 16³ section with an opaque block layer against every one of
        // its six boundaries: every candidate face has an opaque neighbour.
        let mut world = World::new(registry(), 7);
        for (x, z) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            world.insert_chunk(ChunkColumn::new(ChunkPos::new(x, z)));
        }
        let stone = block(&world, "openblock:stone");

        for y in 0..SECTION_EDGE as i32 {
            for z in 0..SECTION_EDGE as i32 {
                for x in 0..SECTION_EDGE as i32 {
                    assert!(world.set_block(BlockPos::new(x, y, z), stone));
                }
            }
        }
        for a in 0..SECTION_EDGE as i32 {
            for b in 0..SECTION_EDGE as i32 {
                for (x, y, z) in [
                    (SECTION_EDGE as i32, a, b),
                    (-1, a, b),
                    (a, SECTION_EDGE as i32, b),
                    (a, -1, b),
                    (a, b, SECTION_EDGE as i32),
                    (a, b, -1),
                ] {
                    assert!(world.set_block(BlockPos::new(x, y, z), stone));
                }
            }
        }

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 0);
        assert_eq!(stats.vertices, 0);
        assert!(mesh.vertices.is_empty());
    }

    #[test]
    fn flat_platform_top_merges_into_one_quad_with_full_uv_extent() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        for z in 0..4 {
            for x in 0..4 {
                assert!(world.set_block(BlockPos::new(x, 0, z), stone));
            }
        }

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        // Top, bottom and four sides: each side run is a 4×1 rectangle.
        assert_eq!(stats.faces, 6, "a 4×4 platform should greedy-merge to six quads");
        let top = quads_at_y(&mesh, 1.0);
        assert_eq!(top.len(), 1, "the 4×4 +Y face should be a single quad");
        let uvs: Vec<[f32; 2]> = top[0].iter().map(|vertex| vertex.uv).collect();
        let min_u = uvs.iter().map(|uv| uv[0]).fold(f32::MAX, f32::min);
        let max_u = uvs.iter().map(|uv| uv[0]).fold(f32::MIN, f32::max);
        let min_v = uvs.iter().map(|uv| uv[1]).fold(f32::MAX, f32::min);
        let max_v = uvs.iter().map(|uv| uv[1]).fold(f32::MIN, f32::max);
        assert_eq!((max_u - min_u, max_v - min_v), (4.0, 4.0));
        // A single merged quad shares one tint, so every top vertex has the
        // same +Y light value within the tint range.
        let first_light = top[0][0].light;
        for vertex in top[0] {
            assert_eq!(vertex.layer, u32::from(textures::STONE.0));
            assert!((vertex.light - first_light).abs() < 1e-6);
            assert!((0.94..=1.04).contains(&vertex.light));
        }
    }

    #[test]
    fn ambient_occlusion_is_deterministic() {
        fn patterned_world() -> World {
            let mut world = world_with_chunk(ChunkPos::new(0, 0));
            let stone = block(&world, "openblock:stone");
            let dirt = block(&world, "openblock:dirt");
            let water = block(&world, "openblock:water");
            let leaves = block(&world, "openblock:leaves");
            for y in 0..6 {
                for z in 0..8 {
                    for x in 0..8 {
                        let id = match (x + 2 * y + 3 * z) % 5 {
                            0 => stone,
                            1 => dirt,
                            2 => water,
                            3 => leaves,
                            _ => BlockId::AIR,
                        };
                        assert!(world.set_block(BlockPos::new(x, y, z), id));
                    }
                }
            }
            world
        }

        let first = patterned_world();
        let second = patterned_world();
        let (first_mesh, first_stats) = mesh_section_with_stats(&first, section());
        let (second_mesh, second_stats) = mesh_section_with_stats(&second, section());

        assert_eq!(first_stats, second_stats);
        assert_eq!(first_mesh.vertices.len(), second_mesh.vertices.len());
        assert_eq!(first_mesh.water_vertices.len(), second_mesh.water_vertices.len());
        assert!(!first_mesh.vertices.is_empty());
        assert!(!first_mesh.water_vertices.is_empty());
        for (a, b) in first_mesh
            .vertices
            .iter()
            .chain(&first_mesh.water_vertices)
            .zip(second_mesh.vertices.iter().chain(&second_mesh.water_vertices))
        {
            assert_eq!(a.position, b.position);
            assert_eq!(a.uv, b.uv);
            assert_eq!(a.layer, b.layer);
            assert_eq!(a.light.to_bits(), b.light.to_bits());
        }
    }

    #[test]
    fn occluded_corner_uses_the_three_occluder_ao_factor() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(1, 0, 1), stone));
        // +Y corner (u0,v0) of the top face samples (-1, +1, 0), (0, +1, +1)
        // and the diagonal (-1, +1, +1) relative to the block.
        for pos in [BlockPos::new(0, 1, 1), BlockPos::new(1, 1, 2), BlockPos::new(0, 1, 2)] {
            assert!(world.set_block(pos, stone));
        }

        let (mesh, _) = mesh_section_with_stats(&world, section());
        let lights: Vec<f32> = mesh
            .vertices
            .iter()
            .filter(|vertex| (vertex.position[1] - 1.0).abs() < 1e-5)
            .map(|vertex| vertex.light)
            .collect();
        assert!(
            lights.iter().any(|light| (0.94..=1.04).contains(&(light / 0.42))),
            "expected a fully occluded 0.42 corner (times tint), got {lights:?}",
        );
    }

    #[test]
    fn water_surface_merges_and_same_water_faces_are_hidden() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let water = block(&world, "openblock:water");
        for z in 0..SECTION_EDGE as i32 {
            for x in 0..SECTION_EDGE as i32 {
                assert!(world.set_block(BlockPos::new(x, 0, z), water));
            }
        }

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        // One merged top, one merged bottom and four merged side strips; the
        // faces between adjacent water blocks are hidden. Water is split out
        // of the opaque list, so the counts live in the water fields.
        assert_eq!(stats.faces, 0);
        assert_eq!(stats.vertices, 0);
        assert_eq!(stats.water_faces, 6);
        assert_eq!(stats.water_vertices, 36);
        assert!(mesh.vertices.is_empty());
        let top: Vec<&[Vertex]> = mesh
            .water_vertices
            .as_chunks::<6>()
            .0
            .iter()
            .filter(|quad| quad.iter().all(|vertex| (vertex.position[1] - 1.0).abs() < 1e-5))
            .map(|quad| &quad[..])
            .collect();
        assert_eq!(top.len(), 1, "the water surface should be a single merged quad");
        let first_light = top[0][0].light;
        for vertex in top[0] {
            assert_eq!(vertex.layer, u32::from(textures::WATER.0));
            assert!((vertex.light - first_light).abs() < 1e-6);
            assert!((0.94..=1.04).contains(&vertex.light));
        }
    }

    #[test]
    fn quads_wind_counter_clockwise_from_outside() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(0, 0, 0), stone));

        let (mesh, _) = mesh_section_with_stats(&world, section());
        assert_eq!(mesh.vertices.len(), 36);
        for quad in mesh.vertices.as_chunks::<6>().0 {
            // A single-block face is constant along exactly one axis, and that
            // constant is either 0 or 1; that axis is the outward normal.
            let mut expected = [0.0f32; 3];
            let mut constant_axes = 0;
            for axis in 0..3 {
                let value = quad[0].position[axis];
                if quad.iter().all(|vertex| (vertex.position[axis] - value).abs() < 1e-5) {
                    constant_axes += 1;
                    assert!(value == 0.0 || value == 1.0, "face is not on a cube plane");
                    expected[axis] = if value == 1.0 { 1.0 } else { -1.0 };
                }
            }
            assert_eq!(constant_axes, 1, "a cube face must be constant on one axis");

            let edge1 = [
                quad[1].position[0] - quad[0].position[0],
                quad[1].position[1] - quad[0].position[1],
                quad[1].position[2] - quad[0].position[2],
            ];
            let edge2 = [
                quad[2].position[0] - quad[0].position[0],
                quad[2].position[1] - quad[0].position[1],
                quad[2].position[2] - quad[0].position[2],
            ];
            let normal = [
                edge1[1] * edge2[2] - edge1[2] * edge2[1],
                edge1[2] * edge2[0] - edge1[0] * edge2[2],
                edge1[0] * edge2[1] - edge1[1] * edge2[0],
            ];
            let dot = normal[0] * expected[0] + normal[1] * expected[1] + normal[2] * expected[2];
            assert!(dot > 0.0, "quad winds clockwise from outside: normal={normal:?}");
        }
    }

    #[test]
    fn empty_section_emits_no_geometry() {
        let world = world_with_chunk(ChunkPos::new(0, 0));
        let (mesh, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats, MesherStats::default());
        assert!(mesh.is_empty());

        // A section whose chunk is not loaded also meshes to nothing.
        let missing = mesh_section(&world, SectionPos::new(4, 0, 4));
        assert!(missing.is_empty());
    }

    #[test]
    fn textured_grass_block_uses_top_and_side_layers() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let grass = block(&world, "openblock:grass");
        assert!(world.set_block(BlockPos::new(0, 0, 0), grass));

        let (mesh, _) = mesh_section_with_stats(&world, section());
        let top = quads_at_y(&mesh, 1.0);
        assert_eq!(top.len(), 1);
        for vertex in top[0] {
            assert_eq!(vertex.layer, u32::from(textures::GRASS_TOP.0));
        }

        // The +X face lies in the x = 1 plane.
        let side = mesh
            .vertices
            .as_chunks::<6>()
            .0
            .iter()
            .map(|quad| &quad[..])
            .find(|quad| quad.iter().all(|vertex| (vertex.position[0] - 1.0).abs() < 1e-5));
        let side = side.expect("the block should have a +X side quad");
        for vertex in side {
            assert_eq!(vertex.layer, u32::from(textures::GRASS_SIDE.0));
            assert!((0.94..=1.04).contains(&(vertex.light / 0.72)));
        }
        // Texture row 0 is the top of the image, so the top edge of a side face
        // must sample V = 0 and the bottom edge V = 1.
        for vertex in side {
            let expected_v = if vertex.position[1] > 0.5 { 0.0 } else { 1.0 };
            assert!((vertex.uv[1] - expected_v).abs() < 1e-5, "side texture is upside down");
        }

        // The bottom face uses the distinct dirt texture.
        let bottom = quads_at_y(&mesh, 0.0)
            .into_iter()
            .find(|quad| {
                quad.iter()
                    .all(|vertex| vertex.position[0] >= 0.0 && vertex.position[0] <= 1.0)
            })
            .expect("the block should have a bottom quad");
        for vertex in bottom {
            assert_eq!(vertex.layer, u32::from(textures::DIRT.0));
        }
    }

    #[test]
    fn section_border_culls_against_neighbour_chunk() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        world.insert_chunk(ChunkColumn::new(ChunkPos::new(1, 0)));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(15, 0, 0), stone));
        assert!(world.set_block(BlockPos::new(16, 0, 0), stone));

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 5);
        assert_eq!(stats.vertices, 30);
        let faces = mesh.vertices.as_chunks::<6>().0;
        let face_at_far_x_plane = faces
            .iter()
            .any(|quad| quad.iter().all(|vertex| (vertex.position[0] - 16.0).abs() < 1e-5));
        assert!(
            !face_at_far_x_plane,
            "face against the neighbouring chunk should have been culled",
        );

        // The neighbour section culls its own -X face the same way.
        let (_, neighbour_stats) = mesh_section_with_stats(&world, SectionPos::new(1, 0, 0));
        assert_eq!(neighbour_stats.faces, 5);
    }

    #[test]
    fn snapshot_mesh_matches_world_mesh_vertex_for_vertex() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        let dirt = block(&world, "openblock:dirt");
        let water = block(&world, "openblock:water");
        let leaves = block(&world, "openblock:leaves");
        for y in 0..6 {
            for z in 0..8 {
                for x in 0..8 {
                    let id = match (x + 2 * y + 3 * z) % 5 {
                        0 => stone,
                        1 => dirt,
                        2 => water,
                        3 => leaves,
                        _ => BlockId::AIR,
                    };
                    assert!(world.set_block(BlockPos::new(x, y, z), id));
                }
            }
        }

        let registry = registry();
        let snapshot = SectionSnapshot::capture(&world, section());
        let from_snapshot = snapshot.mesh(&registry);
        let from_world = mesh_section(&world, section());
        assert!(!from_snapshot.vertices.is_empty());
        assert_eq!(
            from_snapshot.vertices, from_world.vertices,
            "snapshot meshing must reproduce the world mesher exactly",
        );
        assert_eq!(
            from_snapshot.water_vertices, from_world.water_vertices,
            "snapshot water meshing must reproduce the world mesher exactly",
        );
    }

    #[test]
    fn snapshot_captures_chunk_border_and_culls_the_shared_face() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        world.insert_chunk(ChunkColumn::new(ChunkPos::new(1, 0)));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(15, 0, 0), stone));
        assert!(world.set_block(BlockPos::new(16, 0, 0), stone));

        let snapshot = SectionSnapshot::capture(&world, section());
        assert_eq!(snapshot.block(15, 0, 0), stone);
        assert_eq!(
            snapshot.block(16, 0, 0),
            stone,
            "the +1 border must include the next chunk"
        );
        assert_eq!(snapshot.block(-1, 0, 0), BlockId::AIR);

        let mesh = snapshot.mesh(&registry());
        assert_eq!(
            SectionSnapshot::capture(&world, section())
                .mesh(&registry())
                .vertices
                .len(),
            30
        );
        assert_eq!(mesh.vertices.len(), 30);
        assert!(
            !mesh
                .vertices
                .as_chunks::<6>()
                .0
                .iter()
                .any(|quad| quad.iter().all(|vertex| (vertex.position[0] - 16.0).abs() < 1e-5)),
            "the face shared with the next chunk must be culled",
        );
    }

    #[test]
    fn snapshot_captures_vertical_section_border_and_culls_the_shared_face() {
        // Both blocks live in the same chunk column: this is a section border
        // that is not a chunk border.
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(0, 15, 0), stone));
        assert!(world.set_block(BlockPos::new(0, 16, 0), stone));

        let lower = SectionSnapshot::capture(&world, section());
        assert_eq!(lower.block(0, 15, 0), stone);
        assert_eq!(
            lower.block(0, 16, 0),
            stone,
            "the +Y border must reach the next section"
        );
        let lower_mesh = lower.mesh(&registry());
        assert_eq!(lower_mesh.vertices.len(), 30, "the shared top face must be culled");
        assert!(
            !lower_mesh
                .vertices
                .as_chunks::<6>()
                .0
                .iter()
                .any(|quad| quad.iter().all(|vertex| (vertex.position[1] - 16.0).abs() < 1e-5)),
        );

        let upper = SectionSnapshot::capture(&world, SectionPos::new(0, 1, 0));
        assert_eq!(
            upper.block(0, -1, 0),
            stone,
            "the -Y border must reach the previous section"
        );
        assert_eq!(upper.mesh(&registry()).vertices.len(), 30);
    }

    #[test]
    fn snapshot_reads_out_of_world_y_as_air() {
        let mut world = World::new(registry(), 0);
        world.insert_chunk(ChunkColumn::new(ChunkPos::new(0, 0)));
        let stone = block(&world, "openblock:stone");

        let bottom_section_y = WORLD_MIN_Y / SECTION_SIZE as i32;
        assert!(world.set_block(BlockPos::new(0, WORLD_MIN_Y, 0), stone));
        let bottom = SectionSnapshot::capture(&world, SectionPos::new(0, bottom_section_y, 0));
        assert_eq!(bottom.block(0, 0, 0), stone);
        assert_eq!(bottom.block(0, -1, 0), BlockId::AIR);

        let top_section_y = WORLD_MAX_Y / SECTION_SIZE as i32 - 1;
        assert!(world.set_block(BlockPos::new(1, WORLD_MAX_Y - 1, 0), stone));
        let top = SectionSnapshot::capture(&world, SectionPos::new(0, top_section_y, 0));
        assert_eq!(top.block(1, SECTION_SIZE as i32 - 1, 0), stone);
        assert_eq!(top.block(1, SNAPSHOT_MAX_LOCAL, 0), BlockId::AIR);
        assert_eq!(top.block(1, SNAPSHOT_MAX_LOCAL + 1, 0), BlockId::AIR);
    }

    #[test]
    fn empty_snapshot_produces_an_empty_mesh() {
        let world = world_with_chunk(ChunkPos::new(0, 0));
        let snapshot = SectionSnapshot::capture(&world, section());
        assert_eq!(snapshot.block(0, 0, 0), BlockId::AIR);
        let mesh = snapshot.mesh(&registry());
        assert!(mesh.is_empty());
        assert_eq!(mesh.triangle_count(), 0);
    }

    #[test]
    fn quad_tint_is_deterministic_and_bounded() {
        for layer in 0..ob_content::textures::COUNT as u32 {
            for x in -8..8 {
                for z in -8..8 {
                    let anchor = [x as f32, 64.0, z as f32];
                    let tint = quad_tint(anchor, layer);
                    assert!((0.94..=1.04).contains(&tint), "tint {tint} out of range");
                    assert_eq!(tint.to_bits(), quad_tint(anchor, layer).to_bits());
                }
            }
        }
        // Different anchors should not all collapse to the same value.
        let values: std::collections::HashSet<u32> =
            (-8..8).map(|x| quad_tint([x as f32, 64.0, 0.0], 4).to_bits()).collect();
        assert!(values.len() > 8, "quad tint has too little variation");
    }
}
