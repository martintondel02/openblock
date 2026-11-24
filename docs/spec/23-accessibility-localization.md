# 23 — Accessibility and Localization

> Status: normative - Last updated: 2026-11-24
> Depends on: [DEC-009](../decisions.md), [DEC-030](../decisions.md), [DEC-032](../decisions.md), [DEC-034](../decisions.md), [DEC-044](../decisions.md), [DEC-050](../decisions.md), [conventions](00-conventions.md), [technical architecture](01-technical-architecture.md), [rendering](03-rendering.md), [shaders and visuals](04-shaders-and-visuals.md), [audio](14-audio.md), [UI, audio and localization research](../research/08-ui-audio-localization.md)
> Implements: MS-05 (catalogues, lint, one locale, input abstraction, UI scale, AT scaffolding), MS-14 (RTL, CJK, narration, captions, audit)

## 1. Purpose and scope

This chapter is the contract for two things that must exist before the content does: every string the
player can read, and every way the player can perceive and operate the game when the defaults do not
fit. It specifies the Fluent pipeline and the string rules a lint can enforce, number and date
formatting, font fallback, RTL, text-expansion budgets, the translation workflow, and the accessibility
feature set: remappable input, scale, contrast, colour-blind palettes, captions and indicators,
narration and screen-reader semantics, motion and camera comfort, assists, focus and pause.

Both are foundation work, not a polish pass: retrofitting either after content exists is one of the
most expensive mistakes in this genre ([DEC-032](../decisions.md)). MS-05 delivers the machine;
MS-14 delivers breadth and the audit (§3.17).

| Out of scope | Owner |
| --- | --- |
| Widget tree, layout, text shaping internals | [13 — UI and UX](13-ui-and-ux.md) |
| Caption audio events and direction sectors | [14 — Audio](14-audio.md) §3.11 |
| Catalogue schema validation and pack layering | [15 — Content system](15-content-system.md) |
| Chat transport and server moderation policy | [18](18-networking-multiplayer.md), [20](20-security-and-anti-cheat.md) |
| Locale pack signing and distribution | [22 — Platforms](22-platforms-and-distribution.md) |
| Colour-blind palettes inside shading passes | [04 — Shaders](04-shaders-and-visuals.md) §3.12 |

## 2. Requirements

