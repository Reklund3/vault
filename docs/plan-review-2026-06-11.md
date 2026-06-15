# Review: docs/vault-plan.md

**Date:** 2026-06-11 · **Verified against:** working tree at commit `6b08f9d` (branch `init`) · **Status:** findings recorded, follow-up not yet executed

The design plan was written before Steps 1–14.8 were implemented and has drifted: some sections contradict the code, some contradict each other, and some describe behavior as current that was never built. Every finding below was verified against the source, plus a second-opinion advisor pass that confirmed/corrected them. Code-affecting findings are recorded as decisions/tracking items for later slices — nothing here mandates a code change by itself.

---

## Part 1 — Findings

### Top 5 (priority order)

**P1. Hot-path viability: the router cannot meet the hook's latency contract.**
- The plan promises "3 second timeout" as an invariant (lines 495, 568, 909) and Gemma latency of "~100–300ms" (line 128). The deployed model (`gemma-4-31b-bf16`) runs ~15s/call warm — the live `vault.toml` comment says so.
- One knob, two contexts: `[router].timeout_secs` governs both the hook hot path and interactive diagnose (`GemmaRouter::from_config` → `config.router_timeout()`, src/retrieve/router/gemma.rs:30). The live config raised it to 120s for diagnose — which silently rewrites the hook contract. Two failure modes fork from here: default config (3s) → every hook call times out → **systematic silent passthrough**; live config (120s) → the hook *succeeds* at a **~15s tax on every prompt** — roughly **half** of Claude Code's 30s default `UserPromptSubmit` timeout (verified 2026-06-14 against code.claude.com/docs/en/hooks.md; `UserPromptSubmit` is special-cased to 30s vs the 600s default on other events), and the session visibly stalls for that whole ~15s because the hook blocks model processing until it returns; cross 30s and the call is cancelled to silent passthrough. Both unacceptable, different bugs. The plan's `[defaults] timeout` knob is parsed-but-dead in code (`#[allow(dead_code)]`, src/config.rs:30).
- The auto-probe is TCP-reachability only (200ms, src/util/probe.rs) — it cannot detect "reachable but too slow," so auto mode happily selects the unusable backend.
- One `[mlx].router_model` serves both router and classifier (src/config.rs:213–219). Routing needs small/fast; classification tolerates big/slow. No per-role model knob.
- Candidate resolutions to record: dedicated small routing model (or Haiku-for-hook), per-role model + timeout keys, hook-side hard clamp on router timeout.
- *(The companion zero-observability finding was resolved 2026-06-12 — outcome telemetry in `~/.vault/hook.log` + stderr breadcrumb; see the plan's Hook Behavior section. The 15s router tax is now measured per call, not yet fixed.)*

**P2. Query-plan → filter trust gap: silent total-context-loss paths. — RESOLVED 2026-06-14.**
- *All three paths are now closed. Project names filter case-insensitively (`name COLLATE NOCASE IN (...)`), mirroring the `eq_ignore_ascii_case` used for domain-tag resolution, so a router emitting "Vault" against a stored "vault" no longer voids every chunk. `languages` is validated-drop: unrecognized labels are dropped at `from_raw` (`filter_map(Language::from_str(...).ok())`) rather than collapsing to `Language::Unknown` and filtering to nothing; an emptied list means "no language filter" (degrade-to-broad), while an explicit `"unknown"` still survives. The earlier strict-`doc_types` path was resolved 2026-06-12 the same way — unrecognized values dropped, emptied list means no filter, dead `RouterError::Unparseable` removed. The unifying rule across all three: untrusted router-emitted enum labels are dropped on parse, never coerced to a poisoning sentinel.)*

