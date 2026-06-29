# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

`vault` is a single Rust binary that injects relevant project context into every Claude Code prompt before it reaches the Anthropic API. It indexes stable artifacts (proto contracts, design docs, conventions, CLAUDE.md files) into a local SQLite store and decorates prompts via a pre-send hook.

Routing is local-first via Gemma (zero Claude token cost). When Gemma is unreachable, vault falls back to Anthropic Haiku via API — minimal cost (~$0.0002/hook call; the routing prompt is tiny) so the hook keeps working on machines without MLX.

## Build & Run

```bash
cargo build
cargo build --release
cargo test
cargo test <test_name>       # run a single test
cargo run -- <subcommand>    # e.g. cargo run -- diagnose "what does BuildRequest need?"
```

## Architecture

Execution modes from one binary, dispatched by subcommand in `main.rs`:
- **`vault configure`** — first-run setup; provisions `~/.vault/` (0700), seeds a `vault.toml` template **only when absent** (0600), prints the Claude Code hook entry to add, and reports backend readiness. Idempotent. Never edits `~/.claude/settings.json` (print-only); `--force` re-seeds an existing toml.
- **`vault hook`** — pre-send hook (registered globally in `~/.claude/settings.json`); reads prompt JSON from stdin, emits only the `<{domain}-context>` block to stdout (Claude Code appends it to the prompt)
- **`vault index sync <repo>`** — explicit manual indexing; the classifier (Gemma local or Haiku fallback) labels files automatically (black box — no confirm/override), chunks written to SQLite
- **`vault diagnose "<prompt>"`** — full retrieval trace for tuning alpha and token budget
- **`vault tei start|stop|status|logs`** — manage the local TEI embeddings server

### Request Flow (hook mode)

```
prompt → vault hook → Router extracts query plan
                      ├── primary:  Gemma at localhost:8080 (zero token cost)
                      └── fallback: Haiku via Anthropic API (~$0.0002/call, tiny prompt)
       → SQLite hybrid query (FTS5 BM25 + sqlite-vec cosine)
       → score merge (α=0.6 BM25, 0.4 cosine) + token budget (10k)
       → emit <{domain-context}> block on stdout → Claude Code appends it → Anthropic API
```

The router returns `{ skip: true }` for prompts that need no context — immediate passthrough with no SQLite query. 3-second timeout on all router calls (Gemma or Haiku); silent passthrough on timeout or unavailability.

### Key modules

