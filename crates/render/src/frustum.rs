//! View-frustum extraction and conservative AABB tests.
//!
//! Planes are stored as `[a, b, c, d]` with the inside convention
//! `a·x + b·y + c·z + d >= 0`; all six are normalised before use. The
//! extraction follows Gribb–Hartmann for wgpu's `[0, 1]` depth range: near is
//! `row2` and far is `row3 - row2`, unlike the OpenGL `[-1, 1]` case.

use ob_math::{Mat4, SECTION_SIZE, SectionPos};

/// Number of planes in a view frustum.
pub const FRUSTUM_PLANE_COUNT: usize = 6;

/// Extracts the six normalised world-space frustum planes from a
/// view-projection matrix.
///
/// The order is left, right, bottom, top, near, far.
pub fn planes_from_view_projection(view_projection: &Mat4) -> [[f32; 4]; FRUSTUM_PLANE_COUNT] {
    // `to_cols_array_2d` returns columns, so row `i` gathers component `i` of
    // each column. A clip-space point is `M * p` with `p` a column vector.
    let columns = view_projection.to_cols_array_2d();
    let row = |index: usize| {
        [
            columns[0][index],
            columns[1][index],
            columns[2][index],
            columns[3][index],
        ]
    };
    let row0 = row(0);
    let row1 = row(1);
    let row2 = row(2);
    let row3 = row(3);
    let add = |a: [f32; 4], b: [f32; 4]| [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
    let sub = |a: [f32; 4], b: [f32; 4]| [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]];

    [
        normalize_plane(add(row3, row0)), // left:   x + w >= 0
        normalize_plane(sub(row3, row0)), // right:  w - x >= 0
        normalize_plane(add(row3, row1)), // bottom: y + w >= 0
        normalize_plane(sub(row3, row1)), // top:    w - y >= 0
        normalize_plane(row2),            // near:   z >= 0
        normalize_plane(sub(row3, row2)), // far:    w - z >= 0
    ]
}

/// Divides a plane by the length of its normal, leaving `d` scaled too.
fn normalize_plane(plane: [f32; 4]) -> [f32; 4] {
    let length = (plane[0] * plane[0] + plane[1] * plane[1] + plane[2] * plane[2]).sqrt();
    if length > f32::EPSILON {
        [
            plane[0] / length,
            plane[1] / length,
            plane[2] / length,
            plane[3] / length,
        ]
    } else {
        plane
    }
}

/// Whether an AABB lies entirely on the negative side of one plane.
///
/// The "positive vertex" (the corner furthest along the plane normal) is the
/// last corner that can still be inside; if even that corner is behind the
/// plane, the whole box is outside.
pub fn aabb_outside_plane(plane: [f32; 4], min: [f32; 3], max: [f32; 3]) -> bool {
    let x = if plane[0] >= 0.0 { max[0] } else { min[0] };
    let y = if plane[1] >= 0.0 { max[1] } else { min[1] };
    let z = if plane[2] >= 0.0 { max[2] } else { min[2] };
    plane[0] * x + plane[1] * y + plane[2] * z + plane[3] < 0.0
}

/// Conservative AABB-versus-frustum visibility test.
///
/// Returns `true` when the box may intersect the frustum; a box touching a
/// plane counts as visible.
pub fn aabb_visible(frustum: &[[f32; 4]; FRUSTUM_PLANE_COUNT], min: [f32; 3], max: [f32; 3]) -> bool {
    frustum.iter().all(|plane| !aabb_outside_plane(*plane, min, max))
}

/// The world-space bounds of a 16³ section.
pub fn section_bounds(pos: SectionPos) -> ([f32; 3], [f32; 3]) {
    let origin = pos.origin();
    let min = [origin.x as f32, origin.y as f32, origin.z as f32];
    let edge = SECTION_SIZE as f32;
    (min, [min[0] + edge, min[1] + edge, min[2] + edge])
}

/// Whether a section's 16³ AABB may intersect the frustum.
pub fn section_visible(frustum: &[[f32; 4]; FRUSTUM_PLANE_COUNT], pos: SectionPos) -> bool {
    let (min, max) = section_bounds(pos);
    aabb_visible(frustum, min, max)
}

