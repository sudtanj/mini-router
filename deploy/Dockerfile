# mini-router
#
#   docker build -f deploy/Dockerfile -t mini-router .
#   docker run --rm -p 8080:8080 \
#     -e OPENAI_API_KEY=sk-... \
#     -e ANTHROPIC_API_KEY=sk-ant-... \
#     -e MINI_ROUTER_POOL_FAST=openai:gpt-4o-mini,anthropic:claude-haiku-4-5 \
#     mini-router
#
# No config file is needed: everything is environment variables. Mount one at
# /etc/mini-router/mini-router.toml if you would rather keep a base config in
# the image; the environment still overrides it.
#
# Building for a board from an amd64 host:
#   docker buildx build --platform linux/arm64 -f deploy/Dockerfile -t mini-router .

FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src

# Build the dependency tree first so source edits do not re-do it.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release 2>/dev/null || true \
 && rm -rf src

COPY . .
# Touch the real sources so the cached dummy build does not win.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM alpine:3
RUN apk add --no-cache ca-certificates \
 && adduser -D -H -u 10001 minirouter
COPY --from=builder /src/target/release/mini-router /usr/local/bin/mini-router

USER 10001
EXPOSE 8080

# Listen on every interface: inside a container, localhost is nobody.
ENV MINI_ROUTER_LISTEN=0.0.0.0:8080

# /healthz is unauthenticated on purpose, so this works with auth turned on.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/mini-router"]
