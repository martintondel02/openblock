# Client UI font (1.0.2)

> Status: implemented in 1.0.2 · Applies to `crates/client`, `ob_render::ui`
> and the `ui_font_path` field in `ob_core::Settings`.

OpenBlock renders two text layers:

| Call | Font | Use |
| --- | --- | --- |
| `UiFrame::text` / `text_centered` | embedded **Monocraft** TrueType atlas, or a user-supplied Minecraftia TTF/OTF | buttons, rows, labels, HUD, chat, F3, tooltips, hints, footers |
| `UiFrame::title_text` / `title_text_centered` | original 8×8 bitmap font | the `OPENBLOCK` logo, screen/panel headings and the death title |

Both fonts share the original layout metrics: one printable ASCII character
advances 8 logical pixels per scale step and one line is 8 pixels high. No
screen needed repositioning when the default font changed.

## How the dual font works

- `ob_render` embeds **Monocraft 4.2** (`crates/render/assets/ui-font/Monocraft.ttf`,
  SIL Open Font License 1.1). At renderer construction it rasterises printable
  ASCII into the same 16×8-cell, 128×64-texel RGBA atlas layout used by the
  bitmap font. Cap height is normalised to 7 px and coverage is thresholded to
  a crisp binary mask, so the result keeps the pixel-art look at every GUI
  scale.
- `UiFrame::text` samples the UI-font atlas and `UiFrame::title_text` samples
  the 8×8 bitmap atlas. The renderer therefore has four texture bind groups
  (solid, UI font, bitmap title font, block icons); a screen uses at most four
  draw calls.
- `UiRenderer::set_ui_font(Some(bytes))` replaces the UI atlas at runtime;
  `set_ui_font(None)` restores Monocraft. Invalid bytes leave the previous font
  active and report a `UiFontError`.
- The title/heading call sites use the bitmap font: title-screen logo and
  “EARLY ACCESS” subtitle, New World / Load World / Multiplayer panel titles,
  Delete-world dialog, Settings, Inventory, Crafting, Furnace, Chest, the
  multiplayer Connecting/failure/player-list titles, and “You died”. Everything
  else uses the UI font.

## Using exact Minecraftia

Minecraftia is **not bundled** because its licence is personal-use only. If you
own a copy (for example for personal Minecraft-style projects), either place it
in the data-directory `fonts` folder or pass it explicitly:

```sh
# Data directories, in resolution order:
#   $OPENBLOCK_DATA_DIR
#   $XDG_DATA_HOME/openblock
#   ~/.local/share/openblock
#   ./run/data (fallback)
mkdir -p "${XDG_DATA_HOME:-$HOME/.local/share}/openblock/fonts"
cp /path/to/Minecraftia.ttf "${XDG_DATA_HOME:-$HOME/.local/share}/openblock/fonts/"

# Start normally; the folder is discovered automatically.
openblock

# Or select one file explicitly. This persists for future runs.
openblock --ui-font /path/to/Minecraftia.ttf

# Forget an explicit selection and use the bundled Monocraft again.
openblock --default-ui-font
```

Verify the active font in **Settings → Display → UI font** (name and source) or
on the right-hand F3 column (`UI font: Monocraft` / `UI font: custom`). The
Display tab also has **Reset UI font** (clear the saved path, restore Monocraft,
persist and apply immediately) and **Reload UI font** (re-scan the data/system
font folders after installing a file, without restarting). Both actions work
from the title-screen Settings panel because the front-end owns a renderer.

The `.ttf`/`.otf` extension is case-insensitive. In `<data>/fonts/`, a file whose
name contains `minecraftia` is preferred, then one containing `minecraft`, then
the first file in sorted order. A system-wide `Minecraftia` font is also found
in the usual per-user/system font directories (Linux: `$XDG_DATA_HOME/fonts`,
`~/.local/share/fonts`, `~/.fonts`, `/usr/share/fonts`, `/usr/local/share/fonts`;
macOS: `~/Library/Fonts`, `/Library/Fonts`, `/System/Library/Fonts`; Windows:
`%WINDIR%\Fonts`), searching a few directory levels only.

### Resolution order

1. `--ui-font PATH` (CLI override, persisted like `--texture-pack`),
2. a readable `settings.ui_font_path`,
3. auto-discovery in `<data-dir>/fonts/`,
4. a system font file whose name contains `minecraftia`,
5. the embedded Monocraft.

`--default-ui-font` skips all overrides and clears the saved path.

## Loading and fallback behaviour

- Unreadable, missing, too-large (>32 MiB) or invalid font files never stop
  startup. The client logs a warning and keeps the embedded Monocraft font.
- A stale `settings.ui_font_path` logs one warning and continues with the data
  and system font search; if that finds nothing, Monocraft is used.
- An unreadable explicit `--ui-font` path falls straight back to Monocraft for
  that run. The path is still stored so it can be fixed with
  `--default-ui-font`; a later launch will warn again until it is.
- The choice is persisted when the CLI flag is present and applied live from
  the settings screen. The title-screen Settings panel persists and applies
  changes through the front-end renderer; a world/network runtime created later
  reads the same saved setting.

## Licensing

- **Monocraft 4.2** is bundled under the **SIL Open Font License 1.1**; the
  licence text accompanies the asset in `assets/ui-font/OFL.txt`.
- **Minecraftia** is not bundled or redistributed by OpenBlock. Its licence is
  personal-use only, so the screenshots in this repository use the bundled
  Monocraft. Users who want the exact Minecraftia glyphs must obtain the font
  themselves and keep it in their own data directory.

## Verification

```sh
./dev-cargo test -p ob_client -p ob_core
./dev-cargo clippy -p ob_client -p ob_core --all-targets -- -D warnings
```

The client tests cover CLI parsing/persistence, resolution precedence,
`minecraftia`/`minecraft` preference, unreadable/invalid file fallback, the
active-source settings row and reset/reload actions. Renderer tests in
`ob_render` cover the UI versus bitmap atlas variants. The 1.0.2 capture set and
exact commands are in [docs/img/README.md](img/README.md#v102-screenshots-minecraft-style-ui-font).
