# Open findings

Started as a review of `docs/vault-plan.md` (2026-06-11, against commit `6b08f9d`);
now tracks open work from any source. Sections A–C keep their original IDs, so a
reference to "A9" still resolves and a gap means that item was fixed. Section D is
findings about the code as built rather than about the plan drifting from it.

**Open work only.** Every claim below was re-verified against the tree on
2026-08-22, and resolution notes were stripped once verified — a document of what
is left should not also be a changelog. The full original review, with each
resolved finding and its commit, is recoverable:

```
git show af1aeab:docs/plan-review-2026-06-11.md
```

12 of the original 28 items have been fixed.

---

## Priority

### P1. The router cannot meet the hook's latency contract.

The plan promises a 3-second timeout as an invariant and Gemma latency of
"~100–300ms". `gemma-4-31b-bf16` runs ~15s/call warm. At the 3s default every hook
call times out to silent passthrough; raised to 120s the hook *succeeds* at a ~15s
tax on every prompt — half of Claude Code's 30s `UserPromptSubmit` budget, session
visibly stalled, cancelled to passthrough past 30s.

Three structural problems keep this from being only a model-choice issue:

- **`[router].timeout` governs the hook *and* `vault diagnose`.** Both build their
  router through `build_router` (`src/retrieve/router/mod.rs:167`) and every
  backend reads `config.router_timeout()`; `diagnose::Args` has no override.
  Raising it to make an interactive diagnose usable silently rewrites the hook's
  contract.
- **`[mlx].router_model` is one key for both roles.** Routing needs small and fast,
  classification tolerates big and slow, and the local model name is shared.
  *(Per-role `[router]`/`[classifier]` `timeout` and `model` keys do exist and are
  used independently — that half is done. Only the MLX model name is shared.)*
- **The probe cannot detect "reachable but slow"** — see **D4**, where the same
  probe was measured going green 28s early.

Candidates: a dedicated small routing model (or Haiku for the hook), a per-role MLX
model key, a diagnose-side timeout override, a hook-side hard clamp.

### ~~P3. The injection-framing contract is unsatisfied.~~ — CLOSED 2026-08-22

As originally written: `~/.claude/CLAUDE.md` did not exist. Vault deliberately
never sanitises chunk text, so that file is the whole defence — `docs/security.md`
lists it as defence #1 and `CLAUDE.md` states it "handles this for Claude".

**Correction (2026-08-22).** An earlier revision of this section claimed
`~/.claude/settings.json` holds a vault `UserPromptSubmit` entry. It does not —
verified by parsing the file: one matcher group, and its handler is not vault's.
The claim came from a `grep -c UserPromptSubmit` that counted the event name.
Nothing has ever been injected on this machine; all retrieval to date has run
through `vault diagnose`. This does not reopen P3 — the framing file exists and
is correct — but it does mean the hook-registration check in **C3** currently
has nothing to pass, and that registering the hook is a prerequisite for P1's
latency problem to be observable in practice.

Nothing has actually been injected on this machine yet: all 13 `hook.log` records
are failures (`router-build` ×7 for a missing key, `stdin` ×3, `config` ×1, 2 skips)
with no `router_ms` sample. The exposure opens when the router starts working —
i.e. when P1 is fixed.

Two of this finding's three parts closed on 2026-08-22:

- ~~**Coverage drifts by construction.**~~ The tag is now constant and the domain
  is an attribute — `<vault-context domain="software">` — so one section covers
  every domain, including ones that do not exist yet, and the unassigned case
  (no attribute) needs no special mention.
- ~~**The contract describes output vault does not produce.**~~ The template in
  `vault-plan.md` no longer claims the block is "grouped by project"; it describes
  the flat `## label [doc_type]` list `render_block` actually emits. It also no
  longer says the block appears "at the start of a message" — hook stdout is
  appended, not prepended.

~~**What is left is writing the section.**~~ Written 2026-08-22. `~/.claude/CLAUDE.md`
now exists (0600) holding a single `## Vault Context` section, byte-identical to the
template in `vault-plan.md` under "CLAUDE.md Strategy" — checked with `diff`, so the
doc and the deployed file cannot silently disagree about what was installed.

The defence is in place. **What is not in place is anything that keeps it there** —
see C3. One concrete hole it should close: the section names `<vault-context>`
literally, while `defaults.context_tag` is configurable, so changing that key in
`vault.toml` silently stops the instruction matching the tag being emitted.

