# RPM spec for OpenBlock — COPR staging recipe for the 1.0.x line.
#
# STATUS: not published and not yet buildable from a release asset. v1.0.0
# attached only screenshots to GitHub; Source0 (the vendor tarball described
# below) and the COPR project do not exist yet. See packaging/README.md
# "Publication status" and docs/plan/1.0.1-audit.md before relying on this file.
#
# This is a COPR package: it bundles the Rust crate graph in the source archive
# (see packaging/README.md). An official Fedora repository package requires
# unbundling via cargo2rpm and is tracked as OPEN-021.
#
# Build locally from a release tarball placed in the rpmbuild SOURCES directory:
#   rpmbuild -ba packaging/fedora/openblock.spec
#
# The archive must contain Cargo.lock and the full workspace. `%cargo_*` macros
# are deliberately not used yet: the dependency graph is resolved from the
# bundled vendor directory produced by `cargo vendor` in the release tarball.
%global debug_package %{nil}

Name:           openblock
Version:        1.0.2
Release:        1%{?dist}
Summary:        Fast, self-hostable voxel sandbox with survival and multiplayer

License:        MIT OR Apache-2.0
URL:            https://github.com/martintondel02/openblock
Source0:        %{url}/releases/download/v%{version}/openblock-%{version}.tar.gz
# TODO(release/COPR): replace with one `Provides: bundled(crate(NAME)) = VERSION`
# per vendored crate generated from Cargo.lock, as Fedora review requires. The
# previous single `bundled(crate(openblock))` line was not a valid bundled
# provide list, so it is deliberately omitted rather than claiming a capability.
# See packaging/README.md and OPEN-021.

BuildRequires:  gcc gcc-c++ make
BuildRequires:  rust cargo
# Audio output uses runtime-loaded libpulse-simple; no ALSA/PulseAudio
# development headers are needed at build time.
BuildRequires:  pkgconf-pkg-config
BuildRequires:  wayland-devel libxkbcommon-devel
BuildRequires:  vulkan-loader-devel fontconfig-devel freetype-devel
BuildRequires:  systemd-devel systemd-rpm-macros firewalld-filesystem
Requires:       mesa-vulkan-drivers
# libpulse-simple is dlopened at runtime and audio is optional, so recommend it
# rather than making a sound server a hard dependency of the game.
Recommends:     pulseaudio-libs

%description
OpenBlock is an open-source voxel sandbox game. Mine, build, explore and
survive alone or with friends on servers you host yourself. The 1.0 Early
Access release ships the survival loop, mobs/combat, procedural audio and
self-hosted multiplayer. Minecraft parity, the mod/content platform and the
wider distribution channels are post-1.0 work, not capabilities of this
package.

This package installs the client with desktop integration. Audio is synthesised
in memory and played through PipeWire's PulseAudio compatibility service (or
PulseAudio itself); no sound files are bundled and no audio development headers
are needed to build. The dedicated server is in the openblock-server subpackage.

%package server
Summary:        Dedicated server for OpenBlock
Requires:       systemd
Requires(pre):  systemd
Requires(post): systemd
Requires(preun): systemd
Requires(postun): systemd
%description server
The headless OpenBlock dedicated server for the interim JSON/TCP protocol.
No GPU, display server or audio device is required. Includes a hardened
systemd unit, a firewalld service definition for TCP/25565, sysusers/tmpfiles
definitions and the default flat server.toml. The interim protocol has no TLS
or authentication: bind it to a trusted LAN/VPN, not the public internet.

%prep
%autosetup -n openblock-%{version}

%build
export CARGO_HOME="${PWD}/.cargo-home"
export CARGO_NET_OFFLINE=true
%set_build_flags
cargo build --release --locked --offline --workspace \
  --bin openblock --bin openblock-server

%install
install -Dpm 0755 target/release/openblock %{buildroot}%{_bindir}/openblock
install -Dpm 0755 target/release/openblock-server %{buildroot}%{_bindir}/openblock-server

install -Dpm 0644 packaging/fedora/io.github.martintondel02.OpenBlock.desktop \
  %{buildroot}%{_datadir}/applications/io.github.martintondel02.OpenBlock.desktop
install -Dpm 0644 packaging/fedora/openblock.metainfo.xml \
  %{buildroot}%{_metainfodir}/io.github.martintondel02.OpenBlock.metainfo.xml
install -Dpm 0644 packaging/fedora/io.github.martintondel02.OpenBlock.svg \
  %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.github.martintondel02.OpenBlock.svg
# The .obworld MIME definition is intentionally not installed: archive import
# is not implemented in 1.0, so the desktop file does not claim the type. Ship
# both together when the format lands.

install -Dpm 0644 packaging/systemd/openblock-server.service \
  %{buildroot}%{_unitdir}/openblock-server.service
