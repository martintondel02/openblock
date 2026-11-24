//! CPU voxel meshing: visible faces, per-face shading and vertex ambient occlusion.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles

use ob_content::{BlockId, BlockType};
use ob_math::{BlockPos, SECTION_SIZE, SectionPos};
use ob_world::World;

use crate::{MeshData, Vertex};

/// Number of blocks along one section axis, as an integer.
const SECTION_EDGE: i32 = SECTION_SIZE as i32;

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
    /// Corner of the quad relative to the block position (components 0 or 1).
    base: [i32; 3],
    /// Directional shading multiplier.
    brightness: f32,
}

/// Faces in +X, -X, +Y, -Y, +Z, -Z order.
const FACES: [Face; 6] = [
    Face {
        normal: [1, 0, 0],
        u: [0, 0, -1],
        v: [0, 1, 0],
        base: [1, 0, 1],
        brightness: 0.72,
    },
    Face {
        normal: [-1, 0, 0],
        u: [0, 0, 1],
        v: [0, 1, 0],
        base: [0, 0, 0],
        brightness: 0.72,
    },
    Face {
        normal: [0, 1, 0],
        u: [1, 0, 0],
        v: [0, 0, -1],
        base: [0, 1, 1],
        brightness: 1.0,
    },
    Face {
        normal: [0, -1, 0],
        u: [1, 0, 0],
        v: [0, 0, 1],
        base: [0, 0, 0],
        brightness: 0.45,
    },
    Face {
        normal: [0, 0, 1],
        u: [1, 0, 0],
        v: [0, 1, 0],
        base: [0, 0, 1],
        brightness: 0.85,
    },
    Face {
        normal: [0, 0, -1],
        u: [-1, 0, 0],
        v: [0, 1, 0],
        base: [1, 0, 0],
        brightness: 0.85,
    },
];

/// Meshing statistics for one section, used by tests and profiling.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MesherStats {
    /// Visible faces emitted.
    pub faces: u32,
    /// Vertices emitted.
    pub vertices: u32,
}

/// Builds the mesh for one section, reading neighbouring sections through the world.
///
/// The v0.0.1 mesher emits one quad per visible face with baked directional
/// shading and per-corner ambient occlusion. Greedy meshing arrives at MS-04.
pub fn mesh_section(world: &World, pos: SectionPos) -> MeshData {
    mesh_section_with_stats(world, pos).0
}

/// Returns the mesh and its statistics in one pass.
pub fn mesh_section_with_stats(world: &World, pos: SectionPos) -> (MeshData, MesherStats) {
    let mut mesh = MeshData::default();
    let origin = pos.origin();
    for y in 0..SECTION_EDGE {
        for z in 0..SECTION_EDGE {
            for x in 0..SECTION_EDGE {
                let block_pos = BlockPos::new(origin.x + x, origin.y + y, origin.z + z);
                let block_id = world.get_block(block_pos);
                if block_id == BlockId::AIR {
                    continue;
                }
                let block = world.block_type(block_id);
                for face in &FACES {
                    let neighbour = world.block_type(world.get_block(block_pos.offset(
                        face.normal[0],
                        face.normal[1],
                        face.normal[2],
                    )));
                    if face_visible(block, neighbour) {
                        emit_face(&mut mesh.vertices, world, block_pos, block, face);
                    }
                }
            }
        }
    }
    let vertices = mesh.vertices.len() as u32;
    let stats = MesherStats {
        faces: vertices / 6,
        vertices,
    };
    (mesh, stats)
}

/// Whether `neighbour` leaves the face of `current` visible.
///
/// Opaque blocks hide every face. Fluids hide their shared face against the
/// same fluid, while a fluid/air boundary (for example a water surface) stays
/// visible. Air, leaves and other non-opaque blocks never occlude.
fn face_visible(current: &BlockType, neighbour: &BlockType) -> bool {
    if neighbour.opaque {
        return false;
    }
    !(current.fluid && neighbour.fluid && current.id == neighbour.id)
}

/// Emits the six vertices of one visible face with baked shading and AO.
fn emit_face(out: &mut Vec<Vertex>, world: &World, block_pos: BlockPos, block: &BlockType, face: &Face) {
    // Quad corner parameter pairs: base, +u, +u+v, +v.
    const CORNERS: [(i32, i32); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];

    let mut positions = [[0.0f32; 3]; 4];
    let mut colors = [[0.0f32; 4]; 4];
    for (index, (su, sv)) in CORNERS.into_iter().enumerate() {
        let x = block_pos.x + face.base[0] + su * face.u[0] + sv * face.v[0];
        let y = block_pos.y + face.base[1] + su * face.u[1] + sv * face.v[1];
        let z = block_pos.z + face.base[2] + su * face.u[2] + sv * face.v[2];
        positions[index] = [x as f32, y as f32, z as f32];

        // Sample the three blocks sharing this corner in the layer in front of
        // the face: two sides and the diagonal.
        let side_u = if su == 0 { -1 } else { 1 };
        let side_v = if sv == 0 { -1 } else { 1 };
        let layer = face.normal;
        let mut occluders = 0u8;
        if world.is_opaque(block_pos.offset(
            layer[0] + side_u * face.u[0],
            layer[1] + side_u * face.u[1],
            layer[2] + side_u * face.u[2],
        )) {
            occluders += 1;
        }
        if world.is_opaque(block_pos.offset(
            layer[0] + side_v * face.v[0],
            layer[1] + side_v * face.v[1],
            layer[2] + side_v * face.v[2],
        )) {
            occluders += 1;
        }
        if world.is_opaque(block_pos.offset(
            layer[0] + side_u * face.u[0] + side_v * face.v[0],
            layer[1] + side_u * face.u[1] + side_v * face.v[1],
            layer[2] + side_u * face.u[2] + side_v * face.v[2],
        )) {
            occluders += 1;
        }
        let ao = AO_FACTORS[occluders as usize];
        let shade = face.brightness * ao;
        colors[index] = [
            block.color[0] * shade,
            block.color[1] * shade,
            block.color[2] * shade,
            1.0,
        ];
    }
    push_quad_colors(out, positions, colors);
}

