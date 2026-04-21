# symbiotic-agent-v1: Minimal runtime image for swarm CI/reviewer agents.
#
# The agent-runner binary is bind-mounted at /usr/local/bin/symbiotic-agent-runner
# by the daemon's swarm_mounts() function — it is NOT baked into this image.
# The daemon socket is also bind-mounted for JSON-RPC communication.
#
# Build:
#   docker build -t symbiotic-agent-v1 submodules/runtime/docker/agent-runner/
#
# This image is referenced by SWARM_AGENT_VM_IMAGE in swarm_server.rs.
# Override at runtime via SYMBIOTIC_SWARM_AGENT_IMAGE env var.

FROM debian:bookworm-slim

# Runtime dependencies for the agent-runner binary and git operations.
# - libsqlite3-0: SQLite support (used by trust store, memory)
# - libssl3: TLS for HTTPS git clones and LLM API calls
# - git: clone swarm repos, produce diffs for review
# - ca-certificates: TLS certificate verification
# - curl, jq: used by pre-receive hooks and health checks
RUN rm -rf /var/lib/apt/lists/* \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        libsqlite3-0 \
        libssl3 \
        git \
        ca-certificates \
        curl \
        jq \
    && rm -rf /var/lib/apt/lists/*

# Create workspace and runtime directories.
RUN mkdir -p /workspace /var/run/symbiotic

WORKDIR /workspace

# The runner binary is bind-mounted at this path by the daemon.
# If not present at startup, the container will exit with a clear error.
ENTRYPOINT ["/usr/local/bin/symbiotic-agent-runner"]
