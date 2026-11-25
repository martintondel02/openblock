//! Immediate-mode UI geometry and the wgpu pipeline that draws it.
//!
//! [`UiFrame`] is a CPU-side display list. Coordinates are physical pixels
//! with the origin at the top-left (Y grows down); the frame converts them to
//! NDC when a quad is pushed. [`UiRenderer`] uploads the frame to one vertex
//! buffer and draws it with one pipeline, one render pass and four bind groups
//! (solid white, the Monocraft-style UI font atlas, the 8×8 bitmap title font
//! atlas and the block texture array).
//!
//! Vertex colours are authored in sRGB. When the target format is an sRGB
//! format (the window surface and the offscreen test target both are), the
//! renderer converts RGB to linear before upload so a requested colour reads
//! back as the same sRGB byte values. Alpha is straight, not premultiplied.
//!
//! Text is printable ASCII (`32..=126`) in an 8×8 cell grid. [`UiFrame::text`]
//! draws the Minecraft-style TrueType UI font ([`crate::ui_font`]) while
//! [`UiFrame::title_text`] draws the built-in bitmap font, so titles keep their
//! established look. Unknown characters are skipped. Both methods accept `\n`
//! (and ignore `\r`) and return the longest line's advance width;
//! [`UiFrame::text_width`] reports the same measurement without drawing. Glyph
//! advance and line height stay 8 px at scale 1 for both fonts.

use bytemuck::{Pod, Zeroable};
use font8x8::{BASIC_FONTS, UnicodeFonts};

use crate::textures::BlockTextureArray;
use crate::ui_font::{
    UI_ATLAS_COLUMNS, UI_ATLAS_HEIGHT, UI_ATLAS_WIDTH, UI_FIRST_PRINTABLE, UI_GLYPH_SIZE, UI_LAST_PRINTABLE,
    UI_LINE_HEIGHT, UiFontAtlas, UiFontError, default_ui_font, default_ui_font_name, rasterize_ui_font,
};

/// Width and height of one glyph cell at scale 1, in logical pixels.
pub const GLYPH_SIZE: f32 = UI_GLYPH_SIZE;

/// Height of one text line at scale 1, in logical pixels.
pub const LINE_HEIGHT: f32 = UI_LINE_HEIGHT;

/// Font atlas columns (16 cells per row).
const ATLAS_COLUMNS: u32 = UI_ATLAS_COLUMNS;
/// Font atlas width in texels.
const ATLAS_WIDTH: u32 = UI_ATLAS_WIDTH;
/// Font atlas height in texels.
const ATLAS_HEIGHT: u32 = UI_ATLAS_HEIGHT;
/// First supported printable ASCII code point.
const FIRST_PRINTABLE: u32 = UI_FIRST_PRINTABLE;
/// Last supported printable ASCII code point.
const LAST_PRINTABLE: u32 = UI_LAST_PRINTABLE;

/// Texture format shared by the UI's generated array textures.
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Largest magnitude accepted for a pixel coordinate or rectangle extent.
const MAX_COORD: f32 = 1_000_000.0;
/// Largest glyph scale accepted, avoiding unbounded vertex coordinates.
const MAX_SCALE: f32 = 1024.0;
/// Upper bound on vertices retained by one frame, keeping uploads bounded.
const MAX_VERTICES: usize = 1_000_000;
/// Initial GPU vertex-buffer capacity in bytes.
const INITIAL_VERTEX_BYTES: u64 = 64 * 1024;

/// Advance width of one glyph at `scale`.
///
/// This is always `8.0 * scale`; callers that need a safe value should ignore
/// non-positive or non-finite scales before calling.
pub fn glyph_advance(scale: f32) -> f32 {
    GLYPH_SIZE * scale
}

/// Height of one line at `scale`.
pub fn line_height(scale: f32) -> f32 {
    LINE_HEIGHT * scale
}

/// One UI vertex: NDC position, texture UV, array layer and sRGB colour.
///
/// The layout is 36 bytes: `position` 8, `uv` 8, `layer` 4, `color` 16. (The
/// four required fields cannot be packed into 32 bytes without dropping one of
/// them; the GPU stride is 36.)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct UiVertex {
    /// Position in normalised device coordinates.
    pub position: [f32; 2],
    /// Texture-space UV.
    pub uv: [f32; 2],
    /// Texture-array layer.
    pub layer: u32,
    /// Straight-alpha sRGB colour.
    pub color: [f32; 4],
}

// Keep the Rust layout and the pipeline's vertex attributes in sync.
const _: () = assert!(std::mem::size_of::<UiVertex>() == 36);
const _: () = assert!(std::mem::offset_of!(UiVertex, position) == 0);
const _: () = assert!(std::mem::offset_of!(UiVertex, uv) == 8);
const _: () = assert!(std::mem::offset_of!(UiVertex, layer) == 16);
const _: () = assert!(std::mem::offset_of!(UiVertex, color) == 20);

/// Texture source for a UI batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UiTexture {
    /// The 1×1 white solid layer, used by [`UiFrame::rect`].
    Solid,
    /// The Minecraft-style UI font atlas (embedded Monocraft or a user font),
    /// used by [`UiFrame::text`].
    UiFont,
    /// The generated 8×8 bitmap title font atlas, used by
    /// [`UiFrame::title_text`].
    Font,
    /// The block texture array supplied to [`UiRenderer::new`].
    Blocks,
}

impl UiTexture {
    /// Index into the renderer's four bind groups.
    const fn bind_group_index(self) -> usize {
        match self {
            Self::Solid => 0,
            Self::UiFont => 1,
            Self::Font => 2,
            Self::Blocks => 3,
        }
    }
}

/// A contiguous run of quads sharing one texture.
#[derive(Debug)]
pub(crate) struct UiBatch {
    texture: UiTexture,
    vertices: Vec<UiVertex>,
}

/// An immediate-mode UI frame in screen-pixel coordinates.
///
/// Quads are grouped into contiguous batches by texture so painter's-order
/// semantics are preserved. The frame owns no GPU resources; hand it to
/// [`UiRenderer::render`] after building it for the current target size.
#[derive(Debug)]
pub struct UiFrame {
    width: f32,
    height: f32,
    batches: Vec<UiBatch>,
    quad_count: usize,
}

impl UiFrame {
    /// Creates an empty frame for a target of `screen_width` × `screen_height`
    /// physical pixels.
    ///
    /// Non-finite or non-positive dimensions are coerced to 1, and huge
    /// dimensions are clamped, so drawing cannot produce NaN geometry.
    pub fn new(screen_width: f32, screen_height: f32) -> Self {
        Self {
            width: sanitize_screen(screen_width),
            height: sanitize_screen(screen_height),
            batches: Vec::new(),
            quad_count: 0,
        }
    }

    /// The frame's target size in physical pixels.
    pub fn screen_size(&self) -> (f32, f32) {
        (self.width, self.height)
    }

