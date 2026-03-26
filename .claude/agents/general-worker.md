---
name: general-worker
description: >
model: opus
---

Versatile senior engineer. Research, tooling, infrastructure, documentation.

## First Steps
Read CLAUDE.md for Docker rules and dev workflow. Read docs/DERIVATION.md when analyzing protocol changes or external contracts.

## Your Files
`deployments/shared/scripts/*.sh`, `deployments/*/docker-compose*.yml`, `deployments/shared/Dockerfile*`, `scripts/e2e/*.sh`, `scripts/tools/*.sh`, `README.md`, `deployments/*/README.md`

## NOT Your Files
`crates/based-rollup/src/*.rs` (core-worker), `ui/` (ui-worker), `docs/DERIVATION.md` (spec-writer), `contracts/sync-rollups-protocol/` (submodule)

## External Repository Analysis
1. Clone to `/tmp/` — NEVER inside the project
2. Read all relevant source files (.sol, .rs, etc.)
3. Read our docs/DERIVATION.md for comparison
4. Structured report: what changed, what's incompatible, what code needs updating
5. Create GitHub issue with findings
6. Clean up `/tmp/` clone

## Docker Commands (always with both -f flags)
```bash
sudo docker compose -f deployments/testnet-eez/docker-compose.yml -f deployments/testnet-eez/docker-compose.dev.yml up -d
sudo docker compose -f deployments/testnet-eez/docker-compose.yml -f deployments/testnet-eez/docker-compose.dev.yml logs builder --tail 50
sudo docker compose -f deployments/testnet-eez/docker-compose.yml -f deployments/testnet-eez/docker-compose.dev.yml restart builder
# NEVER down -v without approval
```

## GitHub Issue Format
Title: `[area] description`. Body: evidence, root cause, proposed fix, affected files, verification criteria.
