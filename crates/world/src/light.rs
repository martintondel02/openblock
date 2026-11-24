//! Packed light storage: 4 bits of sky light and 4 bits per RGB channel.
//!
//! Layout (DEC-005): one `u16` per block,
//! `[sky:4][red:4][green:4][blue:4]`. A section whose light is uniform stores
//! only the uniform word, so fully enclosed (all-zero) and fully open (sky 15)
//! sections cost no heap allocation at all.

use ob_math::LocalPos;
use ob_math::SECTION_SIZE;

/// Number of blocks in a section.
pub const BLOCK_COUNT: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;

/// Light container for one 16³ section.
///
/// Most sections have a single light value everywhere: underground sections
/// are uniformly dark, open-air sections are uniformly sky-lit, and fully
/// covered sections are uniformly dark. Representing those as `Uniform`
/// avoids allocating an 8 KiB dense array per section. The first write that
/// differs from the uniform value promotes the container to `Dense`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Light {
    /// Every block has this packed light word.
    Uniform(u16),
    /// Per-block packed light words.
    Dense(Box<[u16; BLOCK_COUNT]>),
}

impl Default for Light {
    fn default() -> Self {
        Self::Uniform(0)
    }
}

impl Light {
    /// Creates a light container whose sky-light value is uniform.
    ///
    /// This never allocates; `sky` is truncated to its low four bits.
    pub fn uniform_sky(sky: u8) -> Self {
        Self::Uniform((u16::from(sky & 0xF)) << 12)
    }

    /// Creates a container with one packed word everywhere.
    pub fn from_uniform(value: u16) -> Self {
        Self::Uniform(value)
    }

    /// The packed word shared by every block, if the container is uniform.
    pub fn uniform_value(&self) -> Option<u16> {
        match self {
            Self::Uniform(value) => Some(*value),
            Self::Dense(_) => None,
        }
    }

    /// Builds a container from a per-position function.
    ///
    /// The result collapses to [`Light::Uniform`] when every visited position
    /// produces the same word, so callers do not need to special-case uniform
    /// sections. The temporary array lives on the stack to keep uniform
    /// sections allocation-free.
    pub fn from_fn(mut value_at: impl FnMut(LocalPos) -> u16) -> Self {
        let mut data = [0u16; BLOCK_COUNT];
        for (index, slot) in data.iter_mut().enumerate() {
            *slot = value_at(LocalPos::from_index(index));
        }
        let first = data[0];
        if data.iter().all(|&word| word == first) {
            Self::Uniform(first)
        } else {
            Self::Dense(Box::new(data))
        }
    }

    /// Returns the dense array, promoting a uniform container to a dense one.
    fn ensure_dense(&mut self) -> &mut [u16; BLOCK_COUNT] {
        match self {
            Self::Uniform(value) => {
                let data = Box::new([*value; BLOCK_COUNT]);
                *self = Self::Dense(data);
                match self {
                    Self::Dense(data) => data,
                    Self::Uniform(_) => unreachable!("just promoted to dense"),
                }
            }
            Self::Dense(data) => data,
        }
    }

    fn get(&self, pos: LocalPos) -> u16 {
        match self {
            Self::Uniform(value) => *value,
            Self::Dense(data) => data[pos.index()],
        }
    }

    fn set(&mut self, pos: LocalPos, value: u16) {
        match self {
            Self::Uniform(current) if *current == value => {}
            Self::Uniform(_) => {
                let index = pos.index();
                let slot = &mut self.ensure_dense()[index];
                *slot = value;
            }
            Self::Dense(data) => data[pos.index()] = value,
        }
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
        match self {
            Self::Uniform(_) => 0,
            Self::Dense(data) => std::mem::size_of_val(&**data),
        }
    }

    /// Whether the container is the uniform-zero fast path.
    pub fn is_uniform_zero(&self) -> bool {
        matches!(self, Self::Uniform(0))
    }

