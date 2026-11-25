//! Server-authoritative right-click routing for the 1.2 gameplay blocks.
//!
//! The client never decides whether tilling, planting, harvesting or sleeping
//! succeeds: it only chooses between a **use intent** and the ordinary
//! eat/place path. Both runtimes (single-player `Host::use_block_at` and the
//! multiplayer `UseBlock`/`SleepRequest` messages) then validate reach, held
//! item, light, support and sleep state on the authoritative side.
//!
//! Routing is content-driven and deliberately conservative:
//!
//! * a bed, a wheat block, or a hoe on grass/dirt is always a use intent;
//! * seeds on farmland are a use intent, while a placeable stack on farmland
//!   still places a block above it;
//! * grass/dirt/farmland with no matching tool/item falls through as a use
//!   intent only when the held stack cannot be placed, so the server's
//!   `needs_hoe`/`needs_seeds` hint can reach the HUD without blocking normal
//!   building;
//! * containers keep their existing client-side screen dispatch.
//!
//! The mapping is pure so the interaction matrix can be unit-tested without a
//! window, a host or a socket.

use ob_content::{BlockId, Registry, ToolKind};
use ob_server::ItemStack;

/// What a right-click should attempt for the targeted block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseRoute {
    /// Open the crafting screen (single-player table, or the existing hint in
    /// multiplayer).
    CraftingTable,
    /// Send a container-open intent for a furnace or chest.
    Container,
    /// Send the server-authoritative use intent.
    UseBlock,
    /// Continue with the existing eat/place path.
    Place,
}

