//! Packed light storage: 4 bits of sky light and 4 bits per RGB channel.
//!
//! Layout (DEC-005): one `u16` per block,
//! `[sky:4][red:4][green:4][blue:4]`. A section whose light is all zero stores
//! no array at all (the `uniform_zero` fast path).

use ob_math::LocalPos;
use ob_math::SECTION_SIZE;

/// Number of blocks in a section.
/// Number of blocks in a section.
pub const BLOCK_COUNT: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;

/// Light container for one 16³ section.
#[derive(Debug, Clone, Default)]
pub struct Light {
    /// `None` means every channel is zero (uniform dark), which is the
    /// common case for fully enclosed or underground sections.
    data: Option<Box<[u16; BLOCK_COUNT]>>,
}

impl Light {
    /// Creates a light container whose sky-light value is uniform.
    pub fn uniform_sky(sky: u8) -> Self {
        if sky == 0 {
            return Self::default();
        }
        let packed = (u16::from(sky & 0xF)) << 12;
        Self {
            data: Some(Box::new([packed; BLOCK_COUNT])),
        }
    }

    fn ensure(&mut self) -> &mut [u16; BLOCK_COUNT] {
        self.data.get_or_insert_with(|| Box::new([0u16; BLOCK_COUNT]))
    }

    fn get(&self, pos: LocalPos) -> u16 {
        self.data.as_ref().map_or(0, |d| d[pos.index()])
    }

    fn set(&mut self, pos: LocalPos, value: u16) {
        let index = pos.index();
        let slot = &mut self.ensure()[index];
        *slot = value;
    }

    /// Sky light at a local position (0..=15).
    pub fn sky(&self, pos: LocalPos) -> u8 {
        (self.get(pos) >> 12) as u8
    }

    /// Sets sky light at a local position.
    pub fn set_sky(&mut self, pos: LocalPos, value: u8) {
        let current = self.get(pos);
        self.set(pos, (current & 0x0FFF) | (u16::from(value & 0xF) << 12));
    }

    /// Block light at a local position as RGB nibbles.
    pub fn block_rgb(&self, pos: LocalPos) -> (u8, u8, u8) {
        let v = self.get(pos);
        (((v >> 8) & 0xF) as u8, ((v >> 4) & 0xF) as u8, (v & 0xF) as u8)
    }

    /// Sets block light at a local position.
    pub fn set_block(&mut self, pos: LocalPos, r: u8, g: u8, b: u8) {
        let current = self.get(pos) & 0xF000;
        let packed = current | (u16::from(r & 0xF) << 8) | (u16::from(g & 0xF) << 4) | u16::from(b & 0xF);
        self.set(pos, packed);
    }

    /// Approximate heap bytes used by this container.
    pub fn heap_bytes(&self) -> usize {
        self.data.as_ref().map_or(0, |d| d.len() * 2)
    }

    /// Whether the container is the uniform-zero fast path.
    pub fn is_uniform_zero(&self) -> bool {
        self.data.is_none()
    }

    /// Raw packed words for persistence (`None` for the uniform-zero form).
    pub fn raw(&self) -> Option<&[u16; BLOCK_COUNT]> {
        self.data.as_deref()
    }

    /// Rebuilds a container from raw words.
    pub fn from_raw(data: Option<Box<[u16; BLOCK_COUNT]>>) -> Self {
        Self { data }
    }
}
