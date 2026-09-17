# Build:  docker build -f deploy/Dockerfile -t mini-router .
# Run:    docker run --rm -p 8080:8080 \
#           -v $PWD/mini-router.toml:/etc/mini-router/mini-router.toml:ro \
#           mini-router
#
# For an amd64 host building for a board:
#   docker buildx build --platform linux/arm64 -f deploy/Dockerfile -t mini-router .

FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src

# Cache the dependency build across source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release 2>/dev/null || true \
 && rm -rf src

COPY . .
# Touch the sources so the cached dummy build does not win.
RUN touch src/main.rs src/lib.rs && cargo build --release

FROM alpine:3
RUN apk add --no-cache ca-certificates \
 && adduser -D -H -u 10001 minirouter
COPY --from=builder /src/target/release/mini-router /usr/local/bin/mini-router

USER 10001
EXPOSE 8080
ENV MINI_ROUTER_CONFIG=/etc/mini-router/mini-router.toml

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/mini-router"]
