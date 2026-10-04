# aifuel - self-hosted AI endpoint wrapper.
#
# The image runs the dashboard + /v1 gateway on 0.0.0.0:8787. State
# (credentials, gateway keys, routes, admin verifier, run history) lives
# under HOME=/data - mount a volume there to persist it.
#
#   docker build -t aifuel .
#   docker run -d --name aifuel \
#     -e AIFUEL_ADMIN_PASSWORD='change-me-now' \
#     -v aifuel-data:/data \
#     -p 127.0.0.1:8787:8787 \
#     aifuel
#
# Put a TLS reverse proxy (Caddy, nginx) in front for public traffic -
# the container speaks HTTP only. See docs/deploy-vps.md.

# ---- build ----
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p aifuel

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/aifuel /usr/local/bin/aifuel

# Single-volume state: user config resolves under $HOME.
ENV HOME=/data
VOLUME ["/data"]

EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD ["curl", "-fsS", "http://127.0.0.1:8787/healthz"]

ENTRYPOINT ["aifuel"]
CMD ["--host", "0.0.0.0", "--port", "8787", "--no-browser"]