| ID | Requirement | Verified by | Milestone |
| --- | --- | --- | --- |
| REQ-I18N-001 | Every user-visible string MUST come from an FTL catalogue; no literal UI text in Rust, RON screens, shaders or content definitions. | `xtask lint-i18n`, review | MS-05 |
| REQ-I18N-002 | Keys are stable ASCII `<namespace>.<domain>.<subject>[.<variant>]`, never English text, never positional (`…1`), never reused with a new meaning; a rename keeps an alias for one release. | Key lint, alias test | MS-05 |
| REQ-I18N-003 | Messages are complete sentences with typed placeables; concatenation, fragment assembly and `format!` on visible text are forbidden. | Lint §3.4, review | MS-05 |
| REQ-I18N-004 | A catalogue MUST validate: FTL syntax, placeholder parity with `en-US`, every CLDR plural category for its locale, and the attributes the code reads. | Catalogue validator | MS-05 |
| REQ-I18N-005 | Locale IDs are BCP-47 with canonical case and case-insensitive matching; `auto` resolves from the OS at start-up and on change. | Locale table test | MS-05 |
| REQ-I18N-006 | Fallback is per message: exact tag → parent tags → `en-US` → a visible marker. A missing key MUST NOT render empty, English-by-accident or panic. | Fallback fixtures | MS-05 |
| REQ-I18N-007 | Language switching is live: bundles build off-thread and swap at a frame boundary in ≤ 250 ms, preserving screen state and focus. | Hot-switch test | MS-14 |
| REQ-I18N-008 | RTL locales mirror layout through start/end; directional icons are data-flagged; numbers, coordinates, key names and maps stay LTR; user text is wrapped in bidi isolates (UAX #9). | RTL screenshot suite | MS-14 |
| REQ-I18N-009 | Numbers, dates, times, units and percentages are formatted through ICU4X per locale; coordinates and log timestamps stay locale-neutral ASCII and ISO-8601. | Formatting golden tables | MS-14 |
| REQ-I18N-010 | Every widget wraps, grows to its minimum width or ellipsises with a tooltip; no control clips its label in a shipped locale or in `en-XA`. | Pseudo-locale screenshots | MS-14 |
| REQ-I18N-011 | `en-XA` and `ar-XB` are generated from `en-US` at build time, selectable in developer settings and exercised in CI. | Build artefact, CI job | MS-05 |
| REQ-I18N-012 | Fonts ship as language-pack assets under OFL 1.1 with attribution and reserved-font-name rules; layout NEVER depends on system fonts; the fallback chain covers every script and emoji in §3.6 within PERF-I18N-003; a missing glyph draws a tofu box and logs once. | Licence and coverage tests | MS-14 |
| REQ-I18N-013 | Packs may translate their own keys and `REPLACE` base keys ([DEC-019](../decisions.md)); conflicts are load errors; translations enter `view_hash` only, never `content_hash`, and a view-only difference is advisory at join, refusing a client only when the server opts into strict view matching ([DEC-050](../decisions.md)). | Pack validator, hash test, two-locale join | MS-14 |
| REQ-I18N-014 | The base install ships `en-US` plus one complete non-English locale; partial locales fall back per message and are labelled incomplete in settings. | Release manifest | MS-05 |
| REQ-I18N-015 | Chat, names, signs, books and server-supplied strings are user content: inserted as placeables, stripped of control and bidi-override characters, length-capped and never parsed as FTL; a server sends message keys plus typed arguments, never raw markup. | Fuzz corpus, protocol test | MS-06 |
| REQ-A11Y-001 | Every action is an `ActionId` with per-device bindings; no gameplay code reads a key or button directly, 100% of actions are remappable, an in-context conflict is rejected naming both actions, a cross-context duplicate warns, and profiles import/export as RON. | Remap coverage and binding tests | MS-05 |
| REQ-A11Y-002 | Every screen is completable keyboard-only and gamepad-only, with a visible focus indicator and no keyboard trap; focus order is document order unless overridden, modals trap and restore focus, grids navigate with arrow keys. | Scripted navigation suite | MS-14 |
| REQ-A11Y-003 | AccessKit exposes roles, names, values, states and actions; the tree is diffed and accessibility focus stays synchronised with UI focus. | Tree snapshot tests | MS-14 |
| REQ-A11Y-004 | The narrator is opt-in, speaks through OS TTS, and reads focus, menus, chat and critical HUD events under the priorities and limits of §3.13; identical text within 500 ms is deduplicated and a critical line is never interrupted. | Narration queue and duplicate-suppression tests | MS-14 |
| REQ-A11Y-005 | Captions consume the `CaptionRecord` of [14 §3.11](14-audio.md#311-captions) with direction cues, category filters, size and background settings, ≤ 4 lines and ≤ 8 lines/s; a visual sound indicator covers critical sounds when captions are off. | Shared caption and indicator suites | MS-14 |
| REQ-A11Y-006 | Colour is never the only carrier of meaning: each colour-coded signal also has shape, icon, text or pattern; deuteranopia, protanopia and tritanopia palettes exist for UI, team, waypoint and marker colours and apply live. | Palette fixtures, audit §3.17 | MS-14 |
| REQ-A11Y-007 | Contrast is ≥ 4.5:1 for text and ≥ 3:1 for large text and the focus ring, in default and high-contrast themes, including the HUD over bright scenes. | Automated contrast audit | MS-14 |
| REQ-A11Y-008 | UI scale is an integer ratio 1–8 covering 50–400% of the auto value; 100–200% is pixel-crisp at 1080p and 4K and applies without restart. | Screenshot diff at 5 scales | MS-05 |
| REQ-A11Y-009 | Reduced motion disables bob, shake, roll, motion blur, FOV effects, panorama motion and screen distortion; shake and FOV effects keep their own sliders; FOV is 30–110° (default 70) with a warning above 90°, and no effect flashes above 3 Hz or changes full-frame luminance by more than 30%. | Camera-trace assertion, settings trace, frame analysis | MS-14 |
| REQ-A11Y-010 | Hold-versus-toggle exists for sprint, sneak, attack and use, with documented defaults and a setting each. | Settings matrix | MS-14 |
| REQ-A11Y-011 | Aim assist is offered for gamepad and touch only, 0–100% strength, and is switched off when the host policy disallows it; auto-walk is a rebindable toggle cancelled by movement input, damage or opening a screen and never overrides player input. | Input and scripted-session tests | MS-14 |
| REQ-A11Y-012 | One-handed and left-handed presets keep every screen and action reachable with one hand and mirror the HUD and mouse buttons. | One-handed script | MS-14 |
| REQ-A11Y-013 | Assists affect input or presentation only, never a simulation rule; settings live in client config and are never sent to a server ([13 §4.2](13-ui-and-ux.md)). | Hash test, config inspection | MS-14 |
| REQ-A11Y-014 | Single-player pauses; a host with remote peers refuses with a stated reason; a dedicated server and its clients never pause, and menus say the world keeps running. | Pause matrix test | MS-05 |
| REQ-A11Y-015 | Every feature here has a searchable, localized setting with a playable default, and accessibility settings are per user, never replicated, and survive a locale change. | Settings and persistence tests | MS-14 |
| REQ-A11Y-016 | Desktop AT is a 1.0 requirement (Windows UIA, macOS VoiceOver, Linux AT-SPI); Android TalkBack, font scaling and 48 dp targets are post-1.0 ([DEC-034](../decisions.md)). | Platform matrix | MS-14 |
| REQ-A11Y-017 | The §3.17 audit with two external testers and no open P0 or P1 finding is an MS-14 exit gate. | Audit report | MS-14 |

## 3. Design

### 3.1 Two channels, one vocabulary

Accessibility is not a mode. The game conveys each fact through pixels, text and sound, and every fact
must be available on at least two of them: a sound's caption shares its message key with the subtitle
line, the narrator reads the localized item name the tooltip shows, and a button's AccessKit name is
the same FTL message as its visible label. One vocabulary, one translation effort, no drift.
The pipeline is one-way: negotiated locale and pack catalogues feed a `FluentBundle`, which produces
both the shaped text the UI draws and the plain strings captions, AccessKit names and the narrator use.

### 3.2 Catalogue layout, keys and resolution

Source language is `en-US`. A pack ships one Fluent catalogue per locale, `lang/<bcp47>.ftl`
([15 §3](15-content-system.md)); the base pack keeps per-domain sources (`ui`, `hud`, `chat`,
`content`, `captions`, `a11y`) for translator workflow, and the pack compiler merges them into that
file. The base pack's `en-US` set is the reference every other catalogue is validated against; a
third-party pack adds its own keys and may override base messages only through `REPLACE`
([DEC-019](../decisions.md)).

| Element | Rule | Example |
| --- | --- | --- |
| Key | `namespace.domain.subject[.variant]`, lowercase ASCII, 1–8 segments, ≤ 64 characters | `openblock.menu.singleplayer` |
| Attribute, file | Sub-key for a second surface; UTF-8, LF, one catalogue per locale | `.name` / `.tooltip`; `lang/de-DE.ftl` |
| Placeable, variant | Typed `string`, `number`, `date`, `item` (icon), `key` (glyph); variant chosen by a selector, never by suffix concatenation | `{ $count }`, `{ $name }`; `[one]`, `[few]`, `[many]`, `[other]` |

The bundle is a `FluentBundle` per locale built on the I/O pool. Resolution is per message, so a
partial translation is useful: exact tag, then parent tags (`zh-Hans-CN` → `zh-Hans` → `zh`), then
`en-US`, then a visible marker (`⟦key⟧`) that survives screenshots and is counted in the missing-key
report. CI fails on missing keys only for locales marked complete in the release manifest. Direction
comes from the script (`rtl` for `ar`, `he`, `fa`, `ur`), exposed as a `LayoutDirection` and forceable
for tests. `auto` follows the OS preferred-locale list, re-resolves on change and falls back to
`en-US` with one log line, never a dialog.

### 3.3 Message authoring rules

```ftl
# $count (Number) — blocks placed by the command. CLDR: one/other.
openblock.command.fill.placed =
    { $count ->
        [one] Placed { $count } block.
       *[other] Placed { $count } blocks.
    }
```

1. **One sentence, one message.** Never concatenate, never assemble a sentence from fragments, never
   assume subject–verb–object order. Word order, case and agreement belong to the translation.
2. **Placeables are typed and named.** `{ $count }` is a locale-formatted number, not a pre-formatted
   string; `{ $item }` renders an inline icon. Names and user text are always placeables and always
   isolated (§3.7).
3. **Plural and gender are selectors.** Every locale's full CLDR category set is present, not just
   `one`/`other`: Russian needs `one/few/many/other`, Arabic `zero/one/two/few/many/other`, Polish
   `one/few/many/other`. A `gender` selector exists only where the language requires it, with a
   default variant that is grammatical for "unknown".
4. **Comments and glossary.** `#` and `##` comment the message and its group, `# MAX:` gives a hard
   length limit and `# $var (Type) —` documents every variable; comments are stripped at runtime and no
   markup is parsed. Block, stack, chunk and dimension are translated once in Weblate's glossary.

### 3.4 String discipline and the lint

The UI API takes a `MsgKey(&'static str)`, not a `&str`, so `label("Hello")` does not compile unless
`Hello` is a key, and a key built with `format!` cannot be constructed. Debug builds assert that every
key exists in `en-US` at first use; release builds fall back to the marker and count it.

| Lint rule | Catches | Action |
| --- | --- | --- |
| literal text in a UI or content field | hard-coded labels, death messages | error |
| concatenation or `format!` on a `Msg` | sentence assembly, hand-formatted numbers | error |
| key absent from `en-US`, or placeholder mismatch | typos, dropped or added variables | error |
| missing plural category for the locale | broken Russian or Arabic counts | error |

`xtask lint-i18n` runs on every pull request over Rust sources, RON screens and content JSON, with a
reviewer-signed allowlist for log messages, identifiers and developer overlays, because "it's only a
debug string" is how hard-coded text ships.

### 3.5 Numbers, dates, units and coordinates

| Value | Formatting | Example (`de-DE`, `ar`) |
| --- | --- | --- |
| Count, percentage | ICU4X decimal or percent with locale grouping | `1.234` / `١٬٢٣٤`; `42 %` / `٤٢٪` |
| Coordinate | ASCII digits, comma-separated, always LTR | `(1234, 64, -56)` |
| Distance | metres below 1 km, kilometres with one decimal above | `512 m`, `1,2 km` |
| Clock time, duration | 12 h or 24 h by locale preference; seconds with one decimal | `6:30 PM` / `18:30`; `3,5 s` |
| Date, log timestamp | ICU4X date in UI; ISO-8601 UTC in logs | `24.11.2026`; `2026-11-24T15:47:19Z` |

Game time is a formatting concern on top of a raw tick count: the day clock follows the locale's 12/24
preference, while `day_time`, coordinates and command arguments stay ASCII integers, and a localised
numeral in a text field is normalised before parsing.

### 3.6 Fonts, scripts and glyph coverage

| Script | Face (OFL 1.1) | Shipping | Notes |
| --- | --- | --- | --- |
| Default UI | first-party bitmap atlas, 16×16 cells, 64 columns | base | nearest sampling, integer scale, optional 1 px shadow |
| Latin, Greek, Cyrillic | Noto Sans | base | vector raster at device size, hinted when small |
| CJK; Arabic, Hebrew, Devanagari, Thai | Noto Sans per script | language pack | 16–25 MB per CJK face, subsets per locale; shaping, bidi and reordering from cosmic-text |
| Emoji | Noto Color Emoji | base, subset | colour glyphs in chat and tooltips |

The bitmap atlas is 1 MiB for 4,096 cells at 1024². Vector glyphs rasterise on demand and cache per
`(font, size, subpixel bin)` in 2,048² pages (4 MiB each) with an LRU of four pages and a per-locale
cap of 24 MiB. There is no MSDF requirement: a 4,096² R8 page is 16 MiB, and 20,000 CJK glyphs at
32×32 need 20.5 M pixels, which fits only with tight packing and blurs at small sizes
([research §4.2](../research/08-ui-audio-localization.md)). At 1080p the auto GUI scale is 4, so an
8 px glyph is 32 physical pixels; integer scaling is what keeps it crisp.

Licensing is load-bearing: a pack declares every font with licence file, origin and SHA-256, a missing
licence fails validation, and a subset under a Reserved Font Name must be renamed before it ships.
Attribution appears in the pack credits and the about screen. Layout never falls back to a system font
silently, because its metrics differ per machine and would make screenshots meaningless; a preference
may *prefer* a system face for body text, with the bundled chain behind it. A glyph no face covers is a
hollow tofu box, logged once per codepoint and counted in CI.

### 3.7 RTL and mirroring

| Element | RTL behaviour |
| --- | --- |
| Layout | `start`/`end` everywhere; `left`/`right` only in physical-device code |
| Arrows, back/forward, progress fill | mirrored by a per-asset `mirror_in_rtl` flag |
| Clock, numbers, coordinates, key names, maps | never mirrored, always LTR; north stays up |
| Chat, player list, tab list | direction from the first strong character (UAX #9) |
| Hotbar, inventory grid, slot order | not mirrored: slot order is a game rule, not a reading direction |
| Sliders, scrollbars | mirrored with the layout; value direction unchanged |

Mirroring is per asset and per element, never a blind framebuffer flip: a flipped screenshot puts
health bars right-to-left, while a text-only bidi fix leaves icons pointing the wrong way. Both are
tested with `ar-XB` and with a mixed Arabic/English chat. User text inside a message is wrapped in
First-Strong-Isolate … Pop-Directional-Isolate so a Latin name cannot reorder an Arabic sentence;
control characters and explicit bidi overrides in names are stripped at the network boundary
(REQ-I18N-015).

### 3.8 Text expansion and truncation

Planning factors against `en-US`, re-measured on the shipped catalogue at MS-14 (estimates): `de-DE`
+30%, `fi-FI` +35%, `pl-PL` +25%, `ru-RU` and `ar` +20%, `fr-FR` and `pt-BR` +15%, `ja-JP` −10%,
`ko-KR` −20%, `zh-Hans` −30%.

A label may wrap to two lines; a control's minimum width is 1.3× its `en-US` width; beyond two lines
the text ellipsises and the full string moves to the tooltip and the accessibility name. Buttons in
one row share the widest label. A surface that cannot grow (sign, book page, hotbar label) has a
per-surface character cap enforced at input time. `en-XA` is `en-US` with accents, brackets and about
+30% length; a developer `en-XZ` mode doubles it. Both are generated, never hand-maintained.

### 3.9 Locale packs and the community workflow

| Step | Tool | Rule |
| --- | --- | --- |
| Authoring | self-hosted Weblate, one component per FTL file | glossary, screenshots, translator comments, reviewer role |
| Validation, review | `fluent-syntax` plus `xtask lang check`; one native-speaker reviewer per locale | syntax and placeholder errors fail the PR, incompleteness does not; terminology checked against the glossary |
| Packaging, distribution | `xtask lang pack` → `.oblang`; in-app locale list | manifest, FTL, fonts, credits, SHA-256 and signature; signed, resumable, verified before install, and a failure keeps the previous pack |

A `.oblang` pack declares `locale`, `version`, `complete`, `fonts[]` with licences, `catalogues[]`
with hashes, `credits[]` and `min_engine`. Installation is off-thread and atomic. Translations are
contributed under CC BY 4.0 with per-locale credits, matching the asset licence
([DEC-009](../decisions.md)); the DCO covers code, not translated prose.

### 3.10 Input, remapping and focus

Actions are resource IDs (`openblock:move_forward`, `openblock:inventory`, `openblock:narrator_toggle`)
with per-device bindings: keyboard key (physical and logical), mouse button and wheel, gamepad
button/axis, touch gesture. Contexts (`gameplay`, `menu`, `chat`, `inventory`, `spectator`, `dev`) gate
dispatch; the top context consumes the event, so typing in chat cannot walk the player.

| Rule | Behaviour |
| --- | --- |
| Physical vs logical | movement binds physical positions, so the key labelled `Z` on AZERTY is forward exactly as `W` is on QWERTY; text shortcuts bind logical keys and the rebind UI shows both when they differ |
| Conflict, reserved keys | same context: reject and name both actions; different contexts: allow with a warning; screenshot, debug and quit are rebindable but warn about support screenshots |
| Gamepad, touch | gilrs mapping and hotplug, deadzone 0.15, trigger 0.3, repeat delay 250 ms, glyph set by controller family; touch targets 48 dp with safe-area insets (post-1.0) |
| Defaults, profiles, focus | documented per action and restorable; profiles import/export as versioned RON; Tab/arrow or left-stick traversal, Enter/Space or A activates, B/Escape closes the top modal, focus ring 2 px at ≥ 3:1 |

No gameplay code may consult a key constant; the remap-coverage test rebinds every action to a unique
chord, runs a scripted session and fails if any action never fires.

### 3.11 Scale, contrast, colour and non-colour affordances

UI scale is an integer GUI-pixel ratio `1..=8`, chosen automatically from the window size (formula in
[13](13-ui-and-ux.md)) and presented as 50–400% of that value, so "200%" always means twice what we
chose for this display. Integer ratios keep the bitmap font and 9-slice borders crisp; a fractional
request is rounded and the settings row says so.

On first run `reduced_motion`, `ui_contrast` and text scale seed themselves from the OS accessibility
preferences ([22](22-platforms-and-distribution.md) REQ-PLAT-045) while their setting is `auto`; a manual
choice always wins. Delivery and audit of the seeded behaviour are MS-14 work in this chapter; the
platform hooks that expose the OS preferences are signed off at MS-16 in
[22](22-platforms-and-distribution.md), so a platform gap is tracked there and does not move this
chapter's milestone. Two themes ship, `default` and `high_contrast`, and the contrast audit runs on
reference frames at 1080p and 4K in every theme and palette, measuring text (≥ 4.5:1), large text and
the focus ring (≥ 3:1), including the HUD over bright sky and over lava; a failing frame names the
widget ID and the contrast pair, because "make it more readable" is not actionable. Colour-blind
palettes remap UI accents, team colours, waypoints, map markers, boss-bar tints and damage flashes,
and never rely on hue alone: team colours carry a shape emblem, waypoints an icon, and ore and flower
textures differ in value and pattern as a texture acceptance criterion. A developer view runs the frame
through a deuteranopia, protanopia or tritanopia transform so reviewers see what the player sees.

### 3.12 Motion, flashes and camera comfort

| Effect | Default | Reduced motion | Separate control |
| --- | --- | --- | --- |
| View bob, view roll (bow, strafe), screen shake | on | off | sliders 0–100% |
| FOV effects (sprint, speed, zoom) | on | off | slider 0–100% |
| Motion blur (off) and panorama, nausea/portal overlays (on) | as stated | off | toggles |
| Damage flash, lightning | on | on, ≤ 3 Hz and ≤ 30% luminance change | intensity slider; exposure |
| Audio Doppler | on | off ([14](14-audio.md) REQ-AUDIO-028) | — |

FOV is 30–110°, default 70, with a warning above 90° about distortion and motion sickness; the FOV
*effects* slider is separate so a player can keep a wide view without speed-induced zoom. Reduced
motion is one switch that sets the middle column; each row can then be re-enabled individually, and
the switch never silently re-enables itself when another setting changes.

### 3.13 Captions, indicators, narration and screen readers

Captions render the `CaptionRecord` from [14 §3.11](14-audio.md#311-captions) — key, category,
priority, eight-sector direction, dedupe — and add presentation only: bottom-centre, up to four lines,
background opacity 0–100% (default 60%), text at 75–200% of UI scale, one line per distinct
`(key, sector)` with repeats merged into a count over 1.5 s, a 1.5 s fade and a cap of eight new lines
per second. A caption appears within 100 ms (PERF-AUDIO-012). Category filters are individual toggles
with hostile and player on by default, and direction is an arrow glyph plus an accessible name
("behind left"), so it works with captions off for a screen-reader user and with sound off for a deaf
player. The visual indicator is the lower-effort alternative: an arc at the screen edge shows up to six
critical sounds as category icons with the same sectors, enabled independently of captions.

| Channel | Covers | Default | Limits |
| --- | --- | --- | --- |
| AccessKit | roles, names, values, states, actions; menus and container structure | exposed whenever an AT is present | diff ≤ 0.2 ms per changed screen (PERF-A11Y-001) |
| Narrator | focus, menu titles, chat, HUD events, damage, hunger, advancements | off; onboarding offers it | ≤ 3 utterances/s, queue 8, start ≤ 150 ms (PERF-A11Y-002) |

Both read the same FTL messages. The narrator queue has four priorities — `critical` (death, low
health, disconnect), `focus`, `status` (hunger, pickup, advancement) and `ambient` (chat unless
addressed) — and a `critical` line is never interrupted. Duplicate text within 500 ms is dropped,
which is what stops a screen reader and the narrator from saying the same thing twice; no portable
"screen reader active" query exists, so the narrator is opt-in and the settings text says both can be
on.

TTS goes through the platform layer: SAPI or UIA on Windows, `AVSpeechSynthesizer` on macOS,
speech-dispatcher over SSIP on Linux, Android TTS post-1.0. The voice follows the game locale when a
matching voice exists, otherwise the system language. With no synthesiser the narrator is disabled
with an actionable message and an external provider may register through the platform trait. Speech is
normalised: the engine speaks numbers, item names use the localized name, slot content reads "Oak Log,
32, slot 12", coordinates are read digit by digit, and a text field speaks value and selection.

### 3.14 Gameplay assists

Assists change what the client sends, never what the host accepts. `auto_jump`, the attack indicator,
sprint/sneak/attack toggles and auto-walk are input shaping. Aim assist is the only one that touches
aim, and only for gamepad and touch: 0–100% strength slows the look rate inside a 6° cone, never
selects a target outside the current view, and pulls at most 2°/s. It is disclosed in the join
settings the client honours when the server advertises `world.allow_aim_assist = false` (default on, except
when `world.pvp = true`). Nothing about it crosses the wire, so compliance is a moderation matter, not an
anti-cheat rule ([DEC-029](../decisions.md), [20](20-security-and-anti-cheat.md)).
Auto-walk is a toggle cancelled by movement, damage, a screen or a vehicle. One-handed presets put
movement on one hand's keys or stick, mirror the HUD, swap mouse buttons and enlarge the crosshair;
left-handed mode mirrors without moving the movement cluster.

### 3.15 Pause, focus loss and the menu

Single-player with no remote peers pauses and freezes the tick. An integrated host with peers and any
dedicated client do not, because the host refuses to pause ([02](02-engine-core.md) REQ-CORE-010); their
menus stay live, never dim the world, and show a "the world keeps running" banner. Loading,
disconnecting and crash recovery pause with a modal that cannot be dismissed while a save flushes.
Focus loss auto-pauses only when pausing is allowed and `pause_on_focus_loss` is on (default on);
otherwise losing focus is announced in chat. A menu never blocks incoming damage on a server, which is
why the banner is mandatory rather than cosmetic.

### 3.16 Platforms and the Android deferral

| Platform | Requirement at 1.0 | Notes |
| --- | --- | --- |
| Windows 10/11 | UIA tree, Narrator, high-contrast theme, display scaling, OS reduce-motion and text-scale preferences | accesskit Windows adapter |
| macOS 12+ | VoiceOver through the AppKit bridge, system voices, Retina scaling | voice list includes the game locale |
| Linux X11/Wayland | AT-SPI (Orca, Accerciser), speech-dispatcher or an external provider | the weakest TTS link: narrator degrades, AT does not |
| Steam Deck | readable at 3× (24 px text), gamepad-only completion, on-screen keyboard | Verified-checklist relevant |
| Android (post-1.0), consoles (if pursued) | TalkBack, font scale 85–200%, 48 dp, safe areas; console certification focus | abstractions built now ([DEC-034](../decisions.md)) |

### 3.17 The accessibility audit checklist

Each row runs manually at MS-14 and, where marked, in CI, on every screen of the release build in
`en-US`, `de-DE` and `ar` at UI scale 100% and 200%.

| # | Criterion | Method | Pass |
| --- | --- | --- | --- |
| A1 | Every screen reachable keyboard-only and gamepad-only | scripted Tab/arrow and pad walks | 100% of screens, no trap |
| A2 | Screen reader completes menu, settings, inventory | NVDA, VoiceOver, Orca runs | every control announces role, name, value, state |
| A3 | Every action remaps and fires; captions cover all critical sounds | remap-coverage test (CI), manifest diff, eight-sector stimulus | 100% of actions; 100% covered; ≥ 95% correct sectors |
| A4 | No information is colour-only | review with simulated palettes | zero findings |
| A5 | Contrast meets §3.11; text readable at 100–200% and 1080p/4K; settings findable in three locales | frame audit (CI), screenshot review, settings search | zero violations; no clipping; every key returns its row |
| A6 | `en-XA` shows no untranslated strings | pseudo-locale run (CI) | zero literals, no clipping beyond policy |
| A7 | RTL mirrors correctly | `ar-XB` screenshots and video | no broken hierarchy, no LTR numbers |
| A8 | Reduced motion removes camera motion; flashes safe; one-handed play possible; pause semantics stated | camera trace, luminance series, single-hand session, pause matrix | variance zero; no flash > 3 Hz or > 30%; core verbs reachable; banner where pause is refused |

### 3.18 User research with disabled players

Two paid, consented rounds. Round A at MS-05 with 6 participants (2 blind or low-vision, 2
motor-impaired, 2 deaf or hard-of-hearing) on menus, settings, remapping and the caption prototype;
Round B at MS-14 with 8–12 participants including cognitive and colour-vision participants, on a
60-minute task list (start a world, rebind, enable captions, find the narrator, build and place,
pause, resume, complete a screen-reader session). Sessions are remote, recorded only with opt-in
consent, and no personal data enters the repository. Severity is fixed in advance (P0 blocks a core
task, P1 needs help, P2 is friction, P3 cosmetic), and the MS-14 gate is "no open P0 or P1, every P2
owned", reviewed by the two external testers of REQ-A11Y-017.

## 4. Data structures and formats

### 4.1 Locale pack manifest

```ron
(
  locale: "de-DE", version: 3, complete: true, min_engine: "0.6.0",
  catalogues: [(path: "lang/de-DE.ftl", sha256: "…")],
  fonts: [(path: "fonts/NotoSans-Regular.ttf", licence: "OFL-1.1", sha256: "…")],
  credits: ["Noto Sans, Google Inc. (OFL-1.1)", "Translation: …"],
)
```

### 4.2 Settings surface

All keys below are `Live` and apply within one frame.

| Key | Type | Default |
| --- | --- | --- |
| `language` | `auto` or BCP-47 | `auto` |
| `ui_scale`, `ui_contrast`, `color_blind_palette` | `auto` or 50–400; `default` / `high_contrast`; `none` / three CVD palettes | `auto`, `default`, `none` |
| `subtitles`, `sound_indicators`, `subtitle_scale`, `subtitle_background` | bool, 75–200%, 0–100% | `false`, `false`, `100`, `60` |
| `narrator`, `narrator_verbosity`, `narrator_rate`, `narrator_volume` | enum, 0–100 | off, `brief`, `100`, `100` |
| `reduced_motion`, `camera_bob`, `screen_shake`, `fov_effects`, `fov` | bool, 0–100, 30–110° | `false`, `100`, `100`, `100`, `70` |
| `hold_to_sprint`, `hold_to_sneak`, `toggle_attack` | bool | `true`, `true`, `false` |
| `auto_jump`, `auto_walk`, `aim_assist` | bool, 0–100 | `false`, `false`, `0` |
| `left_handed`, `pause_on_focus_loss`, `hold_to_use` | bool | `false`, `true`, `true` |

Keys are snake_case ASCII ([DEC-044](../decisions.md)), owned by [`ob_client`](13-ui-and-ux.md) and
per user, not per world. A server may advertise policy (`world.allow_aim_assist`, `voice_chat.enabled`) but
never writes a client's accessibility settings.

### 4.3 Bindings and accessibility nodes

```rust
pub struct BindingProfile {
    pub schema: u16,
    pub actions: BTreeMap<ActionId, SmallVec<[Binding; 4]>>,   // device-tagged, §3.10
}
pub struct A11yNode {
    pub id: WidgetId,                 // stable across rebuilds (DEC-030)
    pub role: Role,                   // button, checkbox, slider, text_field, list, grid_cell…
    pub label_key: MsgKey, pub value: Option<ValueKind>,
    pub state: NodeState,             // focused, disabled, selected, expanded, busy
    pub actions: SmallVec<[NodeAction; 4]>,
    pub bounds: Rect, pub children: SmallVec<[WidgetId; 32]>,
}
```

The AccessKit tree is built from `A11yNode`s after layout, diffed against the previous frame and sent
only when a role, label, value or state changed. Narration items reuse `MsgKey` with a four-level
priority and a 500 ms dedupe hash. Large lists (inventories, server lists, advancement trees)
virtualise: only visible nodes enter the tree, and the list announces its total.

## 5. Performance budgets

| ID | Budget | Value |
| --- | --- | --- |
| PERF-I18N-001 | Catalogue parse and bundle build, `en-US`; all shipped locales | ≤ 20 ms; ≤ 150 ms on the I/O pool |
| PERF-I18N-002 | Message format | ≤ 5 µs p99; 200 labels inside the 0.5 ms UI budget of [13](13-ui-and-ux.md) |
| PERF-I18N-003 | Glyph raster; per-locale atlas | ≤ 40 µs p99 per glyph; ≤ 24 MiB, LRU of 4 pages |
| PERF-I18N-004 | Locale pack size; verify and install | ≤ 1.5 MiB text, ≤ 30 MiB with CJK fonts; ≤ 2 s from disk |
| PERF-A11Y-001 | AccessKit diff; first build of a 200-node screen | ≤ 0.2 ms; ≤ 2 ms off the frame path |
| PERF-A11Y-002 | Narration start; rate; queue | ≤ 150 ms p99; ≤ 3/s; ≤ 8 items |
| PERF-A11Y-003 | Caption event to line | ≤ 100 ms p99 (shared with [14](14-audio.md)) |
| PERF-A11Y-004 | UI scale change at 200%; theme and palette cost | ≤ 50 ms, no restart, no lost focus; ≤ 0.05 ms/frame |
| PERF-A11Y-005 | Reduced motion camera trace | exactly zero shake and bob contribution |
| PERF-A11Y-006 | Binding profile load; remap coverage | ≤ 10 ms; 100% of actions |

## 6. Failure modes and edge cases

| Failure | Detection | Containment | User-visible |
| --- | --- | --- | --- |
| Font missing, unlicensed or unrenamed-RFN subset; glyph not covered | pack validation, coverage test | reject the pack and keep the previous locale; draw tofu with one log line per codepoint | "language pack could not be installed"; a visible box, never blank or a panic |
| Translation longer than its widget | `en-XA` run, screenshot diff | wrap, grow, then ellipsis and tooltip | readable label, full text on hover and to AT |
| Missing key in a complete locale, or missing plural category | catalogue validator | per-message fallback, counted; a missing category fails the PR and falls back to `other` at runtime | English line, grammatical fallback, never empty |
| Bidi override or control character in a name; language switch during IME; TTS unavailable | sanitizer, input state, platform probe | strip and log the peer; defer the swap to a frame boundary; narrator off with an actionable message and AT unaffected | name displays correctly; one-frame delay; text alternative remains |
| Narrator spam from a busy HUD | queue depth and rate | merge duplicates, drop `ambient` first, never interrupt `critical` | quiet, prioritised speech |
| Remap conflict, reserved chord, or UI scale change mid-drag | conflict check; layout state | reject in-context, warn cross-context; apply scale at a frame boundary, keep focus by widget ID | both actions named; no focus jump |
| AccessKit tree too large (90-slot inventory) | node-count budget | virtualise unseen rows, send diffs | list announces its total |
| Locale pack download fails or signature mismatch; unknown server message key | hash and signature check; protocol validation | keep the previous pack and offer retry; neutral notice, log once | game remains playable; "message unavailable" |

| Risk | Description | Mitigation | Owner |
| --- | --- | --- | --- |
| RISK-231 | Localization is deferred and content hard-codes text | lint in CI from MS-05, `MsgKey` type, `en-XA` gate | MS-05 |
| RISK-232 | Accessibility is retrofitted after the UI exists | AT tree and focus model in MS-05, audit gate at MS-14 | MS-14 |
| RISK-233 | Font licensing or size blocks a locale at release | OFL-only faces, licences in packs, CJK as a download | MS-13 |
| RISK-234 | Translation workflow stalls, or assists are mistaken for cheats on public servers | Weblate community and glossary; input-only assists, advertised host policy, moderation | MS-14 / MS-15 |

## 7. Testing strategy

| Layer | What is proven | Tooling |
| --- | --- | --- |
| String lint | no hard-coded text, no concatenation, every key exists | `xtask lint-i18n` (CI) |
| Catalogue validation and pseudo-locale | syntax, placeholders, plural categories; expansion and RTL expose clipping and mirroring bugs | validator over the base pack; `en-XA` / `ar-XB` screenshots, SSIM plus text-box diff |
| Locale matrix, glyph coverage | every shipped locale loads, renders and switches live; every codepoint has a glyph | 13 locales × 3 scales × 2 themes nightly; coverage test against the font set |
| Contrast, flashes, focus, remap | WCAG thresholds; keyboard-only and gamepad-only completion; 100% action coverage | frame audit, luminance series, scripted navigation and remap harness |
| Accessibility tree, narration and captions | roles, names, values, states, focus sync; priorities, dedupe, rate limits, contract parity with [14](14-audio.md) | AccessKit snapshots, NVDA/VoiceOver/Orca runs, mock synthesiser, shared caption suite |
| Manual audit | the §3.17 checklist and two external testers | audit report attached to MS-14 |

The locale matrix is small in PR CI and complete nightly; the manual audit is a release gate because no
automated test can tell whether a screen-reader user understands a trade window.

## 8. Open questions

1. **Translation contribution licence.** CC BY 4.0 matches the asset licence ([DEC-009](../decisions.md)),
   but some translators prefer CC0 or copyleft; confirm before the Weblate instance opens. Owner: MS-14.
2. **Locale pack distribution.** Signed in-app downloads versus bundling the ten most common locales
   (60–100 MB, but helps offline players). Owner: MS-16, using the MS-13 size budget.
3. **Translation join policy (settled by DEC-050).** Translation catalogues are view leaves, are
   excluded from `content_hash`, and never block a connection by default; a server may opt into strict
   view matching. Remaining open: whether an incomplete locale pack is mentioned in the join screen or
   only in settings. Owner: MS-14, with MS-06.
4. **Register item needed:** "Input assists are client-side settings a host may disallow through policy;
   they are never disclosed on the wire and never an anti-cheat matter." Owner: MS-15, with
   [20](20-security-and-anti-cheat.md).
5. **Narrator source on Linux.** speech-dispatcher is the only supported path at 1.0 and is often
   absent; ship an external-provider hook, or accept "narrator unavailable"? Owner: MS-14.
6. **RTL and console scope.** Whether every 9-slice asset gets a mirrored variant is decided by the
   `ar-XB` review; console requirements, if pursued, land here and in [22](22-platforms-and-distribution.md).
   Owner: MS-14 / MS-16.

## 9. References

- Decisions: [DEC-009](../decisions.md), [DEC-030](../decisions.md), [DEC-032](../decisions.md),
  [DEC-034](../decisions.md), [DEC-044](../decisions.md), [DEC-050](../decisions.md);
  [conventions](00-conventions.md) §5, §7.
- Chapters: [architecture](01-technical-architecture.md), [engine core](02-engine-core.md),
  [rendering](03-rendering.md), [shaders and visuals](04-shaders-and-visuals.md) §3.12,
  [survival gameplay](08-gameplay-survival.md), [UI and UX](13-ui-and-ux.md), [audio](14-audio.md),
  [content system](15-content-system.md), [networking](18-networking-multiplayer.md),
  [security](20-security-and-anti-cheat.md), [platforms](22-platforms-and-distribution.md),
  [performance budgets](25-performance-budgets.md).
- Research: [UI, audio, localization](../research/08-ui-audio-localization.md) §4 (text and font stacks,
  atlas arithmetic), §6 (Fluent, RTL, pseudo-locale, workflow), §7 (accessibility, platform checklists),
  §8 (budgets); milestones [MS-05, MS-14](../plan/01-milestones.md).
- External: [Project Fluent](https://projectfluent.org/) and its
  [syntax guide](https://projectfluent.org/fluent/guide/),
  [CLDR plural rules](https://cldr.unicode.org/index/cldr-spec/plural-rules),
  [UAX #9 bidi](https://www.unicode.org/reports/tr9/), [ICU4X](https://icu4x.unicode.org/),
  [AccessKit](https://accesskit.dev/), [WCAG 2.2](https://www.w3.org/TR/WCAG22/),
  [SIL OFL 1.1](https://openfontlicense.org/), [Weblate FTL](https://docs.weblate.org/en/latest/formats/fluent.html).
