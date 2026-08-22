# Review: docs/vault-plan.md — open items

**Originally:** 2026-06-11, verified against commit `6b08f9d` (branch `init`)
**Pruned:** 2026-08-21 — resolved findings removed; see below for what went and why

The design plan was written before Steps 1–14.8 were implemented and drifted from
the code. This file recorded that drift. Roughly two thirds of the findings have
since been fixed, and carrying them made the document read as a backlog when most
of it was history.

**What remains below is open work only.** The full original review — every
resolved finding with its resolution note and commit — is in git history:

```
git show af1aeab:docs/plan-review-2026-06-11.md
```

Item IDs are unchanged, so an external reference to "A9" or "C2" still resolves.
Gaps in the numbering mean that item was fixed.

---

## Priority

### P1. The router cannot meet the hook's latency contract.

- The plan promises a 3-second timeout as an invariant and Gemma latency of
  "~100–300ms". The deployed model (`gemma-4-31b-bf16`) runs ~15s/call warm.
- **One knob, two contexts.** `[router].timeout` governs both the hook hot path
  and interactive diagnose (`GemmaRouter::from_config` → `config.router_timeout()`).
  Raising it for diagnose silently rewrites the hook contract. Two failure modes
  fork from here: at the 3s default every hook call times out to silent
  passthrough; at 120s the hook *succeeds* at a ~15s tax on every prompt — half
  of Claude Code's 30s `UserPromptSubmit` budget, with the session visibly
  stalled for that whole time, and cancelled to passthrough past 30s.
- The auto-probe is TCP-reachability only (200ms, `src/util/probe.rs`) — it
  cannot detect "reachable but too slow", so auto mode selects the unusable
  backend happily.
- One `[mlx].router_model` serves both router and classifier. Routing needs
  small and fast; classification tolerates big and slow. No per-role knob.
- Candidate resolutions: a dedicated small routing model (or Haiku for the
  hook), per-role model + timeout keys, a hook-side hard clamp.

*Verified still open 2026-08-21: `[router].timeout` is a single key; no per-role
model or timeout exists.*

### P3. The injection-framing contract is unsatisfied — and it is now live.

When this was written nothing was registered, which made it a design decision.
It is no longer: `~/.claude/settings.json` **has** a `UserPromptSubmit` entry and
`~/.claude/CLAUDE.md` **does not exist**, so injected context reaches the model
today with no data-not-instructions framing at all.

- The proposed global CLAUDE.md text enumerates three domain tags. The fallback
  `<vault-context>` tag — returned whenever no project matches a domain — is not
  covered, and the same hole opens every time a domain is added without the
  two-file edit. Proposal to decide: **one constant wrapper tag with a domain
  attribute** (`<vault-context domain="software">`), so the instruction is
  written once and cannot drift.
- The instruction says the block is "grouped by project". `Context::render_block`
  emits a flat `## label [doc_type]` list — no grouping, no language in the
  header. For a whole-file chunk the model sees `## CLAUDE.md [meta]` with no
  indication of which repo it came from. Either implement grouping or fix the
  contract text.

*Verified still open 2026-08-21: `render_block` (`src/retrieve/mod.rs:107`) is
flat; the global CLAUDE.md is absent while the hook is registered.*

Related: **B4**, cross-domain tag selection is first-assigned-project-wins and so
order-sensitive to router output ordering; the plan never specifies mixed-domain
behaviour. The logic now lives in `Store::resolve_domain`, with the tag derived
as `{domain}-context`. Subsumed by the single-tag proposal above.

### P4. Long prompts still break query-time embedding.

The index-time half is **done** — `whole_file_chunks` windows content over
`MAX_FALLBACK_CHUNK_TOKENS` (1500) into embeddable chunks instead of sending one
oversized blob, and the sync report counts windowed files and truncated lines.

What remains is the query side: `embed_query` sends the prompt untruncated, so a
long prompt (a pasted diff or log) fails the embed and falls through to silent
passthrough — exactly when context would help most.

---

## A. Plan contradicts the implemented system

