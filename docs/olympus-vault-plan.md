# Vault — Design Plan

## Overview

A single local Rust binary (`vault`) providing dynamic context injection for Claude Code
sessions across any project — microservices, bookkeeping, personal tooling, or any domain
where context lives across multiple repositories or document sets. The system indexes
technical artifacts into a SQLite store and decorates every prompt with relevant context
before it reaches Claude — at zero Claude token cost.

The core problem: working across multiple projects means critical context (API contracts,
design decisions, domain conventions, library patterns) lives in other places. Without
tooling, that context is either missing from sessions or manually pasted in at high token
cost with no relevance filtering.

The binary serves three roles from a single executable:
- **Pre-send hook** — intercepts every Claude Code message before it reaches the API;
  Gemma routes locally, SQLite retrieves, context is injected at zero Claude token cost
- **CLI** — humans run it from any project root to init, index, inspect, and diagnose
- **MCP server** (optional, future) — expose retrieval as an explicit tool if on-demand
  access is also wanted alongside the hook

The pre-send hook is the primary interface. All routing and retrieval happens locally via
Gemma + SQLite before Claude ever sees the prompt. Claude's token budget is never consumed
by retrieval decisions — that is the central design goal.

---

## Architecture

### Runtime (hook mode — every send)

```
Your prompt (typed in Claude Code)
        │
        ▼
  vault hook (Rust binary, pre-send hook — registered globally in ~/.claude/settings.json)
  ├── reads raw prompt from stdin (JSON)
  │
  ├── calls Gemma 4 via Ollama HTTP  ← local, free, zero Claude tokens
  │     → extracts query plan: { projects, type_names, topics, doc_types, languages }
  │     → or returns { skip: true } for prompts that need no context
  │
  ├── if skip: passthrough immediately, no retrieval
  │
  ├── hybrid retrieval against SQLite
  │     ├── FTS5 BM25 keyword match on label + content
  │     └── sqlite-vec cosine similarity (nomic-embed-text 768-dim)
  │
  ├── score merge + token-budget selection (10k ceiling)
  ├── assembles <{context_tag}> block grouped by project
  └── prepends context to prompt → stdout → Claude Code → Anthropic API

  Claude only ever sees the decorated prompt.
  Gemma, SQLite, and the Rust binary are invisible to it.

  The context tag is domain-driven — all projects in the same domain share one tag (see Global Config).
```

### How the three components relate

```
Gemma         → translates natural language prompt → structured query signals
                (also used offline: classifies unmapped files at index time)
SQLite        → stores chunks; matches structured signals via FTS5 + vec
Rust binary   → orchestrates both; enforces token budget; speaks hook + MCP protocol
```

Gemma and SQLite never talk to each other. The Rust binary owns both connections and
translates between them.

### Why Gemma instead of FTS5 directly against the raw prompt

Raw prompt text like "what does the build service need for auth before a build can be
requested" cannot be fed directly to FTS5. FTS5 would match common words ("what", "does",
"need") rather than the technical signals ("BuildRequest", "auth", "build-service").

Gemma extracts structured terms the database can actually match against:
```
{
  "projects":   ["build-service"],
  "type_names": ["BuildRequest"],
  "topics":     ["auth"],
  "doc_types":  ["contract", "convention"]
}
```

The `skip: true` path is the token-saving escape hatch — short prompts (typo fixes, syntax
questions) return immediately without touching SQLite at all.

### Index write path (CLI mode)

Sync is always explicit and manual. No git hooks, no automated triggers.
This avoids indexing WIP branch state or another team member's in-progress contracts.

```
vault index sync <repo-path>
        │
  ├── Gemma classifies unmapped files (doc_type + language)
  ├── user confirms or overrides interactively
  ├── parses files into chunks by definition boundary
  ├── generates embeddings via MLX (nomic-embed-text, offline)
  └── upserts into SQLite store (content_hash skips unchanged files)
```

You choose when to sync — typically before starting work on something that touches
multiple services, from a known stable state (main/trunk, not WIP branches).

```bash
# Example: starting auth refactor across services
vault index sync ~/repos/build-service
vault index sync ~/repos/mcp-server
vault index sync ~/repos/auth-lib
# then start Claude Code session
```

---

## Storage Schema

### Tables

