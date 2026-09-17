# mini-router

**One endpoint in front of every LLM provider you already pay for.**

Point any OpenAI client *or* any Anthropic client at mini-router, ask for a
model, and it picks a provider, translates the dialect if it has to, and falls
through to the next provider the moment anything goes wrong.

The models are remote. The router is not: it is built to sit on a small
always-on box on your own network — an Orange Pi Zero 3 is the design target —
in 2.4 MB of binary and under 5 MB of RAM.

```
  OpenAI SDK      ─┐                                        ┌─► api.openai.com
  (base_url=       │   ┌──────────────────────────┐         │
   .../v1)         ├──►│        mini-router       │────────►├─► api.anthropic.com
                   │   │                          │         │
  Anthropic SDK   ─┘   │  auth → pool → order →   │         ├─► api.groq.com
  (base_url=           │  translate → spill over  │         │
   http://pi:8080)     └──────────────────────────┘         └─► openrouter.ai
```

Either front door reaches either kind of provider. An OpenAI-shaped request can
be served by Anthropic and come back OpenAI-shaped; an Anthropic-shaped request
can be served by Groq and come back Anthropic-shaped. Streams, tool calls and
images included.

## Why

If you have accounts at two or three providers, every app you run needs to know
which one it is talking to, which key to use, and what to do when that provider
is rate-limited. mini-router makes that one address and one key. Behind it:

- **Pools.** `fast` is `gpt-4o-mini`, or `claude-haiku-4-5` when OpenAI is
  having a bad day. Your app just asks for `fast`.
- **Spillover on anything.** Not only rate limits: a refused connection, a
  timeout, an expired key, a 400, a model the provider has never heard of — all
  of them move the request to the next member.
- **Protocol translation**, so the pool can span providers that do not speak the
  same dialect.

## Measured footprint

Release build, x86_64, streaming SSE from a mock provider **through the
translator** (the expensive path).

| | |
|---|---|
| Binary, stripped | **2.4 MB** (1.4 MB with `--no-default-features`, no TLS) |
| Idle RSS | **4.8 MB** |
| RSS during 48 concurrent translated streams | **4.8 MB** — unchanged |
| Dependencies | 93 crates |

Memory is flat under load because nothing is buffered. A same-dialect response
is forwarded frame by frame without being read; a translated one goes through
an incremental state machine, still frame by frame. A twenty-minute generation
costs the same as a one-line one.

## Quick start

```sh
git clone https://github.com/sudtanj/mini-router
cd mini-router
cargo build --release

cp mini-router.example.toml mini-router.toml
$EDITOR mini-router.toml

export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
./target/release/mini-router --config mini-router.toml
```

A configuration that does something useful:

```toml
[[upstream]]
name = "openai"
url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[upstream]]
name = "anthropic"
url = "https://api.anthropic.com/v1"
protocol = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[pool.fast]
members = [
  { upstream = "openai",    model = "gpt-4o-mini" },
  { upstream = "anthropic", model = "claude-haiku-4-5" },
]
```

Now every client can use it, whichever SDK it was written against:

```python
from openai import OpenAI
client = OpenAI(base_url="http://pi.local:8080/v1", api_key="not-needed")
client.chat.completions.create(model="fast", messages=[{"role": "user", "content": "hi"}])

from anthropic import Anthropic
client = Anthropic(base_url="http://pi.local:8080", api_key="not-needed")
client.messages.create(model="fast", max_tokens=256,
                       messages=[{"role": "user", "content": "hi"}])
```

Both of those calls can end up at the *same* provider. `fast` resolves to
whichever member is healthy, and the answer is reshaped to match whichever SDK
asked.

```sh
curl http://localhost:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model": "fast", "stream": true,
       "messages": [{"role": "user", "content": "why is the sky blue?"}]}'
```

Check the configuration without starting anything:

```sh
mini-router --config mini-router.toml --check
```

## Pools

A pool is one client-facing name over several provider models, tried in the
order you wrote them:

```toml
[pool.smart]
description = "The good models, for when it matters"
members = [
  { upstream = "anthropic", model = "claude-sonnet-4-5" },
  { upstream = "openai",    model = "gpt-4o" },
]
```

The first member serves. If it fails — for **any** reason — the second one
does, and the client never learns there was a problem. Response headers say
what actually happened:

| Header | Meaning |
|---|---|
| `x-mini-router-upstream` | Which provider answered |
| `x-mini-router-model` | Which model id it was asked for |
| `x-mini-router-translated` | e.g. `anthropic->openai`, absent when untranslated |

Pools can override the global strategy — useful to burn two accounts' quota
evenly rather than exhausting one and then the other:

```toml
[pool.spread]
strategy = "round-robin"
members = [
  { upstream = "openai", model = "gpt-4o-mini", weight = 1 },
  { upstream = "groq",   model = "llama-3.3-70b-versatile", weight = 3 },
]
```

| Strategy | Use it when |
|---|---|
| `priority` *(default)* | Declaration order. "Cheap provider first, good one as backup." |
| `round-robin` | Spread evenly across interchangeable accounts. |
| `weighted` | One account should take *n* times the traffic. |
| `least-conn` | Long generations, and you want the idle provider. |
| `p2c-latency` | Providers whose speed varies; picks on time-to-first-byte × queue depth. |

Whatever the strategy, the ordering is a full list — everything not picked
first is the spillover path, in order.

## Spillover

The default is `spillover = "any-error"`: a candidate is used up by anything
that is not a 2xx, plus every transport failure.

```
connection refused ─┐
timeout             │
401 bad key         ├──► try the next member
404 unknown model   │
429 rate limited    │
500 / 502 / 529     ┘
```

When the list runs out, the client gets **the last provider's own error**,
status and message intact, reshaped into the client's dialect — that is far
more useful than a synthetic "all upstreams failed".

A provider that answers `429` with `Retry-After` is parked for exactly that
long (up to `health.max_cooldown_secs`) instead of being asked again.

Narrow it if you would rather a genuine `400` reach the client immediately:

```toml
[balance]
spillover = "status-list"
retry_on_status = [429, 500, 502, 503, 529]
```

## Protocol translation

| Client speaks | Provider speaks | What happens |
|---|---|---|
| OpenAI | OpenAI | Passthrough, zero-copy |
| Anthropic | Anthropic | Passthrough, zero-copy |
| OpenAI | Anthropic | Translated both ways |
| Anthropic | OpenAI | Translated both ways |

What survives the trip: system prompts (moved between the top-level `system`
field and a `system` message), multi-turn conversations, tool definitions, tool
calls and tool results, images (base64 and URL), `stop`/`stop_sequences`,
temperature and top-p, token usage, and stop/finish reasons.

Streaming is translated incrementally. An Anthropic
`message_start` / `content_block_delta` / `message_stop` sequence becomes a run
of OpenAI `chat.completion.chunk`s ending in `data: [DONE]`, and vice versa —
tool-call argument fragments reassembled correctly in both directions, and the
stream properly terminated even if the provider hangs up mid-generation.

Two things are deliberately dropped rather than mistranslated: Anthropic's
`top_k` (no OpenAI equivalent) and extended-thinking blocks in the
OpenAI→Anthropic direction (they need a signature we cannot forge). Anthropic
thinking deltas going the other way surface as `reasoning_content`, which is
what the OpenAI-compatible providers that expose reasoning have settled on.

Endpoints with no counterpart — embeddings, rerank, audio — are forwarded
unchanged, and only ever to a provider that already speaks the dialect they
were written in.

## Endpoints

| Endpoint | Purpose |
|---|---|
| `POST /v1/chat/completions` | OpenAI dialect in |
| `POST /v1/messages` | Anthropic dialect in |
| `GET /v1/models` | Aggregated catalogue, in whichever dialect you asked |
| `GET /v1/models/{id}` | One model or pool |
| `ANY /v1/*` | Anything else, forwarded to a matching provider |
| `GET /healthz` | Liveness. Unauthenticated; what a supervisor restarts on |
| `GET /readyz` | 200 if any provider is in rotation |
| `GET /metrics` | Prometheus exposition |
| `GET /admin/upstreams` | Providers, pools, health, latency, last error |

`/v1/models` is the one path both dialects share, so the `anthropic-version`
header decides which shape comes back — every Anthropic SDK sends it and
nothing else does. To be explicit, prefix with `/openai/...` or
`/anthropic/...`.

## Authentication

Your clients get one key; each provider gets its own.

```toml
[server.auth]
require_auth = true
api_keys = ["sk-choose-something-long"]
api_key_envs = ["MINI_ROUTER_CLIENT_KEY"]   # or keep it out of the file
```

