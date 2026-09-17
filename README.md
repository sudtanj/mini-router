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
| Binary, stripped | **2.2 MB** (1.3 MB with `--no-default-features`, no TLS) |
| Idle RSS | **4.8 MB** |
| RSS during 48 concurrent translated streams | **4.8 MB** — unchanged |
| Dependencies | 95 crates |

Memory is flat under load because nothing is buffered. A same-dialect response
is forwarded frame by frame without being read; a translated one goes through
an incremental state machine, still frame by frame. A twenty-minute generation
costs the same as a one-line one.

## Quick start

Everything is environment variables. There is no configuration file at all —
nothing to write, mount, template or keep in sync with the image.

```yaml
# docker-compose.yml
services:
  mini-router:
    image: mini-router
    ports: ["8080:8080"]
    environment:
      # A key on its own is enough: mini-router knows these providers.
      OPENAI_API_KEY: sk-...
      ANTHROPIC_API_KEY: sk-ant-...

      # One name over both, first one wins, failures fall through.
      MINI_ROUTER_POOL_FAST: openai:gpt-4o-mini,anthropic:claude-haiku-4-5

      MINI_ROUTER_REQUIRE_AUTH: "true"
      MINI_ROUTER_API_KEYS: sk-your-own-key
```

```sh
cp .env.example .env     # put your provider keys in it
docker compose -f docker-compose.hub.yml up -d
```

Two compose files ship with the repo, both reading the same `.env`:

| File | What it does |
|---|---|
| `docker-compose.hub.yml` | Pulls [`sudtanj/mini-router`](https://hub.docker.com/r/sudtanj/mini-router) — multi-arch, so it runs on an Orange Pi Zero 3 unchanged. Nothing to build. |
| `docker-compose.yml` | Builds from this checkout via `deploy/Dockerfile`. |

Or without Docker:

```sh
cargo build --release
OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-... \
MINI_ROUTER_POOL_FAST=openai:gpt-4o-mini,anthropic:claude-haiku-4-5 \
  ./target/release/mini-router
```

Now every client works, whichever SDK it was written against:

```python
from openai import OpenAI
client = OpenAI(base_url="http://pi.local:8080/v1", api_key="sk-your-own-key")
client.chat.completions.create(model="fast", messages=[{"role": "user", "content": "hi"}])

from anthropic import Anthropic
client = Anthropic(base_url="http://pi.local:8080", api_key="sk-your-own-key")
client.messages.create(model="fast", max_tokens=256,
                       messages=[{"role": "user", "content": "hi"}])
```

Both calls can land on the *same* provider. `fast` resolves to whichever member
is healthy, and the answer is reshaped to match whichever SDK asked.

```sh
curl http://localhost:8080/v1/chat/completions \
  -H 'authorization: Bearer sk-your-own-key' \
  -H 'content-type: application/json' \
  -d '{"model": "fast", "stream": true,
       "messages": [{"role": "user", "content": "why is the sky blue?"}]}'
```

### Check before you start

`--check` prints the whole resolved setup — where each provider came from, and
what is missing:

```
$ mini-router --check
configuration ok: 2 provider(s), 1 pool(s), strategy priority, spillover any-error
  provider  anthropic      anthropic  [auto] https://api.anthropic.com/v1
  provider  openai         openai     [auto] https://api.openai.com/v1
  pool      fast           openai:gpt-4o-mini  ->  anthropic:claude-haiku-4-5
  alias     gpt-3.5-turbo  ->  fast
  endpoints /v1/chat/completions  /v1/messages  /v1/models  /healthz  /readyz  /metrics
  auth      OPEN -- anyone who can reach the port can spend your credits
```

A misspelled variable is an error, not a silent default:

```
$ MINI_ROUTER_STRATEGIE=priority mini-router --check
mini-router: configuration error: unrecognised setting(s): MINI_ROUTER_STRATEGIE.
Run `mini-router --help` for the list
```

## Configuration

Every setting is an environment variable. `mini-router --help` lists all of
them, and `--check` shows what they resolved to.

Two rules make that safe to lean on:

- **A variable mini-router does not recognise is a startup error**, naming the
  variable. A typo costs a failed start, not a week of wondering why a setting
  had no effect.
- **A bad value names the variable, the value and what was expected.**

### Providers

A key on its own registers a provider mini-router already knows, with the right
URL and the right dialect:

| Variable | Provider | Dialect |
|---|---|---|
| `OPENAI_API_KEY` | api.openai.com | openai |
| `ANTHROPIC_API_KEY` | api.anthropic.com | anthropic |
| `GROQ_API_KEY` | api.groq.com | openai |
| `OPENROUTER_API_KEY` | openrouter.ai | openai |
| `DEEPSEEK_API_KEY` | api.deepseek.com | openai |
| `MISTRAL_API_KEY` | api.mistral.ai | openai |
| `TOGETHER_API_KEY` | api.together.xyz | openai |
| `XAI_API_KEY` | api.x.ai | openai |
| `GEMINI_API_KEY` | generativelanguage.googleapis.com | openai |
| `CEREBRAS_API_KEY` | api.cerebras.ai | openai |

Those URLs are defaults, not constraints — override any of them with
`..._URL` below. Autodetection only runs when nothing else is configured, so a
stray `OPENAI_API_KEY` belonging to another tool in the same container cannot
quietly add a provider; `MINI_ROUTER_AUTODETECT=on|off` forces the question.

Anything else — a self-hosted server, a gateway, a provider not on that list —
is spelled out. `<NAME>` is uppercase, and underscores in it become dashes:

| Variable | |
|---|---|
| `MINI_ROUTER_PROVIDER_<NAME>_URL` | `https://host/v1` |
| `MINI_ROUTER_PROVIDER_<NAME>_PROTOCOL` | `openai` or `anthropic` |
| `MINI_ROUTER_PROVIDER_<NAME>_API_KEY` | the key itself |
| `MINI_ROUTER_PROVIDER_<NAME>_API_KEY_ENV` | name of the variable holding it |
| `MINI_ROUTER_PROVIDER_<NAME>_MAX_CONCURRENCY` | in-flight requests |
| `MINI_ROUTER_PROVIDER_<NAME>_WEIGHT` | for the weighted strategy |
| `MINI_ROUTER_PROVIDER_<NAME>_MODELS` | `a,b,c` — default is to discover |
| `MINI_ROUTER_PROVIDER_<NAME>_FALLBACK_ONLY` | `true`/`false` |
| `MINI_ROUTER_PROVIDER_<NAME>_HEADERS` | `k=v,k=v` |
| `MINI_ROUTER_PROVIDER_ORDER` | priority order; default is alphabetical |

```yaml
MINI_ROUTER_PROVIDER_LOCAL_URL: http://ollama:11434/v1
MINI_ROUTER_PROVIDER_LOCAL_MAX_CONCURRENCY: "1"
```

### Pools and routing

| Variable | |
|---|---|
| `MINI_ROUTER_POOL_<NAME>` | `provider:model` per line (or comma-separated), in priority order |
| `MINI_ROUTER_POOL_<NAME>_STRATEGY` | overrides the global strategy |
| `MINI_ROUTER_POOL_<NAME>_WEIGHTS` | one per member |
| `MINI_ROUTER_POOL_<NAME>_DESCRIPTION` | shown in the catalogue |
| `MINI_ROUTER_STRATEGY` | `priority`, `round-robin`, `least-conn`, `weighted`, `p2c-latency` |
| `MINI_ROUTER_SPILLOVER` | `any-error` or `status-list` |
| `MINI_ROUTER_RETRY_ON_STATUS` | `429,500,503` — `status-list` mode only |
| `MINI_ROUTER_MAX_ATTEMPTS` | `0` = try every candidate |
| `MINI_ROUTER_ALIASES` | `gpt-3.5-turbo=fast,gpt-4o-mini=fast` |

Only the first colon separates, so a model id may contain more:
`MINI_ROUTER_POOL_TINY=local:qwen2.5:0.5b` is one member.

### Server

| Variable | Default | |
|---|---|---|
| `MINI_ROUTER_LISTEN` | `0.0.0.0:8080` | |
| `MINI_ROUTER_WORKER_THREADS` | `2` | `0` = one per core |
| `MINI_ROUTER_REQUIRE_AUTH` | `false` | |
| `MINI_ROUTER_API_KEYS` | — | keys your clients present |
| `MINI_ROUTER_API_KEY_ENVS` | — | variables holding those keys |
| `MINI_ROUTER_ADMIN` | `true` | `false` removes `/admin/upstreams` |
| `MINI_ROUTER_METRICS` | `true` | `false` removes `/metrics` |
| `MINI_ROUTER_LOG` | `info` | |
| `MINI_ROUTER_MAX_BODY_BYTES` | 8 MiB | |
| `MINI_ROUTER_MAX_TRANSLATE_BYTES` | 8 MiB | non-streamed translated responses |
| `MINI_ROUTER_HEADER_TIMEOUT_SECS` | `120` | not a limit on generation time |
| `MINI_ROUTER_QUEUE_TIMEOUT_SECS` | `60` | |
| `MINI_ROUTER_POOL_IDLE_TIMEOUT_SECS` | `90` | |

Health and translation: `MINI_ROUTER_HEALTH_INTERVAL_SECS`,
`MINI_ROUTER_HEALTH_TIMEOUT_SECS`, `MINI_ROUTER_HEALTH_PATH`,
`MINI_ROUTER_FAILURE_THRESHOLD`, `MINI_ROUTER_SUCCESS_THRESHOLD`,
`MINI_ROUTER_COOLDOWN_SECS`, `MINI_ROUTER_MAX_COOLDOWN_SECS`,
`MINI_ROUTER_DEFAULT_MAX_TOKENS`, `MINI_ROUTER_ANTHROPIC_VERSION`.

`mini-router --help` lists all of them.


## Pools

A pool is one client-facing name over several provider models, tried in the
order you wrote them:

```yaml
MINI_ROUTER_POOL_SMART: |
  anthropic:claude-sonnet-4-5
  openai:gpt-4o
MINI_ROUTER_POOL_SMART_DESCRIPTION: The good models, for when it matters
```

One member per line, top to bottom in priority order. A real YAML list
(`- anthropic:...`) is not available here: a compose `environment:` value has
to become a process environment variable, and those are strings, so compose
rejects a list. YAML's `|` keeps it one string while still giving you a line
per member. Commas work too — `a:model,b:model` — if you prefer them.

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

```yaml
MINI_ROUTER_POOL_SPREAD: |
  openai:gpt-4o-mini
  groq:llama-3.3-70b-versatile
MINI_ROUTER_POOL_SPREAD_STRATEGY: round-robin
MINI_ROUTER_POOL_SPREAD_WEIGHTS: "1,3"
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

The default is `MINI_ROUTER_SPILLOVER=any-error`: a candidate is used up by
anything that is not a 2xx, plus every transport failure.

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

```yaml
MINI_ROUTER_SPILLOVER: status-list
MINI_ROUTER_RETRY_ON_STATUS: 429,500,502,503,529
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

```yaml
MINI_ROUTER_REQUIRE_AUTH: "true"
MINI_ROUTER_API_KEYS: sk-choose-something-long     # comma-separated for several
```

mini-router warns on startup when auth is off, because an open port here is an
open line to your provider bills.

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

Install as a service. The unit reads its whole configuration from
`/etc/mini-router/env`, so there is no config file to manage:

```sh
sudo install -m755 mini-router /usr/local/bin/
sudo install -m644 deploy/mini-router.service /etc/systemd/system/
sudo install -m600 -D /dev/null /etc/mini-router/env
sudo $EDITOR /etc/mini-router/env       # OPENAI_API_KEY=..., MINI_ROUTER_POOL_FAST=...
sudo systemctl enable --now mini-router
```

It runs as a transient unprivileged user in a hardened sandbox with
`MemoryMax=64M` — ten times what it has ever needed.

Or with Docker, which is the same thing in fewer steps:

```sh
docker compose up -d
```

## What it does not have

No web UI, no dashboard, no admin console, no static assets, no JavaScript.
The two observability endpoints are:

- `GET /metrics` — Prometheus text
- `GET /admin/upstreams` — JSON

Both are switchable off with `MINI_ROUTER_METRICS=false` and
`MINI_ROUTER_ADMIN=false`, which removes the routes outright (they return 404,
not 401). `/healthz` and `/readyz` always stay, because a container runtime
needs something to probe.

For what it is worth, compiling both out entirely saves **23 KB of 2.54 MB** —
under 1%. Turning them off is worth doing to shrink what is listening, not to
shrink the binary.

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

- **There is no configuration file.** One source of truth, one format, and a
  container image that needs nothing mounted into it. Dropping the TOML parser
  also took 233 KB and six crates out of the build.
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
cargo test                                    # 159 tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test --no-default-features              # the no-TLS build
```

### Measuring the footprint yourself

`scripts/perf.py` starts a mock Anthropic provider, points a real mini-router
at it, and fires concurrent OpenAI-shaped streaming requests — so every byte
crosses the translator. Stdlib only, nothing to install:

```sh
cargo build --release
python3 scripts/perf.py --binary target/release/mini-router
```

It fails the run if peak RSS exceeds `--max-rss-mb` or if RSS grows more than
`--max-growth-pct` between rounds, which is the check that catches a response
body being buffered instead of streamed.

On the board itself, or over ssh to it, the same command gives you the real
numbers for your hardware.

### ARM in CI

Two jobs cover the Orange Pi Zero 3, and they do different things on purpose:

- **`arm64-footprint`** runs on GitHub's arm64 hosted runners — real hardware,
  no emulation — and measures inside a container capped at **2 GB and 4 CPUs**
  to match the board. Budgets are enforced, and the numbers land in the job
  summary.
- **`arm64-qemu`** cross-compiles the test binaries and executes them as real
  aarch64 instructions under `qemu-user`. Correctness only, and it runs
  anywhere, so ARM coverage survives even without ARM runners.

The split is deliberate: under emulation QEMU's translation buffers show up in
the same RSS, which measured **15.3 MB idle against 4.1 MB native**. Emulated
runs prove the code works on the architecture; they cannot measure it.

Minimum supported Rust version is **1.85**, checked in CI against the committed
`Cargo.lock`.

The integration tests run real HTTP against mock providers of both dialects on
ephemeral ports, and cover the whole four-way matrix, streaming in both
directions, tool calls across dialects, spillover on every error class,
`Retry-After` parking, per-provider credentials, pools, the catalogue, and a
router configured purely from environment variables. `tests/support/mod.rs` has
the harness.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Status

Early but real: routing, pools, spillover, translation and streaming are
covered by tests and work. Not yet here — cost-aware routing, response caching,
config hot-reload, per-request fallback chains, HTTP/2 to providers. See
[CHANGELOG.md](CHANGELOG.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
