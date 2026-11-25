//! Multiplayer name tags: camera projection, distance fade and overlap culling.
//!
//! The projection is a pure function over a [`Camera`] so the awkward cases
//! (behind the camera, outside the frustum, beyond the fade distance, two
//! players at the same screen position) are unit-testable without a window or a
//! GPU. The network runtime is the only caller; host/local mode never draws
//! name tags.

use ob_math::Vec4;
use ob_render::{Camera, UiFrame};

use crate::ui_state::UiRect;

/// Distance beyond which no name tag is drawn, in metres.
pub const NAME_TAG_MAX_DISTANCE: f32 = 48.0;
/// Distance at which the tag starts fading out, in metres.
pub const NAME_TAG_FADE_START: f32 = 32.0;
/// Height above a player's feet where the label is anchored, in metres.
pub const NAME_TAG_HEIGHT: f64 = 2.05;
/// Text scale of a name tag.
pub const NAME_TAG_SCALE: f32 = 1.15;
/// Horizontal padding inside a tag background, in pixels.
pub const NAME_TAG_PADDING_X: f32 = 7.0;
/// Vertical padding inside a tag background, in pixels.
pub const NAME_TAG_PADDING_Y: f32 = 3.0;

/// One remote player projected into screen space.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedNameTag {
    /// Connection id (stable tie-breaker and avatar-colour seed).
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Anchor x in screen pixels.
    pub x: f32,
    /// Anchor y in screen pixels (above the head).
    pub y: f32,
    /// Distance from the camera in metres.
    pub distance: f32,
    /// Fade amount in `0.0..=1.0`.
    pub alpha: f32,
}

/// Projects remote player positions into screen-space name tags.
///
/// `players` carries `(id, name, feet_position)` triples. Tags behind the
/// camera, outside the screen or beyond `max_distance` are dropped, and the
/// remainder is returned nearest-first so overlap culling keeps the closest
/// label.
pub fn project_name_tags(
    camera: &Camera,
    screen: [f32; 2],
    players: &[(u64, &str, [f64; 3])],
    max_distance: f32,
) -> Vec<ProjectedNameTag> {
    let max_distance = if max_distance.is_finite() && max_distance > 0.0 {
        max_distance
    } else {
        NAME_TAG_MAX_DISTANCE
    };
    let aspect = (screen[0] / screen[1]).max(0.01);
    let view_projection = camera.view_projection(aspect);
    let mut tags = Vec::new();
    for (id, name, feet) in players {
        if !feet.iter().all(|value| value.is_finite()) || name.is_empty() {
            continue;
        }
        let world = [feet[0], feet[1] + NAME_TAG_HEIGHT, feet[2]];
        let dx = world[0] - camera.position.x;
        let dy = world[1] - camera.position.y;
        let dz = world[2] - camera.position.z;
        let distance = (dx * dx + dy * dy + dz * dz).sqrt() as f32;
        if !distance.is_finite() || distance > max_distance {
            continue;
        }
        let clip = view_projection * Vec4::new(world[0] as f32, world[1] as f32, world[2] as f32, 1.0);
        // A non-positive w means the point is behind the camera plane.
        if !clip.w.is_finite() || clip.w <= 0.0001 {
            continue;
        }
        let inv_w = 1.0 / clip.w;
        let ndc_x = clip.x * inv_w;
        let ndc_y = clip.y * inv_w;
        let ndc_z = clip.z * inv_w;
        if !ndc_x.is_finite() || !ndc_y.is_finite() || !ndc_z.is_finite() {
            continue;
        }
        if ndc_z < -1.0 || ndc_z > 1.0 || ndc_x.abs() > 1.0 || ndc_y.abs() > 1.0 {
            continue;
        }
        let x = (ndc_x * 0.5 + 0.5) * screen[0];
        let y = (1.0 - (ndc_y * 0.5 + 0.5)) * screen[1];
        let alpha = if distance <= NAME_TAG_FADE_START {
            1.0
        } else {
            ((max_distance - distance) / (max_distance - NAME_TAG_FADE_START)).clamp(0.0, 1.0)
        };
        if alpha <= 0.01 {
            continue;
        }
        tags.push(ProjectedNameTag {
            id: *id,
            name: (*name).to_string(),
            x,
            y,
            distance,
            alpha,
        });
    }
    tags.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });
    tags
}

