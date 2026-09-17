# Contributing to mini-router

Thanks for taking a look. mini-router is small on purpose, and the easiest way
to help is to keep it that way.

## Ground rules

**The target is a 1 GB single-board computer.** A change that costs memory,
binary size or dependencies has to buy something worth it. If you are adding a
dependency, say in the PR why the alternative — writing the twenty lines
yourself — is worse.

**Response bodies are never buffered.** Anything that collects an upstream
response into memory before forwarding it will be sent back. The one exception
is health probes, which are bounded.

**Behaviour changes come with a test.** `tests/routing.rs` runs real HTTP
against mock upstreams; `tests/support/mod.rs` has the harness, and most new
cases are a dozen lines.

## Before you open a PR

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --no-default-features   # the no-TLS build must keep working
```

CI runs all of the above plus an `aarch64-unknown-linux-musl` cross-build. It
will not pass if `cargo fmt --check` fails.

## Good first contributions

- Compatibility fixes for a specific upstream (llama.cpp, vLLM, LocalAI,
  llamafile, text-generation-webui). If your server does something surprising,
  a mock reproducing it is a genuinely useful PR on its own.
- Token accounting from non-streamed responses.
- Config hot-reload on `SIGHUP`.
- Per-request model fallback chains (`model: ["a", "b"]`).
- Real numbers from real hardware for the footprint table in the README.

## Reporting a bug

Include the output of `GET /admin/upstreams`, the relevant part of your config
with secrets removed, and what the upstream is (llama.cpp? Ollama? which
version?). Run with `MINI_ROUTER_LOG=debug` if you can — routing decisions and
probe failures are logged at that level.

## Security

Please report anything with a security impact privately through GitHub's
"Report a vulnerability" rather than in a public issue.

## License

By contributing you agree that your work is dual-licensed under MIT and
Apache-2.0, matching the project.
