# symbiotic-archeology-v1: Container image for the Source Archeology pipeline.
#
# The runner binary is bind-mounted at /usr/local/bin/symbiotic-archeology-runner
# by the daemon's archeology_mounts() function — it is NOT baked into this image.
# Mirrors the docker/agent-runner/Dockerfile pattern (T116 §13b ratification).
#
# Build:
#   docker build -t symbiotic-archeology-v1 services/symbiotic-archeology-runner/
#
# Override at runtime via SYMBIOTIC_ARCHEOLOGY_VM_IMAGE env var.

FROM debian:bookworm-slim

# Runtime dependencies for the archeology pipeline:
# - git: source-tree inspection (excavate/date stages walk repo history)
# - ca-certificates: TLS for the LLM gateway
# - libsqlite3-0, libssl3: agents-crate transitive runtime deps
# - nodejs + npm + markdownlint-cli: verify-stage lint backend
RUN rm -rf /var/lib/apt/lists/* \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
        libsqlite3-0 \
        libssl3 \
        nodejs \
        npm \
    && npm install -g markdownlint-cli \
    && rm -rf /var/lib/apt/lists/*

# Workspace layout:
# - /workspace/repo          → bind-mounted source clone (read-only)
# - /workspace/input         → archeology-input.json injected by the daemon
# - /workspace/output        → per-stage JSON outputs extracted by the daemon
# - /workspace/checkpoints   → bind-mounted checkpoint root (writable)
RUN mkdir -p /workspace/repo /workspace/input /workspace/output /workspace/checkpoints

WORKDIR /workspace

# The runner binary is bind-mounted at this path by the daemon.
# If not present at startup, the container will exit with a clear error.
ENTRYPOINT ["/usr/local/bin/symbiotic-archeology-runner"]
