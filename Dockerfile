# No `# syntax=docker/dockerfile:1` directive on purpose: it makes BuildKit
# fetch the external dockerfile frontend image, which a locked-down registry
# mirror (e.g. fnOS's docker.fnnas.com) 401s on. The daemon's built-in
# frontend (Docker 23+) already supports the `RUN --mount=type=cache` used
# below, so nothing here needs the external frontend.
#
# Multi-stage build for the shion gateway.
#   builder : rust toolchain + protoc (feishu's protobuf is compiled at build
#             time), produces the release binary
#   runtime : debian-slim + CA certs (TLS to Telegram / Home Assistant / the
#             LLM API needs a trust store) + libssl3 (the wechat channel's
#             `wechatbot` crate pulls reqwest's native-tls, which dynamically
#             links libssl on Linux — without it the binary won't even load) +
#             tzdata (reminders and the briefing run on local time — set TZ)
#
# Build for the NAS's architecture, NOT your laptop's. On Apple Silicon:
#   docker buildx build --platform linux/amd64 -t ghcr.io/solren7/shion:latest --push .
# See docs/truenas-docker.md.

# ---- builder ----------------------------------------------------------------
FROM rust:trixie AS builder

# protoc for lark-websocket-protobuf's build script. `libprotobuf-dev` is
# required too: it ships the well-known protos (google/protobuf/descriptor.proto)
# under /usr/include that protoc resolves imports against — `protobuf-compiler`
# alone omits them, so the build fails with "descriptor.proto: File not found".
# (pin the rust tag, e.g. rust:1.90-bookworm, for fully reproducible builds.)
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Cache the cargo registry and target dir across builds (BuildKit). The target
# dir is a cache mount, so the binary must be copied out to a real layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && cp target/release/shion /usr/local/bin/shion

# ---- runtime ----------------------------------------------------------------
FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/shion /usr/local/bin/shion

# All durable state (config.toml, .env, shion.db / kanban.db / memory.db, logs)
# lives here — mount a TrueNAS dataset to it so nothing is lost on redeploy.
ENV SHION_HOME=/data
VOLUME ["/data"]

# The gateway is outbound-only (Telegram long-poll, Feishu WS, LLM API, LAN HA)
# — no inbound port to EXPOSE. `shion gateway` runs in the foreground; Docker's
# restart policy replaces launchd. One-off CLI ops bypass the entrypoint, e.g.
#   docker exec shion shion pair approve <code>
ENTRYPOINT ["shion"]
CMD ["gateway"]