```sql
CREATE TABLE projects (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  repo_path  TEXT,
  created_at INTEGER NOT NULL
);

CREATE TABLE documents (
  id           INTEGER PRIMARY KEY,
  project_id   INTEGER NOT NULL REFERENCES projects(id),
  doc_type     TEXT NOT NULL CHECK(doc_type IN ('contract','plan','convention','meta')),
  source_path  TEXT NOT NULL,
  title        TEXT NOT NULL,
  content_hash TEXT NOT NULL,   -- sha256, skip re-index if unchanged
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  UNIQUE(project_id, source_path)
);

CREATE TABLE chunks (
  id          INTEGER PRIMARY KEY,
  document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  project_id  INTEGER NOT NULL,           -- denormalized for filter performance
  doc_type    TEXT NOT NULL,              -- denormalized
  language    TEXT NOT NULL CHECK(language IN
                ('go','rust','scala','proto','openapi','helm','markdown','unknown')),
  label       TEXT NOT NULL,              -- "message BuildRequest [build-service]"
  content     TEXT NOT NULL,
  token_est   INTEGER NOT NULL,           -- tiktoken cl100k_base accurate count
  chunk_index INTEGER NOT NULL,
  created_at  INTEGER NOT NULL
);

-- FTS5 with porter stemming — chunks only, never raw documents
CREATE VIRTUAL TABLE chunks_fts USING fts5(
  label, content,
  content='chunks',
  content_rowid='id',
  tokenize='porter unicode61'
);

-- sqlite-vec: nomic-embed-text = 768 dims, locked
CREATE VIRTUAL TABLE chunks_vec USING vec0(
  chunk_id  INTEGER PRIMARY KEY,
  embedding FLOAT[768]
);

-- Retrieval audit — used for tuning alpha and token budget
CREATE TABLE retrieval_log (
  id               INTEGER PRIMARY KEY,
  prompt_hash      TEXT NOT NULL,
  query_plan       TEXT NOT NULL,    -- JSON from Gemma
  chunks_returned  INTEGER NOT NULL,
  tokens_injected  INTEGER NOT NULL,
  created_at       INTEGER NOT NULL
);
```

### FTS5 Sync Triggers

```sql
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, label, content)
  VALUES (new.id, new.label, new.content);
END;

CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, label, content)
  VALUES ('delete', old.id, old.label, old.content);
  INSERT INTO chunks_fts(rowid, label, content)
  VALUES (new.id, new.label, new.content);
END;

CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, label, content)
  VALUES ('delete', old.id, old.label, old.content);
END;
```

---

## Chunking Strategy

`doc_type` and `language` are orthogonal. Chunk boundaries determined by the combination:

| doc_type    | language           | Chunk Boundary                              |
|-------------|--------------------|--------------------------------------------|
| contract    | proto              | Per message / service / enum definition     |
| contract    | openapi            | Per path+method, per schema component       |
| plan        | any                | Whole file (single chunk per document)      |
| convention  | go                 | Per exported symbol + doc comment           |
| convention  | rust               | Per pub fn/struct/enum/trait + doc comment  |
| convention  | scala              | Whole file (v1 — see tracking items)        |
| convention  | markdown           | Per ## heading block                        |
| meta        | markdown           | Per ## heading block                        |

### Parser Boundaries by Language

**Go** — exported symbols only:
- `func [A-Z]`, `type [A-Z]`, `const [A-Z]`, `var [A-Z]`, const/var blocks
- Preceding `//` doc comment block included in chunk content
- Interface definitions chunked as a whole unit (gRPC contracts most useful complete)

**Rust** — public surface only:
- `pub fn`, `pub struct`, `pub enum`, `pub trait`, `pub type`, `pub const`
- `pub mod` shallow only (declaration, not inline body)
- Preceding `///` doc comment block included in chunk content

**Proto** — top-level definitions at column 0:
- `message`, `service`, `enum`
- State machine parser, no AST library required

**OpenAPI** — YAML/JSON:
- Disambiguate from Helm by presence of `openapi:` or `swagger:` root key
- Chunk per path+method combination, per `components/schemas` entry

**Markdown** (plans, conventions, meta):
- Plans: whole file
- Conventions/meta: per `##` heading block (not `#` — top heading is document title)

---

## Indexing

No per-project config files. No git hooks. No automated triggers.

Sync is always explicit — you decide when to index a repo and from what state.
This prevents WIP branch state or a teammate's in-progress contracts from
polluting your vault.

### Classification fallback chain

```
explicit CLI flag (--type, --language) → Gemma content classification → prompt user
```