/// Picks non-overlapping tag rectangles, nearest first.
///
/// `measure` returns the text width in pixels; the drawing code passes the
/// actual bitmap-font measurement and tests pass a fixed estimate. The returned
/// rectangles are clamped inside the screen.
pub fn name_tag_rects(
    tags: &[ProjectedNameTag],
    measure: impl Fn(&str) -> f32,
    screen: [f32; 2],
) -> Vec<(usize, UiRect)> {
    let line = ob_render::line_height(NAME_TAG_SCALE);
    let mut placed: Vec<(usize, UiRect)> = Vec::new();
    for (index, tag) in tags.iter().enumerate() {
        let width = (measure(&tag.name) + NAME_TAG_PADDING_X * 2.0).max(18.0);
        let height = line + NAME_TAG_PADDING_Y * 2.0;
        let x = (tag.x - width / 2.0).clamp(2.0, (screen[0] - width - 2.0).max(2.0));
        let y = (tag.y - height).clamp(2.0, (screen[1] - height - 2.0).max(2.0));
        let rect = UiRect::new(x, y, width, height);
        if placed.iter().any(|(_, existing)| {
            existing.intersects(UiRect::new(rect.x - 2.0, rect.y - 2.0, rect.w + 4.0, rect.h + 4.0))
        }) {
            continue;
        }
        placed.push((index, rect));
    }
    placed
}

