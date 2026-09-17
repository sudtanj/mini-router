# Contributing to mini-router

Thanks for taking a look. mini-router is small on purpose, and the easiest way
to help is to keep it that way.

## Ground rules

**Nothing is buffered on the streaming path.** A response body is either
forwarded untouched or run through an incremental translator. Anything that
collects a provider response into memory before passing it on will be sent
back; the bounded exceptions are health probes and non-streamed bodies being
translated, both capped by config.

**The router runs on a small box.** A change that costs memory, binary size or
dependencies has to buy something worth it. If you are adding a dependency, say
in the PR why writing the twenty lines yourself is worse.

**Translation changes need a test in both directions.** A mapping that works
OpenAI→Anthropic and silently drops something on the way back is worse than no
mapping. `src/translate/tests.rs` covers the pure functions;
`tests/routing.rs` covers the whole path over real HTTP.

**Behaviour changes come with a test.** `tests/support/mod.rs` has mock
providers for both dialects, and most new cases are a dozen lines.

## Before you open a PR

```sh
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features     # the no-TLS build must keep working
```

CI runs all of the above plus MSRV (1.85) and cross-builds for `aarch64` and
`armv7`. It will not pass if `cargo fmt --check` fails.

## Good first contributions

- **A provider that behaves differently.** If Groq, Together, DeepSeek,
  OpenRouter, Bedrock or Vertex does something surprising, a mock reproducing it
  in `tests/support/mod.rs` is a genuinely useful PR on its own.
- **Translation gaps.** Anything either API grows that we drop on the floor:
  structured outputs, prompt caching hints, parallel tool-call flags, audio
  content blocks.
- Cost-aware routing (per-member price, prefer the cheapest healthy member).
- Token accounting from streamed `usage` frames.
- Config hot-reload on `SIGHUP`.
- Real numbers from real hardware for the footprint table in the README.

## Reporting a bug

Include the output of `GET /admin/upstreams`, the relevant part of your config
with secrets removed, and which providers are involved. If it is a translation
problem, the request you sent and the response you got back are the two things
that matter most. Run with `MINI_ROUTER_LOG=debug` for routing decisions and
probe failures.

## Security

Please report anything with a security impact privately through GitHub's
"Report a vulnerability" rather than in a public issue. Note that mini-router
holds provider API keys, so anything touching credential handling, the auth
path, or what gets written to logs counts.

## License

By contributing you agree that your work is dual-licensed under MIT and
Apache-2.0, matching the project.
