# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

`vault` is a single Rust binary that injects relevant project context into every Claude Code prompt before it reaches the Anthropic API — at zero Claude token cost. It indexes stable artifacts (proto contracts, design docs, conventions, CLAUDE.md files) into a local SQLite store and decorates prompts via a pre-send hook using Gemma for local routing.

## Build & Run

```bash
cargo build
cargo build --release
cargo test
cargo test <test_name>       # run a single test
cargo run -- <subcommand>    # e.g. cargo run -- diagnose "what does BuildRequest need?"
```

## Architecture

Three execution modes from one binary, dispatched by subcommand in `main.rs`:
- **`vault hook`** — pre-send hook (registered globally in `~/.claude/settings.json`); reads prompt JSON from stdin, returns decorated prompt to stdout
- **`vault index sync <repo>`** — explicit manual indexing; Gemma classifies files, you confirm, chunks written to SQLite
- **`vault diagnose "<prompt>"`** — full retrieval trace for tuning alpha and token budget

### Request Flow (hook mode)

```
prompt → vault hook → Gemma (localhost:8080) extracts query plan
       → SQLite hybrid query (FTS5 BM25 + sqlite-vec cosine)
       → score merge (α=0.6 BM25, 0.4 cosine) + token budget (10k)
       → prepend <{domain-context}> block → Claude Code → Anthropic API
```

Gemma returns `{ skip: true }` for prompts that need no context — immediate passthrough with no SQLite query. 3-second timeout on all Gemma calls; silent passthrough on timeout or unavailability.

### Key modules

| Path | Responsibility |
|------|---------------|
| `src/hook/mod.rs` | stdin→stdout hook protocol, full pipeline entry |
| `src/store/schema.rs` | embedded SQL, migration runner |
| `src/store/writer.rs` | upsert project/document/chunk/vec |
| `src/store/query.rs` | FTS5 + sqlite-vec hybrid retrieval, score merge, budget trim |
| `src/retrieve/router.rs` | Gemma query plan extraction |
| `src/retrieve/hybrid.rs` | BM25 + cosine score merge |
| `src/retrieve/budget.rs` | token-aware chunk selection |
| `src/parse/` | per-language parsers (proto, go, rust, openapi, markdown) |
| `src/index/classify.rs` | Gemma content classification during sync |
| `src/embed/mlx.rs` | nomic-embed-text embeddings via MLX subprocess |

### Runtime data

```
~/.vault/vault.db      # SQLite store — projects, documents, chunks, FTS5, vec, retrieval_log
~/.vault/vault.toml    # domain assignments, context tags, classification cache, tuning defaults
```

Nothing is written to indexed repositories.

## Implementation Order

The store layer must come before retrieval; `vault diagnose` must work before parsers:

```
Step 0  Confirm embedding dims (curl mlx_lm.server /v1/embeddings) — locks chunks_vec FLOAT[N]
Step 1  store/schema.rs
Step 2  store/writer.rs
Step 3  store/query.rs
Step 4  vault diagnose — validate retrieval with manually seeded data before building parsers
Step 5  parse/proto.rs
Step 6  parse/go_source.rs
Step 7  parse/rust_source.rs
Step 8  embed/mlx.rs
Step 9  index/classify.rs
Step 10 retrieve/router.rs
Step 11 retrieve/hybrid.rs
Step 12 retrieve/budget.rs
Step 13 hook/mod.rs
Step 14 first-run UX (domain + classification prompts on new project sync)
```

## Open Decisions (must resolve before writing schema)

- **Embedding dimensions** — `chunks_vec FLOAT[N]` is locked at schema creation. Confirm Gemma 4 output dims via `curl http://localhost:8080/v1/embeddings` before Step 1.
- **Embedding approach** — fastembed-rs (Rust-native, recommended) vs separate mlx-embeddings server. Decision locks dimensions and whether an extra process is needed.

## Chunking

`doc_type` and `language` are orthogonal. Chunk boundaries:

| doc_type | language | Boundary |
|----------|----------|----------|
| contract | proto | per message/service/enum |
| contract | openapi | per path+method, per schema component |
| plan | any | whole file |
| convention | go/rust | per exported symbol + doc comment |
| convention/meta | markdown | per `##` heading block |
| convention | scala | whole file (v1) |

## Scoring & Tuning

```
final_score = α * bm25_normalized + (1 - α) * cos_sim
α = 0.6 (initial), MinChunkScore = 0.15, TokenBudget = 10_000
```

Tune via `vault diagnose "<prompt>" --alpha X --budget Y` after seeding real data. Budget fill is score-descending with `continue` (not `break`) on oversized chunks.

## Global Hook Registration

```json
// ~/.claude/settings.json
{
  "hooks": {
    "PreToolUse": [{ "command": "vault hook" }]
  }
}
```

Confirm exact hook key against Claude Code docs — wrong key = silent failure with no context injection.

## Context Tags

Tags are domain-level (not project-level), configured in `vault.toml`. Adding a new domain requires a two-file change: `vault.toml` + `~/.claude/CLAUDE.md` (so Claude knows how to interpret the new tag).

## v1 Scope Boundaries

Out of scope: MCP server subcommand, multi-user sharing, CI auto-indexing, git hook sync, per-project `.vault.yaml` files, Helm parser, Scala AST chunking, session state in vault.db.
