#!/usr/bin/env python3
"""Footprint and load harness for mini-router.

Runs the expensive path end to end: an OpenAI-shaped streaming request that
has to be translated into Anthropic's dialect on the way out and back, against
a mock provider on loopback. Then it reports what that costs.

Designed to run unchanged on a workstation and inside an emulated arm64
container, which is why it is stdlib-only and starts everything it needs.

Read the numbers with one caveat, which the report repeats: under emulation
the *memory* figures are real (RSS is RSS, the allocator and the page cache
behave normally) but the *timing* figures are not -- every instruction is
being interpreted. Treat latency here as a regression signal between runs on
the same machine, never as what an Orange Pi Zero 3 would do.

  python3 scripts/perf.py --binary target/release/mini-router
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# --------------------------------------------------------------------------
# Mock provider: Anthropic dialect, so every request exercises translation
# --------------------------------------------------------------------------

TOKENS = 64
MODEL = "claude-haiku-4-5"


class Provider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):  # noqa: D401 - silence the default logging
        pass

    def _json(self, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802 - name fixed by BaseHTTPRequestHandler
        self._json(
            {
                "data": [{"type": "model", "id": MODEL, "display_name": MODEL}],
                "has_more": False,
            }
        )

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("content-length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")

        if not request.get("stream"):
            self._json(
                {
                    "id": "msg_perf",
                    "type": "message",
                    "role": "assistant",
                    "model": request.get("model", MODEL),
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 8, "output_tokens": 2},
                }
            )
            return

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()

        def event(name: str, payload: dict) -> None:
            frame = f"event: {name}\ndata: {json.dumps(payload)}\n\n".encode()
            self.wfile.write(b"%x\r\n" % len(frame) + frame + b"\r\n")
            self.wfile.flush()

        try:
            event(
                "message_start",
                {
                    "type": "message_start",
                    "message": {"id": "msg_perf", "usage": {"input_tokens": 8}},
                },
            )
            event(
                "content_block_start",
                {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                },
            )
            for i in range(TOKENS):
                event(
                    "content_block_delta",
                    {
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": f"tok{i} "},
                    },
                )
            event("content_block_stop", {"type": "content_block_stop", "index": 0})
            event(
                "message_delta",
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": TOKENS},
                },
            )
            event("message_stop", {"type": "message_stop"})
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# --------------------------------------------------------------------------
# Load generation
# --------------------------------------------------------------------------

REQUEST = json.dumps(
    {
        "model": "fast",
        "stream": True,
        "messages": [{"role": "user", "content": "why is the sky blue?"}],
    }
).encode()


class Result:
    __slots__ = ("ok", "ttft", "total", "tokens", "error")

    def __init__(self):
        self.ok = False
        self.ttft = 0.0
        self.total = 0.0
        self.tokens = 0
        self.error = ""


def one_request(port: int, result: Result) -> None:
    """One streamed completion, measured at the first token and at the end."""
    started = time.perf_counter()
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=30) as sock:
            sock.sendall(
                b"POST /v1/chat/completions HTTP/1.1\r\n"
                b"Host: localhost\r\n"
                b"Content-Type: application/json\r\n"
                b"Content-Length: " + str(len(REQUEST)).encode() + b"\r\n"
                b"Connection: close\r\n\r\n" + REQUEST
            )
            sock.settimeout(30)
            seen_body = False
            chunks = 0
            buf = b""
            while True:
                data = sock.recv(65536)
                if not data:
                    break
                buf += data
                if not seen_body and b"\r\n\r\n" in buf:
                    seen_body = True
                    if not buf.split(b"\r\n", 1)[0].endswith(b"200 OK"):
                        result.error = buf.split(b"\r\n", 1)[0].decode(errors="replace")
                        return
                if seen_body and result.ttft == 0.0 and b"chat.completion.chunk" in buf:
                    result.ttft = time.perf_counter() - started
                chunks += buf.count(b"chat.completion.chunk")
                buf = buf[-64:] if seen_body else buf
            result.tokens = chunks
            result.total = time.perf_counter() - started
            result.ok = result.ttft > 0.0
            if not result.ok:
                result.error = "no chunk seen"
    except Exception as exc:  # noqa: BLE001 - any failure is a failed request
        result.error = f"{type(exc).__name__}: {exc}"


def rss_kb(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/status", "r", encoding="ascii") as fh:
            match = re.search(r"^VmRSS:\s+(\d+) kB", fh.read(), re.M)
            return int(match.group(1)) if match else 0
    except OSError:
        return 0


class RssSampler(threading.Thread):
    """Poll RSS while the load runs, so the peak is a real peak."""

    def __init__(self, pid: int, interval: float = 0.05):
        super().__init__(daemon=True)
        self.pid = pid
        self.interval = interval
        self.peak = 0
        self.samples: list[int] = []
        self._done = threading.Event()

    def run(self) -> None:
        while not self._done.is_set():
            value = rss_kb(self.pid)
            if value:
                self.samples.append(value)
                self.peak = max(self.peak, value)
            self._done.wait(self.interval)

    def stop(self) -> None:
        self._done.set()
        self.join(timeout=2)


def wait_for_health(port: int, timeout: float = 30.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                sock.sendall(
                    b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                )
                if b"200" in sock.recv(64):
                    return
        except OSError:
            pass
        time.sleep(0.2)
    raise SystemExit("mini-router did not become healthy in time")


# --------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", default="target/release/mini-router")
    ap.add_argument(
        "--runner",
        default="",
        help="command to launch the binary through, e.g. "
        "'qemu-aarch64 -L /usr/aarch64-linux-gnu' to run an arm64 build on an "
        "x86_64 host. Empty means run it directly.",
    )
    ap.add_argument("--concurrency", type=int, default=16)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument(
        "--max-rss-mb",
        type=float,
        default=64.0,
        help="fail if peak RSS exceeds this (default: the systemd unit's MemoryMax)",
    )
    ap.add_argument(
        "--max-growth-pct",
        type=float,
        default=25.0,
        help="fail if RSS grows more than this from the first round to the last; "
        "catches a response body being buffered instead of streamed",
    )
    ap.add_argument("--json", dest="json_out", help="write raw results here")
    ap.add_argument("--summary", help="write a markdown summary here")
    ap.add_argument(
        "--label",
        default=f"{os.uname().machine} ({os.cpu_count()} cpu)",
        help="what to call this machine in the report",
    )
    args = ap.parse_args()

    binary = os.path.abspath(args.binary)
    if not os.path.isfile(binary):
        raise SystemExit(f"no binary at {binary} -- build it first")
    binary_bytes = os.path.getsize(binary)

    provider_port = free_port()
    router_port = free_port()
    provider = ThreadingHTTPServer(("127.0.0.1", provider_port), Provider)
    provider.daemon_threads = True
    threading.Thread(target=provider.serve_forever, daemon=True).start()

    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "MINI_ROUTER_LISTEN": f"127.0.0.1:{router_port}",
        "MINI_ROUTER_PROVIDER_MOCK_URL": f"http://127.0.0.1:{provider_port}/v1",
        "MINI_ROUTER_PROVIDER_MOCK_PROTOCOL": "anthropic",
        "MINI_ROUTER_PROVIDER_MOCK_API_KEY": "sk-perf",
        "MINI_ROUTER_PROVIDER_MOCK_MAX_CONCURRENCY": str(max(args.concurrency, 1)),
        "MINI_ROUTER_POOL_FAST": f"mock:{MODEL}",
        "MINI_ROUTER_HEALTH_INTERVAL_SECS": "0",
        "MINI_ROUTER_LOG": "warn",
    }
    argv = args.runner.split() + [binary] if args.runner else [binary]
    router = subprocess.Popen(  # noqa: S603 - our own binary, fixed argv
        argv,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    try:
        wait_for_health(router_port)
        # Let start-up allocations settle before calling anything "idle".
        time.sleep(1.0)
        idle = rss_kb(router.pid)

        sampler = RssSampler(router.pid)
        sampler.start()

        rounds: list[dict] = []
        all_results: list[Result] = []
        for round_no in range(args.rounds):
            results = [Result() for _ in range(args.concurrency)]
            threads = [
                threading.Thread(target=one_request, args=(router_port, r))
                for r in results
            ]
            started = time.perf_counter()
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            elapsed = time.perf_counter() - started
            all_results.extend(results)
            rounds.append(
                {
                    "round": round_no + 1,
                    "rss_kb": rss_kb(router.pid),
                    "seconds": round(elapsed, 3),
                    "ok": sum(1 for r in results if r.ok),
                    "failed": sum(1 for r in results if not r.ok),
                }
            )

        sampler.stop()

        ok = [r for r in all_results if r.ok]
        failed = [r for r in all_results if not r.ok]
        ttfts = sorted(r.ttft for r in ok)
        peak = sampler.peak
        first_round = rounds[0]["rss_kb"] or 1
        growth_pct = (rounds[-1]["rss_kb"] - first_round) / first_round * 100.0

        report = {
            "label": args.label,
            "binary_bytes": binary_bytes,
            "idle_rss_kb": idle,
            "peak_rss_kb": peak,
            "rss_growth_pct": round(growth_pct, 2),
            "concurrency": args.concurrency,
            "rounds": rounds,
            "requests": len(all_results),
            "ok": len(ok),
            "failed": len(failed),
            "tokens_per_response": TOKENS,
            "ttft_ms": {
                "min": round(ttfts[0] * 1000, 1) if ttfts else None,
                "median": round(ttfts[len(ttfts) // 2] * 1000, 1) if ttfts else None,
                "max": round(ttfts[-1] * 1000, 1) if ttfts else None,
            },
            "errors": sorted({r.error for r in failed})[:5],
        }

        lines = [
            f"### mini-router footprint — {args.label}",
            "",
            "| Measure | Value |",
            "|---|---|",
            f"| Binary, stripped | {binary_bytes / 1e6:.2f} MB |",
            f"| Idle RSS | {idle / 1024:.1f} MB |",
            f"| Peak RSS under {args.concurrency} concurrent translated streams "
            f"| {peak / 1024:.1f} MB |",
            f"| RSS growth over {args.rounds} rounds | {growth_pct:+.1f}% |",
            f"| Requests | {len(ok)} ok, {len(failed)} failed |",
            f"| Time to first token (median) | {report['ttft_ms']['median']} ms |",
            "",
            "Each request is an OpenAI-shaped streamed completion served by an "
            "Anthropic-dialect provider, so every byte crosses the translator.",
            "",
        ]
        if args.runner:
            lines += [
                f"Run through `{args.runner}`. **Under emulation these numbers "
                "are not the machine's**: QEMU's translation buffers land in the "
                "same RSS, and every instruction is interpreted. Emulated runs "
                "are for proving the code works on the architecture, not for "
                "measuring it.",
            ]
        else:
            lines += [
                "Memory figures are what the process actually used. Timings are "
                "only comparable against another run on the same machine.",
            ]
        summary = "\n".join(lines)
        print(summary)
        if failed:
            print("\nfailures:", *report["errors"], sep="\n  ")

        if args.json_out:
            with open(args.json_out, "w", encoding="utf-8") as fh:
                json.dump(report, fh, indent=2)
        if args.summary:
            with open(args.summary, "a", encoding="utf-8") as fh:
                fh.write(summary + "\n")

        problems = []
        if failed:
            problems.append(f"{len(failed)}/{len(all_results)} requests failed")
        if peak / 1024 > args.max_rss_mb:
            problems.append(
                f"peak RSS {peak / 1024:.1f} MB exceeds budget {args.max_rss_mb} MB"
            )
        if growth_pct > args.max_growth_pct:
            problems.append(
                f"RSS grew {growth_pct:.1f}% across rounds, over the "
                f"{args.max_growth_pct}% budget — is something being buffered?"
            )
        if problems:
            print("\nFAILED:", *problems, sep="\n  - ")
            return 1
        print("\nWithin budget.")
        return 0
    finally:
        router.terminate()
        try:
            router.wait(timeout=10)
        except subprocess.TimeoutExpired:
            router.kill()
        provider.shutdown()


if __name__ == "__main__":
    sys.exit(main())
