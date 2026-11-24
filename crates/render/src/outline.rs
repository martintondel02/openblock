//! A wireframe outline around a block or a block-shaped box.
//!
//! [`BlockOutline`] is CPU data only: the windowed and offscreen renderers turn
//! it into 24 line-list vertices (12 cube edges, two vertices each) and draw
//! them after the world pass with depth testing but no depth writes. The box is
//! inflated outwards by [`DEFAULT_INFLATION`] so the lines sit just in front of
//! the block faces instead of z-fighting with them.
//!
//! The outline colour is authored in sRGB like [`crate::UiFrame`] colours; the
//! renderer converts it to linear RGB before uploading to the sRGB render
//! target.

use bytemuck::{Pod, Zeroable};

use crate::environment::srgb_to_linear_rgba;

/// Number of vertices in one outline: twelve edges, two vertices each.
pub const OUTLINE_VERTEX_COUNT: usize = 24;

/// Default outward inflation in metres: just enough to avoid z-fighting with
/// the block face, small enough to look like a 1-pixel overlay.
pub const DEFAULT_INFLATION: f32 = 0.003;

/// Default edge colour: near-white reads clearly on both grass and stone, and
/// stays visible after the day/night cycle has dimmed the world.
pub const DEFAULT_EDGE_COLOR: [f32; 4] = [0.97, 0.98, 1.0, 1.0];

/// One line-list vertex: world-space position and linear RGB colour.
///
/// The layout is 28 bytes: `position` 12, `color` 16. It matches the outline
/// vertex buffer layout in `pipeline.rs` and `shader.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct OutlineVertex {
    /// World-space position.
    pub position: [f32; 3],
    /// Linear RGB colour plus straight alpha.
    pub color: [f32; 4],
}

// Keep the vertex layout and the pipeline's attribute offsets in sync.
const _: () = assert!(std::mem::size_of::<OutlineVertex>() == 28);
const _: () = assert!(std::mem::align_of::<OutlineVertex>() == 4);

/// A wireframe box drawn around a block selection.
///
/// The public bounds are in world space, with `min` inclusive and `max`
/// exclusive in the same sense as block bounds. [`BlockOutline::new`] builds
/// them from an inclusive integer block range and inflates them outwards;
/// [`BlockOutline::from_bounds`] accepts bounds a caller has already prepared.
///
/// The colour is authored in sRGB. Alpha is straight and the outline pipeline
/// uses alpha blending, so `[0.0, 0.0, 0.0, 0.0]` draws nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockOutline {
    /// Minimum world-space corner.
    pub min: [f32; 3],
    /// Maximum world-space corner.
    pub max: [f32; 3],
    /// sRGB colour and straight alpha.
    pub color: [f32; 4],
}

impl BlockOutline {
    /// Outlines an inclusive integer block range with the default inflation.
    ///
    /// `min` and `max` are block coordinates; a single block passes the same
    /// value twice. The resulting box covers `min ..= max` (i.e. each axis spans
    /// `max - min + 1` blocks) and is inflated by [`DEFAULT_INFLATION`].
    pub fn new(min: [i32; 3], max: [i32; 3], color: [f32; 4]) -> Self {
        Self::with_inflation(min, max, color, DEFAULT_INFLATION)
    }

    /// Outlines an inclusive integer block range with an explicit inflation.
    ///
    /// A non-finite inflation is treated as zero. The default edge colour is
    /// [`DEFAULT_EDGE_COLOR`]; pass a custom `color` to override it.
    pub fn with_inflation(min: [i32; 3], max: [i32; 3], color: [f32; 4], inflation: f32) -> Self {
        let inflation = if inflation.is_finite() { inflation } else { 0.0 };
        // `max` is inclusive, so the geometric upper bound is `max + 1`.
        let upper = [
            max[0].saturating_add(1),
            max[1].saturating_add(1),
            max[2].saturating_add(1),
        ];
        Self {
            min: [
                min[0] as f32 - inflation,
                min[1] as f32 - inflation,
                min[2] as f32 - inflation,
            ],
            max: [
                upper[0] as f32 + inflation,
                upper[1] as f32 + inflation,
                upper[2] as f32 + inflation,
            ],
            color,
        }
    }

    /// Outlines a single block with the default inflation and edge colour.
    pub fn for_block(pos: [i32; 3]) -> Self {
        Self::new(pos, pos, DEFAULT_EDGE_COLOR)
    }

    /// Builds an outline from world-space bounds without additional inflation.
    ///
    /// Use this when the caller already owns inflated bounds (for example a
    /// selection box larger than one block).
    pub const fn from_bounds(min: [f32; 3], max: [f32; 3], color: [f32; 4]) -> Self {
        Self { min, max, color }
    }

    /// Whether every coordinate and colour channel is finite.
    ///
    /// The renderers skip a non-finite outline rather than uploading NaNs.
    pub fn is_finite(&self) -> bool {
        self.min
            .iter()
            .chain(self.max.iter())
            .chain(self.color.iter())
            .all(|value| value.is_finite())
    }