**P3. The injection-framing contract is broken on two axes — and it's a now-decision (nothing is registered yet: `~/.claude/settings.json` has no hooks entry, `~/.claude/CLAUDE.md` doesn't exist).**
- The proposed global CLAUDE.md text enumerates exactly three domain tags; the fallback `<vault-context>` tag (code default, returned whenever no project matches a domain) is **not covered** → context arrives with no data-not-instructions framing. Same hole every time a domain is added without the two-file edit. Improvement to record: **one constant wrapper tag with a domain attribute** (`<vault-context domain="software">`) — instruction written once, never drifts, kills the two-file coupling.
- The instruction says "the block is grouped by project" — but `render_block` (src/hook/mod.rs:75–96) emits a **flat `## label [doc_type]` list**: no project grouping, no language in the header (plan shows `[contract/proto]`). For whole-file chunks Claude sees `## CLAUDE.md [meta]` with no idea which repo it came from. Either implement grouping or fix the contract text — decide before first registration.
- Plan's settings.json example uses the wrong event (`PreToolUse`; reality `UserPromptSubmit`) and a PATH-resolved `vault hook`, violating the plan's own security rule (absolute path).

**P4. Chunk-size pipeline: oversized whole-file chunks are unembeddable and unretrievable.**
- The plan document itself (48,198 bytes ≈ 12k tokens by chars/4) exceeds the 10k budget: budget fill `continue`-skips oversized chunks, so it could never be injected.
- Worse (advisor): `embed_query`/document embedding sends untruncated text to TEI. Oversized content → TEI 413 → embed error → at index time **the file is skipped entirely** (sync records it in `files_skipped`) — the plan doc would likely be absent from the index, not just unretrievable. At hook time, long prompts (pasted diffs/logs) → embed fails → silent passthrough, exactly when context matters most.
- Markdown — the dominant format for convention/meta, including every CLAUDE.md — has **no parser** (whole-file fallback), despite the chunking table presenting `##`-block chunking as current behavior. Arguably the highest-value missing parser; one workstream (markdown parser + size guard/split fallback + embed truncation + sync-time "oversized" warning) fixes indexing coverage and the budget pathology together.