install -Dpm 0644 packaging/systemd/openblock-server.env.example \
  %{buildroot}%{_sysconfdir}/openblock/server.env.example
install -Dpm 0644 packaging/fedora/openblock-server.firewalld.xml \
  %{buildroot}%{_prefix}/lib/firewalld/services/openblock-server.xml
install -Dpm 0644 packaging/systemd/openblock-server.sysusers \
  %{buildroot}%{_sysusersdir}/openblock-server.conf
install -Dpm 0644 packaging/systemd/openblock-server.tmpfiles \
  %{buildroot}%{_tmpfilesdir}/openblock-server.conf
install -dpm 0750 %{buildroot}%{_localstatedir}/lib/openblock
install -Dpm 0644 packaging/systemd/server.toml \
  %{buildroot}%{_sysconfdir}/openblock/server.toml
install -Dpm 0755 scripts/server-backup.sh \
  %{buildroot}%{_bindir}/openblock-server-backup
install -Dpm 0755 scripts/server-restore.sh \
  %{buildroot}%{_bindir}/openblock-server-restore

%pre server
# Create the unprivileged openblock user/group with systemd-sysusers. The same
# definition is installed at %{_sysusersdir}/openblock-server.conf for later
# boots; --inline avoids depending on a payload file that %pre cannot read yet.
systemd-sysusers --inline 'g openblock -' 'u openblock - "OpenBlock dedicated server" /var/lib/openblock /sbin/nologin'
exit 0

%post
update-desktop-database &>/dev/null || :
if command -v appstreamcli >/dev/null 2>&1; then appstreamcli refresh --source=os &>/dev/null || :; fi

%post server
%systemd_post openblock-server.service
systemd-tmpfiles --create %{_tmpfilesdir}/openblock-server.conf || :
%firewalld_reload

%preun server
%systemd_preun openblock-server.service

%postun server
%systemd_postun_with_restart openblock-server.service

%postun
update-desktop-database &>/dev/null || :

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md CHANGELOG.md
%{_bindir}/openblock
%{_datadir}/applications/io.github.martintondel02.OpenBlock.desktop
%{_metainfodir}/io.github.martintondel02.OpenBlock.metainfo.xml
%{_datadir}/icons/hicolor/scalable/apps/io.github.martintondel02.OpenBlock.svg

%files server
%doc packaging/systemd/README.md docs/admin
%{_bindir}/openblock-server
%{_bindir}/openblock-server-backup
%{_bindir}/openblock-server-restore
%{_unitdir}/openblock-server.service
%{_prefix}/lib/firewalld/services/openblock-server.xml
%{_sysusersdir}/openblock-server.conf
%{_tmpfilesdir}/openblock-server.conf
%attr(0750,openblock,openblock) %dir %{_localstatedir}/lib/openblock
%dir %{_sysconfdir}/openblock
%config(noreplace) %{_sysconfdir}/openblock/server.toml
%config(noreplace) %{_sysconfdir}/openblock/server.env.example

%changelog
* Wed Nov 25 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 1.0.0-1
- OpenBlock 1.0.0 Early Access: survival core (worldgen caves/ores, mobs and
  combat, crafting/recipe book, furnace smelting, iron tools and armor, chests,
  item drops/pickup, death/respawn, persistence), self-hosted multiplayer alpha
  with server-authoritative survival and protocol v2, procedural audio, title
  screen/world browser/connect UI, key rebinding/accessibility, resource packs,
  benchmarks/load gates and server operations tooling.
- 1.0 hardening: full acceptance scenario, spawn safety/spawn.txt, dead-player
  action guard, UI/UX audit fixes, macOS/Windows CI fixes, 8-player load gate
  and 10-minute soak.

* Wed Nov 25 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.2.0-1
- Front-end: title screen with Continue/New World/Load World/Multiplayer/
  Settings/Quit, world creation and world browser with safe delete, direct-
  connect screen with recent servers, host-optional settings from the menu.
- Interactive controls rebinding for 17 actions with duplicate rejection, reset
  and persisted keybinds; reduced-motion accessibility option; persisted HUD
  visibility; multiplayer name tags.
- Multiplayer load gate: lock-free server metrics (tick p50/p95/p99/max, overrun
  counters, bandwidth/RSS), --metrics-json, 8-bot load-gate/soak script and CI
  job; measured 8-player p99 0.163 ms, 20 TPS, stable 10-minute soak.
- Cross-platform fix: accepted sockets are forced blocking on Darwin (macOS
  loopback/network failures) and Windows-only clippy test imports fixed.

* Wed Nov 25 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.1.0-1
- Multiplayer alpha: dedicated server (TCP/JSON protocol v2) with
  server-authoritative survival (inventories, vitals, equipment, actions,
  furnace/chest windows, death/respawn, persistence), client connect mode with
  chunk streaming, avatars, chat/roster, and replicated mobs/items.