    /// Appends a filled rectangle.
    ///
    /// Non-finite coordinates, non-positive sizes and invalid colours are
    /// ignored or sanitised; this never panics.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        let Some((x, y, w, h)) = sanitize_rect(x, y, w, h) else {
            return;
        };
        let corners = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        self.push_quad(UiTexture::Solid, corners, uvs, 0, color);
    }

    /// Appends a rectangular outline made of four filled rectangles.
    ///
    /// The thickness is clamped to half of the smaller side; invalid or
    /// non-positive dimensions and thicknesses are ignored.
    pub fn outline(&mut self, x: f32, y: f32, w: f32, h: f32, thickness: f32, color: [f32; 4]) {
        let Some((x, y, w, h)) = sanitize_rect(x, y, w, h) else {
            return;
        };
        if !thickness.is_finite() || thickness <= 0.0 {
            return;
        }
        let thickness = thickness.min(MAX_COORD).min(w * 0.5).min(h * 0.5);
        if thickness <= 0.0 {
            return;
        }
        self.rect(x, y, w, thickness, color);
        self.rect(x, y + h - thickness, w, thickness, color);
        self.rect(x, y + thickness, thickness, h - thickness * 2.0, color);
        self.rect(x + w - thickness, y + thickness, thickness, h - thickness * 2.0, color);
    }

    /// Draws `text` in the Minecraft-style UI font and returns the advance
    /// width of its longest line.
    ///
    /// The font is the embedded Monocraft atlas by default and can be replaced
    /// at runtime with [`UiRenderer::set_ui_font`]. Newlines advance by
    /// [`line_height`] and reset the horizontal cursor to `x`. The returned
    /// width excludes the origin. Invalid coordinates or scales draw nothing
    /// and return 0.
    pub fn text(&mut self, x: f32, y: f32, scale: f32, color: [f32; 4], text: &str) -> f32 {
        self.draw_text(x, y, scale, color, text, UiTexture::UiFont)
    }

    /// Draws `text` in the built-in 8×8 bitmap title font and returns the
    /// advance width of its longest line.
    ///
    /// Use this for headings and other places that must keep the established
    /// bitmap look. Layout, newline handling and error behaviour are identical
    /// to [`Self::text`].
    pub fn title_text(&mut self, x: f32, y: f32, scale: f32, color: [f32; 4], text: &str) -> f32 {
        self.draw_text(x, y, scale, color, text, UiTexture::Font)
    }

    /// Shared implementation for [`Self::text`] and [`Self::title_text`].
    fn draw_text(&mut self, x: f32, y: f32, scale: f32, color: [f32; 4], text: &str, texture: UiTexture) -> f32 {
        let Some(scale) = sanitize_scale(scale) else {
            return 0.0;
        };
        if !x.is_finite() || !y.is_finite() {
            return 0.0;
        }
        let start_x = x.clamp(-MAX_COORD, MAX_COORD);
        let start_y = y.clamp(-MAX_COORD, MAX_COORD);
        let advance = glyph_advance(scale);
        let line_step = line_height(scale);

        let mut column = 0usize;
        let mut line = 0usize;
        let mut longest = 0usize;
        for character in text.chars() {
            match character {
                '\n' => {
                    longest = longest.max(column);
                    column = 0;
                    line = line.saturating_add(1);
                }
                '\r' => {}
                ' '..='~' => {
                    let glyph_x = start_x + column as f32 * advance;
                    let glyph_y = start_y + line as f32 * line_step;
                    if let Some((cell_x, cell_y)) = glyph_cell(character) {
                        let corners = [
                            [glyph_x, glyph_y],
                            [glyph_x + advance, glyph_y],
                            [glyph_x + advance, glyph_y + line_step],
                            [glyph_x, glyph_y + line_step],
                        ];
                        self.push_quad(texture, corners, glyph_uvs(cell_x, cell_y), 0, color);
                    }
                    column = column.saturating_add(1);
                }
                _ => {}
            }
        }
        longest = longest.max(column);
        longest as f32 * advance
    }

    /// Width in pixels of the longest line of `text` at `scale`.
    ///
    /// This is the same value [`Self::text`] returns, measured without
    /// appending geometry. It applies to the UI font and the title font alike
    /// because both use the same 8 px advance and line height.
    pub fn text_width(&self, scale: f32, text: &str) -> f32 {
        let Some(scale) = sanitize_scale(scale) else {
            return 0.0;
        };
        let mut column = 0usize;
        let mut longest = 0usize;
        for character in text.chars() {
            match character {
                '\n' => {
                    longest = longest.max(column);
                    column = 0;
                }
                '\r' => {}
                ' '..='~' => column = column.saturating_add(1),
                _ => {}
            }
        }
        longest.max(column) as f32 * glyph_advance(scale)
    }

    /// Draws `text` horizontally centred on `center_x` in the UI font.
    ///
    /// Multi-line blocks are centred using the longest line, so all lines share
    /// the same left edge. Invalid input is ignored safely.
    pub fn text_centered(&mut self, center_x: f32, y: f32, scale: f32, color: [f32; 4], text: &str) {
        if !center_x.is_finite() {
            return;
        }
        let width = self.text_width(scale, text);
        let x = center_x - width * 0.5;
        self.text(x, y, scale, color, text);
    }

    /// Draws `text` horizontally centred on `center_x` in the bitmap title font.
    ///
    /// Multi-line blocks are centred using the longest line, so all lines share
    /// the same left edge. Invalid input is ignored safely.
    pub fn title_text_centered(&mut self, center_x: f32, y: f32, scale: f32, color: [f32; 4], text: &str) {
        if !center_x.is_finite() {
            return;
        }
        let width = self.text_width(scale, text);
        let x = center_x - width * 0.5;
        self.title_text(x, y, scale, color, text);
    }

    /// Draws an isometric mini-cube made of a top, left and right face.
    ///
    /// The top face uses `top_layer` at full brightness; the left and right
    /// faces use `side_layer` at 0.72 and 0.85 brightness. Each face samples the
    /// full 0..1 UV range of its layer. Non-finite or non-positive sizes are
    /// ignored.
    pub fn block_icon(&mut self, x: f32, y: f32, size: f32, top_layer: u32, side_layer: u32) {
        if !x.is_finite() || !y.is_finite() || !size.is_finite() || size <= 0.0 {
            return;
        }
        let x = x.clamp(-MAX_COORD, MAX_COORD);
        let y = y.clamp(-MAX_COORD, MAX_COORD);
        let size = size.min(MAX_COORD);
        let half = size * 0.5;
        let quarter = size * 0.25;
        let three_quarters = size * 0.75;

        // The three faces share the same corner order; only the shape differs.
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let top = [
            [x + half, y],
            [x + size, y + quarter],
            [x + half, y + half],
            [x, y + quarter],
        ];
        let left = [
            [x, y + quarter],
            [x + half, y + half],
            [x + half, y + size],
            [x, y + three_quarters],
        ];
        let right = [
            [x + half, y + half],
            [x + size, y + quarter],
            [x + size, y + three_quarters],
            [x + half, y + size],
        ];

        self.push_quad(UiTexture::Blocks, top, uvs, top_layer, [1.0, 1.0, 1.0, 1.0]);
        self.push_quad(UiTexture::Blocks, left, uvs, side_layer, [0.72, 0.72, 0.72, 1.0]);
        self.push_quad(UiTexture::Blocks, right, uvs, side_layer, [0.85, 0.85, 0.85, 1.0]);
    }

    /// Number of vertices and quads in the frame.
    pub fn measure(&self) -> (usize, usize) {
        let vertices = self.batches.iter().map(|batch| batch.vertices.len()).sum();
        (vertices, self.quad_count)
    }

    /// The contiguous texture batches, in draw order.
    pub(crate) fn batches(&self) -> &[UiBatch] {
        &self.batches
    }

    /// Converts a pixel position to NDC, flipping Y.
    fn to_ndc(&self, x: f32, y: f32) -> [f32; 2] {
        [(x / self.width) * 2.0 - 1.0, 1.0 - (y / self.height) * 2.0]
    }

    /// Appends two triangles, merging with the previous batch when possible.
    fn push_quad(
        &mut self,
        texture: UiTexture,
        corners: [[f32; 2]; 4],
        uvs: [[f32; 2]; 4],
        layer: u32,
        color: [f32; 4],
    ) {
        if self.quad_count >= MAX_VERTICES / 6 {
            return;
        }
        if corners
            .iter()
            .flatten()
            .chain(uvs.iter().flatten())
            .any(|value| !value.is_finite())
        {
            return;
        }
        let color = sanitize_color(color);
        let ndc = [
            self.to_ndc(corners[0][0], corners[0][1]),
            self.to_ndc(corners[1][0], corners[1][1]),
            self.to_ndc(corners[2][0], corners[2][1]),
            self.to_ndc(corners[3][0], corners[3][1]),
        ];
        let vertex = |index: usize| UiVertex {
            position: ndc[index],
            uv: uvs[index],
            layer,
            color,
        };
        let vertices = [vertex(0), vertex(1), vertex(2), vertex(0), vertex(2), vertex(3)];

        if let Some(last) = self.batches.last_mut()
            && last.texture == texture
        {
            last.vertices.extend_from_slice(&vertices);
        } else {
            self.batches.push(UiBatch {
                texture,
                vertices: vertices.to_vec(),
            });
        }
        self.quad_count += 1;
    }
}