**P5. The doc drift is fleet-wide; repo CLAUDE.md is the worst offender (it steers every session).**
- Repo `CLAUDE.md`: lists `store/writer.rs` / `store/query.rs` (don't exist; it's `sqlite_store.rs`), `parse/openapi|markdown` (don't exist), says hook "returns decorated prompt to stdout" (it emits only the context block).
- `docs/security.md:194–201`: `PreToolUse` example, and falsely claims `vault hook` "exits non-zero" when the API key is missing — directly contradicting the always-exit-0 fail-open contract in `hook::run`.
- `README.md`: repeats PreToolUse + prepend + the caching claim.
- The plan itself: see section A below.

### A. Plan contradicts the implemented system

| # | Plan says | Reality | Where |
|---|-----------|---------|-------|
| A1 | `PreToolUse` hook; "prepends context"; "writes decorated prompt to stdout" | ✅ **fixed 2026-06-14** across all three docs — `UserPromptSubmit`, absolute-path example, and "emits **only** the block; Claude Code appends it" now consistent in README, plan, and CLAUDE.md (matches src/hook/mod.rs:10–23) | plan 43–66, 658–668, 762–763 |
| A2 | `retrieve/hybrid.rs` "absorbed into sqlite_store::hybrid_search — skip Step 11"; Store trait = 5 methods | Reversed by commit 455303d: Store exposes `bm25_search`/`cosine_search` primitives; shared merge in retrieve/hybrid.rs so all backends score identically. Trait also has `get_or_create_project`, `get_document_content_hash`, alpha param | plan 613–617, 637–643, 1025–1028 |
| A3 | Token estimation = "tiktoken cl100k_base, accurate counts" (a "Confirmed" decision) | chars/4 heuristic (`estimate_tokens`, div_ceil). cl100k is OpenAI's tokenizer anyway — never matched Claude | plan 199, 911 vs src/parse/mod.rs |
| A4 | Canonical vault.toml example | **Fails to parse twice**: `[defaults]` has `timeout_ms` but code requires `timeout` (no serde default); `[classifier]` block without `timeout_secs` is a hard error (field required when block present) | plan 681–705 vs src/config.rs:24–57 |
| A5 | CLI: `index add/remove`, `list`, `reindex`, `serve`; diagnose `--budget` | None implemented; sync's `--name`/`--dry-run` undocumented; diagnose has `--alpha` but no `--budget`. `index remove` is load-bearing: project removal needs explicit child deletes (documents FK has no CASCADE) + manual `chunks_vec` cleanup — a manual sqlite3 project delete (2026-06-11) left 16 orphaned vec rows in the live DB | plan 768–790 vs src/main.rs |
| A6 | Binary-structure tree | Missing config.rs, diagnose/, index/{walk,sync,secrets}, stubs, tei/, util/; lists nonexistent mcp/ and parse/{openapi,helm,markdown} (don't itemize in the fix — rewrite the tree) | plan 580–627 |
| A7 | Schema section | Omits the implemented `meta` table (embedding model+dim lock via `verify_or_init_embedding`) and `user_version` migration versioning | plan 169–227 vs src/store/schema.rs:74–77, 130–212 |
| A8 | Hybrid SQL: skip-if-empty for languages only; `c.project_id IN (...)` directly from router "projects" (names!) | All three filters skip-if-empty; projects resolve via name subselect; empty type_names+topics skips the BM25 arm entirely (cosine-only); MATCH = quoted-escaped type_names+topics joined `" OR "` | plan 505–526 vs sqlite_store.rs `build_filter_clause`/`build_match_query` |
| A9 | Re-embed skip + byte-compare collision defense ("Confirmed" behavior) | **Not implemented**: a changed file re-embeds every chunk; `upsert_document` wipes and reinserts; `chunks.content_hash` stored but never compared | plan 389–413 vs src/index/sync.rs, sqlite_store.rs |
| A10 | retrieval_log drives alpha tuning | Zero producers — neither hook nor diagnose calls `log_retrieval` (budget.rs: "once the hook starts writing") | plan 219–227, 965 |
| A11 | "~$0.0002/call **with prompt caching**" | ✅ **fixed 2026-06-14** — Haiku's 4096-token min cacheable prefix verified against current Anthropic docs; ROUTER_SYSTEM (~300 tok) is far below it, so `cache_control` is inert (`cache_creation_input_tokens: 0`, no error) and cost is low only because the prompt is tiny. CLAUDE.md + plan now say so and note caching engages only if the system block grows past ~4096 tokens | plan 12, 106–108, 942; CLAUDE.md 9/33/52/80 |
| A12 | "Gemma 4 MLX model tag — Unconfirmed" tracking item; `router_model = "gemma4-27b-moe"` | Resolved: `/Users/kenobi/git/hub/mlx/gemma-4-31b-bf16` | plan 709, 934, 955 |

### B. Internal contradictions / underspecification

- **B1.** "Hook runtime access: Read-only" (decision, line 904) vs retrieval_log's purpose = collect real hook prompts for replay (line 965). Mutually exclusive as written; code comments already lean toward hook-writes. Whichever wins, note the interplay with **B2**: there is no WAL/busy_timeout anywhere (`schema::open` sets only `foreign_keys`) — hook reads during a long sync write → SQLITE_BUSY → fail-open context loss exactly while refreshing the index; the first hook *write* makes this worse.
- **B3.** Even if written, retrieval_log can't serve replay: `prompt_hash` but no prompt text/embedding, no alpha/budget columns, aggregate counts only. Decide its fate as one decision: bless hook-writes + add columns + WAL in one change, or drop the table and log to a file.
- **B4.** Cross-domain tag selection is first-listed-project-wins (`resolve_context_tag`) — order-sensitive to LLM output ordering; plan never specifies mixed-domain behavior. (Subsumed by P3's single-tag proposal.)
- **B5.** ~~Scoring tradeoff undocumented~~ → **doc DONE 2026-06-14**, calibration follow-up tracked: BM25 normalizes against result-set max, so the top keyword hit always gets 1.0 → final ≥ α (0.6) regardless of absolute relevance, and scores aren't comparable across queries. The behavior + intended evolution is now recorded in the plan's Step-3 scoring section (fixed-divisor / sigmoid / theoretical-max as candidate replacements; RRF rejected as ordinal; **gated on C2**). The *calculation change itself* stays open as a scoring-calibration follow-up, blocked on C2's eval set — raw scores are retained per `Hit` so it needs no migration.
- **B6.** vault.toml is human-edited AND machine-written (`[classifications]` cache; planned domain writeback). Comment-clobbering unaddressed. Writeback isn't implemented yet, so relocating the cache (vault.db or a separate file) is still cheap — record the decision now.
- **B7.** ~~Router system prompt is a copy-paste artifact.~~ *RESOLVED 2026-06-14: the `System prompt:\n  "You are...` wrapper label + unbalanced quote are stripped — `ROUTER_SYSTEM` now opens directly on the instruction; the in-prompt skip example is valid JSON (`{ "skip": true }`), so a faithful echo parses straight to the zero-cost skip path. Both backends still read the byte-identical const, so Haiku's ephemeral cache is unaffected.*
- **B8.** `chunks_vec` has no delete trigger while FTS5 does — deliberate (a vec0-referencing trigger breaks every delete when the extension isn't loaded) but undocumented; fold the rationale + cleanup order into the `index remove` item.
- **B9.** ~~Minor grab-bag: hardcoded "(auto)" diagnose label; empty `migrations/` dir; stale example `[classifications]` block.~~ *RESOLVED/moot 2026-06-14 — three unrelated nits, none open repo work: (1) the hardcoded diagnose label is fixed (commit 4cbc21b — `diagnose` now prints the configured `[router].mode`); (2) no `migrations/` dir exists in the tree (it was an artifact of the machine the review was written on); (3) the `[classifications."~/repos/build-service"]` entry lives only in the machine-local `~/.vault/vault.toml` runtime cache, not the repo, so it is personal-config housekeeping with no codebase change. Tracked here only so the ID isn't mistaken for open work.*

### C. Structural weaknesses (design-level, beyond the top 5)

- **C1. cwd is an unused free signal.** `HookInput` deliberately ignores `cwd` (src/hook/mod.rs:25–32) while `projects.repo_path` exists. cwd→project→domain gives deterministic project bias, deterministic tag resolution, and a degraded-but-useful retrieval path when the router is down. Design after the router story stabilizes.
- **C2. No eval ground truth.** The tuning loop (diagnose + retrieval_log) optimizes retrieval against itself. A small golden-prompt fixture set (prompt → expected chunk labels) as a test would anchor alpha/budget tuning. Do right before tuning, after the markdown parser lands.
- **C3. Trust model is unverifiable by the binary.** The only injection defense is a manually-maintained `~/.claude/CLAUDE.md` instruction vault never checks. A `vault doctor`-style check (instruction present, covers every configured tag, hook registered by absolute path) closes it.

### Demoted as noise (advisor-concurred)
CamelCase/FTS5 tokenization (one-line known-limitation at most — router extracts exact type_names; cosine covers prose). Budget-fill diversity control (premature without C2). Full tree-diff itemization (subsumed by rewrite). Dead `Defaults.timeout` field (mention in doc sync only).

---

## Part 2 — Follow-up: doc-sync checklist (not yet executed)

Doc-only pass; all code changes stay out of scope and land as decisions/tracking items.

### `docs/vault-plan.md`
- [x] Hook contract: UserPromptSubmit everywhere; "emits only the context block, appended by Claude Code"; absolute-path settings.json example. *(done 2026-06-14 — A1; the minor "confirm hook key" plan note still to be removed in a later pass.)*
- [ ] Config: example vault.toml that actually parses (`timeout = 3`, `timeout_secs` in both blocks); document `[router].timeout_secs`/`[classifier].timeout_secs` and the hook-budget implication (P1).
- [ ] Schema: add `meta` table + `verify_or_init_embedding` + user_version note.
- [ ] Retrieval: fix the SQL to match `build_filter_clause` semantics (skip-if-empty ×3, name subselect, BM25-arm skip, MATCH construction); ~~add the B5 scoring-tradeoff paragraph~~ *(done 2026-06-14)*.
- [ ] Chunking table: mark markdown/openapi rows "planned — whole-file fallback today".
- [ ] Indexing: mark the re-embed-skip section "designed, not yet implemented" (A9); add chunks_vec cleanup rationale (B8).
- [ ] Binary structure: rewrite the tree to match `src/`; reverse the Step-11 "absorbed" notes; correct the Store trait listing.
- [ ] CLI: status-mark unimplemented commands; add `--name`/`--dry-run`; note diagnose flag reality.
- [ ] Decisions table: token estimation → chars/4 (with revisit note); prompt-caching → correct mechanism (A11); hybrid placement → extracted (A2); latency table → real 31B numbers.
- [ ] Tracking items: resolve A12; add items for P1 (per-role model+timeout, hook clamp, latency-aware fallback), ~~P2 (name normalization, drop-unknown languages)~~ *(resolved 2026-06-14)*, P3 (single-tag+domain-attribute decision, block grouping vs contract text, doctor check), P4 (markdown parser priority, size guard, embed truncation), B1/B3 (retrieval_log fate + WAL as one decision), B6 (cache relocation), ~~B7 (router prompt cleanup)~~ *(resolved 2026-06-14)*, C1, C2.

### `CLAUDE.md` (repo)
- Key modules table:
  - [x] Store rows aligned to the trait-based, SQLite-only backend (`writer.rs`/`query.rs` → `traits.rs` + `schema.rs` + `sqlite_store.rs` + `postgresql_store.rs` stub + `types.rs`). *(done 2026-06-14 — the doc had referenced the pre-refactor SQLite-only layout; `PostgresStore` is a `todo!()` placeholder, not exported.)*
  - [ ] Add the missing module rows — sub-items: [ ] `config.rs`; [ ] `diagnose/mod.rs`; [ ] `index/walk.rs`; [ ] `index/sync.rs`; [ ] `index/secrets.rs`; [ ] `util/{json,path,probe,fs}.rs`.
  - [ ] Implementation Order section still names `store/writer.rs` (Step 2) / `store/query.rs` (Step 3) → `sqlite_store.rs`; keep the `impl-status` skill's 14-step list in sync.
- [x] ~~Hook wording: "returns decorated prompt" → emits block~~ *(done 2026-06-14 — A1)*. (Parser-list truth split into its own item below.)
- [ ] **Parser list — doc claims parsers that don't exist** *(separate tracking item)*: the module table + Architecture prose list `openapi`/`markdown` parsers, but `src/parse/` has only `proto.rs`/`go_source.rs`/`rust_source.rs`. Correct the list and mark markdown/openapi "planned" — ties to **P4** (markdown is the highest-value missing parser) and the chunking-table "planned — whole-file fallback today" caveat.
- [x] Verify the "30s per-call timeout" claim against current Claude Code docs. *(verified 2026-06-14: CLAUDE.md's 30s is **correct** — `UserPromptSubmit` is special-cased to a 30s default per code.claude.com/docs/en/hooks.md; the advisor's "60s" was wrong. Corrected the stray "60s default hook kill window" in P1 above to 30s instead.)*

### `docs/security.md`
- [x] Fix the `PreToolUse` example + the false "exits non-zero" claim (contradicts fail-open). *(resolved 2026-06-14: missing-key now described as fail-open exit-0 + `hook.log`/breadcrumb, surfacing loudly in `diagnose`/`sync`; example event `PreToolUse` → `UserPromptSubmit`.)*

### `README.md`
- [x] Same hook-event / prepend / caching family of fixes. *(done 2026-06-14 — A1 + A11: `UserPromptSubmit`, emits-only-the-block/appended wording, absolute-path example, fail-open empty-stdout phrasing, and the inert-caching cost note.)*

### Verification (when executing)
- `cargo test` stays green (docs only).
- Validate the new example vault.toml parses (`toml::from_str::<Config>` against src/config.rs required fields — mirror the `indexer_section_optional_for_back_compat` fixture).
- Confirm the Claude Code hook-timeout figure from current docs before writing it into CLAUDE.md.
- Re-read the four edited docs for internal consistency (hook event, tag story, caching claim told identically in all four).
