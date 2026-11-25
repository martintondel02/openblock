#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""OpenBlock 1.0.1 performance/reliability audit harness.

Exercises the real `openblock-server` binary on loopback: repeated save/reload
restart cycles, connect/disconnect churn, a monitored soak with /proc RSS/fd/
thread sampling, corrupted/truncated sidecars, SIGTERM under autosave, a
save+stop race and an out-of-space/file-size-limit proxy.

It reuses the protocol bots and server wrapper from `scripts/load-gate.py` so
the reliability lane speaks the same protocol-v2 framing as the load gate.
Stdlib only, loopback only, no window and no GPU.

Examples:
    python3 scripts/reliability-audit.py --binary target/release/openblock-server
    python3 scripts/reliability-audit.py --soak-seconds 1800 --cycles 20 \\
        --json target/reliability.json --markdown target/reliability.md
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def load_gate_module():
    path = ROOT / "scripts" / "load-gate.py"
    spec = importlib.util.spec_from_file_location("openblock_load_gate", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/load-gate.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


lg = load_gate_module()


def gate_args(binary, clients, seconds, radius, save_interval, game_mode="creative", world="reliability"):
    return lg.parse_args(
        [
            "--binary",
            str(binary),
            "--clients",
            str(clients),
            "--seconds",
            str(seconds),
            "--interest-radius",
            str(radius),
            "--save-interval",
            str(save_interval),
            "--metrics-interval",
            "1",
            "--game-mode",
            game_mode,
            "--world",
            world,
        ]
    )


def read_metrics(path):
    try:
        with open(path, "r", encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(data, dict):
        return None
    return data


def wait_ready(server, timeout=30.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if server.proc.poll() is not None:
            raise RuntimeError("server exited during startup\n%s" % server.log_tail())
        snapshot = server.read_metrics()
        if snapshot and snapshot.get("listen"):
            return snapshot
        time.sleep(0.02)
    raise RuntimeError("server did not become ready within %.1f s" % timeout)


def start_server(binary, workdir, data_dir, args):
    server = lg.ServerProcess(binary, workdir, data_dir, args)
    server.start()
    snapshot = wait_ready(server, args.startup_timeout)
    host, port = snapshot["listen"].rsplit(":", 1)
    return server, host, int(port)


def stop_server(server, timeout=30.0):
    server.stop(timeout=timeout)
    return server.exit_code


def open_hash(binary, data_dir, world, seed=42):
    command = [
        str(binary),
        "--data-dir",
        str(data_dir),
        "--world",
        world,
        "--seed",
        str(seed),
        "--ticks",
        "0",
    ]
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=120, check=False)
    match = re.search(r"state hash ([0-9a-fA-F]+)", result.stdout)
    return {
        "exit": result.returncode,
        "hash": match.group(1) if match else None,
        "output": result.stdout.strip()[-600:],
    }


def reload_twice(binary, data_dir, world, seed=42):
    """Opens two independent copies of the saved world and compares hashes.

    `--ticks 0` is not a pure read in v1.0.0: `Host::open` streams the two-column
    ring around the loaded player and the headless path then saves, so a second
    open of the *same* directory can legitimately see more resident columns.
    Two identical copies isolate the determinism/integrity question from that
    streaming side effect.
    """
    base = Path(tempfile.mkdtemp(prefix="openblock-reload-"))
    try:
        first_copy = base / "a"
        second_copy = base / "b"
        shutil.copytree(data_dir, first_copy)
        shutil.copytree(data_dir, second_copy)
        first = open_hash(binary, first_copy, world, seed)
        second = open_hash(binary, second_copy, world, seed)
    finally:
        shutil.rmtree(base, ignore_errors=True)
    return {
        "ok": first["exit"] == 0 and second["exit"] == 0 and first["hash"] == second["hash"] and first["hash"] is not None,
        "hash": first["hash"],
        "first": first,
        "second": second,
    }


def world_file(data_dir, world):
    return Path(data_dir) / "worlds" / world / "world.obs"


def directory_bytes(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    return total


def connect_bot(host, port, args, stats, stop, index=0):
    bot = lg.Bot(index, "reliability-%02d" % index, host, port, args, stats, stop)
    bot.start()
    return bot


def run_cycles(binary, root, cycles):
    """Start/stop/reload the network server `cycles` times, verifying hashes."""
    args = gate_args(binary, 2, 2.0, 6, 60, world="cycles")
    workdir = root / "cycles-work"
    data_dir = root / "cycles-data"
    results = []
    for cycle in range(1, cycles + 1):
        server, host, port = start_server(binary, workdir, data_dir, args)
        stop = threading.Event()
        stats = lg.Stats()
        bots = [connect_bot(host, port, args, stats, stop, index) for index in range(2)]
        time.sleep(1.2)
        stop.set()
        for bot in bots:
            bot.join(timeout=5.0)
        exit_code = stop_server(server)
        reload = reload_twice(binary, data_dir, "cycles")
        world_path = world_file(data_dir, "cycles")
        results.append(
            {
                "cycle": cycle,
                "server_exit": exit_code,
                "joins": stats.snapshot()["joins"],
                "reload_ok": reload["ok"],
                "hash": reload["hash"],
                "world_bytes": world_path.stat().st_size if world_path.exists() else 0,
                "world_dir_bytes": directory_bytes(world_path.parent) if world_path.exists() else 0,
            }
        )
        print(
            "cycles: %d/%d exit=%s joins=%d reload=%s hash=%s bytes=%d"
            % (cycle, cycles, exit_code, results[-1]["joins"], "PASS" if reload["ok"] else "FAIL", reload["hash"], results[-1]["world_bytes"])
        )
    return {
        "cycles": cycles,
        "passed": all(row["server_exit"] == 0 and row["joins"] == 2 and row["reload_ok"] for row in results),
        "rows": results,
    }


def run_churn(binary, root, seconds, clients=8):
    """Connect/disconnect `clients` bots repeatedly, sampling /proc proxies."""
    args = gate_args(binary, clients, seconds, 6, 10, world="churn")
    workdir = root / "churn-work"
    data_dir = root / "churn-data"
    server, host, port = start_server(binary, workdir, data_dir, args)
    samples = []
    errors = []
    started = time.monotonic()
    cycle = 0
    while time.monotonic() - started < seconds:
        cycle += 1
        stats = lg.Stats()
        stop = threading.Event()
        bots = [connect_bot(host, port, args, stats, stop, index) for index in range(clients)]
        time.sleep(min(2.0, max(0.2, seconds - (time.monotonic() - started))))
        stop.set()
        for bot in bots:
            bot.join(timeout=5.0)
        snapshot = server.read_metrics() or {}
        samples.append(
            {
                "cycle": cycle,
                "joins": stats.snapshot()["joins"],
                "client_errors": len(stats.snapshot()["errors"]),
                "rss_kib": snapshot.get("rss_kib"),
                "open_fds": snapshot.get("open_fds"),
                "threads": snapshot.get("threads"),
                "players": snapshot.get("players_connected"),
                "tick_p99_ms": snapshot.get("tick_p99_ms"),
            }
        )
        if stats.snapshot()["errors"]:
            errors.extend(stats.snapshot()["errors"])
        time.sleep(0.1)
    exit_code = stop_server(server)
    reload = reload_twice(binary, data_dir, "churn")
    first = samples[0] if samples else {}
    last = samples[-1] if samples else {}
    warm = [row["rss_kib"] for row in samples[len(samples) // 4 :] if row["rss_kib"]]
    return {
        "seconds": seconds,
        "clients_per_cycle": clients,
        "cycles": cycle,
        "server_exit": exit_code,
        "client_errors": errors,
        "reload_ok": reload["ok"],
        "rss_first_kib": first.get("rss_kib"),
        "rss_last_kib": last.get("rss_kib"),
        "rss_warm_range_kib": (max(warm) - min(warm)) if warm else None,
        "fds_first": first.get("open_fds"),
        "fds_last": last.get("open_fds"),
        "threads_first": first.get("threads"),
        "threads_last": last.get("threads"),
        "samples": samples,
        "passed": exit_code == 0 and not errors and reload["ok"] and (last.get("open_fds", 0) or 0) <= (first.get("open_fds", 0) or 0) + 8,
    }


def run_soak(binary, root, seconds, clients=8):
    """One server, persistent bots, /proc samples every second."""
    args = gate_args(binary, clients, seconds, 6, 10, world="soak")
    workdir = root / "soak-work"
    data_dir = root / "soak-data"
    server, host, port = start_server(binary, workdir, data_dir, args)
    stats = lg.Stats()
    stop = threading.Event()
    bots = [connect_bot(host, port, args, stats, stop, index) for index in range(clients)]
    samples = []
    started = time.monotonic()
    while time.monotonic() - started < seconds:
        snapshot = server.read_metrics() or {}
        samples.append(
            {
                "elapsed": round(time.monotonic() - started, 2),
                "rss_kib": snapshot.get("rss_kib"),
                "open_fds": snapshot.get("open_fds"),
                "threads": snapshot.get("threads"),
                "players": snapshot.get("players_connected"),
                "tick_p99_ms": snapshot.get("tick_p99_ms"),
                "tick_max_ms": snapshot.get("tick_max_ms"),
                "tick_overruns": snapshot.get("tick_overruns"),
            }
        )
        time.sleep(1.0)
    stop.set()
    for bot in bots:
        bot.join(timeout=5.0)
    exit_code = stop_server(server)
    reload = reload_twice(binary, data_dir, "soak")
    rss = [row["rss_kib"] for row in samples if row["rss_kib"]]
    warm = rss[len(rss) // 4 :]
    fds = [row["open_fds"] for row in samples if row["open_fds"] is not None]
    threads = [row["threads"] for row in samples if row["threads"] is not None]
    log_analysis = lg.analyze_server_log(server.log_path)
    return {
        "seconds": seconds,
        "clients": clients,
        "server_exit": exit_code,
        "reload_ok": reload["ok"],
        "rss_start_kib": rss[0] if rss else None,
        "rss_end_kib": rss[-1] if rss else None,
        "rss_warm_range_kib": (max(warm) - min(warm)) if warm else None,
        "rss_warm_growth_kib": (warm[-1] - warm[0]) if warm else None,
        "fds_min": min(fds) if fds else None,
        "fds_max": max(fds) if fds else None,
        "threads_min": min(threads) if threads else None,
        "threads_max": max(threads) if threads else None,
        "autosaves": log_analysis["autosaves"],
        "autosave_overruns": log_analysis["autosave_overruns"],
        "overrun_warnings": log_analysis["overrun_warnings"],
        "samples": samples,
        "passed": exit_code == 0 and reload["ok"],
    }


def run_corruption(binary, root):
    """Truncate sidecars and world.obs in separate copies and check recovery."""
    args = gate_args(binary, 1, 1.0, 4, 60, world="corrupt")
    workdir = root / "corrupt-work"
    data_dir = root / "corrupt-data"
    server, host, port = start_server(binary, workdir, data_dir, args)
    stop = threading.Event()
    stats = lg.Stats()
    bot = connect_bot(host, port, args, stats, stop)
    time.sleep(1.0)
    stop.set()
    bot.join(timeout=5.0)
    stop_server(server)
    source = Path(data_dir) / "worlds" / "corrupt"
    rows = []
    for name in ("inventory.txt", "items.txt", "mobs.txt", "block_entities.txt", "spawn.txt", "world.obs"):
        case_dir = root / ("corrupt-" + name.replace(".", "-"))
        shutil.copytree(data_dir, case_dir)
        path = case_dir / "worlds" / "corrupt" / name
        original = path.read_bytes()
        truncated = original[: max(1, len(original) // 2)]
        path.write_bytes(truncated)
        result = open_hash(binary, case_dir, "corrupt")
        rows.append(
            {
                "file": name,
                "original_bytes": len(original),
                "truncated_bytes": len(truncated),
                "loaded": result["exit"] == 0 and result["hash"] is not None,
                "exit": result["exit"],
                "hash": result["hash"],
                "output_tail": result["output"][-300:],
            }
        )
        print("corruption: %s %d->%d loaded=%s exit=%s" % (name, len(original), len(truncated), rows[-1]["loaded"], result["exit"]))
    return {
        "rows": rows,
        "sidecars_recover": all(row["loaded"] for row in rows if row["file"] != "world.obs"),
        "world_obs_refused": next(row for row in rows if row["file"] == "world.obs")["exit"] != 0,
        "world_obs_recovered": next(row for row in rows if row["file"] == "world.obs")["loaded"],
        "world_obs_detected": (
            next(row for row in rows if row["file"] == "world.obs")["exit"] != 0
            or next(row for row in rows if row["file"] == "world.obs")["hash"] is not None
        ),
    }


def run_sigterm(binary, root, iterations, radius=6, wait_seconds=0.35):
    """SIGTERM the server while an autosave timer is armed; verify the snapshot."""
    args = gate_args(binary, 1, 3.0, radius, 1, world="sigterm")
    workdir = root / "sigterm-work"
    data_dir = root / "sigterm-data"
    rows = []
    for iteration in range(1, iterations + 1):
        server, host, port = start_server(binary, workdir, data_dir, args)
        stop = threading.Event()
        stats = lg.Stats()
        bot = connect_bot(host, port, args, stats, stop)
        time.sleep(wait_seconds + (iteration % 5) * 0.02)
        started = time.monotonic()
        server.proc.send_signal(signal.SIGTERM)
        try:
            exit_code = server.proc.wait(timeout=30.0)
        except subprocess.TimeoutExpired:
            server.proc.kill()
            exit_code = server.proc.wait(timeout=5.0)
        elapsed = time.monotonic() - started
        stop.set()
        bot.join(timeout=5.0)
        if server.log_handle is not None:
            server.log_handle.close()
            server.log_handle = None
        reload = reload_twice(binary, data_dir, "sigterm")
        log_text = Path(server.log_path).read_text(encoding="utf-8", errors="replace")
        rows.append(
            {
                "iteration": iteration,
                "exit": exit_code,
                "shutdown_seconds": round(elapsed, 3),
                "reload_ok": reload["ok"],
                "hash": reload["hash"],
                "autosaves": log_text.count("autosaved world"),
                "shutdown_logged": "shutdown signal received" in log_text,
            }
        )
        print(
            "sigterm: %d/%d exit=%s shutdown=%.2fs autosaves=%d reload=%s hash=%s"
            % (iteration, iterations, exit_code, elapsed, rows[-1]["autosaves"], "PASS" if reload["ok"] else "FAIL", reload["hash"])
        )
    return {
        "iterations": iterations,
        "passed": all(row["exit"] == 0 and row["reload_ok"] and row["shutdown_logged"] for row in rows),
        "rows": rows,
    }


def run_save_stop_race(binary, root, iterations=5):
    """Request a console save and stop back-to-back, then verify the snapshot."""
    args = gate_args(binary, 1, 2.0, 4, 0, world="save-stop")
    workdir = root / "save-stop-work"
    data_dir = root / "save-stop-data"
    rows = []
    for iteration in range(1, iterations + 1):
        server, host, port = start_server(binary, workdir, data_dir, args)
        stop = threading.Event()
        stats = lg.Stats()
        bot = connect_bot(host, port, args, stats, stop)
        time.sleep(0.5)
        try:
            server.proc.stdin.write(b"save\nstop\n")
            server.proc.stdin.flush()
        except (BrokenPipeError, OSError):
            pass
        try:
            exit_code = server.proc.wait(timeout=30.0)
        except subprocess.TimeoutExpired:
            server.proc.kill()
            exit_code = server.proc.wait(timeout=5.0)
        stop.set()
        bot.join(timeout=5.0)
        if server.log_handle is not None:
            server.log_handle.close()
            server.log_handle = None
        reload = reload_twice(binary, data_dir, "save-stop")
        rows.append({"iteration": iteration, "exit": exit_code, "reload_ok": reload["ok"], "hash": reload["hash"]})
        print("save+stop: %d/%d exit=%s reload=%s" % (iteration, iterations, exit_code, "PASS" if reload["ok"] else "FAIL"))
    return {"iterations": iterations, "passed": all(row["exit"] == 0 and row["reload_ok"] for row in rows), "rows": rows}


def run_disk_pressure(binary, root):
    """File-size-limit and read-only-directory proxies for a full disk."""
    args = gate_args(binary, 1, 1.0, 4, 0, world="disk")
    workdir = root / "disk-work"
    data_dir = root / "disk-data"
    server, host, port = start_server(binary, workdir, data_dir, args)
    stop = threading.Event()
    stats = lg.Stats()
    bot = connect_bot(host, port, args, stats, stop)
    time.sleep(0.8)
    stop.set()
    bot.join(timeout=5.0)
    stop_server(server)
    baseline = reload_twice(binary, data_dir, "disk")

    # Read-only world directory: save must fail and leave the last snapshot.
    readonly_dir = root / "disk-readonly"
    shutil.copytree(data_dir, readonly_dir)
    world_dir = readonly_dir / "worlds" / "disk"
    world_path = world_dir / "world.obs"
    before = world_path.read_bytes()
    os.chmod(world_dir, 0o555)
    save_result = subprocess.run(
        [str(binary), "--data-dir", str(readonly_dir), "--world", "disk", "--seed", "42", "--ticks", "20"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=120,
        check=False,
    )
    os.chmod(world_dir, 0o755)
    after = world_path.read_bytes() if world_path.exists() else b""
    readonly_reload = reload_twice(binary, readonly_dir, "disk")

    # RLIMIT_FSIZE proxy: the snapshot write exceeds the cap mid-save.
    limited_dir = root / "disk-limited"
    shutil.copytree(data_dir, limited_dir)
    limited_world = limited_dir / "worlds" / "disk" / "world.obs"
    limited_before = limited_world.read_bytes()
    command = (
        "ulimit -f 64; exec %s --data-dir %s --world disk --seed 42 --ticks 20"
        % (str(binary), str(limited_dir))
    )
    limited = subprocess.run(
        ["bash", "-c", command],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=120,
        check=False,
    )
    limited_after = limited_world.read_bytes() if limited_world.exists() else b""
    limited_reload = reload_twice(binary, limited_dir, "disk")
    world_ok = limited_after == limited_before
    return {
        "baseline_reload_ok": baseline["ok"],
        "readonly_save_exit": save_result.returncode,
        "readonly_save_failed": save_result.returncode != 0,
        "readonly_snapshot_unchanged": before == after,
        "readonly_reload_ok": readonly_reload["ok"],
        "readonly_log_tail": save_result.stdout.strip()[-300:],
        "limited_exit": limited.returncode,
        "limited_snapshot_unchanged": world_ok,
        "limited_snapshot_valid": limited_reload["ok"],
        "limited_log_tail": limited.stdout.strip()[-300:],
        "passed": baseline["ok"] and readonly_reload["ok"] and limited_reload["ok"] and world_ok,
    }


def render_markdown(report):
    lines = ["# OpenBlock reliability audit", ""]
    lines.append("> Generated: %s  " % report["generated_at"])
    lines.append("> Machine: %s (%d vCPU), %s  " % (report["environment"]["cpu"], report["environment"]["cpu_count"], report["environment"]["os"]))
    lines.append("> Command: `%s`" % report["command"])
    lines.append("")
    cycles = report["cycles"]
    lines += ["## Restart cycles", "", "| Metric | Value |", "| --- | ---: |"]
    lines.append("| Cycles | %d |" % cycles["cycles"])
    if cycles["rows"]:
        pass_count = sum(1 for row in cycles["rows"] if row["server_exit"] == 0 and row["joins"] == 2 and row["reload_ok"])
        lines.append("| Passed cycles | %d/%d |" % (pass_count, cycles["cycles"]))
        lines.append("| Unique hashes | %d |" % len({row["hash"] for row in cycles["rows"] if row["hash"]}))
        lines.append("| World dir range | %d..%d B |" % (min(row["world_dir_bytes"] for row in cycles["rows"]), max(row["world_dir_bytes"] for row in cycles["rows"])))
    else:
        lines.append("| Passed cycles | skipped |")
    lines.append("")
    churn = report["churn"]
    lines += ["## Connect/disconnect churn", "", "| Metric | Value |", "| --- | ---: |"]
    lines.append("| Duration / cycles | %.1f s / %d |" % (churn["seconds"], churn["cycles"]))
    lines.append("| Clients per cycle | %d |" % churn["clients_per_cycle"])
    lines.append("| Client errors | %d |" % len(churn["client_errors"]))
    lines.append("| FDs first -> last | %s -> %s |" % (churn["fds_first"], churn["fds_last"]))
    lines.append("| Threads first -> last | %s -> %s |" % (churn["threads_first"], churn["threads_last"]))
    lines.append("| RSS first / last / warm range | %s / %s / %s KiB |" % (churn["rss_first_kib"], churn["rss_last_kib"], churn["rss_warm_range_kib"]))
    lines.append("| Reload hash stable | %s |" % ("PASS" if churn["reload_ok"] else "FAIL"))
    lines.append("")
    soak = report["soak"]
    lines += ["## Soak", "", "| Metric | Value |", "| --- | ---: |"]
    lines.append("| Duration / clients | %.0f s / %d |" % (soak["seconds"], soak["clients"]))
    lines.append("| RSS start / end / warm range / warm growth | %s / %s / %s / %s KiB |" % (
        soak["rss_start_kib"], soak["rss_end_kib"], soak["rss_warm_range_kib"], soak["rss_warm_growth_kib"]))
    lines.append("| FDs min..max / threads min..max | %s..%s / %s..%s |" % (
        soak["fds_min"], soak["fds_max"], soak["threads_min"], soak["threads_max"]))
    lines.append("| Autosaves / autosave overruns / warnings | %d / %d / %d |" % (
        soak["autosaves"], soak["autosave_overruns"], soak["overrun_warnings"]))
    lines.append("| Reload hash stable | %s |" % ("PASS" if soak["reload_ok"] else "FAIL"))
    lines.append("")
    corruption = report["corruption"]
    lines += ["## Corrupted/truncated files", "", "| File | Original -> truncated B | Loaded | Exit |", "| --- | ---: | :---: | ---: |"]
    for row in corruption["rows"]:
        lines.append("| %s | %d -> %d | %s | %d |" % (
            row["file"], row["original_bytes"], row["truncated_bytes"], "PASS" if row["loaded"] else "FAIL", row["exit"]))
    lines.append("")
    sigterm = report["sigterm"]
    lines += ["## SIGTERM under autosave", "", "| Metric | Value |", "| --- | ---: |"]
    lines.append("| Iterations | %d |" % sigterm["iterations"])
    lines.append("| Passed | %d/%d |" % (sum(1 for row in sigterm["rows"] if row["exit"] == 0 and row["reload_ok"]), sigterm["iterations"]))
    if sigterm["rows"]:
        lines.append("| Shutdown range | %.2f..%.2f s |" % (
            min(row["shutdown_seconds"] for row in sigterm["rows"]), max(row["shutdown_seconds"] for row in sigterm["rows"])))
    else:
        lines.append("| Shutdown range | skipped |")
    lines.append("")
    race = report["save_stop_race"]
    lines += ["## Save + stop race", "", "| Metric | Value |", "| --- | ---: |"]
    lines.append("| Iterations | %d |" % race["iterations"])
    lines.append("| Passed | %d/%d |" % (sum(1 for row in race["rows"] if row["exit"] == 0 and row["reload_ok"]), race["iterations"]))
    lines.append("")
    disk = report["disk_pressure"]
    lines += ["## Disk pressure proxies", "", "| Metric | Value |", "| --- | ---: |"]
    if "readonly_save_exit" not in disk:
        lines.append("| Result | skipped |")
        lines.append("")
        return "\n".join(lines) + "\n"
    lines.append("| Read-only save exit / failed | %d / %s |" % (disk["readonly_save_exit"], disk["readonly_save_failed"]))
    lines.append("| Read-only snapshot unchanged | %s |" % disk["readonly_snapshot_unchanged"])
    lines.append("| Read-only reload | %s |" % ("PASS" if disk["readonly_reload_ok"] else "FAIL"))
    lines.append("| File-size-limit exit | %d |" % disk["limited_exit"])
    lines.append("| File-size-limit snapshot unchanged / valid | %s / %s |" % (disk["limited_snapshot_unchanged"], disk["limited_snapshot_valid"]))
    lines.append("")
    return "\n".join(lines) + "\n"


def parse_args(argv):
    parser = argparse.ArgumentParser(description="OpenBlock reliability audit harness.")
    parser.add_argument("--binary", required=True, help="openblock-server binary")
    parser.add_argument("--cycles", type=int, default=20, help="start/stop/reload cycles (default 20)")
    parser.add_argument("--churn-seconds", type=float, default=60.0, help="connect/disconnect churn duration (default 60 s)")
    parser.add_argument("--soak-seconds", type=float, default=120.0, help="persistent-bot soak duration (default 120 s)")
    parser.add_argument("--sigterm-iterations", type=int, default=10, help="SIGTERM autosave iterations (default 10)")
    parser.add_argument("--sigterm-radius", type=int, default=6, help="interest radius for the SIGTERM lane (default 6)")
    parser.add_argument("--sigterm-wait", type=float, default=0.35, help="seconds before SIGTERM, before per-iteration jitter (default 0.35)")
    parser.add_argument("--skip-churn", action="store_true", help="skip the churn lane")
    parser.add_argument("--skip-soak", action="store_true", help="skip the soak lane")
    parser.add_argument("--json", help="write JSON result")
    parser.add_argument("--markdown", help="write Markdown result")
    parser.add_argument("--keep", action="store_true", help="keep the temporary data root")
    return parser.parse_args(argv)


def main(argv=None):
    options = parse_args(argv)
    binary = Path(options.binary).resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise SystemExit("server binary not found or not executable: %s" % binary)
    root = Path(tempfile.mkdtemp(prefix="openblock-reliability-"))
    started = time.monotonic()
    try:
        print("reliability: root %s" % root)
        cycles = run_cycles(binary, root, options.cycles)
        churn = (
            {"seconds": 0, "cycles": 0, "clients_per_cycle": 8, "client_errors": [], "reload_ok": True,
             "fds_first": None, "fds_last": None, "threads_first": None, "threads_last": None,
             "rss_first_kib": None, "rss_last_kib": None, "rss_warm_range_kib": None, "passed": True}
            if options.skip_churn
            else run_churn(binary, root, options.churn_seconds)
        )
        soak = (
            {"seconds": 0, "clients": 8, "reload_ok": True, "rss_start_kib": None, "rss_end_kib": None,
             "rss_warm_range_kib": None, "rss_warm_growth_kib": None, "fds_min": None, "fds_max": None,
             "threads_min": None, "threads_max": None, "autosaves": 0, "autosave_overruns": 0,
             "overrun_warnings": 0, "passed": True}
            if options.skip_soak
            else run_soak(binary, root, options.soak_seconds)
        )
        corruption = run_corruption(binary, root)
        sigterm = run_sigterm(binary, root, options.sigterm_iterations)
        race = run_save_stop_race(binary, root)
        disk = run_disk_pressure(binary, root)
        report = {
            "schema": "openblock.reliability-audit/1",
            "generated_at": lg.utc_now(),
            "command": " ".join(sys.argv),
            "environment": {
                "cpu": lg.cpu_model(),
                "cpu_count": lg.cpu_count(),
                "os": lg.os_description(),
                "python": sys.version.split()[0],
            },
            "duration_seconds": round(time.monotonic() - started, 1),
            "cycles": cycles,
            "churn": churn,
            "soak": soak,
            "corruption": corruption,
            "sigterm": sigterm,
            "save_stop_race": race,
            "disk_pressure": disk,
        }
        passed = (
            cycles["passed"]
            and churn["passed"]
            and soak["passed"]
            and corruption["sidecars_recover"]
            and corruption["world_obs_detected"]
            and sigterm["passed"]
            and race["passed"]
            and disk["passed"]
        )
        report["result"] = "pass" if passed else "fail"
        if options.json:
            Path(options.json).write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        if options.markdown:
            Path(options.markdown).write_text(render_markdown(report), encoding="utf-8")
        print("result: %s in %.1f s" % (report["result"].upper(), report["duration_seconds"]))
        return 0 if passed else 1
    finally:
        if options.keep:
            print("kept reliability root: %s" % root)
        else:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