/// Counters from the last [`UiRenderer::render`] call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UiStats {
    /// Texture batches drawn (one draw call each).
    pub draw_calls: u32,
    /// Vertices submitted.
    pub vertices: u64,
    /// Quads submitted.
    pub quads: u64,
}

/// Draws [`UiFrame`] data into an existing colour target.
///
/// The renderer owns the pipeline, the 1×1 solid array texture, the UI font
/// atlas, the bitmap title font atlas and the four bind groups. It does not own
/// the block texture array: [`UiRenderer::new`] borrows it only long enough to
/// create the block bind group, which then retains the GPU view and sampler.
/// [`UiRenderer::set_ui_font`] replaces the UI font atlas and its bind group at
/// runtime; the title font and block bind groups are unaffected.
pub struct UiRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_groups: [wgpu::BindGroup; 4],
    /// Kept for rebuilding the UI-font bind group in [`UiRenderer::set_ui_font`].
    bind_group_layout: wgpu::BindGroupLayout,
    /// Kept for rebuilding the UI-font and block bind groups.
    sampler: wgpu::Sampler,
    /// Kept alive for the bind groups that reference its view.
    _solid_texture: wgpu::Texture,
    /// Kept alive for the bind groups that reference its view.
    _ui_font_texture: wgpu::Texture,
    /// Kept alive for the bind groups that reference its view.
    _title_font_texture: wgpu::Texture,
    /// Display name of the active UI font ("Monocraft" or "custom").
    ui_font_name: String,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: u64,
    scratch: Vec<UiVertex>,
    srgb_target: bool,
    stats: UiStats,
}

