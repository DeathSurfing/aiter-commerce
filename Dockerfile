# syntax=docker/dockerfile:1
#
# Multi-stage build for the AITER COMMERCE server.
# Stage 1 compiles ONLY the aiter-server binary (the package ships three:
# aiter-server, aiter-cli, mcp). Stage 2 is a minimal non-root runtime.
#
# Build:  docker build -t aiter-commerce .
# Run:    docker run --rm -p 8080:8080 aiter-commerce
# Probe:  curl -sf localhost:8080/  (or GET /agentic/health)

# ---- Stage 1: cargo builder -----------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /build

# Workspace manifest + lockfile first so dependency layers cache independently
# of source edits.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# Compile only the single binary we ship.
RUN cargo build --release -p aiter-server --bin aiter-server

# ---- Stage 2: minimal runtime ---------------------------------------------
# distroless/cc-debian12: no shell, no package manager, ships a nonroot user.
FROM gcr.io/distroless/cc-debian12

WORKDIR /app

# Run as the image's built-in non-root user (uid 65532).
USER nonroot

COPY --from=builder /build/target/release/aiter-server /usr/local/bin/aiter-server

ENV RUST_LOG=aiter_server=info,tower_http=info

EXPOSE 8080

# No HEALTHCHECK instruction: distroless has no shell to run curl/wget.
# Kubernetes/docker probes should hit GET / or GET /agentic/health on :8080.
ENTRYPOINT ["aiter-server"]
CMD ["run"]
