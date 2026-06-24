# Embeddings

This document explains the embeddings layer in `vault` — what embeddings are, what
they provide, where they fit in the pipeline, and why we picked the specific model
and runtime we did.

For the broader architecture, see `vault-plan.md`. For the design-conversation
log, see `vault-context.md`.

---

## What an embedding is

An embedding is a fixed-length array of floats that represents the *meaning* of a
piece of text. For vault we use **nomic-embed-text-v1.5**, which produces **768 floats
per chunk**:

```
"BuildRequest contains the protobuf payload..."  →  [0.12, -0.34, 0.87, ..., 0.07]
```

The model is trained so that text with similar meaning lands near each other in this
768-dimensional space. Cosine similarity between two vectors tells you how related
their meanings are: 1.0 = identical, 0.0 = unrelated.

---

## What they provide for retrieval

Vault's retrieval is hybrid — half BM25 keyword match, half cosine similarity over
embeddings:

```
final_score = α * bm25_normalized + (1 - α) * cos_sim
α = 0.6 (initial)
```

The two halves catch different things:

- **BM25** is great at exact identifiers (`BuildRequest`, `AuthService`) but blind
  to vocabulary mismatch
- **Cosine** is great at conceptual matches but fuzzy on specific symbols

A query like *"how does the build service know who's calling?"* shares zero words
with a chunk that documents `BuildRequest.auth_token`, but their embeddings will be
close — both texts are about authentication in a build context. BM25 alone misses
it.

This is the entire reason vault stores embeddings. Without them, retrieval degrades
to keyword search and the user has to phrase prompts using the exact terms that
appear in the indexed docs.

---

## Where embeddings fit in the pipeline

### Index time (`vault index sync <repo>`)

1. Parser extracts a chunk (one proto message, one `##` section, one exported Go/Rust
   symbol with its doc comment, etc.)
2. TEI is called with `search_document: <chunk text>` → returns 768 floats
3. Vector is stored in `chunks_vec` (a sqlite-vec virtual table) keyed by
   `chunk_id`, alongside the chunk row in the main table

### Query time (every hook call)

1. Router extracts a query plan from the user's prompt (Gemma locally, or Haiku as
   fallback)
2. The plan's query text → TEI with `search_query: <text>` → 768 floats
3. sqlite-vec computes cosine similarity over the stored vectors → top N by score
4. FTS5 BM25 runs in parallel → top N by keyword score
5. Merged into a single ranked list (`hybrid.rs`), trimmed to the 10k token budget
   (`budget.rs`)

The asymmetric `search_document:` / `search_query:` prefixes are a nomic-embed-text
feature — queries and documents are embedded into compatible but distinct subspaces,
which measurably improves retrieval quality. Both prefixes must be applied
consistently or the cosine scores will be miscalibrated.

---

## Why nomic-embed-text-v1.5

- Apache 2.0 license, free local use
- Strong scores on the MTEB retrieval benchmark in its size class
- 768 dims — small enough for fast sqlite-vec lookups, large enough for quality
- Asymmetric query/document prefixes (above)
- Supported natively by TEI, no model conversion required

The dimension count is **locked at schema creation**: `chunks_vec FLOAT[768]` cannot
be changed without rebuilding the entire index. This is why **Step 0** of the
implementation order is *confirm embedding dims* before writing schema.

---

## Why TEI (text-embeddings-inference)

[`text-embeddings-inference`](https://github.com/huggingface/text-embeddings-inference)
is HuggingFace's official Rust HTTP server for embedding models.

- Single binary, OpenAI-compatible `/embeddings` endpoint, no Python deps
- Cross-platform (Mac primary, Linux/Windows supported)
- CPU-only is fast enough for vault's scale (no GPU required)
- Maintained by HuggingFace — meaningfully different bus-factor from a
  single-maintainer crate

In auto mode, vault calls TEI on `localhost:8081`. **TEI must bind `127.0.0.1`
(loopback) only — never `0.0.0.0`.** Vault treats anything answering on that
port as authoritative; a `0.0.0.0` bind exposes the embedding surface to the
LAN, and a malicious local process binding the port first would intercept every
query embed. See `security.md` → "Localhost is a trust assumption, not a
guarantee" for the full statement.

The endpoint is configurable in `vault.toml`:

```toml
[embeddings]
endpoint     = "http://localhost:8081"
model        = "nomic-ai/nomic-embed-text-v1.5"
dims         = 768
launcher_cmd = "text-embeddings-router --model-id nomic-ai/nomic-embed-text-v1.5 --port 8081"
```

`launcher_cmd` is consumed by the `vault tei start | stop | status | logs`
subcommand group. Vault spawns TEI from this command, detaches it
(`process_group(0)` on Unix, `DETACHED_PROCESS` on Windows), and tracks the
PID + log file in `~/.vault/` (`tei.pid`, `tei.log`). The spawn does
`env_clear()` then re-adds only a minimal allowlist — `PATH`, `HOME`, the
HuggingFace cache vars, and locale (plus the system vars Windows needs to start
a process) — so `ANTHROPIC_API_KEY` is never inherited. `vault tei start` is a
no-op when TEI is already reachable; if `launcher_cmd` is unset it errors and
prints an example line to add. The hook never auto-spawns regardless — see
"Index-time vs hook-time availability" below.

### Index-time vs hook-time availability

- If TEI is unreachable at **index time**, `vault index sync` verifies it first
  and aborts with an error that points you at `vault tei start`. With no
  `launcher_cmd` set, `vault tei start` in turn tells you to add one or launch
  TEI by hand — you can't index without embeddings.
- If TEI is unreachable at **hook time**, the hook silently passes through (no
  context block) under the 3-second timeout, same as any other backend failure.
  **The hook never auto-spawns TEI** — cold-start blows the budget.

---

## Cost and footprint

| Concern        | Cost                                                          |
|----------------|---------------------------------------------------------------|
| Indexing       | ~10–50 ms per chunk on CPU; a 200-file repo indexes in seconds |
| Hook overhead  | One query embed per call, ~10–50 ms; well under the 3 s budget |
| Storage        | 768 × 4 B = ~3 KB per chunk raw; sqlite-vec encodes more compactly |
| API cost       | $0 — TEI runs locally                                          |

---

## What embeddings do not do

- **They don't replace BM25.** Exact symbol lookups (`BuildRequest`, `AuthService`)
  still need keyword matching; cosine surfaces conceptually adjacent chunks rather
  than the specific symbol you named. Both halves of the hybrid score matter.
- **They don't understand code as code.** They index the *text* of source files;
  identifiers are treated as words. Deep semantic code understanding is out of
  scope for v1.
- **English-primary.** nomic-embed-text-v1.5 does not reliably cross
  natural-language boundaries.

---

## Implementation pointers

- **Step 0** — confirm dims (768 for nomic-embed-text-v1.5) and write
  `[embeddings]` block to `vault.toml`
- **Step 1** — `chunks_vec FLOAT[768]` declared in `store/schema.rs`
- **Step 8** — `src/embed/tei.rs` — HTTP client against TEI's `/embeddings` endpoint,
  applies the `search_document:` / `search_query:` prefix, returns `Vec<f32>` of
  length 768
- Failure mode at hook time is the same as any backend timeout: silent passthrough,
  no context block, never breaks Claude Code

---

## Future considerations (out of scope for v1)

- Re-embedding when content changes — index sync uses content-hash to detect new
  chunks, so unchanged chunks keep their existing vector
- Alternative models — switching dims requires a full reindex, so model changes are
  a deliberate migration, not a config flip
- GPU acceleration via TEI — supported by the server itself, not needed at our
  corpus size