impl UiRenderer {
    /// Creates the UI pipeline and its generated textures for `format`.
    ///
    /// `blocks` supplies the block texture array used by
    /// [`UiFrame::block_icon`]. This may be called more than once per device
    /// (for example once for the window surface and once for an offscreen
    /// path). The embedded Monocraft UI font is used; see
    /// [`Self::new_with_ui_font`] to supply custom font bytes.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blocks: &BlockTextureArray,
        format: wgpu::TextureFormat,
    ) -> Self {
        Self::new_with_ui_font(device, queue, blocks, format, None)
    }

    /// Creates the UI pipeline with an optional custom UI font.
    ///
    /// `ui_font` is the raw contents of a TrueType/OpenType file. `None` uses
    /// the embedded Monocraft font. Invalid or unusable bytes also fall back to
    /// Monocraft and log a warning instead of failing renderer construction, so
    /// a bad user setting cannot make the game unrenderable. The UI font can be
    /// replaced later with [`Self::set_ui_font`].
    pub fn new_with_ui_font(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blocks: &BlockTextureArray,
        format: wgpu::TextureFormat,
        ui_font: Option<&[u8]>,
    ) -> Self {
        let (ui_font_atlas, ui_font_name) = load_ui_font_or_default(ui_font);
        let solid_texture = create_solid_texture(device, queue);
        let ui_font_texture = create_atlas_texture(device, queue, "ui-font-atlas", &ui_font_atlas);
        let title_font_texture = create_title_font_texture(device, queue);
        let solid_view = create_array_view(&solid_texture, "ui-solid-view");
        let ui_font_view = create_array_view(&ui_font_texture, "ui-font-atlas-view");
        let title_font_view = create_array_view(&title_font_texture, "ui-title-font-atlas-view");

        let bind_group_layout = create_bind_group_layout(device);
        let sampler = create_sampler(device);
        let solid_bind_group =
            create_bind_group(device, &bind_group_layout, "ui-solid-bind-group", &solid_view, &sampler);
        let ui_font_bind_group = create_bind_group(
            device,
            &bind_group_layout,
            "ui-font-bind-group",
            &ui_font_view,
            &sampler,
        );
        let title_font_bind_group = create_bind_group(
            device,
            &bind_group_layout,
            "ui-title-font-bind-group",
            &title_font_view,
            &sampler,
        );
        let blocks_bind_group = create_bind_group(
            device,
            &bind_group_layout,
            "ui-blocks-bind-group",
            blocks.view(),
            blocks.sampler(),
        );
        let pipeline = create_pipeline(device, format, &bind_group_layout);

        Self {
            device: device.clone(),
            queue: queue.clone(),
            pipeline,
            bind_groups: [
                solid_bind_group,
                ui_font_bind_group,
                title_font_bind_group,
                blocks_bind_group,
            ],
            bind_group_layout,
            sampler,
            _solid_texture: solid_texture,
            _ui_font_texture: ui_font_texture,
            _title_font_texture: title_font_texture,
            ui_font_name,
            vertex_buffer: create_vertex_buffer(device, INITIAL_VERTEX_BYTES),
            vertex_capacity: INITIAL_VERTEX_BYTES,
            scratch: Vec::new(),
            srgb_target: format.is_srgb(),
            stats: UiStats::default(),
        }
    }

    /// Replaces the UI font atlas at runtime.
    ///
    /// `font` is the raw contents of a TrueType/OpenType file; `None` restores
    /// the embedded Monocraft font. On success the next rendered frame uses the
    /// new font and [`Self::ui_font_name`] reports `"Monocraft"` (embedded bytes)
    /// or `"custom"`. On error the previous atlas and name are kept unchanged
    /// and the error is returned.
    pub fn set_ui_font(&mut self, font: Option<&[u8]>) -> Result<(), UiFontError> {
        let (atlas, name) = match font {
            None => (default_ui_font_atlas(), default_ui_font_name().to_owned()),
            Some(bytes) => {
                let atlas = rasterize_ui_font(bytes)?;
                (atlas, ui_font_name_for(bytes))
            }
        };
        self.install_ui_font_atlas(atlas, name);
        Ok(())
    }

    /// Display name of the active UI font: `"Monocraft"` for the embedded
    /// default or `"custom"` for a user-supplied font.
    pub fn ui_font_name(&self) -> &str {
        &self.ui_font_name
    }

    /// Recreates the block-texture bind group after a texture-source change.
    ///
    /// The UI font atlas, title font atlas and pipeline are unaffected, so a
    /// user-selected font survives resource-pack changes.
    pub(crate) fn set_block_textures(&mut self, blocks: &BlockTextureArray) {
        self.bind_groups[UiTexture::Blocks.bind_group_index()] = create_bind_group(
            &self.device,
            &self.bind_group_layout,
            "ui-blocks-bind-group",
            blocks.view(),
            blocks.sampler(),
        );
    }

    /// Uploads a rasterised atlas and replaces the UI-font bind group.
    fn install_ui_font_atlas(&mut self, atlas: UiFontAtlas, name: String) {
        debug_assert_eq!(atlas.width, ATLAS_WIDTH);
        debug_assert_eq!(atlas.height, ATLAS_HEIGHT);
        let texture = create_atlas_texture(&self.device, &self.queue, "ui-font-atlas", &atlas);
        let view = create_array_view(&texture, "ui-font-atlas-view");
        let bind_group = create_bind_group(
            &self.device,
            &self.bind_group_layout,
            "ui-font-bind-group",
            &view,
            &self.sampler,
        );
        self.bind_groups[UiTexture::UiFont.bind_group_index()] = bind_group;
        self._ui_font_texture = texture;
        self.ui_font_name = name;
    }

    /// Uploads `frame` and draws it into `target` with a `LoadOp::Load` pass.
    ///
    /// The target's size must match the size passed to [`UiFrame::new`]. There
    /// is no depth attachment and culling is disabled, so painter's order is
    /// the visible order. Call this after the world pass, before submitting the
    /// encoder and presenting.
    pub fn render(&mut self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, frame: &UiFrame) {
        self.scratch.clear();
        for batch in frame.batches() {
            for vertex in &batch.vertices {
                let mut vertex = *vertex;
                if self.srgb_target {
                    vertex.color = srgb_to_linear(vertex.color);
                }
                self.scratch.push(vertex);
            }
        }

        let needed = (self.scratch.len() * std::mem::size_of::<UiVertex>()) as u64;
        if needed > self.vertex_capacity {
            let capacity = needed.next_power_of_two();
            self.vertex_buffer = create_vertex_buffer(&self.device, capacity);
            self.vertex_capacity = capacity;
        }
        if !self.scratch.is_empty() {
            self.queue
                .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&self.scratch));
        }

        let mut stats = UiStats::default();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            if !self.scratch.is_empty() {
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                let mut first_vertex = 0u32;
                for batch in frame.batches() {
                    let vertex_count = batch.vertices.len() as u32;
                    pass.set_bind_group(0, &self.bind_groups[batch.texture.bind_group_index()], &[]);
                    pass.draw(first_vertex..first_vertex + vertex_count, 0..1);
                    first_vertex += vertex_count;
                    stats.draw_calls += 1;
                    stats.vertices += u64::from(vertex_count);
                    stats.quads += u64::from(vertex_count / 6);
                }
            }
        }
        self.stats = stats;
    }

    /// Counters from the most recent [`Self::render`] call.
    pub fn stats(&self) -> UiStats {
        self.stats
    }
}

/// Coerces the frame's target size into a finite, positive range.
fn sanitize_screen(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value.min(MAX_COORD)
    } else {
        1.0
    }
}

/// Validates and clamps a rectangle before conversion.
fn sanitize_rect(x: f32, y: f32, w: f32, h: f32) -> Option<(f32, f32, f32, f32)> {
    if ![x, y, w, h].iter().all(|value| value.is_finite()) || w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((
        x.clamp(-MAX_COORD, MAX_COORD),
        y.clamp(-MAX_COORD, MAX_COORD),
        w.min(MAX_COORD),
        h.min(MAX_COORD),
    ))
}

/// Returns a usable glyph scale, or `None` when the caller should draw nothing.
fn sanitize_scale(scale: f32) -> Option<f32> {
    if scale.is_finite() && scale > 0.0 {
        Some(scale.min(MAX_SCALE))
    } else {
        None
    }
}

/// Clamps colour channels into 0..=1, mapping non-finite channels to zero.
fn sanitize_color(color: [f32; 4]) -> [f32; 4] {
    color.map(|channel| {
        if channel.is_finite() {
            channel.clamp(0.0, 1.0)
        } else {
            0.0
        }
    })
}

