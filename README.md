# vault

A single local Rust binary that injects relevant project context into every Claude Code prompt before it reaches the Anthropic API — at zero Claude token cost when routing locally via Gemma, or minimal cost (~$0.0002/call) when falling back to Haiku.

## The Problem

Working across many projects means critical context (API contracts, design decisions, library conventions) lives elsewhere. Without tooling it either gets manually pasted in — burning tokens — or it's missing entirely.

## How It Works

`vault` registers as a Claude Code `UserPromptSubmit` hook. Every time you send a prompt:

1. The router (Gemma running locally, or Anthropic Haiku via API as a fallback) reads the prompt and extracts structured retrieval signals — project names, type names, topics — or returns `skip: true` for prompts that need no context
2. SQLite runs a hybrid query: FTS5 BM25 keyword match + cosine similarity against stored embeddings
3. Top chunks are selected within a 10k token budget and emitted on stdout as a `<{domain}-context>` block
4. Claude Code appends that block to your prompt before sending — the router, SQLite, and the binary are invisible to the model

The hook has a 3-second timeout and silently passes through on failure — it never makes Claude Code feel broken. Silent toward Claude Code, not toward you: every call appends a one-line JSON record (outcome, per-stage latency, error detail) to `~/.vault/hook.log`, so an outage is diagnosable after the fact.

