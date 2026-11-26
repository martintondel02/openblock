#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Small, dependency-free load/soak harness for the interim OpenBlock server.

Each bot is a real TCP client speaking the JSON framing documented in
docs/spec/18-networking-multiplayer.md §3.13: a big-endian u32 payload length
followed by UTF-8 JSON. Bots perform the Hello handshake, request chunks, move,
chat, ping, and optionally place/break blocks. The script reports connection,
throughput, operation and latency numbers and exits non-zero if the run fails.

Examples:
    # 4 clients for 10 seconds against a locally running server
    python3 scripts/load-test.py --clients 4 --seconds 10

    # Longer soak with a JSON artifact for CI or a report
    python3 scripts/load-test.py --clients 8 --seconds 300 --scenario mixed \
        --json target/load-test.json

    # Interact scenario needs a creative server (placement requires the block
    # to be in the hotbar in survival):
    openblock-server --listen 127.0.0.1:25565 --data-dir ./data --creative
    python3 scripts/load-test.py --clients 4 --seconds 20 --scenario interact

Stdlib only. The client is intentionally minimal: it does not decode chunk
payloads, simulate physics or verify authoritative state. It is a smoke/load
tool, not an anti-cheat or protocol conformance test.
"""

from __future__ import annotations

import argparse
import json
import math
import random
import socket
import struct
import sys
import threading
import time
from collections import Counter

PROTOCOL_VERSION = 5
MAX_FRAME_LEN = 2 * 1024 * 1024
CHUNK_SIZE = 16
SCENARIOS = ("idle", "movement", "chat", "interact", "mixed")


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


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int(math.ceil(fraction * len(ordered))) - 1))
    return ordered[index]


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.hellos = 0
        self.joins = 0
        self.chunks = 0
        self.disconnects = 0
        self.errors = []
        self.rejections = 0
        self.messages = Counter()
        self.sent = Counter()
        self.pings = []
        self.bytes_received = 0
        self.clients_finished = 0
        self.server_ticks = []

    def add_error(self, label, error):
        with self.lock:
            self.errors.append("%s: %s" % (label, error))

    def add_message(self, kind, size):
        with self.lock:
            self.messages[kind] += 1
            self.bytes_received += size

    def add_sent(self, kind):
        with self.lock:
            self.sent[kind] += 1

    def inc(self, field, amount=1):
        with self.lock:
            setattr(self, field, getattr(self, field) + amount)

    def add_server_tick(self, tick):
        with self.lock:
            self.server_ticks.append(tick)

    def add_ping(self, milliseconds):
        with self.lock:
            self.pings.append(milliseconds)

    def finish(self):
        with self.lock:
            self.clients_finished += 1


class Bot(threading.Thread):
    def __init__(self, index, args, stats, stop, run_id):
        super().__init__(daemon=True)
        self.index = index
        self.run_id = run_id
        self.args = args
        self.stats = stats
        self.stop = stop
        self.name = self._make_name(index)
        self.sock = None
        self.buffer = b""
        self.pos = [0.0, 0.0, 0.0]
        self.spawn = [0.0, 0.0, 0.0]
        self.player_id = None
        self.place_block = 1
        self.pending_pings = {}
        self.next_ping = 0
        self.next_move = 0.0
        self.next_chat = 0.0
        self.next_edit = 0.0
        self.next_chunk = 0.0
        self.start_time = 0.0
        self.tick = 0

    def _make_name(self, index):
        prefix = "".join(ch if (ch.isascii() and (ch.isalnum() or ch in "_.-")) else "-" for ch in self.args.name_prefix)
        # Leave room for -<run id>-<index>, so back-to-back runs do not collide
        # with the previous run's still-online or just-disconnected names.
        prefix = (prefix or "bot")[:23]
        return ("%s-%s-%02d" % (prefix, self.run_id, index))[:32]

    def log(self, message):
        if self.args.verbose:
            print("[%s] %s" % (self.name, message), file=sys.stderr)

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
                self.stats.add_error(self.name, error)
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
        self.sock = socket.create_connection(
            (self.args.host, self.args.port), timeout=self.args.connect_timeout
        )
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.sock.settimeout(self.args.connect_timeout)
        self.send({"type": "hello", "protocol": PROTOCOL_VERSION, "name": self.name}, "hello")
        while time.monotonic() < deadline:
            message = recv_frame(self.sock)
            self.stats.add_message(message.get("type", "unknown"), 0)
            kind = message.get("type")
            if kind == "hello":
                self.stats.inc("hellos")
                self.spawn = [float(value) for value in message.get("spawn", [0.0, 0.0, 0.0])]
                self.pos = list(self.spawn)
                self.stats.add_server_tick(int(message.get("tick", 0)))
                blocks = message.get("blocks") or []
                for block in blocks:
                    if block.get("name") == "openblock:stone":
                        self.place_block = int(block.get("id", 1))
                        break
                self.log(
                    "hello: world=%s seed=%s spawn=%s blocks=%d"
                    % (
                        message.get("world_name"),
                        message.get("seed"),
                        self.spawn,
                        len(blocks),
                    )
                )
                return
            if kind == "disconnect":
                raise ConnectionError("server rejected handshake: %s" % message.get("reason"))
        raise TimeoutError("handshake timed out")

    def _play(self):
        # Poll frequently enough that the harness itself adds only a few
        # milliseconds; the server processes intents on its 50 ms tick.
        self.sock.settimeout(0.02)
        now = time.monotonic()
        self.start_time = now
        self.next_move = now
        self.next_ping = now + self.args.warmup_seconds + 0.25
        self.next_chat = now + 0.5
        self.next_edit = now + 1.0
        self.next_chunk = now + 0.5
        end = now + self.args.seconds

        while not self.stop.is_set() and time.monotonic() < end:
            now = time.monotonic()
            self._maybe_move(now)
            self._maybe_chat(now)
            self._maybe_edit(now)
            self._maybe_chunk(now)
            self._maybe_ping(now)
            self._pump()
            # Keep a little headroom for the server's 120 frames/s inbound cap.
            time.sleep(0.005)

        # Final ping round is intentionally not awaited; the report counts
        # completed pongs only.

    def _maybe_move(self, now):
        if self.args.scenario not in ("movement", "chat", "mixed"):
            return
        if now < self.next_move:
            return
        self.next_move = now + 0.1
        self.tick += 1
        elapsed = now - self.start_time
        # Walk a small circle without changing Y. Step size stays far below the
        # server's 20 m/s allowance even if a tick is delayed.
        radius = 2.0
        angle = elapsed * 0.35 + self.index
        target_x = self.spawn[0] + math.cos(angle) * radius
        target_z = self.spawn[2] + math.sin(angle) * radius
        target_y = self.pos[1]
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
        if self.args.scenario not in ("chat", "mixed"):
            return
        if now < self.next_chat:
            return
        # 5 lines per 10 s; 3 s spacing leaves headroom for clock jitter.
        self.next_chat = now + 3.0
        self.tick += 1
        self.send(
            {"type": "chat", "text": "%s load-test %d" % (self.name, self.tick)},
            "chat",
        )

    def _maybe_edit(self, now):
        if self.args.scenario not in ("interact", "mixed"):
            return
        if now < self.next_edit:
            return
        self.next_edit = now + 2.0
        x = math.floor(self.pos[0])
        y = max(-63, math.floor(self.pos[1]) - 1)
        z = math.floor(self.pos[2])
        # Break the block under the feet, then place it back. Survival servers
        # may reject placement without a held item; that is informational, not
        # a harness failure. Creative servers exercise both paths.
        self.send({"type": "block_edit", "pos": [x, y, z], "block": 0}, "break")
        self.send({"type": "block_edit", "pos": [x, y, z], "block": self.place_block}, "place")

    def _maybe_chunk(self, now):
        if now < self.next_chunk:
            return
        self.next_chunk = now + 2.0
        cx = int(math.floor(self.pos[0] / CHUNK_SIZE))
        cz = int(math.floor(self.pos[2] / CHUNK_SIZE))
        if self.args.scenario == "idle":
            cx += random.randint(-1, 1)
            cz += random.randint(-1, 1)
        else:
            cx += random.randint(-2, 2)
            cz += random.randint(-2, 2)
        self.send({"type": "chunk_request", "cx": cx, "cz": cz}, "chunk_request")

    def _maybe_ping(self, now):
        if now < self.next_ping:
            return
        self.next_ping = now + 1.0
        nonce = self.index * 1_000_000 + int(now * 1000) % 1_000_000
        self.pending_pings[nonce] = time.monotonic()
        self.send({"type": "ping", "id": nonce}, "ping")

    def _pump(self):
        while True:
            try:
                data = self.sock.recv(65536)
            except socket.timeout:
                break
            except BlockingIOError:
                break
            if not data:
                raise ConnectionError("server closed the connection")
            self.buffer += data
        while len(self.buffer) >= 4:
            length = struct.unpack(">I", self.buffer[:4])[0]
            if length == 0:
                raise ConnectionError("server sent an empty frame")
            if length > MAX_FRAME_LEN:
                raise ConnectionError("server frame of %d bytes exceeds the limit" % length)
            if len(self.buffer) < 4 + length:
                break
            payload = self.buffer[4 : 4 + length]
            self.buffer = self.buffer[4 + length :]
            message = json.loads(payload.decode("utf-8"))
            self.stats.add_message(message.get("type", "unknown"), len(payload))
            self._handle(message)

    def _handle(self, message):
        kind = message.get("type")
        if kind == "join":
            if message.get("name") == self.name:
                self.player_id = message.get("id")
                self.stats.inc("joins")
        elif kind == "player_state":
            if self.player_id is not None and message.get("id") == self.player_id:
                pos = message.get("pos")
                if isinstance(pos, list) and len(pos) == 3:
                    self.pos = [float(value) for value in pos]
        elif kind == "pong":
            sent = self.pending_pings.pop(message.get("id"), None)
            if sent is not None:
                self.stats.add_ping((time.monotonic() - sent) * 1000.0)
        elif kind == "chunk_data":
            self.stats.inc("chunks")
        elif kind == "action_rejected":
            self.stats.inc("rejections")
            self.log("action rejected: %s %s" % (message.get("action"), message.get("reason")))
        elif kind == "disconnect":
            reason = message.get("reason", "disconnected")
            self.stats.inc("disconnects")
            raise ConnectionError("server disconnected: %s" % reason)


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="Load/soak harness for the interim OpenBlock JSON/TCP server.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--clients", type=int, default=4, help="number of bot clients (default: 4)")
    parser.add_argument("--seconds", type=float, default=10.0, help="run duration in seconds (default: 10)")
    parser.add_argument("--host", default="127.0.0.1", help="server host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=25565, help="server TCP port (default: 25565)")
    parser.add_argument(
        "--scenario",
        choices=SCENARIOS,
        default="mixed",
        help="idle, movement, chat, interact or mixed (default: mixed)",
    )
    parser.add_argument("--name-prefix", default="bot", help="player name prefix (default: bot)")
    parser.add_argument("--connect-timeout", type=float, default=5.0, help="connect/handshake timeout in seconds")
    parser.add_argument(
        "--warmup-seconds",
        type=float,
        default=2.0,
        help="delay ping measurement while initial chunks stream (default: 2)",
    )
    parser.add_argument(
        "--max-p95-ms",
        type=float,
        default=1000.0,
        help="fail above this p95 ping latency (default: 1000)",
    )
    parser.add_argument("--json", metavar="PATH", help="write a JSON result artifact")
    parser.add_argument("--verbose", action="store_true", help="log per-client progress and rejections")
    args = parser.parse_args(argv)
    if args.clients < 1:
        parser.error("--clients must be at least 1")
    if args.clients > 500:
        parser.error("--clients must be at most 500 (server maximum)")
    if args.seconds < 1:
        parser.error("--seconds must be at least 1")
    if args.warmup_seconds < 0:
        parser.error("--warmup-seconds must not be negative")
    if args.warmup_seconds >= args.seconds:
        parser.error("--warmup-seconds must be less than --seconds")
    if not 1 <= args.port <= 65535:
        parser.error("--port must be in 1..65535")
    return args


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    stats = Stats()
    stop = threading.Event()
    run_id = "%04x" % random.getrandbits(16)
    bots = [Bot(index, args, stats, stop, run_id) for index in range(args.clients)]
    started = time.monotonic()
    for bot in bots:
        bot.start()

    while time.monotonic() - started < args.seconds:
        if not any(bot.is_alive() for bot in bots):
            break
        time.sleep(0.1)
    stop.set()
    deadline = time.monotonic() + max(2.0, args.connect_timeout)
    for bot in bots:
        remaining = max(0.0, deadline - time.monotonic())
        bot.join(timeout=remaining)

    elapsed = time.monotonic() - started
    pings = list(stats.pings)
    p50 = percentile(pings, 0.50)
    p95 = percentile(pings, 0.95)
    p99 = percentile(pings, 0.99)
    errors = list(stats.errors)
    failures = []
    if stats.hellos < args.clients:
        failures.append("only %d/%d clients completed the handshake" % (stats.hellos, args.clients))
    if stats.chunks == 0:
        failures.append("no chunk_data received")
    if len(pings) < args.clients:
        failures.append("only %d pong(s) for %d clients" % (len(pings), args.clients))
    if stats.disconnects:
        failures.append("%d unexpected disconnect(s)" % stats.disconnects)
    if errors:
        failures.append("%d client error(s)" % len(errors))
    if p95 is not None and p95 > args.max_p95_ms:
        failures.append("p95 ping %.1f ms exceeds %.1f ms" % (p95, args.max_p95_ms))

    result = {
        "clients": args.clients,
        "seconds": round(elapsed, 2),
        "warmup_seconds": args.warmup_seconds,
        "name_prefix": "%s-%s" % (args.name_prefix, run_id),
        "scenario": args.scenario,
        "endpoint": "%s:%d" % (args.host, args.port),
        "handshakes": stats.hellos,
        "joins": stats.joins,
        "chunks_received": stats.chunks,
        "pings_completed": len(pings),
        "p50_ms": round(p50, 2) if p50 is not None else None,
        "p95_ms": round(p95, 2) if p95 is not None else None,
        "p99_ms": round(p99, 2) if p99 is not None else None,
        "max_ms": round(max(pings), 2) if pings else None,
        "messages_received": dict(sorted(stats.messages.items())),
        "ops_sent": dict(sorted(stats.sent.items())),
        "action_rejections": stats.rejections,
        "bytes_received": stats.bytes_received,
        "server_ticks_at_join": stats.server_ticks,
        "errors": errors,
        "result": "pass" if not failures else "fail",
        "failures": failures,
    }

    print(
        "OpenBlock load test: %d client(s), %.1f s, scenario=%s, %s:%d (warmup %.1f s)"
        % (args.clients, elapsed, args.scenario, args.host, args.port, args.warmup_seconds)
    )
    print(
        "connected: %d/%d handshakes, %d joins, %d chunks, %d pongs"
        % (stats.hellos, args.clients, stats.joins, stats.chunks, len(pings))
    )
    print(
        "messages: %s"
        % ", ".join("%s=%d" % (key, value) for key, value in sorted(stats.messages.items()))
    )
    print(
        "ops sent: %s"
        % ", ".join("%s=%d" % (key, value) for key, value in sorted(stats.sent.items()))
    )
    if pings:
        print(
            "ping: p50=%.1f ms p95=%.1f ms p99=%.1f ms max=%.1f ms (%d samples)"
            % (p50, p95, p99, max(pings), len(pings))
        )
    print(
        "traffic: %.1f KiB received; action rejections: %d (informational)"
        % (stats.bytes_received / 1024.0, stats.rejections)
    )
    sys.stdout.flush()
    if errors:
        for error in errors[:10]:
            print("error: %s" % error, file=sys.stderr)
        if len(errors) > 10:
            print("error: ... %d more" % (len(errors) - 10), file=sys.stderr)
    if failures:
        for failure in failures:
            print("FAIL: %s" % failure, file=sys.stderr)
    else:
        print("PASS: all clients connected, received chunks and pongs within the latency bound")

    if args.json:
        with open(args.json, "w", encoding="utf-8") as handle:
            json.dump(result, handle, indent=2, sort_keys=True)
            handle.write("\n")
        print("wrote %s" % args.json)
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
