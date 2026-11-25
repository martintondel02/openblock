# Client GUI scale (1.0.1)

> Status: implemented in 1.0.1 · Last updated: 2026-11-25
> Applies to: `crates/client`, the `ui_scale_percent` field in
> `ob_core::Settings`, and the settings/connect layouts described below.

## Setting

`Settings::ui_scale_percent` is persisted in `config.json` as an integer:

| Value | Meaning |
| --- | --- |
| `0` | `Auto` |
| `75` / `100` / `125` / `150` | explicit GUI scale |

Missing values in older configs default to `0` (Auto). On load,
`ob_core::config::normalise_ui_scale_percent` keeps Auto and rounds any other
value to the nearest supported choice after clamping to 75–150 (for example
`110 -> 100`, `999 -> 150`). The Display tab exposes the ordered choices
Auto, 75, 100, 125 and 150 with the normal `<`/`>` arrows; changes apply on the
next frame and are saved by the existing settings persistence path.

`Auto` keeps the 1280x720 and 960x540 release layouts at 100%, selects 125% on
1440p-class displays (`physical height >= 1200`) and 150% on 4K-class displays
(`>= 1800`). `--ui-scale auto|75|100|125|150` overrides the value for one run
(used by the offscreen screenshot matrix); it does not rewrite the persisted
setting.

## Transform and input mapping

The UI is laid out in **logical UI pixels**. `ui_scale::ui_screen(physical,
setting)` returns `physical / scale_factor`; every screen, panel, glyph, toast,
name tag and container window is built at that logical size. The frame is then
created with the logical size, so `UiFrame`'s single normalised-device-
coordinate conversion applies the scale factor to every vertex at once. The
world renderer, surface size and resize handling remain in physical pixels.

Pointer coordinates arrive in physical pixels and are divided by the same
factor before any hover/hit test (`ui_scale::physical_to_ui_point`). The pure
helpers are covered by:

- `ui_scale::tests::pointer_mapping_round_trips_at_every_scale`
- `ui_scale::tests::a_click_at_a_logical_target_hits_after_physical_conversion`
- `ui_state::tests::gui_scale_clicks_convert_physical_pointer_to_the_same_logical_rect`

Changing the scale while the settings screen is open rescales the stored cursor
with `rescale_cursor`, so the pointer does not jump to a different widget.

## Layout guarantees

The 100% layouts are unchanged. `ui_state::settings_metrics` switches to a
compact metric set when the logical panel height is below 420 px (for example
960x540 at 125%, which is 768x432 logical, and at 150%, which is 640x360
logical). In compact mode the Display tab still shows all seven rows, the tab
strip, both footer buttons and two hint lines.

Other small-logical-target handling:

- **Death screen**: below 500 logical pixels the title/cause/note block, two
  34 px buttons and the key hint move up; the hint is clamped above the hearts.
- **Front-end forms** (New World, Load World, Multiplayer): below 480 logical
  pixels fields are 32 px high, labels sit closer, the footer hint moves outside
  the panel, and text is width-clamped so it cannot cross the panel edge.
- **Creative inventory**: below 480 logical pixels it reserves the gameplay
  hotbar band and uses a 40 px title/92 px footer, so the hotbar is not covered.
- **Survival inventory**: the panel contains its own hotbar row; the duplicate
  gameplay hotbar/vitals are not drawn over it.
- **F3 overlay**: below 420 logical pixels it switches to a compact 10-line
  left column (identity, FPS, position, vitals, world/UI scale, target) that
  clears the hotbar and footer; the regular full overlay is used at and above
  that height.
- **Connect/loading/failure**: the panel height is content-driven (220 px while
  contacting, 300 px with world/players/progress, 240 px on failure), endpoint
  and player name are shown on separate lines, stale world/roster/progress
  fields are hidden in error mode, and the Reconnect/Restart and Quit buttons
  support keyboard focus as well as mouse hover/click.

The layout matrix test
`ui_state::tests::gui_scale_matrix_keeps_every_screen_inside_the_logical_target`
covers title/menu, all settings tabs, HUD hotbar/vitals/armor, death, connect,
inventory, crafting, furnace and chest layouts at 75/100/125/150% on both
1280x720 and 960x540 physical targets.

## Limitations

- The bitmap font is rendered at logical resolution and scaled by the GPU. At
  non-integer scales (125/150%) individual glyph pixel columns can be one
  physical pixel different in width; this is the same trade-off as a
  framebuffer-scaled GUI. Integer-only scaling is a post-1.0.1 option.
- The compact F3 set intentionally omits some diagnostics; the full set is
  still available at 1280x720/960x540 with 100% or on larger physical targets.
- At 960x540 with 150% only two recent servers fit on the multiplayer form.
  The full five-entry list is available at 100–125% or on a taller window.
- Localization, colour-blind presets and per-widget text scaling remain
  post-1.0.1.

## Verification

```sh
./dev-cargo test -p ob_client --lib
./dev-cargo test -p ob_core
./dev-cargo clippy -p ob_client -p ob_core --all-targets -- -D warnings
```

Screenshot set and exact commands: [docs/img/README.md](img/README.md#v101-screenshots-gui-scale-and-connect-polish).
