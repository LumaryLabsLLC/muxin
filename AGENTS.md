# Plancia — product and repository contract

The canonical repository is `domenicomassafra/plancia`. The global agent contract is already in your context (every agent receives it from its own native file), and project issues also apply. The former local checkout name (`muxin`) does not establish ownership. This public repository contains the `web/` UI. The FastAPI backend and FleetProbe live in private `domenicomassafra/controlcenter:dashboard/`. The Muxin donor was retired in #60; its history and Apache-2.0 attributions remain.

## Mappa

- `web/` — la UI React/Vite servita con API same-origin; ci scrive chi cambia una schermata; aprilo dopo `PRODUCT.md`, `DESIGN.md` e `web/PROVENANCE.md`.
- `docs/` — dominio, tracker delle issue, etichette di triage e ricerche; ci scrive chi chiude una issue; `docs/agents/issue-tracker.md` dice come si legge una issue.
- `evidence/` — le prove raccolte per un ticket, una cartella per issue; ci scrive chi deve dimostrare qualcosa.
- `scripts/donors/` — gli script dei donor; si leggono, il codice Rust ritirato non si ripristina.
- `DESIGN.md`, `PRODUCT.md`, `SECURITY.md`, `RELEASING.md` — design, prodotto, sicurezza e rilascio: si leggono prima di una modifica grande.

## Architecture boundaries

- `web/` is the React/Vite UI, with traceable provenance from `domenicomassafra/controlcenter:dashboard/web/`. Read `PRODUCT.md`, `DESIGN.md`, and `web/PROVENANCE.md` before making changes. `web/src/lib/api.ts` uses the existing *same-origin* API with authenticated sessions, CSRF cookies, WebSockets, and governed receipts. Never add browser tokens or credentials to localStorage or Vite environment variables.
- Do not restore the retired Muxin donor; preserve its history and licenses.
- ControlCenter `dashboard/` owns FastAPI, authentication, FleetProbe, history, writer APIs and owner adapters. The source was published here in #919 and returned to ControlCenter on 9 November 2026. ControlCenter also owns Fleet/OmniRoute policy, host inventory, and `configs/plancia/` with private inventory and host commands. Databases, credentials, vaults, sessions, and Notion data must never enter public Git.
- `web/` and ControlCenter `dashboard/` are the two pinned sources for Plancia's immutable release; the server delivers the authenticated bundle from that assembled release. Follow `DESIGN.md` for release preparation, authenticated same-origin checks and rollback commands.

## Concurrent change ownership

- #27 historically established the initial `AGENTS.md`, `PRODUCT.md`, `DESIGN.md`, `web/README.md`, `web/PROVENANCE.md`, `web/src/lib/api.ts` contract and UI import. Later changes belong to their current issue author.
- #6 historically owned the shared shell, navigation, global CSS (`web/src/App.tsx`, `web/src/main.tsx`, `web/src/index.css`), and common components. The approved palette is white, black, and gray.
- #12 established the Notion database-first UI in `web/src/views/WorkspaceView.tsx` and local Notion modules; #13 established Obsidian in `web/src/views/WorkspaceObsidianView.tsx` and local modules.
- #18 established PWA, manifest, and mobile support in coordination with #6 on shared shell changes.
- #20 documents the historical frontend integration and releases; #919 documents the backend and FleetProbe migration. Coordinate concurrent edits to `web/src/lib/types.ts`, `package.json`, and `web/src/lib/api.ts` with their active writers.

## Verification and delivery

Run agent work, `npm ci`, builds, tests, and Playwright on Mac Studio, in an issue-specific worktree. Use `python3 ~/.local/share/controlcenter/resource-guard/workload_launcher.py worker -- <command>` for builds and tests. MiniPC (`ssh minipc`) hosts installed services and runtimes and is used for Linux/systemd verification. Run Python tests under ControlCenter `dashboard/`; under `web/`, run `npm ci`, `npm run build`, and `npm test`, all through the launcher. Also run `git diff --check`, inspect privacy/provenance and the diff. Do not restore retired donor Rust sources.

The global agent contract is already in your context (every agent receives it from its own native file); it owns review, merge, installation, verification and issue closure. Use the current issue for its scope and acceptance criteria. Do not commit unapproved device images, secrets, accounts, personal logs, Notion/Obsidian data, or profiles. Preserve donor provenance: `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, `upstream = LumaryLabsLLC/muxin` (Apache-2.0).

## Agent skills

### Issue tracker

GitHub Issues in this repo's `origin` remote, driven with the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage labels, each named exactly as its role. See `docs/agents/triage-labels.md`.

### Domain docs

single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
