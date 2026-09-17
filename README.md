# mini-router

A small OpenAI-compatible **LLM aggregator and load balancer**, built to run on
a single-board computer.

You have an Orange Pi Zero 3 running a 1.5B model. Maybe you have three of
them, or two boards and a workstation that is sometimes on. mini-router puts
one OpenAI-compatible endpoint in front of all of them: it knows which box has
which model, which box is alive, which box is busy, and where the next request
should go.

It is a proxy, not a runtime. llama.cpp, Ollama and vLLM do the hard part;
mini-router is the part that should cost you nothing.

```
                    ┌──────────────────────────────┐
  any OpenAI        │         mini-router          │      ┌──────────────┐
  client       ───► │  auth → route → balance →    │ ───► │ opi-zero3 #1 │ llama.cpp
  (SDK, curl,       │  admission control → stream  │      ├──────────────┤
   Open WebUI)      │                              │ ───► │ opi-zero3 #2 │ ollama
                    │  health probes + discovery   │      ├──────────────┤
                    └──────────────────────────────┘ ───► │ cloud (spill)│ fallback_only
                                                          └──────────────┘
```

## Measured footprint

Release build, x86_64, `--profile release`, against a mock upstream streaming
SSE. An `aarch64` board lands in the same neighbourhood.

| | |
|---|---|
| Binary, stripped | **2.3 MB** (1.3 MB with `--no-default-features`, i.e. no TLS) |
| Idle RSS | **4.4 MB** |
| RSS during 16 concurrent streamed completions | **5.4 MB** |
| Dependencies | 93 crates, no C toolchain needed unless you enable TLS |

Memory stays flat under load because response bodies are never buffered: tokens
are forwarded frame by frame as they arrive.

## What it does

- **Aggregates models.** `GET /v1/models` returns the union of every model your
  boards report, discovered automatically and refreshed on each health probe.
- **Balances load** with five strategies, including a latency-aware
  power-of-two-choices that sheds work from a struggling board to a fast one.
- **Admission control.** `max_concurrency` per upstream, with a queue in front
  and a timeout on the queue. Two 1.5B generations do not fit in 1 GB; this is
  the setting that stops them meeting.
- **Fails over.** A refused connection, a timeout or a 5xx moves the request to
  the next upstream that serves the model. A box that keeps failing drops out of
  rotation and is retried after a cooldown.
- **Streams properly.** SSE passes straight through, and the upstream's slot is
  held until the last token, not until the headers.
- **Aliases models**, so an app hard-coded to `gpt-3.5-turbo` reaches whatever
  you actually run.
- **Keeps credentials separate.** Clients authenticate with your key; each
  upstream gets its own, injected by the router and never forwarded from the
  client.
- **Exposes Prometheus metrics** and a JSON view of the whole shelf.

## Quick start

```sh
git clone https://github.com/sudtanj/mini-router
cd mini-router
cargo build --release

cp mini-router.example.toml mini-router.toml
$EDITOR mini-router.toml

./target/release/mini-router --config mini-router.toml
```

The smallest configuration that does something useful:

```toml
[[upstream]]
name = "opi-zero3"
url = "http://127.0.0.1:11434/v1"
max_concurrency = 1
```

Then point any OpenAI client at it:

```sh
curl http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model": "qwen2.5:1.5b",
       "messages": [{"role": "user", "content": "why is the sky blue?"}],
       "stream": true}'
```

```python
from openai import OpenAI

client = OpenAI(base_url="http://opi.local:8080/v1", api_key="not-needed")
print(client.models.list())
```

Check the configuration without starting anything:

```sh
mini-router --config mini-router.toml --check
```

## Building for an Orange Pi Zero 3

The Zero 3 is `aarch64`. Build a static binary on your workstation and copy it
over — compiling on the board itself works but takes a while, and linking wants
more RAM than a 1 GB board is happy to give.

```sh
rustup target add aarch64-unknown-linux-musl
cargo install cross          # uses Docker/Podman, no cross-toolchain to install

cross build --release --target aarch64-unknown-linux-musl
scp target/aarch64-unknown-linux-musl/release/mini-router orangepi@opi.local:
```

If every upstream is a plain-HTTP box on your LAN, drop TLS and save a megabyte:

```sh
cross build --release --target aarch64-unknown-linux-musl --no-default-features
```

Installing as a service:

```sh
sudo install -m755 mini-router /usr/local/bin/
sudo install -m644 -D mini-router.toml /etc/mini-router/mini-router.toml
sudo install -m644 deploy/mini-router.service /etc/systemd/system/
sudo systemctl enable --now mini-router
```

The unit in `deploy/` runs as a dedicated user with a hardened sandbox and a
`MemoryMax` of 64 MB — generous by a factor of ten, and a safety net on a board
where the OOM killer would otherwise pick the model server.

There is also a `deploy/Dockerfile` if you would rather run it in a container.

## Endpoints

| Endpoint | Purpose |
|---|---|
| `POST /v1/chat/completions` | Routed and streamed |
| `POST /v1/completions`, `/v1/embeddings`, … | Routed the same way |
| `ANY /v1/*` | Anything else is forwarded, so new upstream endpoints keep working |
| `GET /v1/models` | Aggregated catalogue across all upstreams, plus aliases |
| `GET /v1/models/{id}` | One model, or 404 if nothing serves it |
| `GET /healthz` | Liveness. Unauthenticated; what a supervisor restarts on |
| `GET /readyz` | 200 if at least one upstream is in rotation, else 503 |
| `GET /metrics` | Prometheus exposition |
| `GET /admin/upstreams` | Health, load, latency and models per upstream |

