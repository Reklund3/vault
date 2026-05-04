# vault

A single local Rust binary that injects relevant project context into every Claude Code prompt before it reaches the Anthropic API — at zero Claude token cost.

## The Problem

Working across many projects means critical context (API contracts, design decisions, library conventions) lives elsewhere. Without tooling it either gets manually pasted in — burning tokens — or it's missing entirely.

## How It Works

`vault` registers as a Claude Code pre-send hook. Every time you send a prompt:

1. Gemma (running locally) reads the prompt and extracts structured retrieval signals — project names, type names, topics — or returns `skip: true` for prompts that need no context
2. SQLite runs a hybrid query: FTS5 BM25 keyword match + cosine similarity against stored embeddings
3. Top chunks are selected within a 10k token budget and prepended as a `<{domain}-context>` block
4. Claude receives the decorated prompt — Gemma, SQLite, and the binary are invisible to it

The hook has a 3-second timeout and silently passes through on failure — it never makes Claude Code feel broken.

## Usage

```bash
# Register the hook once globally
# Add to ~/.claude/settings.json:
# { "hooks": { "PreToolUse": [{ "command": "vault hook" }] } }

# Index a repo before starting a cross-service session
vault index sync ~/repos/build-service
vault index sync ~/repos/auth-lib

# Inspect what's indexed
vault list
vault list --project build-service

# Diagnose retrieval quality
vault diagnose "what does BuildRequest need for auth?"
vault diagnose "what does BuildRequest need for auth?" --alpha 0.75 --budget 5000

# Force re-index ignoring content hash
vault reindex --project build-service

# Remove a project
vault index remove --project build-service
```

Sync is always explicit — you choose when to index and from what branch. This prevents WIP branch state from polluting the vault. Always sync from main/trunk.

## What Gets Indexed

Stable artifacts only (v1):

- Proto / OpenAPI contracts
- Design docs and plans (whole file)
- CLAUDE.md and convention docs
- Exported Go/Rust symbols with doc comments

Nothing is written to the repos being indexed.

## Storage

```
~/.vault/
├── vault.db      # SQLite: projects, chunks, FTS5 index, embeddings, retrieval log
└── vault.toml    # Domain config, context tags, classification cache, tuning knobs
```

## Configuration

`vault.toml` maps projects to domains. Context tags operate at the domain level — all projects in a domain share one tag, signaling the *kind* of knowledge Claude is receiving.

```toml
[defaults]
token_budget = 10000
alpha        = 0.6      # BM25/cosine weight
timeout_ms   = 3000

[domains.software]
context_tag = "software-context"
projects    = ["build-service", "auth-lib", "vault"]

[domains.finance]
context_tag = "finance-context"
projects    = ["bookkeeping"]
```

The global `~/.claude/CLAUDE.md` should include a `## Vault Context` section explaining the context tags to Claude. When you add a new domain to `vault.toml`, update that file too — it's a two-file change.

## Stack

- **Rust** — single binary, no per-project installation
- **SQLite** via rusqlite (bundled) — FTS5 BM25 + sqlite-vec cosine similarity
- **Gemma 4** via MLX — local routing and file classification, zero API cost
- **nomic-embed-text** (768-dim) — embeddings at index time

## Build

```bash
cargo build --release
```

Requires Gemma 4 running via `mlx_lm.server` on `localhost:8080` for hook routing and index-time classification. Embeddings use fastembed-rs (Rust-native, no external server).
