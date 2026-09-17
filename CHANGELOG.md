# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Planned

- Cost-aware routing: per-member price, prefer the cheapest healthy member.
- Token accounting from streamed `usage` frames.
- Response caching for identical requests.
- Config hot-reload on `SIGHUP`.
- Per-request fallback chains (`model: ["a", "b"]`).
- Optional HTTP/2 to providers.

## [0.1.0] - 2026-09-17

First release.

### Added

**Two front doors, every provider behind them**

- OpenAI-compatible ingress at `POST /v1/chat/completions` and
  Anthropic-compatible ingress at `POST /v1/messages`, on the same port.
- Full translation between the two dialects in both directions, covering system
  prompts, multi-turn conversations, tool definitions, tool calls and results,
  images (base64 and URL), sampling parameters, stop sequences, token usage and
  stop/finish reasons.
- Incremental translation of server-sent event streams, including reassembly of
  tool-call argument fragments, and correct termination even when a provider
  hangs up mid-generation.
- Per-provider `protocol`, defaulting to `openai` because most providers are
  OpenAI-compatible.
- `/v1/models` served in whichever dialect the client asked in, disambiguated by
  the `anthropic-version` header, with explicit `/openai/...` and
  `/anthropic/...` prefixes available.
- Errors reshaped into the client's dialect, so both SDKs can parse a failure.

**Pools and spillover**

- `[pool.name]`: one client-facing model name over several provider models,
  tried in declaration order, spanning providers and dialects.
- `spillover = "any-error"` by default: any non-2xx and every transport failure
  moves the request to the next member, not just rate limits and 5xx.
- The last provider's own error — status and message — is returned when every
  candidate is exhausted, translated into the client's dialect.
- `Retry-After` is honoured, parking a rate-limited provider for exactly as long
  as it asked, up to `health.max_cooldown_secs`.
- Five ordering strategies: `priority` (default), `round-robin`, `least-conn`,
  `weighted` and `p2c-latency`, overridable per pool.
- Model aliases, which may point at a pool.
- `fallback_only` providers, held back until nothing else serves the model.

**Configuration from the environment**

- Every setting is an environment variable, so a `docker compose` file with an
  `environment:` block is a complete deployment and no config file is needed.
- Zero-config providers: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GROQ_API_KEY`,
  `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `MISTRAL_API_KEY`,
  `TOGETHER_API_KEY`, `XAI_API_KEY`, `GEMINI_API_KEY` and `CEREBRAS_API_KEY`
  each register that provider with the right URL and dialect. Autodetection
  only runs when nothing else is configured, so a key belonging to another tool
  in the same container cannot quietly add a provider.
- `MINI_ROUTER_PROVIDER_<NAME>_*` defines anything else, or overrides a
  well-known provider's URL.
- `MINI_ROUTER_POOL_<NAME>=provider:model,provider:model` defines a pool in one
  variable; `_STRATEGY`, `_WEIGHTS` and `_DESCRIPTION` refine it.
- There is no configuration file at all: nothing to write, mount, template or
  keep in sync with the image. Dropping the TOML parser took 233 KB and six
  crates out of the build.
- An unrecognised or malformed variable is a startup error naming the variable
  and what was expected, never a silent default.
- `--check` prints the resolved setup, including where each provider came from
  and whether its key resolved.
- `docker-compose.yml`, `.env.example` and a `.dockerignore` ship with the repo,
  and are themselves checked by a test that rejects any variable mini-router
  would not accept -- so the examples cannot drift from the code.

**Operations**

- Client API keys accepted as `Authorization: Bearer` or `x-api-key`, compared
  in constant time; per-provider credentials that are never taken from the
  client and are sent in the header that provider expects.
- Active health probing that doubles as model discovery, per-provider circuit
  breaker and cooldown.
- Per-provider `max_concurrency` with a bounded queue and a `429` on timeout.
- Prometheus metrics at `/metrics`, plus `/healthz`, `/readyz` and a JSON view
  of providers and pools at `/admin/upstreams`.
- Extra per-provider request headers, for providers that require them.
- `MINI_ROUTER_ADMIN=false` and `MINI_ROUTER_METRICS=false` remove those routes
  outright (404, not 401). `/healthz` and `/readyz` always remain. There is no
  web UI, dashboard or static asset anywhere in mini-router; compiling both
  endpoints out entirely would save 23 KB of a 2.54 MB binary.
- Hardened systemd unit and a Dockerfile in `deploy/`.
- Optional `tls` feature, on by default; turning it off drops about a megabyte
  for an all-local-provider deployment.

### Performance

Measured on x86_64 against a mock provider streaming SSE through the
translator: a 2.2 MB stripped binary (1.3 MB without TLS), 4.8 MB resident at
idle, and 4.8 MB resident during 48 concurrent translated streams.

[Unreleased]: https://github.com/sudtanj/mini-router/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sudtanj/mini-router/releases/tag/v0.1.0