/// Draws the accepted name tags using the shared bitmap UI.
pub fn draw_name_tags(frame: &mut UiFrame, tags: &[ProjectedNameTag], screen: [f32; 2]) {
    if tags.is_empty() {
        return;
    }
    let measure = |text: &str| frame.text_width(NAME_TAG_SCALE, text);
    let rects = name_tag_rects(tags, measure, screen);
    for (index, rect) in rects {
        let tag = &tags[index];
        let alpha = tag.alpha.clamp(0.0, 1.0);
        frame.rect(
            rect.x + 1.0,
            rect.y + 1.0,
            rect.w,
            rect.h,
            [0.0, 0.0, 0.0, 0.38 * alpha],
        );
        frame.rect(rect.x, rect.y, rect.w, rect.h, [0.03, 0.04, 0.06, 0.72 * alpha]);
        frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, [0.55, 0.62, 0.72, 0.55 * alpha]);
        let text_y = rect.y + NAME_TAG_PADDING_Y;
        frame.text_centered(
            rect.center().0,
            text_y,
            NAME_TAG_SCALE,
            [0.97, 0.99, 1.0, alpha],
            &tag.name,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_math::WorldVec;

    fn camera() -> Camera {
        // Yaw zero faces -Z, matching the renderer and the network demo.
        let mut camera = Camera::new(WorldVec::new(0.5, 70.0, 0.5), 0.0, 0.0);
        camera.fov_y = 70f32.to_radians();
        camera.far = 256.0;
        camera
    }

    #[test]
    fn player_in_front_projects_near_the_screen_centre_above_the_crosshair() {
        let camera = camera();
        let tags = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[(7, "alice", [0.5, 70.0, -10.5])],
            NAME_TAG_MAX_DISTANCE,
        );
        assert_eq!(tags.len(), 1);
        let tag = &tags[0];
        assert_eq!(tag.id, 7);
        assert_eq!(tag.name, "alice");
        assert!((tag.x - 640.0).abs() < 1.0, "x {}", tag.x);
        assert!(tag.y < 360.0, "head anchor is above the crosshair: {}", tag.y);
        assert!(tag.y > 0.0);
        assert_eq!(tag.alpha, 1.0);
        assert!((10.0..12.0).contains(&tag.distance), "distance {}", tag.distance);
    }

    #[test]
    fn players_behind_the_camera_are_dropped() {
        let camera = camera();
        let tags = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[(1, "behind", [0.5, 70.0, 10.5])],
            NAME_TAG_MAX_DISTANCE,
        );
        assert!(tags.is_empty(), "a point behind the camera must not project");
    }

    #[test]
    fn offscreen_and_out_of_range_players_are_dropped() {
        let camera = camera();
        let tags = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[
                (1, "far-left", [-200.0, 70.0, -5.0]),
                (2, "too-far", [0.5, 70.0, -f64::from(NAME_TAG_MAX_DISTANCE) - 1.0]),
                (3, "good", [0.5, 70.0, -5.0]),
            ],
            NAME_TAG_MAX_DISTANCE,
        );
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "good");
    }

    #[test]
    fn distance_fade_falls_to_zero_at_the_cap() {
        let camera = camera();
        let near = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[(1, "near", [0.5, 70.0, -f64::from(NAME_TAG_FADE_START) + 1.0])],
            NAME_TAG_MAX_DISTANCE,
        );
        assert_eq!(near[0].alpha, 1.0);
        let middle = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[(
                1,
                "mid",
                [0.5, 70.0, -f64::from(NAME_TAG_FADE_START + NAME_TAG_MAX_DISTANCE) / 2.0],
            )],
            NAME_TAG_MAX_DISTANCE,
        );
        assert!(middle[0].alpha > 0.1 && middle[0].alpha < 0.9, "{}", middle[0].alpha);
        let edge = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[(1, "edge", [0.5, 70.0, -f64::from(NAME_TAG_MAX_DISTANCE)])],
            NAME_TAG_MAX_DISTANCE,
        );
        assert!(edge.is_empty(), "zero-alpha tags are culled");
    }

    #[test]
    fn tags_are_sorted_nearest_first() {
        let camera = camera();
        let tags = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[
                (1, "mid", [0.5, 70.0, -20.0]),
                (2, "near", [0.5, 70.0, -5.0]),
                (3, "far", [0.5, 70.0, -35.0]),
            ],
            NAME_TAG_MAX_DISTANCE,
        );
        let names: Vec<&str> = tags.iter().map(|tag| tag.name.as_str()).collect();
        assert_eq!(names, vec!["near", "mid", "far"]);
    }

    #[test]
    fn overlapping_tags_keep_only_the_nearest() {
        let tags = vec![
            ProjectedNameTag {
                id: 1,
                name: "alice".to_string(),
                x: 400.0,
                y: 300.0,
                distance: 4.0,
                alpha: 1.0,
            },
            ProjectedNameTag {
                id: 2,
                name: "bob".to_string(),
                x: 402.0,
                y: 302.0,
                distance: 8.0,
                alpha: 1.0,
            },
            ProjectedNameTag {
                id: 3,
                name: "carol".to_string(),
                x: 900.0,
                y: 300.0,
                distance: 12.0,
                alpha: 1.0,
            },
        ];
        let rects = name_tag_rects(&tags, |text| text.chars().count() as f32 * 9.2, [1280.0, 720.0]);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].0, 0);
        assert_eq!(rects[1].0, 2);
    }

    #[test]
    fn tag_rects_stay_inside_the_screen() {
        let tags = vec![ProjectedNameTag {
            id: 1,
            name: "a-very-long-player-name".to_string(),
            x: 1.0,
            y: 2.0,
            distance: 4.0,
            alpha: 1.0,
        }];
        for screen in [[1280.0, 720.0], [420.0, 300.0]] {
            let rects = name_tag_rects(&tags, |text| text.chars().count() as f32 * 9.2, screen);
            assert_eq!(rects.len(), 1);
            let rect = rects[0].1;
            assert!(rect.x >= 0.0 && rect.y >= 0.0);
            assert!(rect.x + rect.w <= screen[0]);
            assert!(rect.y + rect.h <= screen[1]);
        }
    }

    #[test]
    fn drawing_tags_appends_ui_geometry() {
        let camera = camera();
        let tags = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[(1, "alice", [0.5, 70.0, -8.0]), (2, "bob", [1.5, 70.0, -12.0])],
            NAME_TAG_MAX_DISTANCE,
        );
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_name_tags(&mut frame, &tags, [1280.0, 720.0]);
        let (_, quads) = frame.measure();
        assert!(quads >= tags.len() * 3, "background, text and outline per tag");
    }

    #[test]
    fn non_finite_and_empty_players_are_ignored() {
        let camera = camera();
        let tags = project_name_tags(
            &camera,
            [1280.0, 720.0],
            &[(1, "", [0.5, 70.0, -5.0]), (2, "nan", [f64::NAN, 70.0, -5.0])],
            NAME_TAG_MAX_DISTANCE,
        );
        assert!(tags.is_empty());
    }
}