`auto` mode (default) tries Gemma first and falls back to Haiku when Gemma is unreachable. Force a specific backend with `[router] mode = "gemma"` or `"haiku"` in `vault.toml`. Haiku mode requires `ANTHROPIC_API_KEY`; per-call cost stays in the ~$0.0002 range because the routing prompt is tiny (the `cache_control` marker is set but inert below Haiku's ~4096-token minimum cacheable prefix).

## Usage

```bash
# Register the hook once globally
# Add to ~/.claude/settings.json:
# { "hooks": { "UserPromptSubmit": [{ "command": "/absolute/path/to/vault hook" }] } }

# Start the embeddings server (needs [embeddings].launcher_cmd in vault.toml)
vault tei start
vault tei status
vault tei logs
vault tei stop

# Index a repo before starting a cross-service session
vault index sync ~/repos/build-service                # first sync prompts for project name + domain
vault index sync ~/repos/auth-lib --domain software   # or pass --domain to skip the prompt
vault index sync ~/repos/build-service --dry-run      # preview: walk + counts only, no writes

# Diagnose retrieval quality
vault diagnose "what does BuildRequest need for auth?"
vault diagnose "what does BuildRequest need for auth?" --alpha 0.75

# Remove a project
vault index remove --project build-service
```

Sync is always explicit — you choose when to index and from what branch. This prevents WIP branch state from polluting the vault. Always sync from main/trunk.

Sync also prunes: chunks for files removed from the repo are dropped on the next sync, and chunks for definitions removed within a file (a deleted proto message, a removed exported function) are dropped when that file is re-parsed. There is no separate prune command — deletion reconciliation is part of every sync.

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
├── vault.db      # SQLite: projects, chunks, FTS5 index, embeddings, classification cache, retrieval log
├── vault.toml    # Context-tag fallback, router/classifier mode, tuning knobs, backend config (hand-authored; never written by vault)
├── hook.log      # Hook telemetry: one JSON line per prompt (outcome, latency, errors); rotated at 5MB
└── tei.pid/.log  # TEI launcher runtime files (vault tei start)
```

## Configuration

Context tags operate at the domain level — all projects in a domain share one tag, signaling the *kind* of knowledge Claude is receiving. A project's domain is assigned during `vault index sync` and stored in `vault.db` (`projects.domain`), not in `vault.toml`; the tag is derived by convention as `{domain}-context`.

```toml
# Abbreviated — [mlx] and [embeddings] are also required; see docs/vault-plan.md for the full file.
[defaults]
context_tag  = "vault-context"  # fallback when a project has no domain assignment
token_budget = 10000
alpha        = 0.6              # BM25/cosine weight
min_score    = 0.15
timeout      = 3                # required; the real hook timeout is [router].timeout_secs

[router]
mode         = "auto"   # "auto" | "gemma" | "haiku"
model        = "haiku"  # vault resolves to the current latest Haiku model
timeout_secs = 3        # optional; defaults to 3

[classifier]
mode         = "auto"   # same selection rules as [router]
model        = "haiku"
timeout_secs = 300      # required when this block is present
```

Project→domain assignment is **not** configured here — it's set during `vault index sync` and stored in `vault.db` (`projects.domain`). The context tag is derived by convention as `{domain}-context`.

The global `~/.claude/CLAUDE.md` should include a `## {domain}-context` section explaining each domain's tag to Claude. Introducing a new domain means adding that section — it's the single source of truth for what a tag means.

## Stack

- **Rust** — single binary, no per-project installation
- **SQLite** via rusqlite (bundled) — FTS5 BM25 + sqlite-vec cosine similarity
- **Gemma 4** via MLX — local routing and file classification, zero API cost (primary)
- **Anthropic Haiku** — fallback router/classifier when Gemma is unavailable; minimal token cost because the routing prompt is tiny (`cache_control` marker set but inert at this size)
- **nomic-embed-text-v1.5** (768-dim) via HuggingFace [text-embeddings-inference](https://github.com/huggingface/text-embeddings-inference) (TEI) — embeddings at index and query time

## Build

```bash
cargo build --release
```

**Router**: either Gemma 4 via `mlx_lm.server` on `localhost:8080` (zero-cost, recommended) or Anthropic Haiku via API (`ANTHROPIC_API_KEY`). `auto` mode picks Gemma when reachable, Haiku otherwise.

**Embeddings**: HuggingFace's `text-embeddings-inference` server on `localhost:8081`, serving `nomic-ai/nomic-embed-text-v1.5` (768 dims). Single binary, no Python deps; install via the prebuilt release, Docker image, or `cargo install --path .` from the TEI repo. Once installed, set `[embeddings].launcher_cmd` in `vault.toml` and use `vault tei start | stop | status | logs` to manage the service — if TEI is down when you run `vault index sync`, it aborts with a hint to run `vault tei start`. The hook never auto-spawns. See `docs/embeddings.md` for the full rationale; this choice is current-best and may change.

## Security

Vault is the central trust pivot of every Claude Code prompt — full design constraints, threat model, and trust boundaries live in [`docs/security.md`](docs/security.md). Highlights:

- **Indexed content is treated as data, not instructions.** Anyone who can write to a file vault indexes (vendored markdown, third-party proto comments, a teammate's `CLAUDE.md`) can attempt prompt injection through the context block. The global `~/.claude/CLAUDE.md` instruction explicitly tells Claude to ignore imperative language inside the block.
- **The indexer never follows symlinks** and applies a non-removable default exclusion list (`.env*`, `*.pem`, `.ssh/**`, `.aws/**`, `node_modules/**`, etc.). An index-time pre-scan also drops chunks matching common secret patterns.
- **Both backend services bind loopback only** — `127.0.0.1:8080` for `mlx_lm.server`, `127.0.0.1:8081` for TEI. Vault assumes anything answering on those ports is authoritative; that is a single-user-workstation assumption.
- **`ANTHROPIC_API_KEY` is environment-only.** Never written to `vault.toml`, never logged, redacted in `vault diagnose`.
- **`~/.vault/` is `0700`, files inside `0600`.** `vault.db` contains plaintext indexed content; rely on OS-level disk encryption for stolen-laptop scenarios.
- **The hook fails open.** Any error path emits empty stdout and exits 0, so your prompt passes through unchanged and a vault failure never blocks Claude Code.

If you index repos containing secrets or content you don't trust, read [`docs/security.md`](docs/security.md) before doing so.
