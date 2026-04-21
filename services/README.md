# Services

Runtime services live here.

- `symbiotic-daemon/` -- Nucleus: orchestration, Matrix routing, queue control, workflow dispatch
- `credential-gateway/` -- Credential request handling, vault interface, session handle issuance
- Install lifecycle orchestration is delegated to `symbiotic-control-plane`; runtime emits `install.*` events from control-plane responses.

**Note:** Gatekeeper (access broker) capability enforcement is crate-integrated in `crates/symbiotic-trust/` for MVP. There is no standalone `access-broker/` service directory. The VPS deployment architecture (`docs/architecture/vps-deployment.md`) describes the containerized Gatekeeper as a future deployment target.
