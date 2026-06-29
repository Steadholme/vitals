# syntax=docker/dockerfile:1
#
# Multi-stage build for Vitals (host probe + metrics TSDB + dashboard).
#   - builder: rust:1.96-slim (Debian trixie).
#   - runtime: debian:trixie-slim (matching glibc), non-root, ca-certificates.
#
# The image carries BOTH binaries:
#   - vitals-server : the TSDB + dashboard (default CMD; container HEALTHCHECK).
#   - vitals-agent  : the host probe (deploy overrides `command:` and mounts host /proc).
#
# sqlx uses `rustls` and the only FFI is `libc` (statvfs/gethostname), so the binaries link
# just glibc — no OpenSSL in either stage. The HEALTHCHECK uses the built-in
# `vitals-server healthcheck` subcommand, so the image needs no curl.

FROM rust:1.96-slim AS builder
WORKDIR /build

# Cache the dependency graph first: build throwaway stubs against the real manifest so a
# later `cargo build` only recompiles our crate when src/ changes, not the whole tree.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > src/agent.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --bins \
    && rm -rf src

# Now build the real binaries. `static/` is needed because the dashboard embeds app.css via
# include_str! at compile time.
COPY src ./src
COPY static ./static
RUN touch src/main.rs src/agent.rs src/lib.rs \
    && cargo build --release --bins \
    && strip target/release/vitals-server target/release/vitals-agent

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (no shell, no home writes needed).
RUN useradd --system --uid 10001 --user-group --no-create-home vitals
COPY --from=builder /build/target/release/vitals-server /usr/local/bin/vitals-server
COPY --from=builder /build/target/release/vitals-agent /usr/local/bin/vitals-agent

USER vitals
# Default in-container bind; overridable at runtime.
ENV BIND_ADDR=0.0.0.0:8300
EXPOSE 8300

# Dependency-free liveness probe -> GET /healthz on the loopback, exit 0/1. (Applies to the
# server; the agent container should disable the healthcheck since it binds no port.)
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD ["vitals-server", "healthcheck"]

# Default to the server. The agent is run by overriding the command, e.g.
#   command: ["vitals-agent"]
CMD ["vitals-server"]
