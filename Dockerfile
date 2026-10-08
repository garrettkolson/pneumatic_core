# syntax=docker/dockerfile:1
# Pneumatic node images (Phase 8 deployment infra).
#
# One image, three binaries: `pneumatic_committer` (dedicated committer role),
# `node-server` (composite runtime hosting every role the node's stake
# qualifies for), and `pneumatic_data_service` (the required per-host
# side-car). Compose selects via `command:`.
#
# Build notes:
# * rust:1.87 pins the workspace toolchain (wasmi 1.1.0 deliberately excludes
#   `wat` to stay compatible with 1.87 — do not "modernize" the base image
#   alongside the wasmparser pin without re-auditing the contract scanner).
# * gcc ships in rust:slim and covers the rustpq C FFI (ML-DSA/ML-KEM).
# * halo2_proofs dominates the release build time (~minutes); one build
#   serves both binaries since they share pneumatic_core.

FROM rust:1.87-slim-bookworm AS build
WORKDIR /src
# Whole-workspace copy first: path-dependency manifests require the member
# trees to exist before any cargo invocation. (Dependency-layer caching can be
# added with a vendored manifest stage if build latency ever matters.)
COPY . .
RUN cargo build --release -p pneumatic_committer --bin pneumatic_committer \
                      -p pneumatic_node_server --bin node-server \
                      -p pneumatic_data_service --bin pneumatic_data_service

FROM debian:bookworm-slim
# curl only for the HEALTHCHECK probe; no shells/tools beyond coreutils.
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl iproute2 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 -m pneumatic
COPY --from=build /src/target/release/pneumatic_committer /usr/local/bin/pneumatic_committer
COPY --from=build /src/target/release/node-server /usr/local/bin/node-server
# The data service ships in the image because the multi-host rehearsal (and the
# Phase 2 runbook's "one data-service sidecar per host") runs it as a container
# sharing the node's network namespace. iproute2 rides along so `tc netem` loss
# injection and socket counts are possible inside a host container.
COPY --from=build /src/target/release/pneumatic_data_service /usr/local/bin/pneumatic_data_service
# /pneumatic holds config.json + the node_identity.json keystore + log files:
# it must be WRITABLE (the keystore is created on first boot), so compose
# mounts a per-service volume here. /env holds the read-only environment
# specs loaded by Config::build().
RUN mkdir -p /pneumatic /env && chown pneumatic:pneumatic /pneumatic
WORKDIR /pneumatic
USER pneumatic
# Health/metrics binds all interfaces inside the container (the healthcheck
# probes localhost; compose publishes to 127.0.0.1 only by default).
ENV PNEUMATIC_HEALTH_ADDR=0.0.0.0:9500
EXPOSE 9500 4242/udp
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:9500/health || exit 1
CMD ["pneumatic_committer"]