Every proxied response carries `x-mini-router-upstream`, naming the box that
answered. When three boards disagree about what a model should say, this is the
header you want.

## Balancing strategies

Set `balance.strategy`:

| Strategy | Use it when |
|---|---|
| `p2c-latency` *(default)* | Mixed hardware. Picks two candidates at random and takes the one with the better time-to-first-byte × queue depth. Follows real capacity without the stampede of pure least-conn. |
| `least-conn` | Uniform boards, long generations. |
| `round-robin` | Uniform boards, short and predictable requests. |
| `weighted` | One box is genuinely *n* times the others; set `weight`. |
| `first-available` | A primary with spares: everything goes to the first healthy upstream. |

Two flags shape the pool before any strategy runs:

- `max_concurrency` — requests in flight per upstream. Anything over the limit
  waits up to `server.queue_timeout_secs`, then gets a `429`. On a 1 GB board
  running a 1.5B model, this should be `1`.
- `fallback_only` — the upstream is skipped unless no ordinary upstream serves
  the requested model. This is how a paid cloud endpoint sits behind a shelf of
  boards without stealing their traffic.

## Health and discovery

Every `health.interval_secs`, each upstream gets a `GET {url}/models`. The
response does double duty: proof of life, and the list of models that box is
currently holding. Swap a model on a board and routing follows within one probe
interval, no restart.

`failure_threshold` consecutive failures — probes or real requests — take an
upstream out of rotation for `cooldown_secs`. It then gets retried, and needs
`success_threshold` consecutive successes to be trusted again. An upstream that
reports no models at all is treated as a wildcard rather than as empty, because
some llama.cpp builds do not implement `/models`.

## Authentication

```toml
[server.auth]
require_auth = true
api_keys = ["sk-choose-something-long"]
api_key_envs = ["MINI_ROUTER_CLIENT_KEY"]   # or keep it out of the file
```

Keys are compared in constant time. `/healthz` and `/readyz` stay open so a
supervisor can reach them; `/metrics` and `/admin/*` require a key when
`require_auth` is on.

Each upstream's own credential is set per-block with `api_key` or `api_key_env`.
The client's `Authorization` header is never forwarded — it is replaced.

## Configuration

Everything is one TOML file, and every key has a default that is sensible on a
small board. `mini-router.example.toml` documents all of them inline. Unknown
keys are a hard error rather than a silent no-op, so a typo fails at `--check`
rather than at 3am.

| Environment variable | Effect |
|---|---|
| `MINI_ROUTER_CONFIG` | Default config path |
| `MINI_ROUTER_LOG` | `error`/`warn`/`info`/`debug`/`trace`, overrides `server.log_level` |

## Metrics

```
mini_router_requests_total
mini_router_responses_total{class="2xx|4xx|5xx"}
mini_router_streaming_requests_total
mini_router_retries_total
mini_router_queue_timeouts_total
mini_router_no_upstream_total
mini_router_unauthorized_total
mini_router_uptime_seconds
mini_router_upstream_up{upstream}
mini_router_upstream_inflight{upstream}
mini_router_upstream_capacity{upstream}
mini_router_upstream_ttfb_ewma_ms{upstream}
mini_router_upstream_requests_total{upstream}
mini_router_upstream_failures_total{upstream}
```

`mini_router_queue_timeouts_total` climbing means the shelf is saturated: add a
board, or raise `max_concurrency` if there is memory headroom to spare.
`mini_router_upstream_ttfb_ewma_ms` diverging between boards usually means one
of them is thermally throttled or swapping.

## Design notes

Choices that follow directly from "this has to run on a 1 GB board":

- **No buffered response bodies.** A completion body is a wrapper around the
  upstream's body that forwards frames as they arrive. Memory does not grow with
  generation length.
- **The concurrency permit lives as long as the stream.** It is held by the
  response body wrapper, so it is released when the last token is sent *or* when
  the client hangs up mid-stream — not when the headers came back.
- **Atomics on the hot path.** Health, load and latency are atomics; the only
  lock is around a model list that changes once per probe.
- **A small dependency tree.** No `rand` (a thread-local xorshift covers
  power-of-two-choices), no metrics framework (a handful of counters and a
  string builder), no HTTP client crate beyond `hyper-util`.
- **512 KB worker stacks and two worker threads by default.** The default
  2 MB × one-per-core is real memory on a board that has little of it.
- **The timeout is on headers, not on the response.** A 20-minute generation is
  not an error; an upstream that never answers is.

## Development

```sh
cargo test                                    # unit + integration
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release --no-default-features   # the small build
```

Minimum supported Rust version is **1.85**, checked in CI against the committed
`Cargo.lock`.

The integration tests run real HTTP against mock upstreams on ephemeral ports
and cover failover, streaming, admission control, aliasing, discovery and auth.
`tests/support/mod.rs` has the harness; adding a case is usually a dozen lines.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Status

Early but real: the routing, balancing, failover, streaming and admission
control paths are covered by tests and work. Not yet here — token accounting,
config hot-reload, request-level model fallback chains, HTTP/2 to upstreams. See
[CHANGELOG.md](CHANGELOG.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