#[cfg(test)]
mod tests {
    use ob_math::WorldVec;

    use crate::Camera;

    use super::*;

    fn test_camera() -> Camera {
        let mut camera = Camera::new(WorldVec::new(8.0, 8.0, 8.0), 0.0, 0.0);
        camera.far = 128.0;
        camera
    }

    #[test]
    fn extracted_planes_are_normalised() {
        let frustum = test_camera().frustum_planes(16.0 / 9.0);
        for plane in frustum {
            let length = (plane[0] * plane[0] + plane[1] * plane[1] + plane[2] * plane[2]).sqrt();
            assert!((length - 1.0).abs() < 1e-5, "plane {plane:?} has length {length}");
        }
    }

    #[test]
    fn section_in_front_of_camera_is_visible() {
        let frustum = test_camera().frustum_planes(16.0 / 9.0);
        assert!(section_visible(&frustum, SectionPos::new(0, 0, -1)));
        assert!(section_visible(&frustum, SectionPos::new(0, 0, -2)));
        // The section containing the eye still intersects the frustum.
        assert!(section_visible(&frustum, SectionPos::new(0, 0, 0)));
        // A section straddling the near plane is visible conservatively.
        assert!(section_visible(&frustum, SectionPos::new(0, 0, -1)));
    }

    #[test]
    fn section_behind_camera_is_culled() {
        let frustum = test_camera().frustum_planes(16.0 / 9.0);
        assert!(!section_visible(&frustum, SectionPos::new(0, 0, 1)));
        assert!(!section_visible(&frustum, SectionPos::new(0, 0, 2)));
        assert!(!section_visible(&frustum, SectionPos::new(-1, 0, 3)));
    }

    #[test]
    fn section_outside_the_side_planes_is_culled() {
        let frustum = test_camera().frustum_planes(16.0 / 9.0);
        assert!(!section_visible(&frustum, SectionPos::new(64, 0, -2)));
        assert!(!section_visible(&frustum, SectionPos::new(-64, 0, -2)));
        assert!(!section_visible(&frustum, SectionPos::new(0, 64, -2)));
        assert!(!section_visible(&frustum, SectionPos::new(0, -64, -2)));
    }

    #[test]
    fn section_beyond_the_far_plane_is_culled() {
        let frustum = test_camera().frustum_planes(16.0 / 9.0);
        // Camera z = 8 and far = 128, so world z < -120 is beyond the plane.
        assert!(!section_visible(&frustum, SectionPos::new(0, 0, -9)));
        assert!(section_visible(&frustum, SectionPos::new(0, 0, -6)));
    }

    #[test]
    fn aabb_plane_helper_accepts_touching_boxes() {
        // Plane z >= 0.
        let plane = [0.0, 0.0, 1.0, 0.0];
        assert!(!aabb_outside_plane(plane, [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]));
        assert!(!aabb_outside_plane(plane, [0.0, 0.0, 1.0], [1.0, 1.0, 2.0]));
        assert!(aabb_outside_plane(plane, [0.0, 0.0, -2.0], [1.0, 1.0, -1.0]));
    }

    #[test]
    fn aabb_plane_helper_handles_negative_normals() {
        // Plane -x >= 0, i.e. x <= 0.
        let plane = [-1.0, 0.0, 0.0, 0.0];
        assert!(!aabb_outside_plane(plane, [-2.0, 0.0, 0.0], [-1.0, 1.0, 1.0]));
        assert!(aabb_outside_plane(plane, [1.0, 0.0, 0.0], [2.0, 1.0, 1.0]));
    }

    #[test]
    fn aabb_frustum_helper_requires_every_plane_to_pass() {
        let frustum = [[0.0, 0.0, 1.0, 0.0]; FRUSTUM_PLANE_COUNT];
        assert!(aabb_visible(&frustum, [0.0, 0.0, 1.0], [1.0, 1.0, 2.0]));
        assert!(!aabb_visible(&frustum, [0.0, 0.0, -2.0], [1.0, 1.0, -1.0]));
    }
}
