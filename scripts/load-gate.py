#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""OpenBlock 1.0 multiplayer load gate: spawn a real server, drive protocol-v2
bots, and report the tick latency, throughput and memory numbers against the
8-player / 6-chunk gate.

The script owns the whole scenario so a run is reproducible from one command:

1. start `openblock-server` on loopback with an ephemeral port, a fresh (or
   caller-supplied) data directory, a fixed seed and a periodic JSON metrics
   snapshot;
2. connect N stock Python bots that speak the JSON/TCP framing of
   docs/spec/18-networking-multiplayer.md §3.13, wait for their interest set
   (169 columns at radius 6), and drive periodic Move (20/s), block edits,
   chat and ping traffic for a fixed duration;
3. collect the server metrics snapshots and the bots' own counters, emit JSON
   and Markdown, and exit non-zero on functional failures or a target miss.

Examples:
    # full 1.0 gate: 8 bots, 60 s, enforce the 35 ms p99 tick target
    python3 scripts/load-gate.py --binary target/release/openblock-server \\
        --json target/load-gate.json --markdown target/load-gate.md

    # CI smoke: 4 bots for 12 s; the tick target is reported but advisory
    python3 scripts/load-gate.py --quick --binary target/release/openblock-server

    # 10-minute soak with 10 s autosaves, RSS/fd/thread plateaus and a
    # save/reload state-hash check
    python3 scripts/load-gate.py --soak --binary target/release/openblock-server \\
        --json target/soak.json --markdown target/soak.md

Stdlib only, loopback TCP only, no window and no GPU. The bots deliberately do
not decode chunk payloads or simulate physics; this is a load/soak harness, not
a protocol conformance or anti-cheat test.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import random
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

PROTOCOL_VERSION = 2
MAX_FRAME_LEN = 2 * 1024 * 1024
CHUNK_SIZE = 16
DEFAULT_RADIUS = 6
DEFAULT_TARGET_P99_MS = 35.0
DEFAULT_CLIENTS = 8
DEFAULT_SECONDS = 60.0
QUICK_CLIENTS = 4
QUICK_SECONDS = 12.0
SOAK_SECONDS = 600.0
SOAK_AUTOSAVE_SECONDS = 10
DEFAULT_RSS_BUDGET_MIB = 2 * 1024  # plus 20 MiB/player (PERF-SRV-003)

CHUNK_PREFIX = b'{"type":"chunk_data"'
CHUNK_COORDS = re.compile(rb'"cx":(-?\d+),"cz":(-?\d+)')
CHAT_TOKEN = re.compile(r"gate (\S+)")

ROOT = Path(__file__).resolve().parents[1]


def percentile(values, fraction):
    """Nearest-rank percentile, matching the Rust benchmark helpers."""
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int(math.ceil(fraction * len(ordered))) - 1))
    return ordered[index]


def fmt_ms(value):
    if value is None:
        return "n/a"
    return "%.3f" % value


def fmt_bytes(value):
    if value is None:
        return "n/a"
    if value >= 1024 * 1024:
        return "%.2f MiB" % (value / (1024.0 * 1024.0))
    if value >= 1024:
        return "%.1f KiB" % (value / 1024.0)
    return "%d B" % value


def fmt_mib(kib):
    if kib is None:
        return "n/a"
    return "%.1f MiB" % (kib / 1024.0)


def utc_now():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def encode_message(message):
    payload = json.dumps(message, separators=(",", ":")).encode("utf-8")
    if not payload or len(payload) > MAX_FRAME_LEN:
        raise ValueError("frame length out of range: %d" % len(payload))
    return struct.pack(">I", len(payload)) + payload


def recv_exact(sock, count):
    chunks = []
    remaining = count
    while remaining:
        data = sock.recv(min(65536, remaining))
        if not data:
            raise ConnectionError("server closed the connection")
        chunks.append(data)
        remaining -= len(data)
    return b"".join(chunks)


def recv_frame(sock):
    header = recv_exact(sock, 4)
    length = struct.unpack(">I", header)[0]
    if length == 0:
        raise ConnectionError("server sent an empty frame")
    if length > MAX_FRAME_LEN:
        raise ConnectionError("server frame of %d bytes exceeds the limit" % length)
    payload = recv_exact(sock, length)
    return json.loads(payload.decode("utf-8"))


