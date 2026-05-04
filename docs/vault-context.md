# Vault — Session Context

This document summarises the full design conversation for the `vault` project.
Use it alongside `vault-plan.md` to bring Claude Code up to speed.

---

## What This Project Is

A single local Rust binary (`vault`) that injects relevant project context into every
Claude Code prompt before it reaches the Anthropic API — at zero Claude token cost.
The problem it solves: working across 30+ projects means critical context (API contracts,
design decisions, conventions) lives elsewhere. Without tooling it gets manually pasted
in, burning tokens with no relevance filtering.

---

## Key Design Decisions Made (and Why)

### Pre-send hook, not MCP tool
Claude Code MCP tools require Claude to decide when to call them — burning tokens on
routing decisions. The pre-send hook intercepts every message locally, routes via Gemma,
and injects context before Claude ever sees the prompt. Zero Claude token cost for
retrieval decisions. This is the central design goal.

### Single Rust binary
One binary serves three modes: `vault hook` (Claude Code pre-send hook), CLI subcommands
(index, list, diagnose, etc.), and optionally `vault serve` (MCP server, future). No
per-project installation. No files written to indexed repos.

### Gemma 4 via MLX for routing
Gemma runs locally via `mlx_lm.server` (OpenAI-compatible API at localhost:8080).
It extracts structured query signals from raw prompt text — project names, type names,
topics — that SQLite can actually match against. Without this layer, raw prompt text
fed to FTS5 matches noise words instead of technical signals.

### Gemma for embeddings — BLOCKED
`mlx_lm.server` does NOT support `/v1/embeddings` (returns 404). The embeddings
endpoint is not implemented. Two options were identified:

**Option A — mlx-embeddings (separate server)**
```bash
pip install mlx-embeddings
python -m mlx_embeddings.server --model nomic-ai/nomic-embed-text-v1.5 --port 8081
```
Back to two processes. nomic-embed-text = 768 dims.

**Option B — fastembed-rs (Rust-native, recommended)**
Pure Rust embedding library, runs inside the vault binary, no external server.
nomic-embed-text supported natively. CPU only (not Apple Silicon GPU) but index-time
embedding for occasional syncs is fine at this corpus size.
```toml
fastembed = "3"   # in Cargo.toml
```
Locks dimensions back to 768. Schema is unblocked. Zero ops overhead.

**Decision pending** — Option B (fastembed-rs) was recommended but not yet confirmed.
This must be decided before Step 1 (store/schema.rs) because vector dimensions are
locked at schema creation and cannot change without a migration.

### SQLite + FTS5 + sqlite-vec
Single embedded database at `~/.vault/vault.db`. No external services.
- FTS5 for BM25 keyword retrieval (porter stemming)
- sqlite-vec for cosine similarity (vector dimensions TBD — see above)
- rusqlite with `bundled` feature (compiles SQLite from source, supports extension loading)

### Manual explicit sync only
No git hooks, no automated triggers. You decide when to index a repo and from what
branch. This prevents WIP branch state and teammate in-progress contracts from
polluting the vault. Typical workflow:
```bash
vault index sync ~/repos/build-service   # from main/trunk
vault index sync ~/repos/auth-lib
# then start Claude Code session
```

### Domain-level context tags
Context tags operate at domain level, not project level:
- `<software-context>` — all engineering projects
- `<finance-context>` — bookkeeping, tax
- `<personal-context>` — everything else
Projects are assigned to domains in `~/.vault/vault.toml`. Tags tell Claude what
*kind* of knowledge it's receiving. Projects are identified by headers inside the block.

### Hook registered globally
```json
// ~/.claude/settings.json
{
  "hooks": {
    "PreToolUse": [{ "command": "vault hook" }]
  }
}
```
One registration covers all projects and sessions. Confirm exact key against Claude
Code docs — wrong key = silent failure.

### No session state in vault.db
The hook binary is read-only at runtime. Session state (current work, next steps)
lives in per-project CLAUDE.md files, not in the vault. This keeps the write path
clean and scoped to explicit index sync operations only.