Gemma inspects file content and proposes `doc_type` and `language`. You confirm or
override interactively on first index. Confirmed classifications are cached in vault.toml
so subsequent syncs of the same repo are non-interactive.

### Staleness handling

`content_hash` (sha256) on each document — files that haven't changed since last sync
are skipped automatically. Re-indexing a previously synced repo is fast.

### When to sync

Before starting work that touches multiple services. Always from a known stable state —
main or trunk, not a feature branch.

```bash
vault index sync ~/repos/build-service
vault index sync ~/repos/mcp-server
vault index sync ~/repos/auth-lib
```

---

## Retrieval Pipeline

### Step 1 — Gemma Query Plan

Model: Gemma 4 (27B MoE or 31B Dense) via Ollama HTTP. Runs locally, zero API cost.

```
System prompt:
  "You are a context router for a personal knowledge vault used across software
   engineering, finance, and general project work.
   Extract retrieval signals from the following prompt.
   Respond with JSON only, no other text.

   Schema:
   {
     projects:   [],   // project or service names mentioned or implied
     type_names: [],   // specific named types: proto messages, Go types, API schemas,
                       // account categories, report names, or any named entity
     topics:     [],   // conceptual topics: auth, events, tax, invoicing, grpc, helm, etc
     doc_types:  [],   // which to search: contract, plan, convention, meta
     languages:  []    // go, rust, proto, openapi, markdown, etc
   }

   If nothing warrants retrieval, return { skip: true }."
```

3 second timeout — silent passthrough on timeout or Gemma unavailability. The hook must
never make Claude Code feel broken.

### Step 2 — Hybrid Query

Both queries filtered by project_id, doc_type, and language from the query plan.
Language filter is applied only when the query plan specifies languages — omitted
when the list is empty to avoid over-filtering.

```sql
-- FTS5: BM25 keyword match
SELECT c.id, c.label, c.content, c.token_est, c.project_id, c.doc_type,
       -rank AS bm25_score
FROM chunks_fts
JOIN chunks c ON c.id = chunks_fts.rowid
WHERE chunks_fts MATCH ?
  AND c.project_id IN (...)           -- from query plan: projects
  AND c.doc_type IN (...)             -- from query plan: doc_types
  AND (? OR c.language IN (...))      -- from query plan: languages (skipped if empty)
ORDER BY rank LIMIT 50;

-- Vec: cosine similarity
SELECT c.id, c.label, c.content, c.token_est, c.project_id, c.doc_type,
       1.0 - vec_distance_cosine(v.embedding, ?) AS cos_sim
FROM chunks_vec v
JOIN chunks c ON c.id = v.chunk_id
WHERE c.project_id IN (...)
  AND c.doc_type IN (...)
  AND (? OR c.language IN (...))
ORDER BY vec_distance_cosine(v.embedding, ?) LIMIT 50;
```

### Step 3 — Score Merge + Budget Selection

```
final_score = α * bm25_normalized + (1 - α) * cos_sim

α = 0.6  (initial — tune via retrieval_log + vault diagnose)
MinChunkScore = 0.15
TokenBudget = 10_000
```

Budget fill: sort by score descending, skip oversized chunks (`continue` not `break`),
stop when budget exhausted.

### Step 4 — Context Assembly

The context tag wraps the injected block. It is determined by the project's domain
assignment in vault.toml (e.g. `<software-context>`, `<finance-context>`, `<personal-context>`).

Chunk labels strip leading `#` characters at index time so markdown heading markers
do not appear inside the context block.

```xml
<{context_tag}>
## build-service
### message BuildRequest [contract/proto]
...chunk content...

### func ParsePrincipal [convention/go]
...chunk content...

## mcp-server
### Auth Considerations [plan/markdown]
...chunk content...
</{context_tag}>

[original prompt]
```

### Hook Behavior

- 3 second timeout — silent passthrough on timeout or Gemma unavailability
- `{ skip: true }` from Gemma — immediate passthrough, no SQLite query
- Empty retrieval results — passthrough, no empty context block injected
- Project in query plan not in vault — silently excluded, no error

---

## Binary Structure

Single binary, modes dispatched by subcommand:

