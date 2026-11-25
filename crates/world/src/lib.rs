//! The voxel world model: paletted sections, chunk columns, lighting and queries.
//!
//! Coordinate types come from [`ob_math`]. The world stores 16³ sections
//! ([`section::Section`]) organised into 16×16 chunk columns ([`chunk::ChunkColumn`]).
//! Mutations mark affected sections dirty; the client turns dirty sections into
//! meshes. The dedicated server uses the same code with no renderer attached.

pub mod chunk;
pub mod light;
pub mod save;
pub mod section;
pub mod world;

pub use chunk::ChunkColumn;
pub use light::Light;
pub use save::{MAX_SAVE_FILE_BYTES, SaveError, from_bytes, read_save_file, registry_fingerprint, to_bytes};
pub use section::{Section, SectionStorage};
pub use world::{RayHit, World};
