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

### P3. The injection-framing contract is unsatisfied.

`~/.claude/settings.json` has a `UserPromptSubmit` entry; `~/.claude/CLAUDE.md`
does not exist. Vault deliberately never sanitises chunk text, so that file is the
whole defence — `docs/security.md` lists it as defence #1 and `CLAUDE.md` states it
"handles this for Claude".

Nothing has actually been injected on this machine yet: all 13 `hook.log` records
are failures (`router-build` ×7 for a missing key, `stdin` ×3, `config` ×1, 2 skips)
with no `router_ms` sample. The exposure opens when the router starts working —
i.e. when P1 is fixed.

- **Coverage drifts by construction.** One `## {domain}-context` section per domain
  means each new domain is uncovered until a second file is edited, and the fallback
  `vault-context` tag needs its own section. Decide: one constant tag with a domain
  attribute (`<vault-context domain="software">`), written once.
- **The contract describes output vault does not produce.** It says the block is
  "grouped by project"; `Context::render_block` (`src/retrieve/mod.rs:107`) emits a
  flat `## label [doc_type]` list — no grouping, no language. Implement grouping or
  fix the text.

Related — **B4**: cross-domain tag selection is first-assigned-project-wins via
`Store::resolve_domain`, so it is sensitive to router output ordering, and mixed
domains are unspecified. Subsumed by the single-tag proposal.

### P4. Long prompts break query-time embedding.

`embed_query` sends the prompt untruncated, so a pasted diff or log fails the embed
and falls through to silent passthrough — exactly when context would help most.
(Index-side windowing is done.)

---

## A. Plan contradicts the implemented system

| # | Plan says | Reality |
|---|-----------|---------|
| A5 | `index add/remove`, `list`, `reindex`, `serve` exist | None do. **`index remove` is the load-bearing one**: the `documents` FK has no CASCADE, so it needs explicit child deletes plus a `chunks_vec` sweep (fold in **B8**) |
| A5b | — | **README was never reconciled.** `vault-plan.md` was (2026-06-21); README has no planned-not-implemented section, and its examples omit `--name` for sync and `--top` plus all five plan-override flags for diagnose |
| A9 | Re-embed skip + byte-compare collision defence | Per-chunk `content_hash` is stored but never compared, so a one-line edit re-embeds every chunk in the file. (The unchanged-*file* gate does work.) |
| A10 | `retrieval_log` drives alpha tuning | Zero producers. `Store::log_retrieval` exists and `SqliteStore` implements it; only test stubs and one unit test call it |

---

## B. Internal contradictions / underspecification

- **B1 / B3 — `retrieval_log`'s fate is undecided.** "Hook runtime access:
  read-only" (plan line 904) contradicts its stated purpose of collecting hook
  prompts for replay (line 965). It could not serve replay anyway: `prompt_hash`
  but no prompt text or embedding, no alpha or budget columns, aggregate counts
  only. One decision — add the columns and write to it, or drop the table and log
  to a file.

- **B5 — scoring calibration.** BM25 normalises against the result-set max, so the
  top keyword hit always scores 1.0 and every final score is ≥ α; scores are not
  comparable across queries. Candidate replacements are already written up in the
  plan's Step-3 section. The calculation change is **gated on C2**; raw scores are
  retained per `Hit`, so no migration is needed.

- **B8 — `chunks_vec` has no delete trigger while FTS5 does.** Deliberate — a
  vec0-referencing trigger breaks every delete when the extension is not loaded —
  but undocumented. Write it down as part of A5.

---

## C. Structural weaknesses

- **C1. `cwd` is an unused free signal.** `HookInput` ignores it while
  `projects.repo_path` exists. cwd → project → domain would give deterministic
  project bias, deterministic tag resolution, and a degraded-but-useful path when
  the router is down. Design after P1 settles.

- **C2. No eval ground truth.** The tuning loop optimises retrieval against itself.
  A golden-prompt fixture set (prompt → expected chunk labels) would anchor alpha
  and budget tuning. Unblocked; blocks B5.

- **C3. The trust model is unverifiable by the binary.** The only injection defence
  is a hand-maintained file vault never checks — and which is currently missing
  (P3). A `vault doctor` check closes it: instruction present, covers every tag
  vault can emit, hook registered by absolute path.

---

## D. Findings from the code, not from the plan

**D1** and **D2** are left over from the `lib-cli-split` code review; **D3** and
**D4** were found on 2026-08-22 starting TEI for the test suite.

- **D1. `Vault::open` builds the router before the store** (`src/vault.rs:164`):
  `planner: QueryPlanner::new(config)?` runs before `store: VaultStore::open(...)`,
  so a consumer that only wants to index gets `VaultError::RouterBuild` for a
  backend `sync` never calls. `VaultStore::open` is the undocumented workaround.
  The library/CLI split exists for a service or MCP consumer, and "index a repo" is
  the first thing one does. Open the store first and build the planner lazily, or
  document `VaultStore` as the indexing entry point.

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

- **D4. Reachability probes are TCP-only, so "reachable" precedes "serving".**
  `util::probe::port_reachable` is a TCP connect; Docker publishes the port at
  container start, ~28s before TEI binds its HTTP server. Measured: `tei start` and
  `tei status` both reported reachable while `/health`, `/info` and `/` all refused
  connections. False green for TEI (a following `index sync` hard-errors); for MLX
  it is P1's probe bullet. Fix: probe the health endpoint — `tei_reachable` already
  takes the URL, so it is a change of method, not signature.

---

## Loose ends

- **`e2b7c12` does not compile standalone**, so `git bisect` dies on it — `lib.rs`
  declared `pub mod vault;` while `src/vault.rs` was untracked, and `8b1805e` fixed
  it forward. Both pushed, so correcting it means rewriting shared history. Use
  `git bisect skip` through that range.

---

## Doc-sync checklist

- [ ] `vault-plan.md` indexing section: note the per-chunk incremental skip is open
      (A9); add the `chunks_vec` cleanup rationale (B8).
- [ ] `vault-plan.md:132` latency table still claims Gemma is "~100–300ms" and
      Haiku "~400–800ms (still under 3s hook timeout)". The measured local figure
      is ~15s/call, which is P1. The table is the source of the 3s invariant.
- [ ] `vault-plan.md` tracking items: P1, P3, P4, B1/B3, C1, C2.
- [ ] `vault-plan.md` on `vault tei start|status`: neither reports on the service,
      only on a TCP socket (D4), and `start` does not verify the child lived (D3).
- [ ] README: the A5b gaps above.
