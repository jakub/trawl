# Trawl

Self-hosted log collection and search for homelabs and small installations.
Single-node by design. No clustering, sharding, or multi-tenancy.
Rust workspace, DuckDB query engine, Parquet storage, Leptos browser UI.

## Working here

The Flow charter owns the engineering workflow. Keep this file about Trawl.
Module documentation and nearby tests explain current implementation contracts;
`docs/adr/` records decisions and their amendments. Read what the task touches.
Do not duplicate command inventories or implementation histories in this file.

`bin/dev` runs the user's persistent interactive stack. Agent tests use
disposable infrastructure. A saved CLI profile may address the live homelab;
select the target explicitly instead of treating a profile as authorization.

## Find the implementation

- DSL and shared query semantics: `crates/trawl-core/`
- DuckDB execution: `crates/trawl-engine/`
- Ingest, catalog, recovery, and API: `crates/trawl-server/`
- CLI and TUI: `crates/trawl-cli/`
- Browser proxy and SPA: `crates/trawl-web/`, `crates/trawl-web-ui/`
- Shared auth, UI, and admin: `crates/fleet-auth/`, `crates/fleet-ui/`, `crates/fleet-admin/`

Fleet crates also serve Coastwatch through sibling path dependencies. Check
both consumers when changing a shared contract. Cargo manifests own versions
and dependencies; `lefthook.yml` and `.github/workflows/` own automated checks.

## Task skills

Load the skill relevant to the task, not the whole collection:

- [trawl-dev](.agents/skills/trawl-dev/SKILL.md): local development and focused verification
- [trawl-experiment](.agents/skills/trawl-experiment/SKILL.md): disposable full-app experiments
- [trawl-tui](.agents/skills/trawl-tui/SKILL.md): terminal UI control and rendering checks
- [trawl-deploy](.agents/skills/trawl-deploy/SKILL.md): build, install, or upgrade a named deployment
- [trawl-ops](.agents/skills/trawl-ops/SKILL.md): queries, health, schema, and recovery tasks

Skills live in `.agents/skills/`; `.claude/skills/` links to the same files.
`CLAUDE.md` links to this file. Keep one copy of each instruction.

## Contexts

- [context.md](context.md): shared event, storage, catalog, and query terms
- [Web UI context](crates/trawl-web-ui/context.md): browser state and navigation
- [Fleet UI context](crates/fleet-ui/context.md): shared UI vocabulary
- [ADRs](docs/adr/): decision rationale; qualify external ADRs by repository