```
vault/                           -- source repository
├── Cargo.toml
└── src/
    ├── main.rs                  -- subcommand dispatch
    ├── hook/
    │   └── mod.rs               -- stdin→stdout hook protocol
    ├── mcp/
    │   └── mod.rs               -- MCP stdio protocol (optional future)
    ├── store/
    │   ├── mod.rs
    │   ├── schema.rs            -- embedded SQL, migration runner
    │   ├── writer.rs            -- upsert documents + chunks
    │   └── query.rs             -- FTS5 + vec retrieval
    ├── parse/
    │   ├── mod.rs               -- Parser trait + registry (ext → parser)
    │   ├── proto.rs
    │   ├── go_source.rs
    │   ├── rust_source.rs
    │   ├── openapi.rs           -- disambiguates from Helm by root key
    │   ├── helm.rs
    │   └── markdown.rs
    ├── embed/
    │   └── mlx.rs               -- nomic-embed-text via MLX subprocess
    ├── retrieve/
    │   ├── router.rs            -- Gemma query plan extraction
    │   ├── hybrid.rs            -- FTS5 + vec merge + scoring
    │   └── budget.rs            -- token-aware chunk selection
    └── index/
        └── classify.rs          -- Gemma content classification

~/.vault/                        -- runtime data (never in source repo)
├── vault.db                     -- SQLite store
└── vault.toml                   -- domains, classification cache, defaults
```


---

## Global Config

Both `vault.toml` and `vault.db` live in `~/.vault/`. The hook is registered once globally
in `~/.claude/settings.json` — no per-project installation needed.

```json
// ~/.claude/settings.json
{
  "hooks": {
    "PreToolUse": [{ "command": "vault hook" }]
  }
}
```

> **Note:** Confirm exact hook key against Claude Code docs before wiring the binary.
> Wrong key = silent failure with no context injection and no error.

`vault.toml` controls domain groupings, context tags, and vault-wide defaults.

The context tag operates at the **domain level**, not the project level. All projects
within a domain share the same tag — the tag signals what kind of knowledge Claude is
receiving, not which specific project it came from. Projects are already identified by
the grouping headers inside the context block.

```toml
# vault.toml

[defaults]
context_tag  = "vault-context"   # fallback if project has no domain assignment
token_budget = 10000
alpha        = 0.6               # BM25/cosine weight — 0.0 = pure semantic, 1.0 = pure keyword
min_score    = 0.15
timeout_ms   = 3000              # hook Gemma timeout before passthrough

[domains.software]
context_tag = "software-context"
projects    = ["olympus", "mcp-server", "vault"]

[domains.finance]
context_tag = "finance-context"
projects    = ["bookkeeping", "tax-notes"]

[domains.personal]
context_tag = "personal-context"
projects    = ["homelab", "research"]

[mlx]
endpoint      = "http://localhost:8080"  # mlx_lm.server
router_model  = "gemma4-27b-moe"        # confirm exact tag from mlx_lm
embed_model   = "gemma4-27b-moe"        # same model for embeddings
embed_dims    = 0                        # MUST be confirmed before schema is finalized
                                         # run: curl http://localhost:8080/v1/embeddings

# Classification cache — written by vault index sync on first run for each file pattern.
# Prevents re-prompting on subsequent syncs of the same repo.
[classifications."~/repos/build-service"]
"**/*.proto"   = { doc_type = "contract", language = "proto" }
"**/docs/*.md" = { doc_type = "plan",     language = "markdown" }
"CLAUDE.md"    = { doc_type = "meta",     language = "markdown" }
```

The `[classifications]` block is written automatically during `vault index sync`.
You never edit it manually — but you can delete an entry to force re-classification
on the next sync.

The assembled context block for a software session:

```xml
<software-context>
## build-service
### message BuildRequest [contract/proto]
...

### func ParsePrincipal [convention/go]
...
</software-context>

[original prompt]
```

Domain assignment happens during `vault index sync` — if a project is new, you are
prompted to assign it to a domain. Adding a new project to an existing domain requires
no new tag decision.

---

## CLI

```bash
# Hook — invoked by Claude Code automatically, not run manually
# Register once in ~/.claude/settings.json (see Global Config)
# vault hook reads prompt JSON from stdin, writes decorated prompt to stdout
vault hook

# Indexing — always explicit, never automated
vault index sync <repo-path>              # Gemma classifies, you confirm; skips unchanged files
vault index add <path> --project <name> --type <doc_type> [--language <lang>]
vault index remove --project <name>       # drop all documents for a project

# Inspection
vault list                                # all projects + chunk/doc counts
vault list --project <name>              # all documents in project with types

# Diagnostics
vault diagnose "<prompt>"                 # full retrieval trace (Gemma plan + SQLite results)
vault diagnose "<prompt>" --budget 5000  # test different token budgets
vault diagnose "<prompt>" --alpha 0.75   # test different BM25/cosine weights

# Maintenance
vault reindex --project <name>           # force full re-index ignoring hash

# MCP server (optional future — same binary, different subcommand)
vault serve                              # expose retrieval as MCP tool over stdio
```