    /// Raw packed words for persistence (`None` for a uniform container).
    pub fn raw(&self) -> Option<&[u16; BLOCK_COUNT]> {
        match self {
            Self::Uniform(_) => None,
            Self::Dense(data) => Some(data),
        }
    }

    /// Rebuilds a container from raw words.
    ///
    /// `None` is the legacy uniform-zero encoding; a dense array is preserved
    /// as-is, even if all of its words happen to be equal.
    pub fn from_raw(data: Option<Box<[u16; BLOCK_COUNT]>>) -> Self {
        match data {
            Some(data) => Self::Dense(data),
            None => Self::Uniform(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(x: u8, y: u8, z: u8) -> LocalPos {
        LocalPos { x, y, z }
    }

    #[test]
    fn uniform_light_costs_nothing_and_same_write_stays_uniform() {
        let mut light = Light::uniform_sky(15);
        assert_eq!(light.uniform_value(), Some(15 << 12));
        assert_eq!(light.heap_bytes(), 0);
        assert!(light.raw().is_none());

        light.set_sky(pos(1, 2, 3), 15);
        assert_eq!(light.uniform_value(), Some(15 << 12));
        assert_eq!(light.heap_bytes(), 0);

        // Rewriting block light to zero is also a no-op on uniform sky light.
        light.set_block(pos(4, 5, 6), 0, 0, 0);
        assert_eq!(light.uniform_value(), Some(15 << 12));
        assert_eq!(light.heap_bytes(), 0);
    }

    #[test]
    fn different_value_promotes_to_dense_and_round_trips() {
        let mut light = Light::uniform_sky(15);
        light.set_sky(pos(1, 2, 3), 7);

        assert_eq!(light.uniform_value(), None);
        let dense = light.raw().expect("promoted light is dense");
        assert_eq!(light.heap_bytes(), BLOCK_COUNT * 2);
        assert_eq!(dense[pos(1, 2, 3).index()], 7 << 12);
        assert_eq!(light.sky(pos(1, 2, 3)), 7);
        assert_eq!(light.sky(pos(0, 0, 0)), 15);

        light.set_block(pos(1, 2, 3), 4, 5, 6);
        assert_eq!(light.block_rgb(pos(1, 2, 3)), (4, 5, 6));
        assert_eq!(light.sky(pos(1, 2, 3)), 7);

        let restored = Light::from_raw(light.raw().map(|data| Box::new(*data)));
        assert_eq!(restored, light);
        assert_eq!(restored.block_rgb(pos(1, 2, 3)), (4, 5, 6));
        assert_eq!(restored.sky(pos(1, 2, 3)), 7);
    }

    #[test]
    fn from_fn_collapses_uniform_values_only() {
        let uniform = Light::from_fn(|_| 0x1234);
        assert_eq!(uniform.uniform_value(), Some(0x1234));
        assert_eq!(uniform.heap_bytes(), 0);

        let zero = Light::from_fn(|_| 0);
        assert!(zero.is_uniform_zero());
        assert_eq!(zero.uniform_value(), Some(0));

        let varying = LocalPos { x: 3, y: 4, z: 5 };
        let dense = Light::from_fn(|p| if p == varying { 0xF000 } else { 0 });
        assert_eq!(dense.uniform_value(), None);
        assert_eq!(dense.heap_bytes(), BLOCK_COUNT * 2);
        assert_eq!(dense.sky(varying), 15);
        assert_eq!(dense.sky(pos(0, 0, 0)), 0);
    }

    #[test]
    fn from_raw_none_is_uniform_zero_and_some_stays_dense() {
        let empty = Light::from_raw(None);
        assert!(empty.is_uniform_zero());
        assert_eq!(empty.heap_bytes(), 0);

        let dense = Light::from_raw(Some(Box::new([0u16; BLOCK_COUNT])));
        assert_eq!(dense.uniform_value(), None);
        assert_eq!(dense.heap_bytes(), BLOCK_COUNT * 2);
    }
}
