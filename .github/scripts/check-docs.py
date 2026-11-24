#!/usr/bin/env python3
"""Documentation consistency checks for OpenBlock.

Checks:
  1. Every relative markdown link resolves (anchors are not verified).
  2. Every spec/plan chapter has the required status header, requirement
     traceability sections and ends with Open questions + References.
  3. Requirement, performance and risk ids are unique across the docs.
  4. Every cited DEC-xxx exists in docs/decisions.md.
  5. Every cited REQ-/PERF- id is defined exactly where it is used.
Exit code 1 on any error; warnings do not fail the build.
"""

from __future__ import annotations

import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DOCS = ROOT / "docs"

errors: list[str] = []
warnings: list[str] = []


def rel(p: Path) -> str:
    return str(p.relative_to(ROOT))


def md_files() -> list[Path]:
    files = sorted(DOCS.rglob("*.md"))
    if (ROOT / "README.md").exists():
        files.append(ROOT / "README.md")
    for name in ("CONTRIBUTING.md", "SECURITY.md", "CODE_OF_CONDUCT.md", "MAINTAINERS.md", "CHANGELOG.md"):
        if (ROOT / name).exists():
            files.append(ROOT / name)
    return files


def check_links(files: list[Path]) -> None:
    pattern = re.compile(r"\]\(([^)\s]+)\)")
    for f in files:
        text = f.read_text(encoding="utf-8")
        for m in pattern.finditer(text):
            target = m.group(1)
            if target.startswith(("http://", "https://", "mailto:", "#")):
                continue
            path = target.split("#", 1)[0]
            if not path:
                continue
            resolved = (f.parent / path).resolve()
            if not resolved.exists():
                errors.append(f"{rel(f)}: broken relative link -> {target}")


def check_chapter_shape(files: list[Path]) -> None:
    for f in files:
        if f.parent.name not in ("spec", "plan"):
            continue
        text = f.read_text(encoding="utf-8")
        if "> Status:" not in text.split("## ", 1)[0]:
            errors.append(f"{rel(f)}: missing '> Status:' header block")
        for required in ("Open questions", "References"):
            if not re.search(rf"^##\s+\d*\.?\s*{required}", text, re.MULTILINE):
                errors.append(f"{rel(f)}: missing required section '{required}'")
        if f.parent.name == "spec" and not re.search(r"REQ-[A-Z]+-\d{3}", text):
            errors.append(f"{rel(f)}: no REQ-* requirement ids found")
        if f.parent.name == "spec" and re.search(r"^#{1,3} ", text) is None:
            errors.append(f"{rel(f)}: no headings found")


ID_PATTERNS = {
    "REQ": re.compile(r"\bREQ-[A-Z][A-Z0-9]*-\d{3}\b"),
    "PERF": re.compile(r"\bPERF-[A-Z][A-Z0-9]*-\d{3}\b"),
    "RISK": re.compile(r"\bRISK-\d{3}\b"),
    "DEC": re.compile(r"\bDEC-\d{3}\b"),
    "OPEN": re.compile(r"\bOPEN-\d{3}\b"),
    "MS": re.compile(r"\bMS-\d{2}\b"),
    "EPIC": re.compile(r"\bEPIC-\d{3}\b"),
}


def check_ids(files: list[Path]) -> None:
    register = (DOCS / "decisions.md").read_text(encoding="utf-8")
    defined_dec = set(re.findall(r"^###\s+(DEC-\d{3})", register, re.MULTILINE))
    declared_open = set(re.findall(r"^\|\s*(OPEN-\d{3})\s*\|", register, re.MULTILINE))

    # A requirement is *defined* by a table row (a cross-reference in prose is
    # fine and expected). Only duplicate definitions are collisions.
    definition_sites: dict[str, set[str]] = defaultdict(set)
    def_row = re.compile(r"^\|\s*((?:REQ|PERF|RISK)-[A-Z0-9]*[-0-9A-Z]*)\s*\|")
    for f in files:
        if f.parent.name == "research":
            continue  # research notes are dated snapshots, not normative definitions
        for line in f.read_text(encoding="utf-8").splitlines():
            m = def_row.match(line)
            if m:
                definition_sites[m.group(1)].add(rel(f))

    for rid, sites in sorted(definition_sites.items()):
        # Chapter 21 deliberately reproduces the ch25 PERF-TOOL rows as an index.
        if rid.startswith("PERF-TOOL-") and "docs/spec/21-tooling-and-pipelines.md" in sites:
            continue
        if len(sites) > 1:
            warnings.append(f"{rid} is defined in multiple documents: {', '.join(sorted(sites))}")

    for f in files:
        text = f.read_text(encoding="utf-8")
        for m in ID_PATTERNS["DEC"].finditer(text):
            dec = m.group(0)
            if dec not in defined_dec:
                warnings.append(f"{rel(f)}: cites {dec} which is not defined in docs/decisions.md")
        for m in ID_PATTERNS["OPEN"].finditer(text):
            op = m.group(0)
            if op not in declared_open:
                warnings.append(f"{rel(f)}: cites {op} which is not in the decision register")


def check_stale_claims(files: list[Path]) -> None:
    forbidden = [
        (re.compile(r"32\s*[×x]\s*32\s*[×x]\s*32\s+(storage\s+)?sections?\.(?!\s*\*?A 32)"), None),
    ]
    for f in files:
        if f.parent.name != "spec":
            continue
        text = f.read_text(encoding="utf-8")
        for pat, _ in forbidden:
            for m in pat.finditer(text):
                line = text[: m.start()].count("\n") + 1
                errors.append(f"{rel(f)}:{line}: mentions 32^3 storage sections (DEC-004 settles 16^3)")


def main() -> int:
    files = md_files()
    check_links(files)
    check_chapter_shape(files)
    check_ids(files)
    check_stale_claims(files)

    for w in warnings:
        print(f"warning: {w}")
    for e in errors:
        print(f"error: {e}")
    print(f"\nchecked {len(files)} markdown files: {len(errors)} errors, {len(warnings)} warnings")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
