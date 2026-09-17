# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Planned

- Token accounting from non-streamed responses and stream `usage` frames.
- Config hot-reload on `SIGHUP`.
- Per-request model fallback chains.
- Optional HTTP/2 to upstreams.

## [0.1.0] - 2026-09-17

First release.

### Added

- OpenAI-compatible proxy for `/v1/*`, with explicit handling of `/v1/models`
  and a catch-all that forwards any other endpoint unchanged.
- Model aggregation across upstreams, with automatic discovery from
  `GET {url}/models` refreshed on every health probe.
- Five balancing strategies: `p2c-latency` (default), `least-conn`,
  `round-robin`, `weighted` and `first-available`.
- Per-upstream admission control (`max_concurrency`) with a bounded queue and a
  `429` when the queue times out.
- Failover across upstreams on connection errors, timeouts and configurable
  status codes, with a circuit breaker and cooldown per upstream.
- Active health probing with configurable failure and success thresholds.
- Streaming passthrough: SSE bodies are forwarded frame by frame, and an
  upstream's slot is held until the stream ends or the client disconnects.
- Model aliases, rewritten in the request body before forwarding.
- Client API-key authentication with constant-time comparison; separate
  per-upstream credentials that are never taken from the client.
- `fallback_only` upstreams, held back until no ordinary upstream serves the
  requested model.
- Prometheus metrics at `/metrics`, `/healthz`, `/readyz` and a JSON view of the
  upstream pool at `/admin/upstreams`.
- systemd unit and Dockerfile in `deploy/`.
- Optional `tls` feature (on by default) so a LAN-only deployment can drop about
  a megabyte of binary.

[Unreleased]: https://github.com/sudtanj/mini-router/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sudtanj/mini-router/releases/tag/v0.1.0