    /// Generates the 24 line-list vertices for this box.
    ///
    /// The order is deterministic: four bottom edges, four top edges and four
    /// vertical edges. Colours are converted from sRGB to linear RGB, which is
    /// what the sRGB render target expects.
    pub fn vertices(&self) -> [OutlineVertex; OUTLINE_VERTEX_COUNT] {
        let [x0, y0, z0] = self.min;
        let [x1, y1, z1] = self.max;
        // Bottom face then top face, each in counter-clockwise order when seen
        // from above; the vertical edges join corresponding corners.
        let corners = [
            [x0, y0, z0],
            [x1, y0, z0],
            [x1, y0, z1],
            [x0, y0, z1],
            [x0, y1, z0],
            [x1, y1, z0],
            [x1, y1, z1],
            [x0, y1, z1],
        ];
        let edges = [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 0),
            (4, 5),
            (5, 6),
            (6, 7),
            (7, 4),
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7),
        ];
        let color = srgb_to_linear_rgba(self.color);
        let mut vertices = [OutlineVertex {
            position: [0.0; 3],
            color,
        }; OUTLINE_VERTEX_COUNT];
        for (index, (start, end)) in edges.iter().copied().enumerate() {
            vertices[index * 2] = OutlineVertex {
                position: corners[start],
                color,
            };
            vertices[index * 2 + 1] = OutlineVertex {
                position: corners[end],
                color,
            };
        }
        vertices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_vertex_layout_matches_the_pipeline() {
        assert_eq!(std::mem::size_of::<OutlineVertex>(), 28);
        assert_eq!(std::mem::align_of::<OutlineVertex>(), 4);
        assert_eq!(std::mem::offset_of!(OutlineVertex, position), 0);
        assert_eq!(std::mem::offset_of!(OutlineVertex, color), 12);
    }

    #[test]
    fn a_single_block_becomes_an_inflated_unit_cube() {
        let outline = BlockOutline::new([2, 3, 4], [2, 3, 4], DEFAULT_EDGE_COLOR);
        assert_eq!(outline.min, [2.0 - 0.003, 3.0 - 0.003, 4.0 - 0.003]);
        assert_eq!(outline.max, [3.0 + 0.003, 4.0 + 0.003, 5.0 + 0.003]);
        let vertices = outline.vertices();
        assert_eq!(vertices.len(), OUTLINE_VERTEX_COUNT);
        // Every vertex sits on a corner of the box.
        for vertex in &vertices {
            for axis in 0..3 {
                let value = vertex.position[axis];
                assert!(
                    (value - outline.min[axis]).abs() < 1e-6 || (value - outline.max[axis]).abs() < 1e-6,
                    "vertex {vertex:?} is not on the box"
                );
            }
            assert_eq!(vertex.color, srgb_to_linear_rgba(DEFAULT_EDGE_COLOR));
        }
        // Twelve edges, two vertices each, every one axis-aligned.
        for edge in 0..12 {
            let a = vertices[edge * 2].position;
            let b = vertices[edge * 2 + 1].position;
            let differing = (0..3).filter(|axis| (a[*axis] - b[*axis]).abs() > 1e-6).count();
            assert_eq!(differing, 1, "edge {edge} is not axis-aligned: {a:?} -> {b:?}");
        }
    }

    #[test]
    fn explicit_inflation_and_bounds_are_honoured() {
        let flat = BlockOutline::with_inflation([0, 0, 0], [1, 2, 3], [1.0, 1.0, 1.0, 1.0], 0.0);
        assert_eq!(flat.min, [0.0, 0.0, 0.0]);
        assert_eq!(flat.max, [2.0, 3.0, 4.0]);

        let custom = BlockOutline::from_bounds([-0.5; 3], [0.5; 3], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(custom.min, [-0.5; 3]);
        assert_eq!(custom.max, [0.5; 3]);
        assert_eq!(custom.vertices()[0].position, [-0.5, -0.5, -0.5]);
        assert!(custom.is_finite());

        let bad = BlockOutline::from_bounds([f32::NAN; 3], [1.0; 3], [1.0; 4]);
        assert!(!bad.is_finite());
        let sanitised = BlockOutline::with_inflation([0; 3], [0; 3], [1.0; 4], f32::NAN);
        assert!(sanitised.is_finite());
        assert_eq!(sanitised.min, [0.0; 3]);
    }

    #[test]
    fn outline_colour_is_converted_from_srgb() {
        // The default near-white is converted like any other authored sRGB
        // colour; a pure white override round-trips exactly.
        let white = BlockOutline::new([0; 3], [0; 3], [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(white.vertices()[0].color, [1.0, 1.0, 1.0, 1.0]);
        let default_edge = BlockOutline::for_block([0; 3]);
        assert_eq!(
            default_edge.vertices()[0].color,
            srgb_to_linear_rgba(DEFAULT_EDGE_COLOR)
        );
        assert_eq!(default_edge.color, DEFAULT_EDGE_COLOR);

        let grey = BlockOutline::new([0; 3], [0; 3], [0.5, 0.5, 0.5, 0.5]);
        let color = grey.vertices()[0].color;
        assert!((color[0] - 0.2140).abs() < 1e-3, "linear grey is {color:?}");
        assert_eq!(color[3], 0.5);
    }

    #[test]
    fn edge_colour_alpha_is_preserved_and_order_is_stable() {
        let outline = BlockOutline::new([0, 0, 0], [0, 0, 0], [1.0, 0.0, 0.0, 0.25]);
        let first = outline.vertices();
        let second = outline.vertices();
        assert_eq!(first, second);
        assert_eq!(first[0].color[3], 0.25);
        // The first edge is the bottom-front edge along +X.
        assert_eq!(first[0].position, outline.min);
        assert_eq!(first[1].position, [outline.max[0], outline.min[1], outline.min[2]]);
    }
}