| # | Plan says | Reality | Where |
|---|-----------|---------|-------|
| A2 | `retrieve/hybrid.rs` "absorbed into `sqlite_store::hybrid_search` — skip Step 11"; `Store` trait = 5 methods | Reversed by `455303d`: the trait exposes `bm25_search`/`cosine_search` primitives with the merge shared in `retrieve/hybrid.rs`, so all backends score identically. Also has `get_or_create_project`, `get_document_content_hash`, and an alpha param | plan 613–617, 637–643, 1025–1028 |
| A3 | Token estimation = "tiktoken cl100k_base, accurate counts", listed as a *Confirmed* decision | chars/4 heuristic (`estimate_tokens`, `div_ceil`). cl100k is OpenAI's tokenizer and never matched Claude anyway | plan 199, 911 vs `src/parse/mod.rs:194` |
| A5 | CLI: `index add/remove`, `list`, `reindex`, `serve` | Docs reconciled 2026-06-21 (marked planned-not-implemented). **Code still open: `index remove` itself** — load-bearing, because the `documents` FK has no CASCADE, so it needs explicit child deletes plus a `chunks_vec` sweep. A manual sqlite3 delete on 2026-06-11 left 16 orphaned vec rows in the live DB | plan 768–790 vs `src/main.rs` |
| A9 | Re-embed skip + byte-compare collision defence | Unchanged-file skip landed 2026-06-17 (`7dac21d`) via the `documents.content_hash` gate. **Still open for a changed file:** per-chunk `content_hash` is stored but never compared, so a one-line edit re-embeds every chunk in that file | plan 389–413 vs `src/index/sync.rs` |
| A10 | `retrieval_log` drives alpha tuning | Zero producers. `Store::log_retrieval` exists and `SqliteStore` implements it, but nothing in the hook or diagnose calls it — the only callers are test stubs and one unit test | plan 219–227, 965 |

---

## B. Internal contradictions / underspecification

- **B1 / B3 — `retrieval_log`'s fate is undecided.** "Hook runtime access:
  read-only" (plan line 904) contradicts retrieval_log's stated purpose of
  collecting real hook prompts for replay (line 965). Even if written, the table
  cannot serve replay: it has a `prompt_hash` but no prompt text or embedding, no
  alpha or budget columns, and aggregate counts only. Decide as one change —
  bless hook-writes and add the columns, or drop the table and log to a file.
  *(The WAL half of this finding is resolved: `schema::apply_pragmas` sets
  `journal_mode=WAL` and a `busy_timeout` on every connection.)*

- **B5 — scoring calibration.** BM25 normalises against the result-set max, so
  the top keyword hit always scores 1.0 and the final score is ≥ α (0.6)
  regardless of absolute relevance; scores are not comparable across queries. The
  behaviour and candidate replacements (fixed divisor, sigmoid, theoretical max;
  RRF rejected as ordinal) are documented in the plan's Step-3 scoring section.
  The calculation change itself is **gated on C2's eval set**. Raw scores are
  retained per `Hit`, so no migration is needed.

- **B8 — `chunks_vec` has no delete trigger while FTS5 does.** Deliberate: a
  vec0-referencing trigger breaks every delete when the extension is not loaded.
  Undocumented. Fold the rationale and the required cleanup order into the
  `index remove` work (A5).

---

## C. Structural weaknesses

- **C1. `cwd` is an unused free signal.** `HookInput` deliberately ignores `cwd`
  while `projects.repo_path` exists. cwd → project → domain would give
  deterministic project bias, deterministic tag resolution, and a
  degraded-but-useful retrieval path when the router is down. Design after the
  router story (P1) stabilises.

- **C2. No eval ground truth.** The tuning loop optimises retrieval against
  itself. A small golden-prompt fixture set (prompt → expected chunk labels) as a
  test would anchor alpha and budget tuning. Unblocked since the markdown parser
  landed; blocks B5.

- **C3. The trust model is unverifiable by the binary.** The only injection
  defence is a hand-maintained `~/.claude/CLAUDE.md` instruction that vault never
  checks — and which is currently **missing** while the hook is registered (see
  P3). A `vault doctor` check would close it: instruction present, covers every
  configured tag, hook registered by absolute path.

---

## Doc-sync checklist — what is left

### `docs/vault-plan.md`
- [ ] Indexing: mark the unchanged-file re-embed skip **implemented**, note the
      per-chunk incremental skip is still open (A9); add the `chunks_vec` cleanup
      rationale (B8).
- [ ] Decisions table: token estimation → chars/4 with a revisit note (A3);
      hybrid placement → extracted, not absorbed (A2); latency table → real 31B
      numbers (P1).
- [ ] Tracking items: add entries for P1 (per-role model + timeout, hook clamp,
      latency-aware fallback), P3 (single-tag + domain-attribute decision, block
      grouping vs contract text, doctor check), P4 (query-side embed truncation),
      B1/B3 (retrieval_log fate), C1, C2.

### Verification when executing
- `cargo test` stays green (docs only).
- Validate any new example `vault.toml` parses against `src/config.rs`.
- Re-read the edited docs for internal consistency — the hook event, the tag
  story, and the caching claim must be told identically in all of them.