### v1 scope — stable artifacts only
Concern raised: code signatures change frequently and re-indexing is a duplication
maintenance burden. Decision: v1 indexes stable artifacts only:
- Proto / OpenAPI contracts
- Design docs and plans (whole file)
- CLAUDE.md and conventions
Go/Rust source symbol indexing deferred — add later when retrieval quality data
justifies the sync overhead.

---

## What Was Explicitly Ruled Out (v1)

- Git hook automated sync
- Per-project .vault.yaml config files
- Session state in vault.db
- MCP server subcommand (out of scope, tracked for later)
- Multi-user sharing
- CI pipeline auto-indexing
- Scala deterministic chunking (whole file for v1)
- Helm parser (deferred)

---

## MemPalace Comparison

MemPalace (github.com/MemPalace/mempalace) was reviewed. It solves a different problem:
conversation memory (what did we decide, what was said). Vault solves artifact retrieval
(what does BuildRequest look like, what are the auth conventions). They are complementary,
not competing. MemPalace + Vault together cover both concerns cleanly.

---

## Runtime Data Layout

```
~/.vault/
├── vault.db       — SQLite store (projects, documents, chunks, vectors, FTS5, retrieval_log)
└── vault.toml     — domains, context tags, MLX config, classification cache, defaults
```

Nothing is written to indexed repos.

---

## CLAUDE.md Setup Required

Two files need to be configured before the hook is useful:

**~/.claude/CLAUDE.md** (global, always injected) — add:
```markdown
## Vault Context

When a <software-context>, <finance-context>, or <personal-context> block appears at the
start of a message, it contains relevant artifacts retrieved from my local project vault.
Treat it as authoritative reference material for the current task — prefer it over general
knowledge when there is a conflict. The block is grouped by project with labeled chunks
(contract, plan, convention, meta). If no context block is present, none was relevant.
```

**Note:** Adding a new domain to vault.toml requires updating this file too — two-file change.

**<project>/CLAUDE.md** (per-project) — current work state only:
```markdown
# <Project Name>

## Non-negotiable rules
- [minimal rules]

## Current work
[task state]

## Next steps
[next session pickup]
```

---

## Outstanding Before Writing Code

### Blocking (must resolve before Step 1)
1. **Embedding approach** — fastembed-rs (Option B, recommended) vs mlx-embeddings
   separate server (Option A). Decision locks vector dimensions in schema.
2. **Vector dimensions** — 768 if nomic-embed-text (either option), confirm if
   different model chosen.

### Non-blocking (empirical, validate with vault diagnose)
- Alpha tuning (BM25/cosine weight) — start 0.6/0.4
- Token budget ceiling — start 10k
- Context block ordering — score-descending within project grouping

### Deferred
- Gemma 4 exact MLX model tag — needed for vault.toml router_model
- Helm chunk strategy
- Scala deterministic chunking (v1 = whole file)

---

## Implementation Order

```
Step 0  Confirm embedding approach + vector dims → update schema + vault.toml
Step 1  store/schema.rs     — embedded SQL, migration runner
Step 2  store/writer.rs     — upsert project, document, chunk, vec
Step 3  store/query.rs      — FTS5 + vec queries, score merge, budget trim
Step 4  vault diagnose      — validate retrieval with real data before parsers exist
Step 5  parse/proto.rs      — first parser
Step 6  parse/go_source.rs
Step 7  parse/rust_source.rs
Step 8  embed/*             — embedding generation (depends on Step 0 decision)
Step 9  index/classify.rs   — Gemma content classification
Step 10 retrieve/router.rs  — Gemma query plan extraction
Step 11 retrieve/hybrid.rs  — score merge
Step 12 retrieve/budget.rs  — token budget selection
Step 13 hook/mod.rs         — full pipeline wired
Step 14 first-run UX        — domain + classification prompts on new project sync
```
