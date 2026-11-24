#!/usr/bin/env bash
# Architecture guard for DEC-003: every workspace crate is declared exactly
# once, internal edges are explicit, gpu_free crates stay headless, and the
# dedicated server never links presentation.
#
# Usage: .github/scripts/check-arch.sh   (run from the repository root or CI)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if ! command -v python3 >/dev/null 2>&1; then
  echo "::error::arch-guard requires python3 3.11+ (tomllib)" >&2
  exit 1
fi

python3 - <<'PY'
from __future__ import annotations

import json
import re
import subprocess
import sys
from collections import defaultdict, deque
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    print("::error::arch-guard: python3 3.11+ is required for tomllib")
    raise SystemExit(2)

ROOT = Path.cwd()
MANIFEST = ROOT / "architecture.toml"
TARGETS = [
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin",
]
LAYERS = {"foundation", "data", "presentation", "hosts", "tooling"}

errors: list[str] = []


def fail(message: str) -> None:
    print(f"::error::arch-guard: {message}")
    raise SystemExit(2)


def check(message: str) -> None:
    errors.append(message)


try:
    with MANIFEST.open("rb") as fh:
        doc = tomllib.load(fh)
except (OSError, tomllib.TOMLDecodeError) as exc:
    fail(f"cannot read {MANIFEST.name}: {exc}")

if doc.get("version") != 2:
    fail(f"{MANIFEST.name}: version must be 2")

raw_crates = doc.get("crates")
if not isinstance(raw_crates, list) or not raw_crates:
    fail(f"{MANIFEST.name}: [[crates]] must define at least one crate")

crates: dict[str, dict] = {}
for entry in raw_crates:
    if not isinstance(entry, dict):
        fail(f"{MANIFEST.name}: every [[crates]] entry must be a table")
    name = entry.get("name")
    if not isinstance(name, str) or not name:
        fail(f"{MANIFEST.name}: every [[crates]] entry needs a non-empty name")
    if name in crates:
        fail(f"{MANIFEST.name}: crate {name} appears more than once")
    unknown = set(entry) - {"name", "layer", "gpu_free", "allowed_deps"}
    if unknown:
        fail(f"{MANIFEST.name}: crates.{name} has unknown keys: {', '.join(sorted(unknown))}")
    if entry.get("layer") not in LAYERS:
        fail(f"{MANIFEST.name}: crates.{name}.layer must be one of {sorted(LAYERS)}")
    if not isinstance(entry.get("gpu_free"), bool):
        fail(f"{MANIFEST.name}: crates.{name}.gpu_free must be true or false")
    allowed = entry.get("allowed_deps")
    if not isinstance(allowed, list) or not all(isinstance(dep, str) for dep in allowed):
        fail(f"{MANIFEST.name}: crates.{name}.allowed_deps must be a list of crate names")
    if len(allowed) != len(set(allowed)):
        fail(f"{MANIFEST.name}: crates.{name}.allowed_deps contains duplicates")
    if name in allowed:
        fail(f"{MANIFEST.name}: crates.{name}.allowed_deps must not list itself")
    crates[name] = entry

for name, entry in crates.items():
    for dep in entry["allowed_deps"]:
        if dep not in crates:
            fail(f"{MANIFEST.name}: crates.{name} allows unknown crate {dep}")

pattern_doc = doc.get("forbidden_patterns")
if not isinstance(pattern_doc, dict):
    fail(f"{MANIFEST.name}: [forbidden_patterns] is missing")
gpu_patterns = pattern_doc.get("gpu_free")
if not isinstance(gpu_patterns, list) or not gpu_patterns:
    fail(f"{MANIFEST.name}: forbidden_patterns.gpu_free must be a non-empty list")
if not all(isinstance(pattern, str) for pattern in gpu_patterns):
    fail(f"{MANIFEST.name}: forbidden_patterns.gpu_free must contain strings")
try:
    compiled_patterns = [re.compile(pattern) for pattern in gpu_patterns]
except re.error as exc:
    fail(f"{MANIFEST.name}: invalid forbidden pattern: {exc}")


def load_metadata(target: str) -> dict:
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--all-features",
        "--filter-platform",
        target,
    ]
    process = subprocess.run(command, cwd=ROOT, text=True, capture_output=True)
    if process.returncode != 0:
        sys.stderr.write(process.stderr)
        fail(f"cargo metadata failed for {target} (exit {process.returncode})")
    try:
        return json.loads(process.stdout)
    except json.JSONDecodeError as exc:
        fail(f"cargo metadata produced invalid JSON for {target}: {exc}")


workspace: set[str] = set()
direct: defaultdict[str, set[str]] = defaultdict(set)
closure: defaultdict[str, set[str]] = defaultdict(set)
gpu_free = {name for name, spec in crates.items() if spec["gpu_free"] is True}

for target_index, target in enumerate(TARGETS):
    metadata = load_metadata(target)
    package_name = {package["id"]: package["name"] for package in metadata["packages"]}
    members = {package_name[package_id] for package_id in metadata["workspace_members"]}
    if target_index == 0:
        workspace = members
    elif members != workspace:
        check(f"workspace membership differs for {target}")

    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    adjacency: defaultdict[str, set[str]] = defaultdict(set)
    for node in nodes.values():
        source = package_name[node["id"]]
        for dependency in node.get("deps", []):
            dep_kinds = dependency.get("dep_kinds") or [{"kind": None}]
            if any(kind.get("kind") != "dev" for kind in dep_kinds):
                adjacency[source].add(package_name[dependency["pkg"]])

    for name in members:
        direct[name].update(dep for dep in adjacency[name] if dep in members)

    for name in gpu_free & members:
        seen: set[str] = set()
        queue = deque(adjacency[name])
        while queue:
            dependency = queue.popleft()
            if dependency in seen:
                continue
            seen.add(dependency)
            queue.extend(adjacency[dependency] - seen)
        closure[name].update(seen)

missing = workspace - set(crates)
extra = set(crates) - workspace
for name in sorted(missing):
    check(f"workspace crate {name} is missing from {MANIFEST.name}")
for name in sorted(extra):
    check(f"{MANIFEST.name} declares {name}, which is not a workspace crate")

for name in sorted(workspace & set(crates)):
    allowed = set(crates[name]["allowed_deps"])
    for dependency in sorted(direct.get(name, set()) - allowed):
        check(f"{name} depends on {dependency}, which is not in its allowed_deps")

for name in sorted(gpu_free & workspace):
    for dependency in sorted(closure.get(name, set())):
        for pattern in compiled_patterns:
            if pattern.search(dependency):
                check(
                    f"gpu_free crate {name} reaches forbidden dependency "
                    f"{dependency} (pattern {pattern.pattern})"
                )
                break

server = crates.get("ob_server")
if server is None:
    check(f"{MANIFEST.name} does not define ob_server")
else:
    if server["gpu_free"] is not True:
        check("ob_server must set gpu_free = true")
    presentation = {
        name for name, spec in crates.items() if spec["layer"] == "presentation"
    } | {"ob_client"}
    for dependency in sorted(closure.get("ob_server", set()) & presentation):
        check(f"ob_server reaches presentation crate {dependency}")

if errors:
    for message in errors:
        print(f"::error::arch-guard: {message}")
    print("Architecture guard failed (see architecture.toml).", file=sys.stderr)
    raise SystemExit(1)

print(
    f"Architecture guard: OK ({len(workspace)} crates, "
    f"{len(TARGETS)} target filters, {len(compiled_patterns)} forbidden patterns)"
)
PY