Related — **B4**: which domain wins is first-assigned-project-wins via
`Store::resolve_domain`, so it is sensitive to router output ordering, and mixed
domains are unspecified. Less severe now that a wrong domain attribute still
lands inside a correctly-framed block, but still unspecified.

### P4. Long prompts silently truncate query-time embedding.

`embed_query` is `self.embed(&format!("search_query: {text}"))` with no length
guard. The index side has one (`whole_file_chunks` windows at
`MAX_FALLBACK_CHUNK_TOKENS`); the query side has none.

**Correction (2026-08-23).** This finding said the embed *fails* and falls
through to silent passthrough. Measured against the live server, it does not:
TEI 1.9.3 reports `auto_truncate: true`, so an over-long prompt returns a
perfectly good vector computed from **only the first 8192 tokens**. Nothing
errors and nothing is reported. That is worse in one respect — a failure is at
least visible in `hook.log` as `embed-query`, whereas this produces confident
retrieval against the head of a pasted diff while the actual question, if it sat
at the bottom, was never embedded. It is also deployment-dependent: a TEI
started with auto-truncate off fails as originally described.

Design notes for the fix:

- **Head-only truncation is wrong here.** The index truncates head-only so the
  per-chunk secret scan cannot be bisected. A query yields *one* vector, so
  truncating is not windowing — it is choosing what the query means. A prompt
  ending in "why does this fail?" after 400 lines of stack trace loses the
  question. Head+tail is the better default.
- **The ceiling should be read, not hardcoded.** `/info` reports
  `max_input_length` (8192 here). `verify_against_server` already calls the
  server, so the handshake can learn it.
- **The router has the same exposure.** `build_user_prompt` ends with
  `out.push_str(prompt)`, untruncated. Haiku's 200k context will not error, but
  the whole paste is billed on every long prompt, and a local Gemma with a
  smaller window would fail outright. The same guard belongs there.
- **The BM25 arm is unaffected** — it is built from the router's `type_names`
  and `topics`, not the raw prompt — so a truncated query still gets full
  keyword retrieval. That lowers the stakes on getting the split perfect.

---

## A. Code gaps the plan used to paper over

The documentation half of every A finding was fixed on 2026-08-22 — `vault-plan.md`
and README now describe what the code does. **A5b closed entirely** (it was
doc-only) and **A10 merged into B1/B3**, since what is left of it is the
`retrieval_log` decision. What remains here is the code these findings were
pointing at.

| # | Gap |
|---|-----|
| A5 | **`vault index remove` does not exist.** Load-bearing for cleanup: the `documents` FK has no CASCADE, so it needs explicit child deletes plus a `chunks_vec` sweep. Fold in **B8** while writing it |
| A9 | **Per-chunk `content_hash` is written and never read.** A one-line edit re-embeds every chunk in that file — `upsert_document` deletes and re-inserts the whole set unconditionally. The design for the skip, including the byte-compare collision defence, is recorded in `vault-plan.md` under a heading now marked planned-not-implemented. (The unchanged-*file* gate does work.) |

---

## B. Internal contradictions / underspecification

- **B1 / B3 / A10 — `retrieval_log`'s fate is undecided.** "Hook runtime access:
  read-only" (plan line 904) contradicts its stated purpose of collecting hook
  prompts for replay (line 965). It could not serve replay anyway: `prompt_hash`
  but no prompt text or embedding, no alpha or budget columns, aggregate counts
  only. One decision — add the columns and write to it, or drop the table and log
  to a file.

- **B5 — scoring calibration.** Two defects, not one. *Across* queries, BM25
  normalises against the result-set max, so the top keyword hit always scores
  1.0 and scores are not comparable between prompts. *Within* a query — measured
  2026-08-23, and the one that actually bites — the arms are wildly
  incommensurate: `bm25_normalized` spans 0–1 while `cosine` is used **raw**, and
  its observed range on this corpus is 0.619–0.709. At α=0.3 that is 0.300 of
  BM25 variation against 0.063 of cosine, so the documented "60/40 blend" is
  nearer 92/8, and the arms only balance around α≈0.08.

  Now measurable via C2. What the fixture set has already established:

  - **α=0.1 is the optimum** (first-rank total 7, vs 11 at the 0.6 default), and
    **α=0.0 collapses to 20** — three cases lose their answer entirely. The
    keyword arm is load-bearing, just weighted ~6× too heavily.
  - **RRF is not the fix.** Recomputed offline over real scores, k=60 was
    *identical* to the current linear blend: with only 8 of 46 candidates
    carrying a BM25 rank it degenerates to cosine-plus-a-nudge. It had been
    recommended repeatedly on theory before being measured.
  - **Min-max normalising both arms** is the promising candidate — it moved
    `fn build_router` from #9 to #1 on the tuning prompt — but it was derived
    from that *one* prompt, which is the only fixture with headroom. Four of
    five cases already sit at #1, so it can help one and risk four. Re-run
    `alpha_sweep` against any change before believing it.
  - Min-max interacts with `min_score`: once both arms are stretched to 0–1
    per query, `0.15` stops being an absolute quality bar, since a uniformly
    irrelevant result set is stretched to fill the range.

  Raw scores are retained per `Hit`, so no migration is needed.