/// Converts one sRGB channel to linear space.
fn srgb_to_linear_channel(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Converts an sRGB vertex colour to linear RGB, leaving alpha unchanged.
fn srgb_to_linear(color: [f32; 4]) -> [f32; 4] {
    [
        srgb_to_linear_channel(color[0]),
        srgb_to_linear_channel(color[1]),
        srgb_to_linear_channel(color[2]),
        color[3],
    ]
}

/// Maps a printable character to its font-atlas cell.
fn glyph_cell(character: char) -> Option<(u32, u32)> {
    let code = character as u32;
    if !(FIRST_PRINTABLE..=LAST_PRINTABLE).contains(&code) {
        return None;
    }
    let index = code - FIRST_PRINTABLE;
    Some((index % ATLAS_COLUMNS, index / ATLAS_COLUMNS))
}

/// Returns the four UV corners of a font cell, inset by half a texel.
fn glyph_uvs(cell_x: u32, cell_y: u32) -> [[f32; 2]; 4] {
    let half_u = 0.5 / ATLAS_WIDTH as f32;
    let half_v = 0.5 / ATLAS_HEIGHT as f32;
    let u0 = (cell_x * 8) as f32 / ATLAS_WIDTH as f32 + half_u;
    let v0 = (cell_y * 8) as f32 / ATLAS_HEIGHT as f32 + half_v;
    let u1 = ((cell_x + 1) * 8) as f32 / ATLAS_WIDTH as f32 - half_u;
    let v1 = ((cell_y + 1) * 8) as f32 / ATLAS_HEIGHT as f32 - half_v;
    [[u0, v0], [u1, v0], [u1, v1], [u0, v1]]
}

/// Rasterises printable ASCII into a transparent 128×64 RGBA8 atlas.
fn font_atlas_pixels() -> Vec<u8> {
    let mut pixels = vec![0u8; (ATLAS_WIDTH * ATLAS_HEIGHT * 4) as usize];
    for code in FIRST_PRINTABLE..=LAST_PRINTABLE {
        let character = char::from_u32(code).unwrap_or('?');
        let Some((cell_x, cell_y)) = glyph_cell(character) else {
            continue;
        };
        let glyph = BASIC_FONTS.get(character).unwrap_or([0; 8]);
        for (row, bits) in glyph.iter().enumerate() {
            for (column, mask) in [1u8, 2, 4, 8, 16, 32, 64, 128].into_iter().enumerate() {
                if bits & mask == 0 {
                    continue;
                }
                let x = cell_x * 8 + column as u32;
                let y = cell_y * 8 + row as u32;
                let offset = ((y * ATLAS_WIDTH + x) * 4) as usize;
                pixels[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    pixels
}

/// Rasterises the embedded Monocraft UI font, falling back to a blank atlas if
/// the bundled asset ever fails to parse (it is covered by tests, so this is
/// only a defensive guard against turning a rendering path into a panic).
fn default_ui_font_atlas() -> UiFontAtlas {
    match rasterize_ui_font(default_ui_font()) {
        Ok(atlas) => atlas,
        Err(error) => {
            log::error!(
                "embedded {} UI font failed to rasterise: {error}; using a blank atlas",
                default_ui_font_name()
            );
            UiFontAtlas {
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                pixels: vec![0u8; (ATLAS_WIDTH * ATLAS_HEIGHT * 4) as usize],
            }
        }
    }
}

/// Resolves optional custom font bytes for renderer construction.
///
/// Invalid bytes keep the renderer constructible: the embedded default is used
/// and a warning is logged. Runtime changes report errors through
/// [`UiRenderer::set_ui_font`] instead.
fn load_ui_font_or_default(font: Option<&[u8]>) -> (UiFontAtlas, String) {
    match font {
        None => (default_ui_font_atlas(), default_ui_font_name().to_owned()),
        Some(bytes) => match rasterize_ui_font(bytes) {
            Ok(atlas) => (atlas, ui_font_name_for(bytes)),
            Err(error) => {
                log::warn!(
                    "failed to load custom UI font ({} bytes): {error}; falling back to {}",
                    bytes.len(),
                    default_ui_font_name()
                );
                (default_ui_font_atlas(), default_ui_font_name().to_owned())
            }
        },
    }
}

/// Returns the display name for successfully rasterised UI font bytes.
fn ui_font_name_for(bytes: &[u8]) -> String {
    if bytes == default_ui_font() {
        default_ui_font_name().to_owned()
    } else {
        "custom".to_owned()
    }
}

/// Creates and uploads the 1×1 white array texture used for solid quads.
fn create_solid_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui-solid-texture"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let data: [u8; 4] = [255, 255, 255, 255];
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    texture
}

/// Creates and uploads the generated 8×8 bitmap title font atlas.
fn create_title_font_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::Texture {
    let atlas = UiFontAtlas {
        width: ATLAS_WIDTH,
        height: ATLAS_HEIGHT,
        pixels: font_atlas_pixels(),
    };
    create_atlas_texture(device, queue, "ui-title-font-atlas", &atlas)
}

/// Creates and uploads one RGBA8 128×64 font atlas as a texture array layer.
fn create_atlas_texture(device: &wgpu::Device, queue: &wgpu::Queue, label: &str, atlas: &UiFontAtlas) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: atlas.width,
            height: atlas.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    debug_assert_eq!(atlas.pixels.len(), (atlas.width * atlas.height * 4) as usize);
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &atlas.pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(atlas.width * 4),
            rows_per_image: Some(atlas.height),
        },
        wgpu::Extent3d {
            width: atlas.width,
            height: atlas.height,
            depth_or_array_layers: 1,
        },
    );
    texture
}

/// Creates a D2Array view for one of the generated array textures.
fn create_array_view(texture: &wgpu::Texture, label: &str) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some(label),
        format: Some(TEXTURE_FORMAT),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
        aspect: wgpu::TextureAspect::All,
        base_mip_level: 0,
        mip_level_count: None,
        base_array_layer: 0,
        array_layer_count: None,
    })
}

/// Creates the dedicated UI bind-group layout: `texture_2d_array` + sampler.
fn create_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("ui-bind-group-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// Creates a nearest-neighbour clamp sampler for solid and font quads.
fn create_sampler(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("ui-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        lod_min_clamp: 0.0,
        lod_max_clamp: 0.0,
        compare: None,
        anisotropy_clamp: 1,
        border_color: None,
    })
}

/// Binds one texture array view and sampler.
fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    label: &str,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Creates the UI pipeline: textured, alpha-blended, no depth, no culling.
fn create_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("ui-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("ui-shader"),
        source: wgpu::ShaderSource::Wgsl(UI_SHADER.into()),
    });
    let vertex_attributes = [
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 8,
            shader_location: 1,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Uint32,
            offset: 16,
            shader_location: 2,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x4,
            offset: 20,
            shader_location: 3,
        },
    ];
    let vertex_buffers = [Some(wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<UiVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &vertex_attributes,
    })];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("ui-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &vertex_buffers,
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::SrcAlpha,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Creates a vertex buffer large enough for the requested number of bytes.
fn create_vertex_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ui-vertices"),
        size,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// WGSL for the UI pipeline. Colours arrive linearised by the renderer.
const UI_SHADER: &str = r"
struct UiVertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
    @location(2) color: vec4<f32>,
};

@group(0) @binding(0) var ui_texture: texture_2d_array<f32>;
@group(0) @binding(1) var ui_sampler: sampler;

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) layer: u32,
    @location(3) color: vec4<f32>,
) -> UiVertexOut {
    var out: UiVertexOut;
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.uv = uv;
    out.layer = layer;
    out.color = color;
    return out;
}