/// Chooses the right-click route for `target` with the selected stack.
///
/// `creative` keeps the single-player creative palette usable: the palette
/// only holds blocks, so grass/dirt/farmland are placement targets there
/// instead of turning every right-click into a doomed `needs_hoe` request.
pub fn route_use(registry: &Registry, target: BlockId, selected: Option<ItemStack>, creative: bool) -> UseRoute {
    if target == BlockId::AIR {
        return UseRoute::Place;
    }
    let block = registry.block(target);
    match block.id {
        "openblock:crafting_table" => return UseRoute::CraftingTable,
        "openblock:furnace" | "openblock:chest" => return UseRoute::Container,
        "openblock:bed_foot" | "openblock:bed_head" => return UseRoute::UseBlock,
        // Every wheat stage is harvested by a use intent; the block is not a
        // placement target because it is a non-solid cross.
        id if id.starts_with("openblock:wheat_") => return UseRoute::UseBlock,
        _ => {}
    }

    // A hostile/corrupt stack id must not index the item table.
    let selected = selected.filter(|stack| stack.count > 0 && (stack.item.0 as usize) < registry.item_len());
    let can_place = selected.is_some_and(|stack| registry.item(stack.item).block.is_some());
    let holds_hoe = selected.is_some_and(|stack| {
        registry
            .item(stack.item)
            .tool
            .is_some_and(|tool| tool.kind == ToolKind::Hoe)
    });
    let holds_seeds = registry
        .item_id("openblock:seeds")
        .is_some_and(|seeds| selected.is_some_and(|stack| stack.count > 0 && stack.item == seeds));

    if creative {
        return UseRoute::Place;
    }

    match block.id {
        "openblock:grass" | "openblock:dirt" => {
            if holds_hoe {
                UseRoute::UseBlock
            } else if can_place {
                UseRoute::Place
            } else {
                // Empty-handed or holding seeds/food: surface the server's
                // `needs_hoe` hint instead of silently doing nothing.
                UseRoute::UseBlock
            }
        }
        "openblock:farmland_dry" | "openblock:farmland_wet" => {
            if holds_seeds {
                UseRoute::UseBlock
            } else if can_place {
                UseRoute::Place
            } else {
                UseRoute::UseBlock
            }
        }
        _ => UseRoute::Place,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::ItemId;

    fn registry() -> Registry {
        Registry::base()
    }

    fn stack(registry: &Registry, name: &str, count: u8) -> Option<ItemStack> {
        registry.item_id(name).map(|item| ItemStack {
            item,
            count,
            durability: None,
        })
    }

    fn block(registry: &Registry, name: &str) -> BlockId {
        registry.id(name).unwrap_or_else(|| panic!("missing block {name}"))
    }

    #[test]
    fn hoe_on_grass_and_dirt_is_a_use_intent() {
        let registry = registry();
        for name in ["openblock:grass", "openblock:dirt"] {
            let target = block(&registry, name);
            for hoe in ["openblock:wood_hoe", "openblock:stone_hoe", "openblock:iron_hoe"] {
                assert_eq!(
                    route_use(&registry, target, stack(&registry, hoe, 1), false),
                    UseRoute::UseBlock,
                    "{hoe} on {name}"
                );
            }
            // A placeable block on grass still builds instead of trying to
            // till without a hoe.
            assert_eq!(
                route_use(&registry, target, stack(&registry, "openblock:cobblestone", 8), false),
                UseRoute::Place,
                "building on {name}"
            );
            // Empty hand can surface the authoritative needs_hoe hint.
            assert_eq!(
                route_use(&registry, target, None, false),
                UseRoute::UseBlock,
                "empty hand on {name}"
            );
        }
    }

    #[test]
    fn seeds_on_farmland_are_a_use_intent_but_blocks_still_place() {
        let registry = registry();
        for farmland in ["openblock:farmland_dry", "openblock:farmland_wet"] {
            let target = block(&registry, farmland);
            assert_eq!(
                route_use(&registry, target, stack(&registry, "openblock:seeds", 3), false),
                UseRoute::UseBlock,
                "seeds on {farmland}"
            );
            assert_eq!(
                route_use(&registry, target, stack(&registry, "openblock:cobblestone", 1), false),
                UseRoute::Place,
                "placing on {farmland}"
            );
            assert_eq!(route_use(&registry, target, None, false), UseRoute::UseBlock);
        }
    }

    #[test]
    fn wheat_beds_and_containers_route_before_placement() {
        let registry = registry();
        for wheat in [
            "openblock:wheat_0",
            "openblock:wheat_1",
            "openblock:wheat_2",
            "openblock:wheat_3",
        ] {
            for held in [
                None,
                stack(&registry, "openblock:cobblestone", 1),
                stack(&registry, "openblock:seeds", 1),
            ] {
                assert_eq!(
                    route_use(&registry, block(&registry, wheat), held, false),
                    UseRoute::UseBlock,
                    "{wheat} must harvest even while holding a block"
                );
            }
        }
        let bed = stack(&registry, "openblock:bed_foot", 1);
        assert_eq!(
            route_use(&registry, block(&registry, "openblock:bed_foot"), bed, false),
            UseRoute::UseBlock
        );
        assert_eq!(
            route_use(&registry, block(&registry, "openblock:bed_head"), bed, false),
            UseRoute::UseBlock
        );
        assert_eq!(
            route_use(&registry, block(&registry, "openblock:furnace"), None, false),
            UseRoute::Container
        );
        assert_eq!(
            route_use(&registry, block(&registry, "openblock:chest"), None, false),
            UseRoute::Container
        );
        assert_eq!(
            route_use(&registry, block(&registry, "openblock:crafting_table"), None, false),
            UseRoute::CraftingTable
        );
        assert_eq!(
            route_use(
                &registry,
                block(&registry, "openblock:torch"),
                stack(&registry, "openblock:torch", 2),
                false
            ),
            UseRoute::Place
        );
        assert_eq!(
            route_use(&registry, block(&registry, "openblock:stone"), None, false),
            UseRoute::Place
        );
    }

    #[test]
    fn creative_palette_keeps_grass_and_farmland_placeable() {
        let registry = registry();
        for name in ["openblock:grass", "openblock:dirt", "openblock:farmland_dry"] {
            assert_eq!(
                route_use(
                    &registry,
                    block(&registry, name),
                    stack(&registry, "openblock:wood_hoe", 1),
                    true
                ),
                UseRoute::Place,
                "creative on {name} keeps the palette placement path"
            );
        }
    }

    #[test]
    fn every_new_block_routes_without_panicking_for_unknown_items() {
        let registry = registry();
        let unknown = ItemStack {
            item: ItemId(u16::MAX),
            count: 1,
            durability: None,
        };
        for (id, _) in registry.iter() {
            let _ = route_use(&registry, id, Some(unknown), false);
            let _ = route_use(&registry, id, None, true);
        }
    }
}