def cpu_model():
    try:
        with open("/proc/cpuinfo", encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "unknown"


def cpu_count():
    try:
        return len(os.sched_getaffinity(0))
    except AttributeError:
        return os.cpu_count() or 1


def os_description():
    kernel = platform.release()
    return "%s %s (%s)" % (platform.system(), kernel, platform.machine())


def default_server_binary():
    candidates = []
    target = os.environ.get("CARGO_TARGET_DIR")
    if target:
        candidates.append(Path(target) / "release" / "openblock-server")
    candidates.extend(
        [
            ROOT / "target" / "release" / "openblock-server",
            ROOT / "target-1.0-load" / "release" / "openblock-server",
        ]
    )
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return candidates[0]


class GateError(RuntimeError):
    """A setup or server failure that prevents a measurement."""


class Stats:
    """Thread-safe aggregate counters shared by every bot."""

    def __init__(self):
        self.lock = threading.Lock()
        self.hellos = 0
        self.joins = 0
        self.chunks = 0
        self.disconnects = 0
        self.errors = []
        self.rejections = Counter()
        self.action_rejections = 0
        self.messages = Counter()
        self.sent = Counter()
        self.pings = []
        self.chat_latencies = []
        self.bytes_received = 0
        self.clients_finished = 0
        self.own_edit_broadcasts = 0
        self.chat_sent_at = {}

    def add_error(self, label, error):
        with self.lock:
            self.errors.append("%s: %s" % (label, error))

    def inc(self, field, amount=1):
        with self.lock:
            setattr(self, field, getattr(self, field) + amount)

    def add_message(self, kind, size):
        with self.lock:
            self.messages[kind] += 1
            self.bytes_received += size

    def add_sent(self, kind):
        with self.lock:
            self.sent[kind] += 1

    def add_ping(self, milliseconds):
        with self.lock:
            self.pings.append(milliseconds)

    def add_rejection(self, action, reason):
        with self.lock:
            self.action_rejections += 1
            self.rejections["%s:%s" % (action, reason)] += 1

    def add_own_edit(self):
        with self.lock:
            self.own_edit_broadcasts += 1

    def note_chat(self, token):
        with self.lock:
            self.chat_sent_at[token] = time.monotonic()

    def complete_chat(self, token):
        with self.lock:
            sent = self.chat_sent_at.pop(token, None)
            if sent is not None:
                self.chat_latencies.append((time.monotonic() - sent) * 1000.0)

    def finish(self):
        with self.lock:
            self.clients_finished += 1

    def snapshot(self):
        with self.lock:
            return {
                "hellos": self.hellos,
                "joins": self.joins,
                "chunks": self.chunks,
                "disconnects": self.disconnects,
                "errors": list(self.errors),
                "rejections": dict(self.rejections),
                "action_rejections": self.action_rejections,
                "messages": dict(self.messages),
                "sent": dict(self.sent),
                "pings": list(self.pings),
                "chat_latencies": list(self.chat_latencies),
                "bytes_received": self.bytes_received,
                "clients_finished": self.clients_finished,
                "own_edit_broadcasts": self.own_edit_broadcasts,
            }


class Bot(threading.Thread):
    """One minimal protocol-v2 bot: handshake, streaming, Move, edit, chat, ping."""

    def __init__(self, index, name, host, port, args, stats, stop):
        super().__init__(daemon=True, name="bot-%02d" % index)
        self.index = index
        self.bot_name = name
        self.host = host
        self.port = port
        self.args = args
        self.stats = stats
        self.stop = stop
        self.sock = None
        self.buffer = bytearray()
        self.player_id = None
        self.spawn = [0.0, 0.0, 0.0]
        self.home = [0.0, 0.0, 0.0]
        self.stone_id = 1
        self.join_time = None
        self.interest_loaded_seconds = None
        self.received_chunks = set()
        self.expected_chunks = set()
        self.pending_pings = {}
        self.play_start = None
        self.last_move = None
        self.next_move = 0.0
        self.next_ping = 0.0
        self.next_chat = 0.0
        self.next_edit = 0.0
        self.next_request = 0.0
        self.chat_sequence = 0
        self.ping_sequence = 0
        self.edit_dx, self.edit_dz = self._edit_offset(index)

    def _edit_offset(self, index):
        offsets = [(dx, dz) for dz in (-2, 2) for dx in (-2, -1, 0, 1, 2)]
        return offsets[index % len(offsets)]

    def log(self, message):
        if self.args.verbose:
            print("[%s] %s" % (self.bot_name, message), file=sys.stderr)

    def send(self, message, kind):
        self.sock.sendall(encode_message(message))
        self.stats.add_sent(kind)

    def run(self):
        try:
            self._connect_and_handshake()
            self._play()
        except Exception as error:  # noqa: BLE001 - report any client failure
            if self.stop.is_set() and isinstance(error, (ConnectionError, OSError)):
                self.log("connection closed during shutdown")
            elif not self.stop.is_set():
                self.stats.add_error(self.bot_name, error)
        finally:
            if self.sock is not None:
                try:
                    self.send({"type": "disconnect"}, "disconnect")
                except Exception:  # noqa: BLE001 - best effort
                    pass
                try:
                    self.sock.close()
                except OSError:
                    pass
            self.stats.finish()

    def _connect_and_handshake(self):
        deadline = time.monotonic() + self.args.connect_timeout
        self.sock = socket.create_connection((self.host, self.port), timeout=self.args.connect_timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.sock.settimeout(self.args.connect_timeout)
        self.send({"type": "hello", "protocol": PROTOCOL_VERSION, "name": self.bot_name}, "hello")
        while time.monotonic() < deadline:
            message = recv_frame(self.sock)
            kind = message.get("type")
            self.stats.add_message(kind or "unknown", 0)
            if kind == "hello":
                self.stats.inc("hellos")
                self.spawn = [float(value) for value in message.get("spawn", [0.0, 0.0, 0.0])]
                # Stay in the spawn column: a 0.15 m circle and a 0.2 m offset
                # cannot cross a chunk edge because spawn coordinates end in .5.
                self.home = [
                    self.spawn[0] + 0.2 * (self.index % 3 - 1),
                    self.spawn[1],
                    self.spawn[2] + 0.2 * (self.index // 3 - 1),
                ]
                center_x = int(math.floor(self.spawn[0] / CHUNK_SIZE))
                center_z = int(math.floor(self.spawn[2] / CHUNK_SIZE))
                radius = self.args.interest_radius
                self.expected_chunks = {
                    (center_x + dx, center_z + dz)
                    for dz in range(-radius, radius + 1)
                    for dx in range(-radius, radius + 1)
                }
                for block in message.get("blocks") or []:
                    if block.get("name") == "openblock:stone":
                        self.stone_id = int(block.get("id", 1))
                        break
                self.log(
                    "hello: world=%s seed=%s spawn=%s expected_chunks=%d"
                    % (
                        message.get("world_name"),
                        message.get("seed"),
                        self.spawn,
                        len(self.expected_chunks),
                    )
                )
                return
            if kind == "disconnect":
                raise ConnectionError("server rejected handshake: %s" % message.get("reason"))
        raise TimeoutError("handshake timed out")

    def _play(self):
        self.sock.setblocking(False)
        now = time.monotonic()
        self.play_start = now
        self.next_move = now
        self.next_ping = now + 0.25
        self.next_chat = now + 0.5
        self.next_edit = now + 1.0
        self.next_request = now
        end = now + self.args.seconds + 30.0  # main thread normally stops us first
        while not self.stop.is_set() and time.monotonic() < end:
            now = time.monotonic()
            self._maybe_move(now)
            self._maybe_chat(now)
            self._maybe_edit(now)
            self._maybe_request(now)
            self._maybe_ping(now)
            self._pump()
            time.sleep(0.005)

    def _maybe_move(self, now):
        if now < self.next_move:
            return
        self.next_move = now + 0.05  # 20 Move intents per second
        self.last_move = now
        elapsed = now - self.play_start
        # A tiny circle keeps every bot in the spawn chunk so the expected
        # interest set is exactly the (2r+1)^2 columns delivered by the server.
        angle = elapsed * 0.25 + self.index
        target_x = self.home[0] + math.cos(angle) * 0.15
        target_z = self.home[2] + math.sin(angle) * 0.15
        target_y = self.home[1]
        self.send(
            {
                "type": "move",
                "pos": [target_x, target_y, target_z],
                "yaw": angle,
                "pitch": 0.0,
            },
            "move",
        )

    def _maybe_chat(self, now):
        if now < self.next_chat:
            return
        self.next_chat = now + 3.0  # under the server's 5-per-10 s cap
        self.chat_sequence += 1
        token = "%s-%d" % (self.bot_name, self.chat_sequence)
        self.stats.note_chat(token)
        self.send({"type": "chat", "text": "gate %s" % token}, "chat")

    def _maybe_edit(self, now):
        if now < self.next_edit:
            return
        self.next_edit = now + 1.5
        x = int(math.floor(self.spawn[0])) + self.edit_dx
        y = int(math.floor(self.spawn[1])) + 1
        z = int(math.floor(self.spawn[2])) + self.edit_dz
        # Creative: place a stone cell above the surface, then break it. Both
        # are accepted deterministically, unlike survival placement without a
        # hotbar item. Each bot owns a distinct column within reach.
        self.send({"type": "block_edit", "pos": [x, y, z], "block": self.stone_id}, "edit_place")
        self.send({"type": "block_edit", "pos": [x, y, z], "block": 0}, "edit_break")

    def _maybe_request(self, now):
        if len(self.received_chunks) >= len(self.expected_chunks):
            return
        # Normally automatic radius streaming completes in a few seconds. This
        # fallback only asks for a missing column after a grace period, at a
        # rate far below the server's 16 requests/s cap.
        if now - self.play_start < 3.0 or now < self.next_request:
            return
        self.next_request = now + 0.4
        for position in sorted(self.expected_chunks - self.received_chunks):
            self.send({"type": "chunk_request", "cx": position[0], "cz": position[1]}, "chunk_request")
            break

    def _maybe_ping(self, now):
        if now < self.next_ping:
            return
        self.next_ping = now + 1.0
        self.ping_sequence += 1
        nonce = ((self.index + 1) << 20) | (self.ping_sequence & 0xFFFFF)
        self.pending_pings[nonce] = now
        stale = [key for key, sent in self.pending_pings.items() if now - sent > 30.0]
        for key in stale:
            del self.pending_pings[key]
        self.send({"type": "ping", "id": nonce}, "ping")

    def _pump(self):
        while True:
            try:
                data = self.sock.recv(262144)
            except BlockingIOError:
                break
            except InterruptedError:
                continue
            if not data:
                raise ConnectionError("server closed the connection")
            self.buffer.extend(data)
        while len(self.buffer) >= 4:
            length = struct.unpack_from(">I", self.buffer, 0)[0]
            if length == 0:
                raise ConnectionError("server sent an empty frame")
            if length > MAX_FRAME_LEN:
                raise ConnectionError("server frame of %d bytes exceeds the limit" % length)
            if len(self.buffer) < 4 + length:
                break
            payload = bytes(self.buffer[4 : 4 + length])
            del self.buffer[: 4 + length]
            self.stats.add_message(self._message_kind(payload), len(payload) + 4)
            if payload.startswith(CHUNK_PREFIX):
                self._on_chunk_payload(payload)
            else:
                self._handle(json.loads(payload.decode("utf-8")))

    def _message_kind(self, payload):
        if payload.startswith(CHUNK_PREFIX):
            return "chunk_data"
        try:
            decoded = json.loads(payload.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            return "invalid"
        return decoded.get("type", "unknown")

    def _on_chunk_payload(self, payload):
        # Chunk payloads dominate the byte stream; skip the base64 decode and
        # read only the two coordinates so the bot stays a fast reader.
        match = CHUNK_COORDS.search(payload[:256])
        if match is None:
            message = json.loads(payload.decode("utf-8"))
            self._on_chunk(int(message.get("cx", 0)), int(message.get("cz", 0)))
            return
        self._on_chunk(int(match.group(1)), int(match.group(2)))

    def _on_chunk(self, cx, cz):
        self.stats.inc("chunks")
        self.received_chunks.add((cx, cz))
        if self.interest_loaded_seconds is None and len(self.received_chunks) >= len(self.expected_chunks):
            if self.join_time is not None:
                self.interest_loaded_seconds = time.monotonic() - self.join_time
            else:
                self.interest_loaded_seconds = 0.0

    def _handle(self, message):
        kind = message.get("type")
        if kind == "join":
            if message.get("name") == self.bot_name:
                self.player_id = message.get("id")
                self.join_time = time.monotonic()
                self.stats.inc("joins")
        elif kind == "player_state":
            if self.player_id is not None and message.get("id") == self.player_id:
                self.log("server position %s" % (message.get("pos"),))
        elif kind == "pong":
            sent = self.pending_pings.pop(message.get("id"), None)
            if sent is not None:
                self.stats.add_ping((time.monotonic() - sent) * 1000.0)
        elif kind == "chat":
            text = message.get("text", "")
            match = CHAT_TOKEN.search(text)
            if match:
                self.stats.complete_chat(match.group(1))
        elif kind == "block_edit":
            if self.player_id is not None and message.get("player") == self.player_id:
                self.stats.add_own_edit()
        elif kind == "action_rejected":
            self.stats.add_rejection(str(message.get("action")), str(message.get("reason")))
        elif kind == "disconnect":
            self.stats.inc("disconnects")
            raise ConnectionError("server disconnected: %s" % message.get("reason"))


class ServerProcess:
    """A spawned `openblock-server` in dedicated TCP mode."""

    def __init__(self, binary, workdir, data_dir, args):
        self.binary = Path(binary)
        self.workdir = Path(workdir)
        self.data_root = Path(data_dir)
        self.args = args
        self.metrics_path = self.workdir / "metrics.json"
        self.log_path = self.workdir / "server.log"
        self.proc = None
        self.log_handle = None
        self.started_at = None
        self.ready_seconds = None
        self.ready_snapshot = None
        self.exit_code = None
        self.shutdown_seconds = None

    def command(self):
        command = [
            str(self.binary),
            "--listen",
            "127.0.0.1:0",
            "--data-dir",
            str(self.data_root),
            "--world",
            self.args.world,
            "--seed",
            str(self.args.seed),
            "--max-players",
            str(self.args.clients),
            "--interest-radius",
            str(self.args.interest_radius),
            "--save-interval",
            str(self.args.save_interval),
            "--metrics-json",
            str(self.metrics_path),
            "--metrics-interval",
            str(self.args.metrics_interval),
        ]
        command.append("--creative" if self.args.game_mode == "creative" else "--survival")
        return command

    def start(self):
        self.workdir.mkdir(parents=True, exist_ok=True)
        self.log_handle = open(self.log_path, "wb")
        self.started_at = time.monotonic()
        self.proc = subprocess.Popen(
            self.command(),
            cwd=str(ROOT),
            stdin=subprocess.PIPE,
            stdout=self.log_handle,
            stderr=subprocess.STDOUT,
        )
        deadline = self.started_at + self.args.startup_timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise GateError("server exited during startup with code %s\n%s" % (self.proc.returncode, self.log_tail()))
            snapshot = self.read_metrics()
            if snapshot and snapshot.get("listen"):
                self.ready_snapshot = snapshot
                self.ready_seconds = time.monotonic() - self.started_at
                return
            time.sleep(0.05)
        raise GateError("server did not write a metrics snapshot within %.1f s\n%s" % (self.args.startup_timeout, self.log_tail()))

    def read_metrics(self):
        try:
            with open(self.metrics_path, "r", encoding="utf-8") as handle:
                snapshot = json.load(handle)
        except (OSError, json.JSONDecodeError):
            return None
        if not isinstance(snapshot, dict) or snapshot.get("schema") != "openblock.server-metrics/1":
            return None
        return snapshot

    def log_tail(self, lines=40):
        try:
            text = self.log_path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            return "(no server log)"
        return "\n".join(text[-lines:])

    def stop(self, timeout=30.0):
        if self.proc is None:
            return
        if self.proc.poll() is None:
            self.started_stop = time.monotonic()
            try:
                self.proc.stdin.write(b"stop\n")
                self.proc.stdin.flush()
            except (BrokenPipeError, OSError):
                pass
            try:
                self.proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=5.0)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=5.0)
            self.shutdown_seconds = time.monotonic() - self.started_stop
        self.exit_code = self.proc.returncode
        if self.log_handle is not None:
            self.log_handle.close()
            self.log_handle = None

    def wait_for_snapshot(self, predicate, timeout):
        deadline = time.monotonic() + timeout
        latest = self.read_metrics()
        while time.monotonic() < deadline:
            snapshot = self.read_metrics()
            if snapshot is not None:
                latest = snapshot
                if predicate(snapshot):
                    return snapshot
            if self.proc.poll() is not None:
                break
            time.sleep(0.1)
        return latest


def load_reload_hash(binary, data_root, world, seed, log_path):
    """Reopens the stopped world twice and compares the reported state hashes."""
    hashes = []
    for attempt in (1, 2):
        command = [
            str(binary),
            "--data-dir",
            str(data_root),
            "--world",
            world,
            "--seed",
            str(seed),
            "--ticks",
            "0",
        ]
        result = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=120,
            check=False,
        )
        match = re.search(r"state hash ([0-9a-fA-F]+)", result.stdout)
        if match is None or result.returncode != 0:
            return {
                "ok": False,
                "hashes": hashes,
                "error": "reload attempt %d failed (exit %d): %s"
                % (attempt, result.returncode, result.stdout.strip()[-500:]),
            }
        hashes.append(match.group(1))
        if log_path is not None:
            with open(log_path, "a", encoding="utf-8") as handle:
                handle.write("\n--- reload attempt %d ---\n%s\n" % (attempt, result.stdout))
    return {"ok": hashes[0] == hashes[1], "hashes": hashes}


def delta(end, start, key):
    return int(end.get(key, 0) or 0) - int(start.get(key, 0) or 0)


def parse_duration_ms(text):
    """Parses a Rust `Duration` debug string into milliseconds."""
    match = re.match(r"([0-9]+(?:\.[0-9]+)?)\s*(ns|µs|us|ms|s)", text.strip())
    if match is None:
        return None
    value = float(match.group(1))
    unit = match.group(2)
    if unit == "ns":
        return value / 1_000_000.0
    if unit in ("µs", "us"):
        return value / 1000.0
    if unit == "s":
        return value * 1000.0
    return value


def analyze_server_log(path):
    """Extracts autosave ticks and overrun warnings from the server log.

    The server logs an overrun immediately after the tick that produced it, and
    an autosave logs at the end of the same tick. A warning on the line right
    after an autosave line therefore attributes the pause to that save without
    instrumenting the server.
    """
    try:
        lines = Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return {"autosaves": 0, "overrun_warnings": 0, "autosave_overruns": 0, "overrun_ms": []}
    autosave_ticks = []
    overruns = []
    autosave_overruns = 0
    last_autosave_line = -10
    for index, line in enumerate(lines):
        autosave = re.search(r"autosaved world .* at tick (\d+)", line)
        if autosave:
            autosave_ticks.append(int(autosave.group(1)))
            last_autosave_line = index
        overrun = re.search(r"tick overran the 50 ms budget: (.+)$", line)
        if overrun:
            milliseconds = parse_duration_ms(overrun.group(1))
            if milliseconds is not None:
                overruns.append(milliseconds)
                if index - last_autosave_line <= 1:
                    autosave_overruns += 1
    return {
        "autosaves": len(autosave_ticks),
        "first_autosave_tick": autosave_ticks[0] if autosave_ticks else None,
        "last_autosave_tick": autosave_ticks[-1] if autosave_ticks else None,
        "overrun_warnings": len(overruns),
        "autosave_overruns": autosave_overruns,
        "overrun_max_ms": max(overruns) if overruns else None,
        "overrun_ms": [round(value, 3) for value in overruns],
    }


def run_gate(args):
    binary = Path(args.binary).resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise GateError("server binary not found or not executable: %s" % binary)

    if args.data_dir:
        data_root = Path(args.data_dir).resolve()
        workdir = data_root.parent / (data_root.name + "-gate-artifacts")
        workdir.mkdir(parents=True, exist_ok=True)
        keep_workdir = True
    else:
        workdir = Path(tempfile.mkdtemp(prefix="openblock-load-gate-"))
        data_root = workdir / "data"
        keep_workdir = False

    server = ServerProcess(binary, workdir, data_root, args)
    stats = Stats()
    stop = threading.Event()
    bots = []
    history = []
    reload_result = None
    failure_detail = None
    started = time.monotonic()
    try:
        print("load-gate: starting %s" % binary)
        server.start()
        print(
            "load-gate: server ready in %.2f s at %s (world %s seed %d)"
            % (
                server.ready_seconds,
                server.ready_snapshot.get("listen"),
                server.ready_snapshot.get("world"),
                server.ready_snapshot.get("seed"),
            )
        )
        host, port_text = server.ready_snapshot["listen"].rsplit(":", 1)
        port = int(port_text)
        run_id = "%04x" % random.getrandbits(16)
        bots = [
            Bot(index, "%s-%02d" % (args.name_prefix, index), host, port, args, stats, stop)
            for index in range(args.clients)
        ]
        for bot in bots:
            bot.start()

        join_deadline = time.monotonic() + args.connect_timeout + 10.0
        while stats.joins < args.clients and time.monotonic() < join_deadline:
            if server.proc.poll() is not None:
                break
            time.sleep(0.1)
        if stats.joins < args.clients:
            failure_detail = "only %d/%d clients joined before measurement" % (stats.joins, args.clients)

        # Optionally let the cold interest-set streaming finish before the
        # measured window starts, so the tick percentiles describe steady-state
        # play rather than the join burst. Off by default to keep the 1.0
        # profile comparable with the published numbers.
        interest_wait_seconds = 0.0
        if args.wait_interest:
            interest_started = time.monotonic()
            interest_deadline = interest_started + min(30.0, max(10.0, args.seconds + 5.0))
            while time.monotonic() < interest_deadline:
                if all(bot.interest_loaded_seconds is not None for bot in bots):
                    break
                if server.proc.poll() is not None:
                    break
                time.sleep(0.1)
            interest_wait_seconds = time.monotonic() - interest_started
            if not all(bot.interest_loaded_seconds is not None for bot in bots):
                print("load-gate: warning: interest sets did not complete within %.1f s" % interest_wait_seconds)

        # Align the measurement window with a snapshot taken after the joins,
        # so the rate deltas cover load rather than server start-up.
        start_snapshot = server.wait_for_snapshot(
            lambda snapshot: int(snapshot.get("players_connected", 0)) >= max(1, stats.joins),
            timeout=max(5.0, args.metrics_interval * 2.0 + 2.0),
        )
        if start_snapshot is None:
            raise GateError("no metrics snapshot after clients joined\n%s" % server.log_tail())

        measure_start = time.monotonic()
        print(
            "load-gate: %d bot(s) running for %.1f s (interest radius %d, autosave %d s)"
            % (args.clients, args.seconds, args.interest_radius, args.save_interval)
        )
        last_sample = None
        while time.monotonic() - measure_start < args.seconds:
            snapshot = server.read_metrics()
            if snapshot is not None:
                key = (snapshot.get("timestamp_unix_ms"), snapshot.get("tick_count"))
                if key != last_sample:
                    history.append(snapshot)
                    last_sample = key
            if all(not bot.is_alive() for bot in bots):
                break
            time.sleep(0.5)
        measurement_wall = time.monotonic() - measure_start
        stop.set()
        for bot in bots:
            bot.join(timeout=max(2.0, args.connect_timeout))
        end_snapshot = server.wait_for_snapshot(
            lambda snapshot: int(snapshot.get("players_connected", 0)) == 0,
            timeout=max(5.0, args.metrics_interval + 5.0),
        )
        if end_snapshot is None or int(end_snapshot.get("players_connected", 0)) != 0:
            end_snapshot = server.read_metrics() or start_snapshot
        server.stop()
        server_log = analyze_server_log(server.log_path)
        if server.exit_code != 0:
            failure_detail = "server exited with code %s" % server.exit_code
        print(
            "load-gate: server stopped (exit %s, drain %.2f s)"
            % (server.exit_code, server.shutdown_seconds or 0.0)
        )

        # Cumulative counters start at server readiness: the bots' first chunks
        # are queued during the join tick, before a post-join snapshot exists.
        baseline = server.ready_snapshot
        rates_elapsed = max(
            0.001,
            float(end_snapshot.get("uptime_seconds", 0.0)) - float(baseline.get("uptime_seconds", 0.0)),
        )
        measured_elapsed = max(
            0.001,
            float(end_snapshot.get("uptime_seconds", 0.0)) - float(start_snapshot.get("uptime_seconds", 0.0)),
        )
        client = stats.snapshot()
        pings = client["pings"]
        chat_latencies = client["chat_latencies"]
        p50 = percentile(pings, 0.50)
        p95 = percentile(pings, 0.95)
        p99 = percentile(pings, 0.99)
        chat_p50 = percentile(chat_latencies, 0.50)
        chat_p95 = percentile(chat_latencies, 0.95)

        per_client = []
        for bot in bots:
            per_client.append(
                {
                    "name": bot.bot_name,
                    "joined": bot.join_time is not None,
                    "interest_loaded_seconds": round(bot.interest_loaded_seconds, 3)
                    if bot.interest_loaded_seconds is not None
                    else None,
                    "unique_chunks": len(bot.received_chunks),
                    "expected_chunks": len(bot.expected_chunks),
                }
            )
        interest_times = [entry["interest_loaded_seconds"] for entry in per_client if entry["interest_loaded_seconds"] is not None]
        clients_loaded = len(interest_times)

        edits_accepted = delta(end_snapshot, baseline, "edits_accepted")
        edits_attempted = delta(end_snapshot, baseline, "edits_attempted")
        edits_rejected = delta(end_snapshot, baseline, "edits_rejected")
        actions_rejected = delta(end_snapshot, baseline, "actions_rejected")
        chunks_delta = delta(end_snapshot, baseline, "chunks_queued")
        chunk_bytes_delta = delta(end_snapshot, baseline, "chunk_bytes_queued")
        bytes_in_delta = delta(end_snapshot, baseline, "bytes_in")
        bytes_out_delta = delta(end_snapshot, baseline, "bytes_out")
        moves_delta = delta(end_snapshot, baseline, "moves")
        chats_delta = delta(end_snapshot, baseline, "chats")
        pings_delta = delta(end_snapshot, baseline, "pings")
        chunk_requests_delta = delta(end_snapshot, baseline, "chunk_requests")
        ticks_delta = delta(end_snapshot, start_snapshot, "tick_count")
        rss_history = [
            (float(snapshot.get("uptime_seconds", 0.0)), int(snapshot.get("rss_kib", 0) or 0))
            for snapshot in history
            if snapshot.get("rss_kib")
        ]
        rss_kib = end_snapshot.get("rss_kib")
        if rss_kib is None and rss_history:
            rss_kib = rss_history[-1][1]
        rss_peak_kib = max((value for _, value in rss_history), default=rss_kib)

        expected_chunks_total = args.clients * (2 * args.interest_radius + 1) ** 2
        interest_complete = clients_loaded == args.clients
        tps_measured = ticks_delta / measured_elapsed if measured_elapsed > 0 else 0.0
        tick_p99 = end_snapshot.get("tick_p99_ms")
        tick_p95 = end_snapshot.get("tick_p95_ms")
        tick_p50 = end_snapshot.get("tick_p50_ms")
        tick_max = end_snapshot.get("tick_max_ms")

        checks = []
        warnings = []
        failures = []

        def check(name, ok, detail):
            checks.append({"name": name, "ok": bool(ok), "detail": detail})
            if not ok:
                failures.append("%s: %s" % (name, detail))

        check("handshakes", client["hellos"] >= args.clients, "%d/%d" % (client["hellos"], args.clients))
        check("joins", client["joins"] >= args.clients, "%d/%d" % (client["joins"], args.clients))
        check(
            "interest-set loaded",
            interest_complete,
            "%d/%d clients reached %d chunks" % (clients_loaded, args.clients, (2 * args.interest_radius + 1) ** 2),
        )
        check("client errors", not client["errors"], "%d error(s)" % len(client["errors"]))
        check("unexpected disconnects", client["disconnects"] == 0, "%d" % client["disconnects"])
        check("pong samples", len(pings) >= args.clients, "%d pong(s)" % len(pings))
        check(
            "all chunks received",
            sum(entry["unique_chunks"] for entry in per_client) >= expected_chunks_total,
            "%d/%d unique client chunks" % (sum(entry["unique_chunks"] for entry in per_client), expected_chunks_total),
        )
        if args.game_mode == "creative":
            check("accepted block edits", edits_accepted > 0, "%d accepted of %d attempted" % (edits_accepted, edits_attempted))

        rss_budget_kib = (DEFAULT_RSS_BUDGET_MIB + 20 * args.clients) * 1024
        gate_enforced = not args.no_gate and (not args.quick or args.enforce_quick)
        gate_checks = []
        if tick_p99 is not None:
            ok = float(tick_p99) <= float(args.max_p99_ms)
            gate_checks.append(
                {
                    "name": "tick p99",
                    "ok": ok,
                    "detail": "%.3f ms (target <= %.1f ms)" % (tick_p99, args.max_p99_ms),
                }
            )
            if not ok:
                if gate_enforced:
                    failures.append("tick p99 %.3f ms exceeds %.1f ms" % (tick_p99, args.max_p99_ms))
                else:
                    warnings.append("tick p99 %.3f ms exceeds %.1f ms (advisory)" % (tick_p99, args.max_p99_ms))
        if tps_measured < 19.0:
            detail = "%.2f TPS over the measurement window" % tps_measured
            gate_checks.append({"name": "tick rate", "ok": False, "detail": detail})
            if gate_enforced:
                failures.append("tick rate " + detail)
            else:
                warnings.append("tick rate " + detail + " (advisory)")
        else:
            gate_checks.append({"name": "tick rate", "ok": True, "detail": "%.2f TPS" % tps_measured})
        if rss_kib is not None:
            ok = int(rss_kib) <= rss_budget_kib
            gate_checks.append(
                {
                    "name": "RSS budget",
                    "ok": ok,
                    "detail": "%s (budget %s)" % (fmt_mib(int(rss_kib)), fmt_mib(rss_budget_kib)),
                }
            )
            if not ok:
                if gate_enforced:
                    failures.append("RSS %s exceeds %s" % (fmt_mib(int(rss_kib)), fmt_mib(rss_budget_kib)))
                else:
                    warnings.append("RSS %s exceeds %s (advisory)" % (fmt_mib(int(rss_kib)), fmt_mib(rss_budget_kib)))

        soak = None
        if args.soak:
            soak = soak_checks(args, server, data_root, history, rss_history, binary, workdir)
            checks.extend(soak["checks"])
            failures.extend(soak["failures"])
            warnings.extend(soak["warnings"])
            reload_result = soak["reload"]

        result_pass = not failures
        result = {
            "schema": "openblock.load-gate/1",
            "generated_at": utc_now(),
            "profile": "quick" if args.quick else ("soak" if args.soak else "full"),
            "result": "pass" if result_pass else "fail",
            "gate_enforced": gate_enforced,
            "command": " ".join(sys.argv),
            "environment": {
                "cpu": cpu_model(),
                "cpu_count": cpu_count(),
                "os": os_description(),
                "python": platform.python_version(),
                "machine": platform.machine(),
            },
            "scenario": {
                "clients": args.clients,
                "seconds": args.seconds,
                "interest_radius": args.interest_radius,
                "expected_chunks_per_client": (2 * args.interest_radius + 1) ** 2,
                "seed": args.seed,
                "world": args.world,
                "game_mode": args.game_mode,
                "save_interval_seconds": args.save_interval,
                "metrics_interval_seconds": args.metrics_interval,
                "name_prefix": args.name_prefix,
                "target_p99_ms": args.max_p99_ms,
                "rss_budget_kib": rss_budget_kib,
            },
            "server": {
                "binary": str(binary),
                "listen": server.ready_snapshot.get("listen"),
                "startup_seconds": round(server.ready_seconds, 3),
                "shutdown_seconds": round(server.shutdown_seconds, 3) if server.shutdown_seconds is not None else None,
                "exit_code": server.exit_code,
                "log": str(server.log_path) if args.keep or args.data_dir else None,
                "world_columns_at_start": server.ready_snapshot.get("loaded_columns"),
                "world_columns_at_end": end_snapshot.get("loaded_columns"),
            },
            "run": {
                "measured_seconds": round(measurement_wall, 3),
                "rate_window_seconds": round(rates_elapsed, 3),
                "interest_wait_seconds": round(interest_wait_seconds, 3),
                "run_id": run_id,
            },
            "clients": {
                "handshakes": client["hellos"],
                "joins": client["joins"],
                "clients_loaded": clients_loaded,
                "interest_loaded_seconds": {
                    "p50": round(percentile(interest_times, 0.50), 3) if interest_times else None,
                    "p95": round(percentile(interest_times, 0.95), 3) if interest_times else None,
                    "max": round(max(interest_times), 3) if interest_times else None,
                },
                "per_client": per_client,
                "chunks_received": client["chunks"],
                "unique_chunks": sum(entry["unique_chunks"] for entry in per_client),
                "bytes_received": client["bytes_received"],
                "moves_sent": client["sent"].get("move", 0),
                "chats_sent": client["sent"].get("chat", 0),
                "pings_sent": client["sent"].get("ping", 0),
                "edits_sent": client["sent"].get("edit_place", 0) + client["sent"].get("edit_break", 0),
                "chunk_requests_sent": client["sent"].get("chunk_request", 0),
                "own_edit_broadcasts": client["own_edit_broadcasts"],
                "action_rejections": client["action_rejections"],
                "rejection_reasons": client["rejections"],
                "ping_ms": {
                    "p50": round(p50, 2) if p50 is not None else None,
                    "p95": round(p95, 2) if p95 is not None else None,
                    "p99": round(p99, 2) if p99 is not None else None,
                    "max": round(max(pings), 2) if pings else None,
                    "samples": len(pings),
                },
                "chat_ms": {
                    "p50": round(chat_p50, 2) if chat_p50 is not None else None,
                    "p95": round(chat_p95, 2) if chat_p95 is not None else None,
                    "samples": len(chat_latencies),
                },
                "errors": client["errors"],
                "disconnects": client["disconnects"],
                "messages": client["messages"],
            },
            "server_metrics": {
                "tick_p50_ms": tick_p50,
                "tick_p95_ms": tick_p95,
                "tick_p99_ms": tick_p99,
                "tick_max_ms": tick_max,
                "tick_samples": end_snapshot.get("tick_samples"),
                "tps": round(tps_measured, 2),
                "tps_reported": end_snapshot.get("tps"),
                "tick_overruns": end_snapshot.get("tick_overruns"),
                "tick_over_35ms": end_snapshot.get("tick_over_35ms"),
                "players_connected_end": end_snapshot.get("players_connected"),
                "rss_kib": rss_kib,
                "rss_mib": round(int(rss_kib) / 1024.0, 2) if rss_kib is not None else None,
                "rss_peak_kib": rss_peak_kib,
                "rss_peak_mib": round(int(rss_peak_kib) / 1024.0, 2) if rss_peak_kib is not None else None,
                "threads": end_snapshot.get("threads"),
                "open_fds": end_snapshot.get("open_fds"),
                "mobs": end_snapshot.get("mobs"),
                "item_entities": end_snapshot.get("item_entities"),
                "loaded_columns": end_snapshot.get("loaded_columns"),
                "chunks_queued": chunks_delta,
                "chunks_per_second": round(chunks_delta / rates_elapsed, 2),
                "chunk_bytes_queued": chunk_bytes_delta,
                "chunk_bytes_per_second": round(chunk_bytes_delta / rates_elapsed, 2),
                "bytes_in": bytes_in_delta,
                "bytes_out": bytes_out_delta,
                "bandwidth_out_bytes_per_second": round(bytes_out_delta / rates_elapsed, 2),
                "bandwidth_in_bytes_per_second": round(bytes_in_delta / rates_elapsed, 2),
                "bandwidth_out_kib_per_second_per_client": round(bytes_out_delta / rates_elapsed / 1024.0 / max(1, args.clients), 2),
                "edits_accepted": edits_accepted,
                "edits_attempted": edits_attempted,
                "edits_rejected": edits_rejected,
                "actions_rejected": actions_rejected,
                "moves": moves_delta,
                "chats": chats_delta,
                "pings": pings_delta,
                "chunk_requests": chunk_requests_delta,
                "rss_series": [
                    {"uptime_seconds": round(uptime, 2), "rss_kib": value}
                    for uptime, value in rss_history
                ],
            },
            "server_log": server_log,
            "checks": checks,
            "gate_checks": gate_checks,
            "warnings": warnings,
            "failures": failures,
            "soak": soak,
        }
        if failure_detail:
            result["failures"].append(failure_detail)
            result["result"] = "fail"
        if result["failures"]:
            result["server"]["log_tail"] = server.log_tail(30)
        return result
    except Exception:
        stop.set()
        for bot in bots:
            bot.join(timeout=1.0)
        try:
            server.stop()
        except Exception:  # noqa: BLE001 - keep the original error
            pass
        raise
    finally:
        if args.keep:
            print("load-gate: kept artifacts in %s" % workdir)
        elif not keep_workdir and not args.data_dir:
            shutil.rmtree(workdir, ignore_errors=True)


def soak_checks(args, server, data_root, history, rss_history, binary, workdir):
    """Memory-plateau, fd/thread, autosave and save/reload checks for --soak."""
    checks = []
    failures = []
    warnings = []

    def check(name, ok, detail):
        checks.append({"name": name, "ok": bool(ok), "detail": detail})
        if not ok:
            failures.append("%s: %s" % (name, detail))

    log_text = ""
    try:
        log_text = server.log_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        pass
    autosaves = log_text.count("autosaved world")
    expected_autosaves = max(1, int(args.seconds // max(1, args.save_interval)) - 1)
    check(
        "autosaves",
        autosaves >= expected_autosaves,
        "%d (expected at least %d with a %d s interval)" % (autosaves, expected_autosaves, args.save_interval),
    )

    if len(rss_history) >= 3:
        warmup = min(120.0, max(20.0, args.seconds * 0.2))
        stable = [(uptime, rss) for uptime, rss in rss_history if uptime >= warmup]
        if len(stable) >= 2:
            first = stable[0][1]
            last = stable[-1][1]
            growth_mib = (last - first) / 1024.0
            half = stable[len(stable) // 2 :]
            values = [value for _, value in half]
            range_mib = (max(values) - min(values)) / 1024.0
            check(
                "RSS plateau",
                growth_mib <= 128.0 and range_mib <= 64.0,
                "growth %+.1f MiB after %.0f s warm-up, last-half range %.1f MiB"
                % (growth_mib, warmup, range_mib),
            )
            rss = {"first_kib": first, "last_kib": last, "growth_mib": round(growth_mib, 2), "range_mib": round(range_mib, 2)}
        else:
            rss = None
            warnings.append("RSS plateau not evaluated: too few samples")
    else:
        rss = None
        warnings.append("RSS plateau not evaluated: too few samples")

    fd_values = [
        int(snapshot.get("open_fds", 0) or 0)
        for snapshot in history
        if snapshot.get("open_fds") is not None
    ]
    thread_values = [
        int(snapshot.get("threads", 0) or 0)
        for snapshot in history
        if snapshot.get("threads") is not None
    ]
    fds = None
    threads = None
    if len(fd_values) >= 3:
        half = fd_values[len(fd_values) // 2 :]
        fds = {"first": fd_values[len(fd_values) // 2], "last": fd_values[-1], "max_half": max(half)}
        check(
            "file descriptors",
            fds["last"] - fds["first"] <= 16,
            "first-half %d -> end %d, half max %d" % (fds["first"], fds["last"], fds["max_half"]),
        )
    if len(thread_values) >= 3:
        half = thread_values[len(thread_values) // 2 :]
        threads = {"first": thread_values[len(thread_values) // 2], "last": thread_values[-1], "max_half": max(half)}
        check(
            "server threads",
            threads["last"] - threads["first"] <= 4,
            "first-half %d -> end %d, half max %d" % (threads["first"], threads["last"], threads["max_half"]),
        )

    reload_log = workdir / "reload.log"
    reload_result = load_reload_hash(binary, data_root, args.world, args.seed, reload_log)
    check(
        "save/reload state hash",
        reload_result["ok"],
        "hashes %s" % (reload_result.get("hashes") or reload_result.get("error")),
    )
    world_dir = Path(data_root) / "worlds" / args.world
    world_file = world_dir / "world.obs"
    save_bytes = world_file.stat().st_size if world_file.is_file() else None
    check("world saved", save_bytes is not None and save_bytes > 0, "%s bytes" % (save_bytes or 0))

    return {
        "checks": checks,
        "failures": failures,
        "warnings": warnings,
        "autosaves": autosaves,
        "rss": rss,
        "fds": fds,
        "threads": threads,
        "reload": reload_result,
        "save_bytes": save_bytes,
        "world_dir": str(world_dir),
    }


def render_markdown(result):
    scenario = result["scenario"]
    server = result["server"]
    clients = result["clients"]
    metrics = result["server_metrics"]
    profile = result["profile"]
    lines = []
    lines.append("# OpenBlock 1.0 multiplayer load gate — %s" % profile)
    lines.append("")
    lines.append("> Generated: %s  " % result["generated_at"])
    lines.append("> Machine: %s (%d vCPU), %s  " % (result["environment"]["cpu"], result["environment"]["cpu_count"], result["environment"]["os"]))
    lines.append("> Command: `%s`" % result["command"])
    lines.append("")
    lines.append("## Result")
    lines.append("")
    verdict = "**PASS**" if result["result"] == "pass" else "**FAIL**"
    enforcement = "enforced" if result["gate_enforced"] else "advisory (functional checks still enforced)"
    lines.append("%s — target gate %s." % (verdict, enforcement))
    lines.append("")
    lines.append("| Target | Value |")
    lines.append("| --- | --- |")
    lines.append("| Players / interest radius | %d / %d chunks |" % (scenario["clients"], scenario["interest_radius"]))
    lines.append("| Duration | %.1f s |" % scenario["seconds"])
    lines.append("| Game mode / seed | %s / %d |" % (scenario["game_mode"], scenario["seed"]))
    lines.append("| Tick p99 target | <= %.1f ms |" % scenario["target_p99_ms"])
    lines.append("| Tick p50 / p95 / p99 / max | %s / %s / %s / %s ms |" % (fmt_ms(metrics["tick_p50_ms"]), fmt_ms(metrics["tick_p95_ms"]), fmt_ms(metrics["tick_p99_ms"]), fmt_ms(metrics["tick_max_ms"])))
    lines.append("| TPS (measured window) | %.2f |" % metrics["tps"])
    lines.append("| RSS / peak | %s / %s |" % (fmt_mib(metrics["rss_kib"]), fmt_mib(metrics["rss_peak_kib"])))
    lines.append("| Tick overruns (>50 ms) / >35 ms | %s / %s |" % (metrics["tick_overruns"], metrics["tick_over_35ms"]))
    lines.append("| Server startup | %.2f s |" % server["startup_seconds"])
    lines.append("| Shutdown drain | %.2f s |" % (server["shutdown_seconds"] or 0.0))
    lines.append("")
    lines.append("## Load")
    lines.append("")
    lines.append("| Metric | Value |")
    lines.append("| --- | --- |")
    lines.append("| Handshakes / joins | %d / %d of %d |" % (clients["handshakes"], clients["joins"], scenario["clients"]))
    lines.append("| Interest sets loaded | %d/%d clients, p95 %s s, max %s s |" % (
        clients["clients_loaded"],
        scenario["clients"],
        clients["interest_loaded_seconds"]["p95"] if clients["interest_loaded_seconds"]["p95"] is not None else "n/a",
        clients["interest_loaded_seconds"]["max"] if clients["interest_loaded_seconds"]["max"] is not None else "n/a",
    ))
    lines.append("| Unique chunks received | %d (%.1f/client) |" % (clients["unique_chunks"], clients["unique_chunks"] / max(1, scenario["clients"])))
    lines.append("| Server chunk frames queued | %d (%.2f/s) |" % (metrics["chunks_queued"], metrics["chunks_per_second"]))
    lines.append("| Server out / in | %s/s / %s/s (%s/s per client down) |" % (
        fmt_bytes(metrics["bandwidth_out_bytes_per_second"]),
        fmt_bytes(metrics["bandwidth_in_bytes_per_second"]),
        fmt_bytes(int(metrics["bandwidth_out_kib_per_second_per_client"] * 1024)),
    ))
    lines.append("| Client ping p50/p95/p99 | %s / %s / %s ms |" % (clients["ping_ms"]["p50"], clients["ping_ms"]["p95"], clients["ping_ms"]["p99"]))
    lines.append("| Chat delivery p50/p95 | %s / %s ms |" % (clients["chat_ms"]["p50"], clients["chat_ms"]["p95"]))
    lines.append("| Moves / chats / pings / chunk requests (server) | %d / %d / %d / %d |" % (
        metrics["moves"], metrics["chats"], metrics["pings"], metrics["chunk_requests"],
    ))
    lines.append("| Block edits accepted / attempted / rejected (server) | %d / %d / %d |" % (
        metrics["edits_accepted"], metrics["edits_attempted"], metrics["edits_rejected"],
    ))
    lines.append("| Action rejections / client errors / disconnects | %d / %d / %d |" % (
        clients["action_rejections"], len(clients["errors"]), clients["disconnects"],
    ))
    lines.append("| Loaded columns at end | %s |" % metrics["loaded_columns"])
    lines.append("| Interest-set wait before measurement | %.2f s |" % result["run"].get("interest_wait_seconds", 0.0))
    lines.append("")
    server_log = result.get("server_log") or {}
    if server_log:
        lines.append("## Autosave / overrun correlation")
        lines.append("")
        lines.append("| Metric | Value |")
        lines.append("| --- | --- |")
        lines.append("| Autosaves logged | %d |" % server_log.get("autosaves", 0))
        lines.append("| Autosave ticks (first / last) | %s / %s |" % (
            server_log.get("first_autosave_tick") if server_log.get("first_autosave_tick") is not None else "n/a",
            server_log.get("last_autosave_tick") if server_log.get("last_autosave_tick") is not None else "n/a",
        ))
        lines.append("| Tick overrun warnings (>45 ms) | %d |" % server_log.get("overrun_warnings", 0))
        lines.append("| Overruns on the autosave tick | %d |" % server_log.get("autosave_overruns", 0))
        lines.append("| Longest logged tick | %s ms |" % fmt_ms(server_log.get("overrun_max_ms")))
        lines.append("")
        if server_log.get("overrun_ms"):
            lines.append("Logged overrun samples (ms): %s" % ", ".join("%.1f" % value for value in server_log["overrun_ms"]))
            lines.append("")
    lines.append("## Checks")
    lines.append("")
    lines.append("| Check | Result | Detail |")
    lines.append("| --- | :---: | --- |")
    for check in result["checks"]:
        lines.append("| %s | %s | %s |" % (check["name"], "PASS" if check["ok"] else "FAIL", check["detail"]))
    for check in result["gate_checks"]:
        state = "PASS" if check["ok"] else ("FAIL" if result["gate_enforced"] else "ADVISORY")
        lines.append("| %s | %s | %s |" % (check["name"], state, check["detail"]))
    lines.append("")
    if result["warnings"]:
        lines.append("## Warnings")
        lines.append("")
        for warning in result["warnings"]:
            lines.append("- %s" % warning)
        lines.append("")
    if result["failures"]:
        lines.append("## Failures")
        lines.append("")
        for failure in result["failures"]:
            lines.append("- %s" % failure)
        lines.append("")
    soak = result.get("soak")
    if soak:
        lines.append("## Soak")
        lines.append("")
        lines.append("| Metric | Value |")
        lines.append("| --- | --- |")
        lines.append("| Autosaves | %d |" % soak["autosaves"])
        lines.append("| Save size | %s |" % fmt_bytes(soak["save_bytes"]))
        lines.append("| Save/reload hashes | %s |" % ", ".join(soak["reload"].get("hashes") or []))
        if soak["rss"]:
            lines.append("| RSS growth after warm-up | %+.1f MiB (last-half range %.1f MiB) |" % (soak["rss"]["growth_mib"], soak["rss"]["range_mib"]))
        if soak["fds"]:
            lines.append("| FDs first-half -> end | %d -> %d |" % (soak["fds"]["first"], soak["fds"]["last"]))
        if soak["threads"]:
            lines.append("| Threads first-half -> end | %d -> %d |" % (soak["threads"]["first"], soak["threads"]["last"]))
        lines.append("")
    lines.append("## Notes")
    lines.append("")
    lines.append("- The tick window is the server's rolling 1,200-tick (60 s at 20 Hz) nearest-rank histogram. In the full 60 s profile the window fills with the load phase, so cold join/streaming ticks roll out of the p99 while the cumulative overrun counters retain them; the measured-window TPS is `delta(tick_count) / delta(uptime)`.")
    lines.append("- `chunks_queued` counts ChunkData frames queued to per-client writer threads; `bytes_out` counts bytes actually written to player sockets.")
    lines.append("- Bots stay in the spawn chunk (a 0.15 m circle) so the expected interest set is exactly (2r+1)^2 columns; a small explicit chunk-request fallback only runs for a missing column after 3 s.")
    lines.append("- RSS is `/proc/self/status` VmRSS for the whole server process, including allocator retention; it is a proxy, not the host world heap.")
    lines.append("- The client `ping`/chat numbers include Python scheduling and the server's 20 Hz tick, so they are end-to-end application latency, not network RTT.")
    lines.append("")
    return "\n".join(lines) + "\n"


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="OpenBlock 1.0 multiplayer load gate and bot soak harness.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--binary", default=None, help="openblock-server binary (default: target/release or $CARGO_TARGET_DIR)")
    parser.add_argument("--clients", type=int, default=None, help="bot count (default: 8; quick 4)")
    parser.add_argument("--seconds", type=float, default=None, help="measured duration (default: 60; quick 12; soak 600)")
    parser.add_argument("--interest-radius", type=int, default=DEFAULT_RADIUS, help="server interest radius in chunks (default: 6)")
    parser.add_argument("--seed", type=int, default=42, help="world seed (default: 42)")
    parser.add_argument("--world", default="load-gate", help="world directory name (default: load-gate)")
    parser.add_argument("--game-mode", choices=("creative", "survival"), default="creative", help="server default game mode (default: creative, so edits are deterministic)")
    parser.add_argument("--save-interval", type=int, default=60, help="server autosave interval in seconds, 0 disables (soak forces 10 unless set)")
    parser.add_argument("--wait-interest", action="store_true", help="start the measured window only after every bot has its full interest set")
    parser.add_argument("--metrics-interval", type=int, default=2, help="server metrics snapshot interval in seconds (default: 2)")
    parser.add_argument("--max-p99-ms", type=float, default=DEFAULT_TARGET_P99_MS, help="tick p99 target (default: 35 ms)")
    parser.add_argument("--connect-timeout", type=float, default=5.0, help="per-bot connect/handshake timeout (default: 5 s)")
    parser.add_argument("--startup-timeout", type=float, default=60.0, help="server startup timeout (default: 60 s)")
    parser.add_argument("--name-prefix", default="gate", help="player name prefix (default: gate)")
    parser.add_argument("--data-dir", default=None, help="reuse this data directory instead of a temporary one")
    parser.add_argument("--json", metavar="PATH", help="write the JSON result")
    parser.add_argument("--markdown", metavar="PATH", help="write the Markdown report")
    parser.add_argument("--keep", action="store_true", help="keep the temporary data directory and server log")
    parser.add_argument("--quick", action="store_true", help="CI profile: 4 bots, 12 s; tick target is advisory")
    parser.add_argument("--soak", action="store_true", help="10-minute soak with 8 bots, 10 s autosaves and save/reload checks")
    parser.add_argument("--no-gate", action="store_true", help="report target misses but do not fail the process")
    parser.add_argument("--enforce-quick", action="store_true", help="enforce the tick target in --quick mode too")
    parser.add_argument("--verbose", action="store_true", help="log per-bot progress")
    args = parser.parse_args(argv)

    if args.quick and args.soak:
        parser.error("--quick and --soak are mutually exclusive")
    if args.quick:
        profile = "quick"
    elif args.soak:
        profile = "soak"
    else:
        profile = "full"
    args.profile = profile
    if args.clients is None:
        args.clients = QUICK_CLIENTS if args.quick else DEFAULT_CLIENTS
    if args.seconds is None:
        args.seconds = QUICK_SECONDS if args.quick else (SOAK_SECONDS if args.soak else DEFAULT_SECONDS)
    if args.soak:
        if args.save_interval == 60:
            args.save_interval = SOAK_AUTOSAVE_SECONDS
        if args.metrics_interval == 2:
            args.metrics_interval = 5
        args.clients = args.clients or DEFAULT_CLIENTS
    if args.binary is None:
        args.binary = str(default_server_binary())
    if args.clients < 1:
        parser.error("--clients must be at least 1")
    if args.clients > 500:
        parser.error("--clients must be at most 500")
    if args.seconds < 1:
        parser.error("--seconds must be at least 1")
    if not 1 <= args.interest_radius <= 16:
        parser.error("--interest-radius must be in 1..=16")
    if not 0 <= args.save_interval <= 86_400:
        parser.error("--save-interval must be in 0..=86400")
    if not 1 <= args.metrics_interval <= 3600:
        parser.error("--metrics-interval must be in 1..=3600")
    if args.max_p99_ms <= 0:
        parser.error("--max-p99-ms must be positive")
    if args.connect_timeout <= 0:
        parser.error("--connect-timeout must be positive")
    if args.startup_timeout <= 0:
        parser.error("--startup-timeout must be positive")
    return args


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        result = run_gate(args)
    except GateError as error:
        print("load-gate: FAIL: %s" % error, file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("load-gate: interrupted", file=sys.stderr)
        return 130

    scenario = result["scenario"]
    clients = result["clients"]
    metrics = result["server_metrics"]
    print(
        "OpenBlock load gate (%s): %d clients, %.1f s, radius %d, target p99 <= %.1f ms"
        % (
            result["profile"],
            scenario["clients"],
            scenario["seconds"],
            scenario["interest_radius"],
            scenario["target_p99_ms"],
        )
    )
    print(
        "server: tick p50/p95/p99/max = %s / %s / %s / %s ms, %.2f TPS, %d overruns"
        % (
            fmt_ms(metrics["tick_p50_ms"]),
            fmt_ms(metrics["tick_p95_ms"]),
            fmt_ms(metrics["tick_p99_ms"]),
            fmt_ms(metrics["tick_max_ms"]),
            metrics["tps"],
            int(metrics["tick_overruns"] or 0),
        )
    )
    print(
        "clients: %d/%d joins, %d/%d interest sets, %d chunks, ping p95 %s ms"
        % (
            clients["joins"],
            scenario["clients"],
            clients["clients_loaded"],
            scenario["clients"],
            clients["unique_chunks"],
            clients["ping_ms"]["p95"],
        )
    )
    print(
        "server load: %d chunks queued (%.2f/s), out %s/s, in %s/s, RSS %s"
        % (
            metrics["chunks_queued"],
            metrics["chunks_per_second"],
            fmt_bytes(metrics["bandwidth_out_bytes_per_second"]),
            fmt_bytes(metrics["bandwidth_in_bytes_per_second"]),
            fmt_mib(metrics["rss_kib"]),
        )
    )
    if result["soak"]:
        soak = result["soak"]
        print(
            "soak: %d autosaves, save %s, reload hashes %s"
            % (soak["autosaves"], fmt_bytes(soak["save_bytes"]), ", ".join(soak["reload"].get("hashes") or []))
        )
    if result["warnings"]:
        for warning in result["warnings"]:
            print("warning: %s" % warning, file=sys.stderr)
    if result["failures"]:
        for failure in result["failures"]:
            print("FAIL: %s" % failure, file=sys.stderr)
    else:
        print("PASS: load gate checks succeeded")
    if args.json:
        path = Path(args.json)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print("wrote %s" % path)
    if args.markdown:
        path = Path(args.markdown)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(render_markdown(result), encoding="utf-8")
        print("wrote %s" % path)
    return 0 if result["result"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
