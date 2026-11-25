//! Single-player 1.2 light integration for the client mesh scheduler.
//!
//! The host owns lighting and exposes dirty sections exactly once through
//! `Host::take_dirty_sections`; this test proves the client contract without a
//! window: a torch edit marks the touched section (and a lit section-border
//! neighbour), the dirty set drains once, and the resulting worker mesh carries
//! non-zero block light.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_client::mesh_scheduler::MeshScheduler;
use ob_content::{BlockId, Registry, textures};
use ob_math::{BlockPos, SectionPos};
use ob_render::SectionSnapshot;
use ob_server::{Host, HostConfig};

fn open_host() -> (std::path::PathBuf, Host) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("openblock-client-light-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temporary data directory");
    let registry = Arc::new(Registry::base());
    let host = Host::open(
        registry,
        HostConfig {
            seed: 42,
            world_name: "client-light".to_string(),
            data_dir: Some(dir.clone()),
            render_distance_chunks: 3,
            generate_budget: 16,
        },
    )
    .expect("open host");
    (dir, host)
}

fn wait_for_mesh(scheduler: &mut MeshScheduler, timeout: Duration) -> Option<ob_client::mesh_scheduler::MeshResult> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(result) = scheduler.try_recv() {
            return Some(result);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn torch_dirty_sections_drain_once_and_remesh_with_block_light() {
    let (dir, mut host) = open_host();
    for _ in 0..256 {
        if host.ensure_streaming(32) == 0 {
            break;
        }
    }
    let registry = Arc::clone(host.registry());

    // Drain the streaming dirty set so the assertion below is about the torch.
    while !host.take_dirty_sections().is_empty() {}

    let stone = registry.id("openblock:stone").expect("stone");
    let torch = registry.id("openblock:torch").expect("torch");
    let spawn = host.player().pos;
    // Local x=15 is the section border: the torch light must dirty both the
    // touched section and the +X neighbour whose snapshot border light changed.
    let base = BlockPos::new(
        spawn.x.floor() as i32 + 15,
        spawn.y.floor() as i32,
        spawn.z.floor() as i32,
    );
    for dy in -1..=2 {
        for dx in -1..=1 {
            for dz in -1..=1 {
                assert!(host.world_mut().set_block(base.offset(dx, dy, dz), BlockId::AIR));
            }
        }
    }
    assert!(host.world_mut().set_block(base.offset(0, -1, 0), stone));
    assert!(host.world_mut().set_block(base, torch));
    assert_eq!(host.world().get_block(base), torch);
    assert_eq!(host.world().light_sample(base).block_level(), 14);
    assert!(host.world().light_sample(base.offset(1, 0, 0)).block_level() >= 13);

    let dirty = host.take_dirty_sections();
    let section = base.section();
    assert!(
        dirty.contains(&section),
        "the torch section {section:?} must be dirty: {dirty:?}"
    );
    let neighbour = SectionPos::new(section.x + 1, section.y, section.z);
    assert!(
        dirty.contains(&neighbour),
        "the lit border neighbour {neighbour:?} must be dirty: {dirty:?}"
    );
    assert!(
        host.take_dirty_sections().is_empty(),
        "the client consumes the host dirty set exactly once"
    );

    let mut scheduler = MeshScheduler::new(2, Arc::clone(&registry));
    assert!(scheduler.submit(host.world(), section));
    let result = wait_for_mesh(&mut scheduler, Duration::from_secs(10)).expect("torch section mesh");
    assert_eq!(result.pos, section);
    assert!(
        result
            .mesh
            .vertices
            .iter()
            .filter(|vertex| vertex.layer == u32::from(textures::TORCH.0))
            .any(|vertex| vertex.block_byte() > 0),
        "torch cross vertices must carry host-computed block light"
    );
    assert!(
        result
            .mesh
            .vertices
            .iter()
            .filter(|vertex| vertex.layer == u32::from(textures::STONE.0))
            .any(|vertex| vertex.block_byte() > 0),
        "the stone floor face next to the torch must be lit"
    );
    // The snapshot path used by the worker sees the same light words as the
    // live world; no client relight was needed.
    let snapshot = SectionSnapshot::capture(host.world(), section);
    assert_eq!(
        snapshot.light_word(base.local().x as i32, base.local().y as i32, base.local().z as i32),
        host.world().light_word(base)
    );

    let _ = std::fs::remove_dir_all(dir);
}