- **B8 — `chunks_vec` has no delete trigger while FTS5 does.** Deliberate — a
  vec0-referencing trigger breaks every delete when the extension is not loaded —
  but undocumented. Write it down as part of A5.

---

## C. Structural weaknesses

- ~~**C1. `cwd` is an unused free signal.**~~ — **mostly closed 2026-08-22.**
  `HookInput` now carries `cwd` (optional, so a client that omits it still
  parses), `Store::project_for_path` resolves it to an indexed project by
  longest-prefix match at a component boundary, and `QueryPlan::prefer_project`
  moves that project to the front. The bias is additive — the router's projects
  survive, since a prompt asked from one repo about a sibling service is
  ordinary — and cwd-first makes `resolve_domain` deterministic rather than
  dependent on router output ordering, which closes the practical half of
  **B4**. Resolution failures are swallowed: a hint must not fail a retrieval
  the router and embedder already paid for.

  The note to "design after P1 settles" was wrong and is dropped: P1 is about
  router *latency*, and the whole value of cwd is that it needs no router.

  **What is left** is the third clause — *"a degraded-but-useful path when the
  router is down"*. Today a router failure is still total passthrough. Falling
  back to "everything from this project, cosine-only" would change the hook's
  failure semantics and needs its own design pass; it also only helps when the
  router is down *and* TEI is up.

- ~~**C2. No eval ground truth.**~~ — **closed 2026-08-23.**
  `src/retrieve/golden.toml` holds the fixture set (9 corpus files, 5 cases) and
  `src/retrieve/eval.rs` the harness: real files, production parsers, real TEI
  embeddings, no classifier (`doc_type`/`language` come from an extension rule,
  so a run costs nothing and cannot drift with a model). Cases assert on chunk
  **labels**, never content, so ordinary refactors do not break them.

  Three entry points: `golden_prompts_retrieve_their_expected_chunks` is the
  gate, `alpha_sweep` is the tuning loop, and
  `fixtures_are_well_formed_and_the_corpus_exists` runs in normal CI without TEI
  so a deleted corpus path fails immediately. That last one earned its keep on
  the first run by catching a missing corpus file.

  It unblocked **B5** and immediately corrected two conclusions reached from a
  single prompt — see there.

  Five cases is a floor. The suite should grow whenever a retrieval bug is
  found, the same way a regression test does.

- **C3. The trust model is unverifiable by the binary.** The injection defence is a
  hand-maintained file vault never checks. It now exists (P3), which moves this from
  "the defence is missing and nothing says so" to "the defence is present and nothing
  keeps it that way". A `vault doctor` check closes it:

  - `~/.claude/CLAUDE.md` exists and has a `## Vault Context` section;
  - that section names the tag actually being emitted — `defaults.context_tag` is
    configurable, so a changed key silently orphans the instruction;
  - the hook is registered in `settings.json`, by absolute path, in the nested
    matcher-group shape rather than the flat one that does not load.

---

## D. Findings from the code, not from the plan

**D1** and **D2** are left over from the `lib-cli-split` code review; **D3** and
**D4** were found on 2026-08-22 starting TEI for the test suite, and both were
reproduced live on 2026-08-23. **D5** was found the same day, by hitting it.

- **D1. `Vault::open` still builds a router an indexing-only consumer never
  calls.** *Half-fixed 2026-08-22.* The ordering half is done — router grounding
  made the planner depend on the store, so `Vault::open` now opens the store
  first and a store failure is reported as one instead of being masked by a
  `RouterBuild` error. What remains is that the planner is still built
  **eagerly**, so a consumer that only wants to index still pays for router
  construction and still gets `VaultError::RouterBuild` when the backend it
  never uses is misconfigured. `VaultStore::open` remains the undocumented
  workaround. Build the planner lazily, or document `VaultStore` as the
  indexing entry point.

