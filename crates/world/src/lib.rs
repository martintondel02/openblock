//! The voxel world model: paletted sections, chunk columns, lighting and queries.
//!
//! Coordinate types come from [`ob_math`]. The world stores 16³ sections
//! ([`section::Section`]) organised into 16×16 chunk columns ([`chunk::ChunkColumn`]).
//! Mutations mark affected sections dirty; the client turns dirty sections into
//! meshes. The dedicated server uses the same code with no renderer attached.

pub mod chunk;
pub mod light;
pub mod lighting;
pub mod save;
pub mod section;
pub mod world;

pub use chunk::ChunkColumn;
pub use light::{Light, LightSample};
pub use save::{
    LIGHT_VERSION, LightMigration, MAX_SAVE_FILE_BYTES, MAX_SAVE_PAYLOAD_BYTES, SaveError, WORLD_META_FILE, WorldMeta,
    WorldSnapshot, from_bytes, load_world_dir, load_world_dir_with_report, read_save_file, read_world_meta,
    registry_fingerprint, save_world_dir, to_bytes, to_bytes_checked, write_world_meta,
};
pub use section::{Section, SectionStorage};
pub use world::{RayHit, World};