- Mobs and combat (cow/pig/sheep/chicken/zombie, AI, spawning, drops, attack,
  armor mitigation, knockback, day burn); ores (coal/iron/copper), furnace
  smelting/fuel, iron tools and leather/iron armor, chests.
- Procedural audio engine (PulseAudio on Linux, silent stub elsewhere) with
  footsteps/material sounds, UI/action sounds and ambient music, volumes in a
  new Audio settings tab.
- Worldgen caves/ravines and deterministic ore veins; benchmark/soak tooling
  (worldgen/server/save/soak) and published 1.0 performance envelope.
- Server operations: systemd unit, container image + compose, backup/restore
  scripts, load-test bot, admin handbook; graceful SIGINT/SIGTERM saves.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.9-1
- Readable F3 debug overlay: larger line spacing, four grouped sections with
  gaps and separators, dimmer secondary column, word-wrapped GPU name, adaptive
  scaling/stacking at small resolutions and a footer that no longer overlaps the
  hotbar.
- More breathing room in the settings, crafting, inventory and death screens
  (larger panels, row pitch/gaps, section spacing) with updated layout tests.
- Settings can now be lowered as well as raised: explicit clickable < and >
  arrows on range/choice rows, row-body clicks only focus, plus clearer footer
  hints; keyboard Left/Right and mouse wheel both directions verified.
- Dropped items are now collected from a 3 m radius around the player: ready
  items are attracted toward the player at 7 m/s and picked up at 0.9 m, with
  the existing pickup delay and partial-pickup behaviour.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.8-1
- Crafting and tools: planks, cobblestone and crafting table blocks; sticks and
  wooden/stone pickaxes, axes, shovels and swords; 11 recipes with table gating;
  tool-dependent mining speed and drops; tool durability; procedural tool icons
  and durability bars; C crafting screen plus RMB on a crafting table.
- Dropped items now spawn as visible, physics-simulated item entities and are
  picked up automatically (persisted in items.txt, covered by the state hash).
- A full tabbed Settings screen opened with Esc (Game/Display/Controls/Textures,
  keyboard + mouse, immediate persistence) replaces the old pause list.
- New block textures in all four procedural styles and resource-pack mappings;
  save format v4 with append-only migration from v1-v3.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.7-1
- Survival foundations: health/hunger, fall damage, starvation/regeneration,
  block drops into a 9-slot item inventory, apples from leaves, eating, death
  and respawn, persisted vitals/inventory, and a survival HUD (hearts, hunger,
  item counts, damage flash, pickup toasts, death screen).
- Crouch/sneak (Shift: slower, lower eye height, edge protection) and sprint
  (Ctrl with FOV boost); gravity/jump in walk mode; --survival/--creative.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.6-1
- Load Minecraft-format resource packs at runtime (--texture-pack PATH), with
  standard block filenames, animated-texture first frames, per-layer procedural
  fallback, vanilla biome tinting for grayscale grass/leaves and grass-side
  overlay compositing. The choice persists in config.json and can be toggled in
  the settings menu; pack assets are never bundled.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.5-1
- Inventory screen (E) with all blocks and hotbar assignment, 9-slot hotbar.
- Hold-to-mine with per-block hardness, progress bar and targeted-block outline.
- Day/night cycle with sun/moon, stars, moonlight and horizon-following fog.
- Five biomes (plains, forest, desert, snowy taiga, mountains) with blended
  terrain, biome surfaces, tree types and decorations.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.4-1
- Four selectable texture styles: classic (Minecraft-inspired), painterly
  (Hytale-inspired), lush (detailed nature) and minimal (clean modern).
- Full mip chains for block textures, alpha cut-out leaves, subtle per-quad
  brightness variation, distance fog and a sky gradient.
- Texture style selectable in the settings menu and via --texture-style.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.3-1
- HUD (crosshair, hotbar with block icons), F3 debug overlay and F1 toggle.
- Pause/settings menu (Esc): render distance, FOV, mouse sensitivity,
  fullscreen, HUD/debug toggles, save and quit; persisted to config.json.
- F2 in-game screenshot; parallel chunk meshing on worker threads; parallel
  chunk generation (8.8x); data-driven content pack loader; save format v3
  with a content-registry fingerprint.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.2-1
- Textured greedy-meshed rendering, frustum culling and procedural texture array.
- zstd-compressed save format v2 with compact light storage and v1 read support.
- World generation quality pass (mountains, snow peaks, shores, trees, boulders).
- Benchmark harness and CI render/bench/save-size gates.

* Tue Nov 24 2026 OpenBlock contributors <openblock@users.noreply.github.com> - 0.0.1-1
- First tech preview: windowed client, headless host, terrain, physics,
  block break/place, save/load, determinism smoke test.
