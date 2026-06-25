# skillberry-praxis-filters

> ⚠️ **Work in Progress** — This repository is actively evolving. Features, APIs, and configuration may change at any time. Please monitor it frequently for updates.

External [Praxis](https://github.com/praxis-proxy/praxis) filters for the Skillberry ecosystem.

## Filters

| Filter | Description |
|--------|-------------|
| `context_extractor` | Extracts request headers into filter metadata for downstream filters |
| `skill_resolver` | Resolves skill UUIDs from environment variables or via skillberry-store API lookup |
| `vmcp_manager` | Creates Virtual MCP (VMCP) servers and fetches available MCP tools |
| `mcp_tools_enricher` | Injects MCP tools into OpenAI-compatible chat completion request bodies |

## Usage

Add this crate as a dependency of the Praxis server:

```toml
[dependencies]
skillberry-praxis-filters = { git = "https://github.com/skillberry-ai/skillberry-praxis-filters.git", branch = "phase-0" }
```

The Praxis build system auto-discovers external filter crates via the `[package.metadata.praxis-filters]` marker and registers them at compile time.

### Building Praxis

```console
cargo build --package praxis
```

### Rebuilding after filter updates

Cargo caches git dependencies. When this repo changes, force a re-fetch before rebuilding:

```console
cargo update && cargo build --package praxis
```

## Filter Chain

These filters are designed to work together in sequence:

1. **`context_extractor`** — Reads configured request headers, validates them, stores values in `filter_metadata` (e.g. `env_id`)
2. **`skill_resolver`** — Reads `SKILL_UUID` or `SKILL_NAME` env vars, resolves to a UUID, stores in `filter_metadata["skill_uuid"]`
3. **`vmcp_manager`** — Creates a VMCP server (using skill UUID + env ID from metadata), fetches MCP tools via SSE, stores in `filter_metadata["mcp_tools"]`
4. **`mcp_tools_enricher`** — Reads tools from metadata, injects them into the request body's `tools` array

## Configuration

Full Praxis configuration (`praxis.yaml`):

```yaml
listeners:
  - name: skillberry_proxy
    address: 0.0.0.0:8080
    filter_chains:
      - skillberry_chain

filter_chains:
  - name: skillberry_chain
    filters:
      - filter: context_extractor
        headers:
          - name: x-skillberry-env-id
            metadata_key: env_id
            default: "default-env"
            required: true
            pattern: "^[a-zA-Z0-9_-]+$"
            max_length: 64
          - name: x-skillberry-user-id
            metadata_key: user_id
            default: "anonymous"
            required: false
            pattern: "^[a-zA-Z0-9_-]+$"
            max_length: 64
          - name: x-skillberry-session-id
            metadata_key: session_id
            required: false
            pattern: "^[a-zA-Z0-9_-]+$"
            max_length: 128

      - filter: skill_resolver
        store_base_url: "http://localhost:8000"
        skill_uuid_env: "SKILL_UUID"
        skill_name_env: "SKILL_NAME"
        timeout_ms: 5000

      - filter: vmcp_manager
        store_base_url: "http://localhost:8000"
        vmcp_name_template: "vmcp-{env_id}"
        timeout_ms: 10000
        always_create: true
        cleanup_on_error: false

      - filter: mcp_tools_enricher
        timeout_ms: 5000
        tool_choice: auto
        max_body_bytes: 10485760
        on_invalid: continue

      - filter: router
        routes:
          - path_prefix: "/"
            cluster: llm_backend

      - filter: load_balancer
        clusters:
          - name: llm_backend
            endpoints:
              - "localhost:4000"
            connection_timeout_ms: 5000
            read_timeout_ms: 60000
            write_timeout_ms: 60000

runtime:
  threads: 4
  max_connections: 10000
```