- **D2. Temp-directory helpers are duplicated** — 15 `std::env::temp_dir()` sites
  across 10 files, up two during the review pass, which is the argument for fixing
  it. Most carry a `Drop` guard; `schema.rs::temp_db_path` returns a bare
  `PathBuf`, so a panicking test leaks the file and nothing ever removes the
  `-wal`/`-shm` sidecars holding whatever that test indexed.

- **D3. `vault tei start` reports success for a child that has already died.**
  `spawn()` succeeds when the *launcher* starts — for the Docker launcher that is
  the client, not the server. `start` writes the pidfile and prints
  `Started TEI (pid N)` without checking (`src/tei/launcher.rs:90-104`); the
  readiness loop then prints *"TEI process is running but the endpoint is not
  answering yet — first run can take minutes"* and returns `Ok(())`. Every clause is
  false when the client died. Observed with a leftover `Exited (0)` container
  `--rm` had not cleaned up; the real error was only in `~/.vault/tei.log`.
  Fix: check liveness before claiming success, and tail the log when it fails.

  **`vault tei status` has the same blind spot** (observed 2026-08-23): it
  printed `pidfile: ~/.vault/tei.pid (pid 33619)` for a process that did not
  exist and a container that was gone. It reports the pidfile's *contents*, not
  whether the pid is alive. Whatever liveness check `start` gains, `status`
  needs too.

- **D4. Reachability probes are TCP-only, so "reachable" precedes "serving".**
  `util::probe::port_reachable` is a TCP connect; Docker publishes the port at
  container start, ~28s before TEI binds its HTTP server. Measured: `tei start` and
  `tei status` both reported reachable while `/health`, `/info` and `/` all refused
  connections. False green for TEI (a following `index sync` hard-errors); for MLX
  it is P1's probe bullet. Fix: probe the health endpoint — `tei_reachable` already
  takes the URL, so it is a change of method, not signature.

  **Reproduced 2026-08-23.** `vault tei start` printed *"TEI is reachable on
  http://localhost:8081"* while `/info` still refused the connection; it took a
  further poll loop before the HTTP server answered. The tool declared healthy a
  server that could not have served a single embed.

---

- **D5. Parser behaviour is unversioned, so a parser change yields a silently
  stale index.** The unchanged-file gate compares a file's hash against
  `documents.content_hash`. That is the right key for *content* drift and the
  wrong one for *parser* drift: when the rust parser stopped requiring `pub`
  (2026-08-23), every already-indexed file still hashed the same, so `index
  sync` reported `Unchanged: 67` and kept chunks the current parser would never
  produce. The index was left in a mix — three reparsed files chunked per
  private item, sixty-seven still holding pub-only chunks — with nothing on
  screen indicating it.

  The recovery is `rm ~/.vault/vault.db*` plus a full re-sync, which costs a
  full reclassification pass against the remote backend. Worse, the failure is
  silent: retrieval keeps working, just against chunk boundaries that no longer
  match the code.

  The embedding side already solved this — `meta` records `(embedding_model,
  embedding_dim)` and every open verifies it, so a model change is a loud error
  with an explicit remedy. Parsers have no equivalent. Fix: a `parser_version`
  in `meta`, bumped when any parser's chunk boundaries change, that invalidates
  the `content_hash` gate for affected languages — or, at minimum, refuses the
  open with the same "full re-index needed" error the dim lock produces.

## Loose ends

- **`e2b7c12` does not compile standalone**, so `git bisect` dies on it — `lib.rs`
  declared `pub mod vault;` while `src/vault.rs` was untracked, and `8b1805e` fixed
  it forward. Both pushed, so correcting it means rewriting shared history. Use
  `git bisect skip` through that range.

---

## Doc-sync checklist

- [ ] `vault-plan.md` indexing section: add the `chunks_vec` delete-trigger
      rationale (B8) — the A5/A9 reconciliations are done.
- [ ] `vault-plan.md` tracking items: P1, P3, P4, B1/B3, C1, C2.
- [ ] `vault-plan.md` chunking section: rust chunks all visibilities now, not
      just exported symbols, and a parser that extracts nothing takes the
      whole-file fallback rather than skipping the file. `CLAUDE.md` is updated;
      the plan is not.
- [ ] `docs/embeddings.md` / `runbook.md`: TEI reports `auto_truncate: true`, so
      an over-long input is silently truncated rather than rejected (P4).
- [ ] `vault-plan.md` on `vault tei start|status`: neither reports on the service,
      only on a TCP socket (D4), and `start` does not verify the child lived (D3).