Client keys are accepted as `Authorization: Bearer` *or* `x-api-key`, so both
SDKs work, and are compared in constant time. The client's credential is never
forwarded: it is replaced with the provider's own, in the header that provider
expects (`Authorization: Bearer` for OpenAI-compatible, `x-api-key` plus
`anthropic-version` for Anthropic).

`/healthz` and `/readyz` stay open for supervisors; `/metrics` and `/admin/*`
need a key when `require_auth` is on.

## Health and discovery

Every `health.interval_secs`, each provider gets a `GET {url}/models` with its
own credentials. It proves the key still works and refreshes the model list, so
a provider that adds a model starts serving it within one interval.

`failure_threshold` consecutive failures — probes or real requests — take a
provider out for `cooldown_secs`, after which it is retried and needs
`success_threshold` successes to be trusted again.

## Running it on a small board

The Zero 3 is `aarch64`. Build on your workstation and copy the binary over.

```sh
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu      # .cargo/config.toml wires it up

cargo build --release --target aarch64-unknown-linux-gnu
scp target/aarch64-unknown-linux-gnu/release/mini-router orangepi@pi.local:
```

Install as a service:

```sh
sudo install -m755 mini-router /usr/local/bin/
sudo install -m644 -D mini-router.toml /etc/mini-router/mini-router.toml
sudo install -m600 -D /dev/null /etc/mini-router/env   # provider keys go here
sudo install -m644 deploy/mini-router.service /etc/systemd/system/
sudo systemctl enable --now mini-router
```

The unit in `deploy/` runs as a transient unprivileged user with a hardened
sandbox and `MemoryMax=64M` — ten times what it has ever needed. Put the
provider keys in `/etc/mini-router/env` (`OPENAI_API_KEY=...`, one per line,
mode 600) and reference them with `api_key_env`, so the config file stays safe
to commit. There is a `deploy/Dockerfile` too.

## Metrics

```
mini_router_requests_total
mini_router_responses_total{class="2xx|4xx|5xx"}
mini_router_streaming_requests_total
mini_router_translated_total
mini_router_retries_total
mini_router_spilled_out_total
mini_router_queue_timeouts_total
mini_router_no_upstream_total
mini_router_unauthorized_total
mini_router_uptime_seconds
mini_router_upstream_info{upstream,protocol}
mini_router_upstream_up{upstream}
mini_router_upstream_inflight{upstream}
mini_router_upstream_capacity{upstream}
mini_router_upstream_ttfb_ewma_ms{upstream}
mini_router_upstream_requests_total{upstream}
mini_router_upstream_failures_total{upstream}
```

`mini_router_spilled_out_total` climbing means requests are exhausting every
member of a pool — usually a key that expired or a provider-wide outage; check
`last_error` in `/admin/upstreams`. A high `mini_router_retries_total` with a
low `spilled_out_total` is the system working as intended.

## Design notes

- **Nothing is buffered on the streaming path.** Same-dialect responses are a
  thin wrapper over the provider's body. Translated ones run each frame through
  an SSE state machine that holds only a partial event.
- **The concurrency permit lives as long as the stream**, owned by the response
  body, so it is released when the last token ships *or* when the client hangs
  up — not when the headers came back.
- **Ordering is a list, not a pick.** Spillover needs to know who is next, and
  next after that.
- **A provider's own error beats ours.** When every candidate fails, the client
  gets the last real response, translated but otherwise intact.
- **Atomics on the hot path.** Health, load and latency are atomics; the only
  lock guards a model list that changes once per probe.
- **A small dependency tree.** No `rand` (a thread-local xorshift covers
  power-of-two-choices), no metrics framework, no date crate, no HTTP client
  crate beyond `hyper-util`.

## Development

```sh
cargo test                                    # 127 tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test --no-default-features              # the no-TLS build
```

Minimum supported Rust version is **1.85**, checked in CI against the committed
`Cargo.lock`.

The integration tests run real HTTP against mock providers of both dialects on
ephemeral ports, and cover the whole four-way matrix, streaming in both
directions, tool calls across dialects, spillover on every error class,
`Retry-After` parking, per-provider credentials, pools and the catalogue.
`tests/support/mod.rs` has the harness.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Status

Early but real: routing, pools, spillover, translation and streaming are
covered by tests and work. Not yet here — cost-aware routing, response caching,
config hot-reload, per-request fallback chains, HTTP/2 to providers. See
[CHANGELOG.md](CHANGELOG.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
