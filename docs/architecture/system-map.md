# System Map


> **Historical note:** This document describes the current implemented / migration-era architecture view. For the approved future target topology, see `docs/design/system-map.md`.

## Overview

This document stitches together the core architecture and marks what is **high value** vs **deferred**. It is the single navigation hub for the system.

## Component Map

```mermaid
flowchart TB
    subgraph Inputs["Input Channels"]
        App[Symbiotic App]
        Share[OS Share Sheet]
        CLI[CLI Intake]
        Bookmarks[Bookmarks Source<br/>X API or Browser]
    end

    subgraph ControlPlane["Control + Messaging"]
        Rooms[Matrix Rooms<br/>#control #status #goal-* #task-* #intake #credentials]
        Transport[Matrix Transport Adapter]
        Router[Message Router]
        Cmd[Command Handler]
        GoalGate[Goal Gateway]
        IntakeH[Intake Handler]
        CredGate[Credential Gateway]
        Push[Push Gateway + Ack]
    end

    subgraph DataPlane["Data Plane"]
        Intake[Unified Intake]
        Sensitive[Sensitivity Filter]
        Queue[(Queue System)]
        Fetch[Fetch Workers<br/>HTTP/X API/Browser fallback]
        Archive[(Archive)]
        Review[(Review Store)]
        NeuralGraph[(Neural Graph)]
        Vector[(Vector Search)]
        Recall[Recall Gateway]
        Orchestrator[Agent Orchestration]
        Agents[Agent Runtime]
    end

    subgraph Security
        Broker[Gatekeeper]
        Vault[Credential Sandbox]
        Sessions[Session Handles]
        VM[VM Sandbox]
    end

    subgraph Control
        Goals[Goals Layer]
        Skills[Skills System]
        Metrics[Metrics Layer]
    end

    App --> Rooms
    Share --> App
    Rooms <--> Transport
    Transport --> Router
    Router --> Cmd
    Router --> GoalGate
    Router --> IntakeH
    Router --> CredGate
    Router --> Push

    CLI --> Intake
    Bookmarks --> Intake
    IntakeH --> Intake

    Intake --> Sensitive
    Sensitive -->|Safe| Queue
    Sensitive -->|Secure| Vault
    Queue --> Fetch
    Fetch --> Archive
    Archive --> Review
    Archive --> NeuralGraph
    Archive --> Vector
    NeuralGraph --> Vector
    Vector --> Recall
    Recall <--> Orchestrator

    Cmd --> Orchestrator
    GoalGate --> Goals
    Goals --> Orchestrator
    Goals --> Queue

    Orchestrator --> Queue
    Queue --> Agents
    Agents --> Orchestrator

    Orchestrator --> Skills
    Orchestrator --> Metrics

    Orchestrator --> Broker
    Agents --> Broker
    Orchestrator --> Transport
    Agents --> Transport
    Orchestrator --> CredGate
    CredGate --> Vault
    Vault --> Sessions
    Sessions --> Orchestrator
    Agents --> VM
    VM --> Agents
```

**Note:** The system map covers both **data plane** (ingest -> queue -> Archive/Neural Graph -> Recall Gateway) and **control plane** (Matrix messaging -> routing -> orchestration/goal execution), including Gatekeeper and Vault enforcement paths.

## Phasing & Value

| Area | Value Add | Phase |
| --- | --- | --- |
| Unified intake + sensitivity filter + queue | Reliable ingestion, secret-safe routing, dedupe, retries | MVP |
| Archive + Brief workflow | Base knowledge system | MVP |
| Recall Gateway | Privacy enforcement + token control | MVP |
| Neural Graph | Long‑term personal context | Beta |
| Vector search + hierarchical retrieval | Accurate recall at scale | Beta |
| Goals layer | Multi‑domain orchestration | Beta |
| Metrics layer | Evidence‑based self‑improvement | Beta |
| Skills system | Reusable, consistent workflows | Beta |
| VM sandboxing | Strong isolation for risky code | Post‑Beta |
| Zero‑touch onboarding | Productization | Post‑Beta |

## Core Flows

### Ingestion

```mermaid
sequenceDiagram
    participant U as User
    participant I as Intake
    participant Q as Queue
    participant A as Archive
    participant R as Archive Review

    U->>I: Submit URL(s)
    I->>Q: Enqueue ingest.fetch
    Q->>A: Store entry
    A->>R: Queue review
    R-->>U: Brief + tags
```

### Memory Access

```mermaid
sequenceDiagram
    participant A as Agent
    participant C as Recall Gateway
    participant M as Neural Graph
    participant V as Vector Index

    A->>C: Context Request
    C->>M: Policy‑filtered memory fetch
    C->>V: Semantic retrieval (or lexical fallback)
    C-->>A: Context Pack (token‑bounded)
```

### Orchestration

```mermaid
sequenceDiagram
    participant O as Orchestrator
    participant Q as Queue
    participant G as Goals
    participant A as Agents

    O->>G: Start goal workflow
    G->>Q: Enqueue jobs
    Q->>A: Execute tasks
    A-->>O: Results
```

## Related Docs

- `docs/architecture/ingestion-pipeline.md`
- `docs/architecture/knowledge-storage.md`
- `docs/design/vault-as-truth.md`
- `docs/architecture/context-delivery.md`
- `docs/architecture/vector-search.md`
- `docs/design/context-graphs.md`
- `docs/design/temporal-modeling.md`
- `docs/architecture/queue-system.md`
- `docs/architecture/goals-layer.md`
- `docs/architecture/agent-orchestration.md`
- `docs/architecture/symbiotic-daemon.md`
- `docs/architecture/metrics-layer.md`
- `docs/architecture/skills-system.md`
- `docs/architecture/vm-sandboxing.md`
- `docs/architecture/onboarding.md`
- `docs/architecture/setup-experience.md`
- `docs/architecture/session-handles.md`
- `docs/architecture/redaction-policy.md`
- `docs/design/memory-extraction.md`
- `docs/architecture/queue-persistence.md`
- `docs/design/embedding-chunking.md`
- `docs/architecture/device-trust-bootstrap.md`
- `docs/architecture/repo-structure.md`
- `docs/architecture/runtime-workflows.md`
- `docs/architecture/implementation-map.md`
- `docs/architecture/mvp-readiness.md`