| Path | Responsibility |
|------|---------------|
| `src/hook/mod.rs` | stdin→stdout hook protocol, full pipeline entry; outcome taxonomy (injected / skip / failed-at-stage) |
| `src/hook/log.rs` | hook telemetry — one metadata-only JSONL record per call to `~/.vault/hook.log` (5MB rotation) |
| `src/store/traits.rs` | `Store` trait + `StoreError` — backend abstraction; carries the embedding model/dim lock error |
| `src/store/schema.rs` | embedded SQL, migration runner |
| `src/store/sqlite_store.rs` | the live (SQLite-only) backend — upsert + sync-time prune (file/document/chunk diff, reconciles deletions every sync) **and** FTS5 + sqlite-vec hybrid retrieval, score merge, budget trim |
| `src/store/postgresql_store.rs` | `PostgresStore` — `todo!()` placeholder for a future distributed backend (pgvector/tsvector); declared but not exported, not wired up |
| `src/store/types.rs` | shared store types: `Document`, `Chunk`, `Hit`, `RetrievalLogEntry` |
| `src/retrieve/router/mod.rs` | Router trait + auto/gemma/haiku/openai mode selection (auto's remote fallback set by `[router].remote`) |
| `src/retrieve/router/gemma.rs` | Local Gemma impl (mlx_lm.server HTTP) |
| `src/retrieve/router/haiku.rs` | Anthropic Haiku impl (sets `cache_control`; inert at current prompt size) |
| `src/retrieve/router/openai_compat.rs` | Generic OpenAI-compatible impl (Gemini AI Studio / Vertex express / any `/chat/completions`); static key from `api_key_env`, Bearer or `x-goog-api-key` auth |
| `src/retrieve/hybrid.rs` | BM25 + cosine score merge |
| `src/retrieve/budget.rs` | token-aware chunk selection |
| `src/parse/` | per-language parsers (proto, go, rust, openapi, markdown); `select_parser` dispatches on `(doc_type, language)` — `plan` and unrecognized types fall back to `whole_file_chunks` (single chunk when small, line-windowed when over the embed ceiling) |
| `src/index/classify/mod.rs` | Classifier trait + auto/gemma/haiku/openai selection (mirrors the router) |
| `src/index/classify/gemma.rs` | Local Gemma classifier |
| `src/index/classify/haiku.rs` | Anthropic Haiku classifier (cost prompt on first use) |
| `src/index/classify/openai_compat.rs` | Generic OpenAI-compatible classifier (mirrors `router/openai_compat.rs`; generic billing-confirm prompt) |
| `src/index/walk.rs` | repo walker — globset exclusions, symlink refusal, canonical-root bound (enforces the indexer security rules) |
| `src/index/sync.rs` | `vault index sync` pipeline — classify → parse (whole-file fallback) → embed → upsert; `SyncReport` |
| `src/index/secrets.rs` | index-time secret pre-scan (`RegexSet`) — drops chunks matching AWS/GitHub/Anthropic/OpenAI/JWT/PEM patterns before storage |
| `src/embed/tei.rs` | nomic-embed-text-v1.5 embeddings via TEI HTTP (`localhost:8081`) |
| `src/tei/launcher.rs` | `vault tei start\|stop\|status\|logs` — spawn TEI from `[embeddings].launcher_cmd` with env scrubbing; PID + log in `~/.vault/`; cross-platform detach |
| `src/diagnose/mod.rs` | `vault diagnose "<prompt>"` — full retrieval trace for tuning α and token budget |
| `src/config.rs` | `vault.toml` parsing — `Config`, `ConfigError`, context-tag fallback (`default_context_tag`), router/classifier mode + timeout knobs |
| `src/types.rs` | top-level shared enums — `Language`, `DocType` (orthogonal axes used across parse/classify/router) |
| `src/util/` | `fs.rs` (0700/0600 hardening for `~/.vault/`), `json.rs` (balanced-brace extraction from model replies), `path.rs` (`~` expansion), `probe.rs` (200ms loopback TCP probe for auto-mode) |

### Router selection

Both the runtime router (hook mode) and the index-time classifier follow the same trait-based pattern. Mode is set in `vault.toml`:

```toml
[router]
mode  = "auto"      # "auto" | "gemma" | "haiku" | "openai" (alias "gemini")
model = "haiku"     # alias — vault resolves to the current latest Haiku model
remote = "haiku"    # which backend `auto` falls back to: "haiku" (default) | "openai"

# Only consulted by the openai backend (mode/remote = "openai"):
base_url    = "https://generativelanguage.googleapis.com/v1beta/openai"  # AI Studio Gemini
api_key_env = "GEMINI_API_KEY"   # name of the env var holding the key (never the key itself)
auth_header = "bearer"           # "bearer" (AI Studio) | "x-goog-api-key" (Vertex express)

[classifier]
mode  = "auto"
model = "haiku"
timeout = 300        # in seconds; optional, defaults to 300
# remote / base_url / api_key_env / auth_header mirror [router].
```

- **`auto`** (default) — probe `localhost:8080` once at startup with a 200ms timeout. If reachable, use Gemma; otherwise fall back to `remote` (`haiku` by default, `openai` if set). Decision is cached for the process lifetime; no per-call probing. Local Gemma stays primary so the zero-token-cost guarantee holds whenever it's up.
- **`gemma`** — force local Gemma. Silent passthrough if unavailable (preserves the zero-token-cost guarantee).
- **`haiku`** — force remote Haiku. Requires `ANTHROPIC_API_KEY`.
- **`openai`** (alias **`gemini`**) — force the generic OpenAI-compatible backend (Google AI Studio Gemini, Vertex express, or any `/chat/completions` provider). Requires the key in `api_key_env` (default `GEMINI_API_KEY`); `model` is sent verbatim (set it to e.g. `gemini-3.5-flash`, not the `haiku` alias). For Vertex express set `base_url = "https://aiplatform.googleapis.com/v1"` and `auth_header = "x-goog-api-key"`. This is the recommended remote now that Anthropic endpoints are locked down.

Haiku impls set `cache_control: ephemeral` on the system block, but the marker is **inert today**: prompt caching only engages once the cached prefix reaches Haiku's ~4096-token minimum, and `ROUTER_SYSTEM` (schema + instruction) is only a few hundred tokens — so no cache entry is ever created (`cache_creation_input_tokens: 0`, no error). Per-call cost is ~$0.0002 because the prompt is *tiny*, not because caching is working. The marker is forward-looking: if the system block ever grows past ~4096 tokens (e.g. added few-shot examples), caching kicks in and the byte-identical-between-backends requirement on `ROUTER_SYSTEM` starts mattering.

`vault index sync` shows a one-time cost prompt the first time a session falls back to a remote backend for classification. Haiku quotes an estimate (e.g. *"Gemma not detected. Use Haiku for classification? Estimated cost: ~$0.03 for 200 files. [y/N]"*); the openai backend confirms generically without a figure (no pricing table) — *"Gemma not detected. Use the configured remote API (openai) for classification? N files — provider billing applies. [y/N]"*.

### Runtime data

```
~/.vault/vault.db      # SQLite store — projects (incl. projects.domain assignment), documents, chunks, FTS5, vec, retrieval_log; documents.content_hash is the classification/re-embed cache
~/.vault/vault.toml    # context-tag fallback, router/classifier mode, tuning defaults, backend config (hand-authored; vault writes it only via `vault configure` when absent — never otherwise)
~/.vault/hook.log      # hook telemetry — one JSONL record per hook call (outcome, stage, latency, backend); rotated to hook.log.1 at 5MB
```

Nothing is written to indexed repositories.

## Implementation Order

The store layer must come before retrieval; `vault diagnose` must work before parsers:

```
Step 0  Confirm embedding stack (TEI reachable, nomic-embed-text-v1.5 = 768 dims) — locks chunks_vec FLOAT[768]
Step 1  store/schema.rs
Step 2  store/sqlite_store.rs — upsert + sync-time prune (behind the Store trait in store/traits.rs)
Step 3  store/sqlite_store.rs — FTS5 + sqlite-vec hybrid query, score merge, budget trim
Step 4  vault diagnose — validate retrieval with manually seeded data before building parsers
Step 5  parse/proto.rs
Step 6  parse/go_source.rs
Step 7  parse/rust_source.rs
Step 7a parse/openapi.rs                       — paths × methods + schemas (yaml-rust2; JSON parses as YAML)
Step 7b parse/markdown.rs                      — per `##` block (convention/meta; plan stays whole-file)
Step 8a embed/tei.rs                          — HTTP client against TEI /embeddings
Step 8b tei/launcher.rs                       — `vault tei start|stop|status|logs` subcommands
Step 9  index/classify/{mod,gemma,haiku}.rs   — Classifier trait + impls (cost prompt on first Haiku use)
Step 10 retrieve/router/{mod,gemma,haiku}.rs  — Router trait + impls (auto-mode startup probe, prompt caching)
Step 11 retrieve/hybrid.rs
Step 12 retrieve/budget.rs
Step 13 hook/mod.rs
Step 14 first-run UX (project-name + domain prompts on new project sync; classification is automatic — no confirm/override)
```

## Embeddings

See `docs/embeddings.md` for the full write-up. Current decisions (subject to change):

- **Backend** — HuggingFace [text-embeddings-inference](https://github.com/huggingface/text-embeddings-inference) (TEI), an official Rust HTTP server. Single binary, no Python deps, OpenAI-compatible `/embeddings` endpoint. Endpoint defaults to `http://localhost:8081`.
- **Model** — `nomic-ai/nomic-embed-text-v1.5`. Apply the `search_document:` prefix at index time and `search_query:` at query time.
- **Dimensions** — defaults to **768** (nomic-embed-text-v1.5). `chunks_vec` is created at the dim from `[embeddings].dims`, then **locked per-DB**: the first sync records `(model, dim)` in the `meta` table and later opens must match. Changing the model/dim means a full reindex (delete `~/.vault/vault.db` and re-sync). The schema no longer hardcodes 768 — only the config default does.

`vault index sync` requires TEI reachable (hard error if not). At hook time, TEI unreachable falls under the same 3-second silent passthrough as any other backend failure.

The remaining open knobs are empirical, not blocking:

- α tuning (BM25 vs cosine weight) — start 0.6, validate with `vault diagnose`
- Token budget ceiling — start 10k, validate with `vault diagnose`
- Context block ordering — score-descending within project grouping for now

## Chunking

`doc_type` and `language` are orthogonal. Chunk boundaries:

| doc_type | language | Boundary |
|----------|----------|----------|
| contract | proto | per message/service/enum |
| contract | openapi | per path+method, per schema component |
| plan | any | whole file, unless over the embed ceiling → windowed (see below) |
| convention | go/rust | per exported symbol + doc comment |
| convention/meta | markdown | per `##` heading block |
| convention | scala | whole file (v1) |

Whole-file fallback (`plan` docs and any file no structural parser claims) is **windowed**: content under `MAX_FALLBACK_CHUNK_TOKENS` (1500, well under nomic's 8192-token context) stays a single chunk — identical to the historical behavior — while larger content is greedily packed by whole lines into ordered, embeddable chunks. This keeps a large file from exceeding the embedder's input limit and aborting the whole document. A single line longer than the ceiling (minified blob, one-line log) is truncated head-only rather than char-split, so the per-chunk secret scan can't be bisected. The sync report counts windowed files and truncated lines.

## Scoring & Tuning

```
final_score = α * bm25_normalized + (1 - α) * cos_sim
α = 0.6 (initial), MinChunkScore = 0.15, TokenBudget = 10_000
```

Tune `alpha` via `vault diagnose "<prompt>" --alpha X` after seeding real data; the token budget is set in `vault.toml` (`defaults.token_budget`), not a diagnose flag. Budget fill is score-descending with `continue` (not `break`) on oversized chunks.

## Global Hook Registration

```json
// ~/.claude/settings.json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "/absolute/path/to/vault hook" } ] }
    ]
  }
}
```

`UserPromptSubmit` entries are matcher groups, each holding a nested `hooks` array of `{ "type": "command", "command": ... }` handlers — the flat `[{ "command": ... }]` shorthand does **not** load. `vault configure` prints this exact shape with the absolute path filled in. `UserPromptSubmit` is the event Claude Code fires before sending the user's prompt to the model. Stdout from the hook is **appended** to the prompt context (not a replacement) — that's why `vault hook` emits only the `<vault-context>...</vault-context>` block and never the user's prompt. Exit 0 with empty stdout = silent passthrough. The per-call timeout for this event is 30s.

## Context Tags

Tags are domain-level (not project-level). A project's domain is assigned during `vault index sync` and stored in `vault.db` (`projects.domain`); the hook derives the tag by convention as `{domain}-context`, falling back to `defaults.context_tag` when unassigned. Introducing a *new* domain requires adding a matching `## {domain}-context` section to `~/.claude/CLAUDE.md` (the single source of truth for what a tag means) — vault.toml is not involved.

## Security

Vault is on the hot path of every Claude Code prompt. Full design constraints, threat model, and trust-boundary table are in `docs/security.md`. Non-negotiable rules to apply when writing code:

- **Indexed content is untrusted data, not instructions.** The global `~/.claude/CLAUDE.md` framing handles this for Claude; vault never sanitizes chunk text. Don't change that without revisiting the trust model.
- **SQL parameter binding everywhere.** Router output (`projects`, `type_names`, `topics`, `doc_types`, `languages`) is untrusted-shaped — bind it via rusqlite's named/positional params. Never `format!` into SQL.
- **Provider API keys are environment-only.** This covers `ANTHROPIC_API_KEY` and the openai backend's key (named by `[router]/[classifier].api_key_env`, e.g. `GEMINI_API_KEY`). `vault.toml` stores only the env-var *name*, never the secret. Never read a key from `vault.toml` or any file vault writes; never log or echo it; redact in `vault diagnose`. The remote router/classifier structs don't derive `Debug` so a key can't leak through a debug print.
- **Loopback only.** Vault talks to `127.0.0.1:8080` (mlx_lm.server) and `127.0.0.1:8081` (TEI). Treating localhost as authoritative is a documented assumption, not a guarantee.
- **`~/.vault/` is `0700`, files inside `0600`.** Indexed content is plaintext and may be proprietary.
- **Indexer never follows symlinks** and is bounded to the canonical repo root. Default exclusion list (`.env`, `*.pem`, `.ssh/**`, etc.) is non-removable in v1.
- **Index-time secret pre-scan.** Chunks matching common secret patterns (AWS keys, GitHub/Anthropic/OpenAI tokens, JWT, PEM headers) are dropped before storage.
- **Classifier sees filename + extension + first 1KB only**, never full files. Full content reaches Anthropic only via retrieval-time injection, which the user controls via `vault diagnose`.
- **Hook fails open.** Any error → empty stdout, exit 0 — never block the user. Failures stay observable without breaking that contract: one stderr breadcrumb (visible only in Claude Code debug mode) plus a metadata-only JSONL record in `~/.vault/hook.log` — never prompt text, never chunk content; error detail truncated.
- **`~/.claude/settings.json` should reference vault by absolute path** (not `vault hook` resolved via PATH).

## v1 Scope Boundaries

Out of scope: MCP server subcommand, multi-user sharing, CI auto-indexing, git hook sync, per-project `.vault.yaml` files, Helm parser, Scala AST chunking, session state in vault.db.