/// Pushes a flat-shaded quad as two triangles (kept for future unshaded passes).
#[allow(dead_code)] // reserved for passes that do not bake per-corner AO
pub(crate) fn push_quad(out: &mut Vec<Vertex>, corners: [[f32; 3]; 4], color: [f32; 4]) {
    push_quad_colors(out, corners, [color; 4]);
}

/// Pushes a quad with one baked colour per corner as two triangles.
pub(crate) fn push_quad_colors(out: &mut Vec<Vertex>, corners: [[f32; 3]; 4], colors: [[f32; 4]; 4]) {
    for index in TRIANGLE_INDICES {
        out.push(Vertex {
            position: corners[index],
            color: colors[index],
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ob_content::{BlockId, Registry};
    use ob_math::{BlockPos, ChunkPos};
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

    fn assert_color_near(actual: [f32; 4], expected: [f32; 3]) {
        assert!(
            (actual[0] - expected[0]).abs() < 1e-5
                && (actual[1] - expected[1]).abs() < 1e-5
                && (actual[2] - expected[2]).abs() < 1e-5
                && (actual[3] - 1.0).abs() < 1e-5,
            "colour {actual:?} does not match {expected:?}",
        );
    }

    #[test]
    fn single_stone_block_emits_six_faces() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(0, 0, 0), stone));

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 6);
        assert_eq!(stats.vertices, 36);
        assert_eq!(mesh.vertices.len(), 36);
        assert_eq!(mesh.triangle_count(), 12);
        assert_eq!(mesh_section(&world, section()).vertices.len(), mesh.vertices.len());
    }

    #[test]
    fn two_adjacent_stone_blocks_emit_ten_faces() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let stone = block(&world, "openblock:stone");
        assert!(world.set_block(BlockPos::new(0, 0, 0), stone));
        assert!(world.set_block(BlockPos::new(1, 0, 0), stone));

        let (_, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 10);
        assert_eq!(stats.vertices, 60);
    }

    #[test]
    fn fully_enclosed_solid_section_emits_no_faces() {
        // A section-level equivalent of "a stone block fully surrounded by
        // stone": every block of the section is stone and every one of the six
        // neighbouring section layers is stone too, so not a single face is
        // exposed.
        let mut world = World::new(registry(), 7);
        for (x, z) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            world.insert_chunk(ChunkColumn::new(ChunkPos::new(x, z)));
        }
        let stone = block(&world, "openblock:stone");

        for y in 0..SECTION_EDGE {
            for z in 0..SECTION_EDGE {
                for x in 0..SECTION_EDGE {
                    assert!(world.set_block(BlockPos::new(x, y, z), stone));
                }
            }
        }
        for a in 0..SECTION_EDGE {
            for b in 0..SECTION_EDGE {
                for (x, y, z) in [
                    (SECTION_EDGE, a, b),
                    (-1, a, b),
                    (a, SECTION_EDGE, b),
                    (a, -1, b),
                    (a, b, SECTION_EDGE),
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
    fn water_surface_renders_its_top_face() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let water = block(&world, "openblock:water");
        assert!(world.set_block(BlockPos::new(0, 0, 0), water));

        let (mesh, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 6);
        assert_eq!(stats.vertices, 36);

        let base_color = world.registry().block(water).color;
        let top_vertices: Vec<&Vertex> = mesh
            .vertices
            .iter()
            .filter(|vertex| {
                (vertex.position[1] - 1.0).abs() < 1e-5
                    && (vertex.color[0] - base_color[0]).abs() < 1e-5
                    && (vertex.color[1] - base_color[1]).abs() < 1e-5
                    && (vertex.color[2] - base_color[2]).abs() < 1e-5
            })
            .collect();
        assert_eq!(
            top_vertices.len(),
            6,
            "the top water face should use the unshaded base colour",
        );
        for vertex in top_vertices {
            assert_color_near(vertex.color, base_color);
        }
    }

    #[test]
    fn water_against_water_hides_the_shared_face() {
        let mut world = world_with_chunk(ChunkPos::new(0, 0));
        let water = block(&world, "openblock:water");
        assert!(world.set_block(BlockPos::new(0, 0, 0), water));
        assert!(world.set_block(BlockPos::new(1, 0, 0), water));

        let (_, stats) = mesh_section_with_stats(&world, section());
        assert_eq!(stats.faces, 10);
    }

    #[test]
    fn meshing_is_deterministic() {
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
        assert!(!first_mesh.vertices.is_empty());
        for (a, b) in first_mesh.vertices.iter().zip(&second_mesh.vertices) {
            assert_eq!(a.position, b.position);
            assert_eq!(a.color, b.color);
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
        let (faces, _) = mesh.vertices.as_chunks::<6>();
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
}
