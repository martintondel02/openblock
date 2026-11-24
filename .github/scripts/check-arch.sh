#!/usr/bin/env bash
# Dependency guard for DEC-003: the headless server must not link GPU, window
# or audio crates, and no crate may depend on a layer above it.
set -euo pipefail

FORBIDDEN='wgpu|winit|ash|ash-window|cpal|kira|gilrs|raw-window-handle'
HEADLESS='ob_core ob_math ob_world ob_worldgen ob_content ob_sim ob_protocol ob_net ob_modding'

fail=0
for crate in $HEADLESS; do
  tree="$(cargo tree -p "$crate" -e normal,build --prefix none --no-dedupe 2>/dev/null || true)"
  if echo "$tree" | grep -Eq "^($FORBIDDEN)[ v]"; then
    echo "::error::$crate must stay headless but links a forbidden dependency:"
    echo "$tree" | grep -E "^($FORBIDDEN)[ v]" | sort -u
    fail=1
  fi
done

# The dedicated server binary additionally must not pull presentation crates.
server_tree="$(cargo tree -p ob_server -e normal,build --prefix none --no-dedupe 2>/dev/null || true)"
if echo "$server_tree" | grep -Eq "^(ob_render|ob_ui|ob_audio)[ v]"; then
  echo "::error::ob_server must not depend on presentation crates"
  echo "$server_tree" | grep -E "^(ob_render|ob_ui|ob_audio)[ v]" | sort -u
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "Architecture guard failed (see architecture.toml)."
  exit 1
fi
echo "Architecture guard: OK"