`vault diagnose` output shows:
- Gemma query plan
- Candidate chunks with individual BM25 + cosine scores
- Post-budget selection with token counts
- Final assembled context block

`vault index sync` first-run behavior (new project):
- Prompts for project name if not already in vault.toml
- Prompts for domain assignment (software / finance / personal / new)
- Gemma classifies each file, you confirm or override
- Classifications cached in vault.toml for future syncs
- No files written to the repo being indexed

---

## CLAUDE.md Strategy

Two levels of CLAUDE.md — both matter.

### ~/.claude/CLAUDE.md (global — always injected)

The user-level CLAUDE.md is where the vault context instruction lives. Since the hook
is global and vault context blocks appear in every session, Claude needs to know how to
interpret them at the global level — not in per-project files.

```markdown
## Vault Context

When a <software-context>, <finance-context>, or <personal-context> block appears at the
start of a message, it contains relevant artifacts retrieved from my local project vault.
Treat it as authoritative reference material for the current task — prefer it over general
knowledge when there is a conflict. The block is grouped by project with labeled chunks
(contract, plan, convention, meta). If no context block is present, none was relevant.
```

Keep everything else in this file minimal — it is loaded on every session regardless of
domain or project.

**Important:** adding a new domain to `vault.toml` requires a matching update here so
Claude knows how to interpret the new context tag. It is a two-file change:
`vault.toml` + `~/.claude/CLAUDE.md`.

### <project>/.claude/CLAUDE.md (per-project — injected when in that directory)

Per-project CLAUDE.md is the session handoff mechanism — current work state, next
steps, and any immediate context that doesn't belong in the vault. Domain conventions,
library patterns, and reference docs move into the vault as `doc_type=convention` chunks
and are injected by the hook when relevant.

No session state lives in vault.db. The hook binary is read-only at runtime.

```markdown
# <Project Name>

## Non-negotiable rules
- [minimal rules that always apply regardless of context]

## Current work
[immediate task state — updated at session checkpoints]

## Next steps
[what to pick up next session]
```

---

## Decisions

### Confirmed

| Decision | Choice | Reason |
|----------|--------|--------|
| Language | Rust | Single binary, good parser ergonomics, rusqlite for sqlite-vec support |
| SQLite driver | rusqlite with bundled feature | Compiles SQLite from source, supports sqlite-vec extension loading |
| Embedding model | Gemma 4 via MLX (same model as router) | Single MLX process, one port; simplifies operations for v1 |
| Vector dimensions | TBD — Gemma 4 output dims | Locked at schema creation — confirm before Step 1 (see Implementation Order) |
| Primary interface | Pre-send hook (vault hook) | All routing/retrieval before Claude sees prompt; zero Claude token cost |
| Hook runtime access | Read-only | No session writes; vault.db only written during explicit index sync |
| Context tag | Domain-level in vault.toml | Tag signals knowledge domain (software, finance, personal) not individual project; projects grouped inside the block by header |
| Routing model | Gemma 4 (27B MoE or 31B Dense) via Ollama | Local, free, handles natural language → structured query signals |
| Routing strategy | Every send with skip escape hatch | Gemma decides relevance; short prompts return immediately |
| Hook timeout | 3 seconds | Silent passthrough on timeout — never block the session |
| Context injection | Prepend as `<{context_tag}>` block | Tag driven by domain assignment in vault.toml |
| Token estimation | tiktoken cl100k_base | Accurate counts matter at 10k budget ceiling |
| Token budget | 10k initial | Validate and tune via vault diagnose before hardcoding |
| Chunk unit | Chunks not documents | Retrieval unit is definition-level, not file-level |
| Session state | Per-project markdown files | Keeps vault write path clean; hook binary is read-only at runtime |
| Plan chunks | Whole file | Plans are coherent units, fragmentation loses intent |
| Scala chunks | Whole file for v1 | Deterministic chunking requires AST; defer to v1+ |
| Re-index trigger | Manual explicit sync | Avoids WIP branch state and teammate branch pollution |
| Indexing primary | Explicit CLI sync | No per-project config files; no files written to indexed repos |
| Indexing classification | Gemma content classification | Classifies on first sync; cached in vault.toml for subsequent syncs |
| Cold start / missing project | Silent no-op | Partial vault is normal state; no error warranted |
| Multi-language support | language field on chunks | Orthogonal to doc_type, enables language-scoped retrieval |
| Sharing (v1) | Out of scope | Validate retrieval quality before adding distribution complexity |
| Distribution | Single binary + SQLite file + vault.toml | No server, no service to configure |
| Go interface chunking | Whole interface as one chunk | gRPC service contracts most useful as complete units |
| MCP server | Optional future | Same binary, different subcommand; add if on-demand access needed |