@fragment
fn fs_main(input: UiVertexOut) -> @location(0) vec4<f32> {
    let sampled = textureSample(ui_texture, ui_sampler, input.uv, i32(input.layer));
    return sampled * input.color;
}
";

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn ui_vertex_layout_matches_the_pipeline() {
        assert_eq!(std::mem::size_of::<UiVertex>(), 36);
        assert_eq!(std::mem::align_of::<UiVertex>(), 4);
        assert_eq!(std::mem::offset_of!(UiVertex, position), 0);
        assert_eq!(std::mem::offset_of!(UiVertex, uv), 8);
        assert_eq!(std::mem::offset_of!(UiVertex, layer), 16);
        assert_eq!(std::mem::offset_of!(UiVertex, color), 20);
    }

    #[test]
    fn rect_pushes_one_quad() {
        let mut frame = UiFrame::new(320.0, 180.0);
        frame.rect(10.0, 20.0, 30.0, 40.0, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(frame.measure(), (6, 1));
        assert_eq!(frame.batches.len(), 1);
        assert_eq!(frame.batches[0].texture, UiTexture::Solid);
        assert_eq!(frame.batches[0].vertices[0].layer, 0);
    }

    #[test]
    fn outline_pushes_four_rects() {
        let mut frame = UiFrame::new(320.0, 180.0);
        frame.outline(10.0, 20.0, 100.0, 60.0, 3.0, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(frame.measure(), (24, 4));
        assert_eq!(frame.batches.len(), 1);
        assert_eq!(frame.batches[0].texture, UiTexture::Solid);
    }

    #[test]
    fn text_width_is_monotonic_and_multiline() {
        let frame = UiFrame::new(320.0, 180.0);
        assert_eq!(frame.text_width(1.0, ""), 0.0);
        assert_eq!(frame.text_width(1.0, "A"), 8.0);
        assert_eq!(frame.text_width(2.0, "AB"), 32.0);
        assert!(frame.text_width(1.0, "AB") >= frame.text_width(1.0, "A"));
        assert!(frame.text_width(1.0, "ABC") >= frame.text_width(1.0, "AB"));
        // The longest line wins, regardless of where it appears.
        assert_eq!(frame.text_width(1.0, "A\nABCD\nAB"), 32.0);
        // A trailing newline does not create a longer line.
        assert_eq!(frame.text_width(1.0, "ABCD\n"), 32.0);
        // Spaces advance the cursor even though their glyph is transparent.
        assert_eq!(frame.text_width(1.0, "A B"), 24.0);
    }

    #[test]
    fn text_uses_the_ui_font_and_returns_longest_line_advance() {
        let mut frame = UiFrame::new(320.0, 180.0);
        let advance = frame.text(10.0, 20.0, 2.0, [1.0, 1.0, 1.0, 1.0], "AB\nC");
        assert_eq!(advance, 32.0);
        assert_eq!(frame.measure(), (18, 3));
        assert_eq!(frame.batches.len(), 1);
        assert_eq!(frame.batches[0].texture, UiTexture::UiFont);
    }

    #[test]
    fn title_text_uses_the_bitmap_font_and_shares_layout_metrics() {
        let mut frame = UiFrame::new(320.0, 180.0);
        let advance = frame.title_text(10.0, 20.0, 2.0, [1.0, 1.0, 1.0, 1.0], "AB\nC");
        assert_eq!(advance, 32.0);
        assert_eq!(advance, frame.text_width(2.0, "AB\nC"));
        assert_eq!(frame.measure(), (18, 3));
        assert_eq!(frame.batches.len(), 1);
        assert_eq!(frame.batches[0].texture, UiTexture::Font);

        let mut frame = UiFrame::new(100.0, 50.0);
        frame.title_text_centered(50.0, 10.0, 1.0, [1.0; 4], "AB");
        let first_x = frame.batches[0].vertices[0].position[0];
        let expected = (42.0 / 100.0) * 2.0 - 1.0;
        assert!((first_x - expected).abs() < 1e-6);
    }

    #[test]
    fn mixed_ui_and_title_text_batches_preserve_order() {
        let mut frame = UiFrame::new(320.0, 180.0);
        frame.text(0.0, 0.0, 1.0, [1.0; 4], "UI");
        frame.title_text(0.0, 20.0, 1.0, [1.0; 4], "TITLE");
        frame.text(0.0, 40.0, 1.0, [1.0; 4], "UI");
        let textures: Vec<UiTexture> = frame.batches.iter().map(|batch| batch.texture).collect();
        assert_eq!(textures, vec![UiTexture::UiFont, UiTexture::Font, UiTexture::UiFont]);
        assert_eq!(frame.measure(), (12 + 30 + 12, 2 + 5 + 2));
    }

    #[test]
    fn batches_group_by_texture_and_preserve_insertion_order() {
        let mut frame = UiFrame::new(320.0, 180.0);
        frame.rect(0.0, 0.0, 10.0, 10.0, [1.0; 4]);
        frame.rect(10.0, 10.0, 10.0, 10.0, [1.0; 4]);
        frame.text(0.0, 20.0, 1.0, [1.0; 4], "A");
        frame.title_text(0.0, 30.0, 1.0, [1.0; 4], "T");
        frame.block_icon(40.0, 40.0, 16.0, 1, 2);
        frame.rect(0.0, 60.0, 10.0, 10.0, [1.0; 4]);

        let textures: Vec<UiTexture> = frame.batches.iter().map(|batch| batch.texture).collect();
        assert_eq!(
            textures,
            vec![
                UiTexture::Solid,
                UiTexture::UiFont,
                UiTexture::Font,
                UiTexture::Blocks,
                UiTexture::Solid,
            ]
        );
        assert_eq!(frame.batches[0].vertices.len(), 12);
        assert_eq!(frame.measure(), (12 + 6 + 6 + 18 + 6, 2 + 1 + 1 + 3 + 1));
    }

    #[test]
    fn invalid_and_huge_inputs_are_safe() {
        let mut frame = UiFrame::new(f32::NAN, f32::INFINITY);
        frame.rect(f32::NAN, 0.0, 10.0, 10.0, [1.0; 4]);
        frame.rect(0.0, 0.0, -10.0, 10.0, [1.0; 4]);
        frame.rect(0.0, 0.0, f32::INFINITY, 10.0, [1.0; 4]);
        frame.outline(0.0, 0.0, 10.0, 10.0, f32::NAN, [1.0; 4]);
        frame.outline(0.0, 0.0, 10.0, 10.0, -2.0, [1.0; 4]);
        assert_eq!(frame.measure(), (0, 0));
        assert_eq!(frame.text(0.0, 0.0, f32::NAN, [1.0; 4], "bad"), 0.0);
        assert_eq!(frame.text(0.0, 0.0, -1.0, [1.0; 4], "bad"), 0.0);
        frame.block_icon(0.0, 0.0, f32::NAN, 1, 2);
        frame.block_icon(0.0, 0.0, -3.0, 1, 2);
        assert_eq!(frame.measure(), (0, 0));

        // Huge but finite values clamp instead of producing NaN geometry.
        frame.rect(f32::MAX, f32::MAX, f32::MAX, f32::MAX, [1.0; 4]);
        frame.outline(-f32::MAX, -f32::MAX, f32::MAX, f32::MAX, f32::MAX, [1.0; 4]);
        frame.block_icon(f32::MAX, f32::MAX, f32::MAX, u32::MAX, 0);
        let (vertices, quads) = frame.measure();
        assert!(vertices > 0);
        assert_eq!(vertices, quads * 6);
        assert!(
            frame
                .batches
                .iter()
                .flat_map(|batch| batch.vertices.iter())
                .all(|vertex| vertex.position.iter().all(|value| value.is_finite()))
        );
    }

    #[test]
    fn pixel_coordinates_are_flipped_to_ndc() {
        let mut frame = UiFrame::new(100.0, 50.0);
        frame.rect(0.0, 0.0, 100.0, 50.0, [1.0; 4]);
        let first = frame.batches[0].vertices[0];
        let third = frame.batches[0].vertices[2];
        assert_eq!(first.position, [-1.0, 1.0]);
        assert_eq!(third.position, [1.0, -1.0]);
    }

    #[test]
    fn block_icon_uses_three_shaded_faces() {
        let mut frame = UiFrame::new(320.0, 180.0);
        frame.block_icon(10.0, 10.0, 32.0, 3, 4);
        assert_eq!(frame.measure(), (18, 3));
        assert_eq!(frame.batches.len(), 1);
        assert_eq!(frame.batches[0].texture, UiTexture::Blocks);
        let vertices = &frame.batches[0].vertices;
        assert!(vertices[..6].iter().all(|vertex| vertex.layer == 3));
        assert!(vertices[..6].iter().all(|vertex| vertex.color == [1.0; 4]));
        assert!(
            vertices[6..12]
                .iter()
                .all(|vertex| vertex.layer == 4 && vertex.color == [0.72, 0.72, 0.72, 1.0])
        );
        assert!(
            vertices[12..]
                .iter()
                .all(|vertex| vertex.layer == 4 && vertex.color == [0.85, 0.85, 0.85, 1.0])
        );
    }

    #[test]
    fn centered_text_uses_the_longest_line() {
        let mut frame = UiFrame::new(100.0, 50.0);
        frame.text_centered(50.0, 10.0, 1.0, [1.0; 4], "AB");
        let first_x = frame.batches[0].vertices[0].position[0];
        // Longest line is 16 px, so the left edge is at 42 px.
        let expected = (42.0 / 100.0) * 2.0 - 1.0;
        assert!((first_x - expected).abs() < 1e-6);
    }

    #[test]
    fn font_atlas_is_white_on_transparent() {
        let pixels = font_atlas_pixels();
        assert_eq!(pixels.len(), (ATLAS_WIDTH * ATLAS_HEIGHT * 4) as usize);
        let texels = pixels.as_chunks::<4>().0;
        assert!(texels.contains(&[255, 255, 255, 255]));
        assert!(texels.contains(&[0, 0, 0, 0]));
        // The unused cells at the end of the atlas are fully transparent.
        let last_cell_offset = (((ATLAS_HEIGHT - 8) * ATLAS_WIDTH + (ATLAS_WIDTH - 8)) * 4) as usize;
        assert!(
            pixels[last_cell_offset..last_cell_offset + 8 * 4]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    /// Builds a headless device, returning `None` when no adapter exists or when
    /// GPU tests are disabled (containers with an unstable software Vulkan).
    fn test_gpu() -> Option<(wgpu::Device, wgpu::Queue, String)> {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            return None;
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let options = |power_preference| wgpu::RequestAdapterOptions {
            power_preference,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        };
        let adapter = pollster::block_on(instance.request_adapter(&options(wgpu::PowerPreference::LowPower)))
            .or_else(|_| pollster::block_on(instance.request_adapter(&options(wgpu::PowerPreference::default()))))
            .ok()?;
        let info = adapter.get_info();
        let summary = format!("{} ({}, {})", info.name, info.backend, info.driver);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ob-ui-test"),
            ..Default::default()
        }))
        .ok()?;
        Some((device, queue, summary))
    }

    /// Copies an RGBA8 texture back into tightly packed top-row-first bytes.
    fn read_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let bytes_per_row =
            (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-test-readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ui-test-readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let mapped: Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> = Arc::new(Mutex::new(None));
        {
            let mapped = Arc::clone(&mapped);
            slice.map_async(wgpu::MapMode::Read, move |result| {
                if let Ok(mut slot) = mapped.lock() {
                    *slot = Some(result);
                }
            });
        }
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll readback");
        let map_result = pollster::block_on(std::future::poll_fn(|_cx| {
            let mut slot = mapped.lock().expect("map callback state poisoned");
            match slot.take() {
                Some(result) => std::task::Poll::Ready(result),
                None => std::task::Poll::Pending,
            }
        }));
        map_result.expect("map UI readback buffer");

        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        {
            let data = slice.get_mapped_range().expect("mapped range");
            let row_bytes = (width * 4) as usize;
            for row in 0..height as usize {
                let start = row * bytes_per_row as usize;
                pixels.extend_from_slice(&data[start..start + row_bytes]);
            }
        }
        buffer.unmap();
        pixels
    }

    /// Reads one pixel from a tightly packed top-row-first RGBA8 image.
    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * width + x) * 4) as usize;
        [
            pixels[offset],
            pixels[offset + 1],
            pixels[offset + 2],
            pixels[offset + 3],
        ]
    }

    /// Asserts that two colours are within `tolerance` on every channel.
    fn assert_color_near(actual: [u8; 4], expected: [u8; 4], tolerance: u8, what: &str) {
        for (channel, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                actual.abs_diff(*expected) <= tolerance,
                "{what}: got {actual:?}, expected {expected:?} (channel {channel})"
            );
        }
    }

    /// Whether two colours differ by more than `tolerance` on any channel.
    fn differs(actual: [u8; 4], expected: [u8; 4], tolerance: u8) -> bool {
        actual
            .iter()
            .zip(expected.iter())
            .any(|(actual, expected)| actual.abs_diff(*expected) > tolerance)
    }

    /// Renders one UI frame into a fresh sRGB target and reads the pixels back.
    fn render_ui_frame(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ui: &mut UiRenderer,
        width: u32,
        height: u32,
        frame: &UiFrame,
    ) -> Vec<u8> {
        const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui-font-test-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ui-font-test-frame"),
        });
        {
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-font-test-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.01,
                            g: 0.01,
                            b: 0.02,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        ui.render(&mut encoder, &view, frame);
        queue.submit([encoder.finish()]);
        read_texture(device, queue, &target, width, height)
    }

    #[test]
    fn ui_gpu_draws_rect_outline_text_and_block_icon() {
        let Some((device, queue, summary)) = test_gpu() else {
            eprintln!("skipping ui_gpu_draws_rect_outline_text_and_block_icon: no GPU adapter");
            return;
        };
        eprintln!("ui test GPU: {summary}");

        const WIDTH: u32 = 320;
        const HEIGHT: u32 = 180;
        const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

        let blocks = BlockTextureArray::new(&device, &queue);
        let mut ui = UiRenderer::new(&device, &queue, &blocks, FORMAT);
        assert_eq!(ui.ui_font_name(), default_ui_font_name());
        // Constructing a second renderer on the same device is the windowed +
        // offscreen configuration used by the client. `None` selects the
        // embedded default UI font; invalid bytes also fall back to it.
        let second_ui = UiRenderer::new_with_ui_font(&device, &queue, &blocks, FORMAT, None);
        assert_eq!(second_ui.stats(), UiStats::default());
        assert_eq!(second_ui.ui_font_name(), default_ui_font_name());
        let fallback_ui = UiRenderer::new_with_ui_font(&device, &queue, &blocks, FORMAT, Some(b"not a font"));
        assert_eq!(fallback_ui.ui_font_name(), default_ui_font_name());

        let mut frame = UiFrame::new(WIDTH as f32, HEIGHT as f32);
        let rect_color = [0.2, 0.4, 0.6, 1.0];
        frame.rect(12.0, 12.0, 80.0, 48.0, rect_color);
        let outline_color = [0.9, 0.3, 0.2, 1.0];
        frame.outline(108.0, 12.0, 76.0, 48.0, 3.0, outline_color);
        let text = "OpenBlock F3\nGPU UI test";
        let text_advance = frame.text(12.0, 76.0, 2.0, [1.0, 1.0, 1.0, 1.0], text);
        let title = "8x8 TITLE TEXT";
        let title_advance = frame.title_text(12.0, 112.0, 1.0, [1.0, 1.0, 1.0, 1.0], title);
        frame.block_icon(
            232.0,
            76.0,
            64.0,
            ob_content::textures::GRASS_TOP.0 as u32,
            ob_content::textures::GRASS_SIDE.0 as u32,
        );

        let (vertices, quads) = frame.measure();
        assert_eq!(text_advance, 192.0);
        assert_eq!(text_advance, frame.text_width(2.0, text));
        assert_eq!(title_advance, frame.text_width(1.0, title));
        assert_eq!(vertices, quads * 6);
        let textures: HashSet<UiTexture> = frame.batches.iter().map(|batch| batch.texture).collect();
        assert!(textures.contains(&UiTexture::Solid));
        assert!(textures.contains(&UiTexture::UiFont));
        assert!(textures.contains(&UiTexture::Font));
        assert!(textures.contains(&UiTexture::Blocks));

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui-test-target"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ui-test-frame"),
        });
        {
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-test-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.01,
                            g: 0.01,
                            b: 0.02,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        ui.render(&mut encoder, &view, &frame);
        queue.submit([encoder.finish()]);

        let pixels = read_texture(&device, &queue, &target, WIDTH, HEIGHT);
        assert_eq!(pixels.len(), (WIDTH * HEIGHT * 4) as usize);

        let background = pixel(&pixels, WIDTH, 0, 0);
        let rect_pixel = pixel(&pixels, WIDTH, 50, 30);
        assert_color_near(rect_pixel, [51, 102, 153, 255], 3, "filled rect");

        let outline_pixel = pixel(&pixels, WIDTH, 146, 13);
        assert_color_near(outline_pixel, [230, 77, 51, 255], 3, "outline edge");
        let outline_interior = pixel(&pixels, WIDTH, 146, 40);
        assert!(
            !differs(outline_interior, background, 3),
            "outline interior should stay background: {outline_interior:?} vs {background:?}"
        );

        let mut text_pixels = 0usize;
        for y in 76..108 {
            for x in 12..220 {
                if differs(pixel(&pixels, WIDTH, x, y), background, 3) {
                    text_pixels += 1;
                }
            }
        }
        eprintln!(
            "ui GPU: background={background:?} rect={rect_pixel:?} outline={outline_pixel:?} text_pixels={text_pixels}"
        );
        assert!(
            text_pixels > 100,
            "font text should add many non-background pixels, got {text_pixels}"
        );

        let mut title_pixels = 0usize;
        for y in 112..120 {
            for x in 12..140 {
                if differs(pixel(&pixels, WIDTH, x, y), background, 3) {
                    title_pixels += 1;
                }
            }
        }
        eprintln!("ui GPU: title bitmap text pixels={title_pixels}");
        assert!(
            title_pixels > 20,
            "title bitmap text should add non-background pixels, got {title_pixels}"
        );

        let mut block_colors = HashSet::new();
        for y in 76..140 {
            for x in 232..296 {
                let colour = pixel(&pixels, WIDTH, x, y);
                block_colors.insert([colour[0], colour[1], colour[2]]);
            }
        }
        eprintln!("ui GPU: block icon distinct colours={}", block_colors.len());
        assert!(
            block_colors.len() > 8,
            "block icon should sample several texture colours, got {}",
            block_colors.len()
        );

        assert_eq!(ui.stats().vertices, vertices as u64);
        assert_eq!(ui.stats().quads, quads as u64);
        assert_eq!(ui.stats().draw_calls, frame.batches.len() as u32);
        assert!(ui.stats().draw_calls >= 3);
    }

    #[test]
    fn ui_gpu_font_override_is_rejected_atomically_and_default_bytes_restore() {
        let Some((device, queue, summary)) = test_gpu() else {
            eprintln!("skipping ui_gpu_font_override_is_rejected_atomically_and_default_bytes_restore: no GPU adapter");
            return;
        };
        eprintln!("ui font test GPU: {summary}");

        const WIDTH: u32 = 240;
        const HEIGHT: u32 = 80;
        let blocks = BlockTextureArray::new(&device, &queue);
        let mut ui = UiRenderer::new_with_ui_font(&device, &queue, &blocks, wgpu::TextureFormat::Rgba8UnormSrgb, None);
        assert_eq!(ui.ui_font_name(), default_ui_font_name());

        let mut frame = UiFrame::new(WIDTH as f32, HEIGHT as f32);
        frame.text(8.0, 8.0, 2.0, [1.0; 4], "Monocraft 0123");
        frame.title_text(8.0, 40.0, 2.0, [0.8, 0.9, 1.0, 1.0], "TITLE 8x8");

        let baseline = render_ui_frame(&device, &queue, &mut ui, WIDTH, HEIGHT, &frame);
        // Both atlases must actually draw: count non-background pixels in the
        // UI-font row and the bitmap-title row.
        let background = pixel(&baseline, WIDTH, 0, 0);
        let count = |x0: u32, x1: u32, y0: u32, y1: u32| {
            let mut count = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    if differs(pixel(&baseline, WIDTH, x, y), background, 3) {
                        count += 1;
                    }
                }
            }
            count
        };
        let ui_font_pixels = count(8, 180, 8, 32);
        let title_pixels = count(8, 130, 40, 56);
        eprintln!("ui font override: ui={ui_font_pixels} title={title_pixels}");
        assert!(ui_font_pixels > 40, "UI font drew only {ui_font_pixels} pixels");
        assert!(title_pixels > 20, "title font drew only {title_pixels} pixels");

        // Invalid bytes must keep the old atlas and name.
        let error = ui
            .set_ui_font(Some(b"definitely not a TTF"))
            .expect_err("invalid font bytes must be rejected");
        assert!(
            matches!(error, UiFontError::InvalidFont(_)),
            "unexpected error: {error}"
        );
        assert_eq!(ui.ui_font_name(), default_ui_font_name());
        let after_invalid = render_ui_frame(&device, &queue, &mut ui, WIDTH, HEIGHT, &frame);
        assert_eq!(
            baseline, after_invalid,
            "a rejected font override must not change the rendered frame"
        );

        // Explicit default bytes are accepted and reproduce the same image.
        ui.set_ui_font(Some(default_ui_font()))
            .expect("default font bytes are valid");
        assert_eq!(ui.ui_font_name(), default_ui_font_name());
        let after_default = render_ui_frame(&device, &queue, &mut ui, WIDTH, HEIGHT, &frame);
        assert_eq!(
            baseline, after_default,
            "reloading the default font must render the same frame"
        );

        // `None` restores the default and remains renderable.
        ui.set_ui_font(None).expect("default font is always available");
        assert_eq!(ui.ui_font_name(), default_ui_font_name());
        let after_none = render_ui_frame(&device, &queue, &mut ui, WIDTH, HEIGHT, &frame);
        assert_eq!(baseline, after_none, "None must restore the embedded font");
    }
}