### Remaining / Open

| Decision | Status | Notes |
|----------|--------|-------|
| Gemma 4 MLX model tag | Unconfirmed | Verify exact mlx_lm model name |
| Gemma 4 embedding dimensions | Unconfirmed | Must be confirmed before schema is written — chunks_vec dimension is locked at creation |
| Alpha tuning (BM25/cosine) | Empirical | Start 0.6/0.4, tune via retrieval_log + vault diagnose |
| Token budget ceiling | Empirical | Start 10k, validate with vault diagnose on real prompts |
| Context block ordering | Empirical | Validate natural ordering via vault diagnose before hardcoding |
| Helm chunk strategy | Deferred | Defer to implementation phase |
| Scala deterministic chunking | Deferred to v1+ | Scalameta (AST, JVM dep) vs accepted file-level granularity |

---

## Tracking Items

```
[ ] Gemma 4 MLX model tag — confirm exact mlx_lm model name
[ ] Gemma 4 embedding dimensions — curl /v1/embeddings, verify output dims before Step 1
    curl http://localhost:8080/v1/embeddings -H "Content-Type: application/json" \
    -d '{"model": "<tag>", "input": "test"}' | python3 -m json.tool | grep -c embedding
[ ] Scala deterministic chunking — evaluate Scalameta after Go/Rust parsers validated
[ ] Alpha tuning — use retrieval_log replay once real prompts collected
[ ] Token budget ceiling — validate empirically with vault diagnose
[ ] Sharing / read-only access — binary becomes HTTP service; out of scope v1
[ ] MCP server subcommand — add if on-demand retrieval needed alongside hook
[ ] Keep ~/.claude/CLAUDE.md domain list in sync with vault.toml domains — two-file change when adding a new domain
```

---


## Implementation Order

Bottom-up so retrieval is testable before the full stack is wired:

```
Step 0  confirm embed dims  — curl mlx_lm.server embeddings endpoint, verify Gemma 4 output
                              dimensions, update chunks_vec FLOAT[N] and vault.toml embed_dims
                              before any schema code is written
Step 1  store/schema.rs     — embedded SQL, migration runner, open DB
Step 2  store/writer.rs     — upsert project, document, chunk, vec
Step 3  store/query.rs      — FTS5 + vec queries, score merge, budget trim
Step 4  vault diagnose      — CLI command to test retrieval manually with real data
                              validates schema and scoring before parsers exist
Step 5  parse/proto.rs      — first parser, cleanest boundary detection
Step 6  parse/go_source.rs  — exported symbol + doc comment extraction
Step 7  parse/rust_source.rs
Step 8  embed/mlx.rs        — MLX subprocess call, get embeddings flowing
Step 9  index/classify.rs   — Gemma content classification fallback
Step 10 retrieve/router.rs  — Gemma query plan extraction
Step 11 retrieve/hybrid.rs  — score merge
Step 12 retrieve/budget.rs  — token budget selection
Step 13 hook/mod.rs         — stdin/stdout protocol, full pipeline wired
Step 14 first-run UX        — new project prompts during vault index sync (domain, classification cache)
```

`vault diagnose` at step 4 is intentional — manually seed the DB with a few chunks
and validate retrieval quality against real prompts before building parsers.
Parser correctness and embedding quality problems are much easier to diagnose here
than after the full hook pipeline is running.

---

## Out of Scope (v1)

- Multi-user / team sharing of vault
- MCP server subcommand
- Semantic de-duplication across projects
- Automated CLAUDE.md checkpoint writing
- Cross-vault federation
- CI pipeline auto-indexing
- Per-project .vault.yaml config files
- Git hook automated sync
