# Code Review Findings: `lib-cli-split` vs `main`

### 1. Self-tripping audit script on documentation references
- **File:** `scripts/supply-chain-audit.sh` (lines 70–76, 96)
- **Issue:** Section 2 (`git log -S`) and Section 3 (`git cat-file`) search commit history and raw blobs for `BAD_CRATES` (which includes `arrayref` and `proc-macro1`). Because `CLAUDE.md` (line 34), `.github/workflows/ci.yml` (line 16), and `docs/lib-split-plan.md` reference the incident by name in comments and documentation, running `./scripts/supply-chain-audit.sh` triggers false positives on the repo's own docs and always fails with exit code 1 (`RESULT: NEEDS REVIEW`).

### 2. Mislabeled `SkipReason` on empty/whitespace prompts in `retrieve_with`
- **File:** `src/hook/mod.rs` (lines 191–200) vs `src/vault.rs` (lines 69–74, 198–200)
- **Issue:** `QueryPlanner::route` treats empty or whitespace-only prompts as a skip and returns `Ok(None)`. In `src/hook/mod.rs`, `retrieve_with` unconditionally maps `routed? == None` to `Ok(Retrieval::Skip(SkipReason::RouterSkip))`. If `retrieve_with` or `pipeline_with` is called directly with empty/whitespace input, it mislabels the outcome as `RouterSkip` instead of `EmptyPrompt` (which `Vault::retrieve` explicitly guards and returns).

### 3. Potential integer overflow panic under large token budget
- **File:** `src/retrieve/budget.rs` (lines 35–38)
- **Issue:** Line 35 bounds checks using `tokens_used.saturating_add(hit.token_est) > token_budget`. However, line 38 performs unchecked addition: `tokens_used += hit.token_est;`. If `select_within_budget` is called with a large budget (e.g., `u32::MAX`), the condition passes, but line 38 panics on overflow in debug mode instead of saturating.

### 4. Silent test skips in CI integration suite
- **File:** `tests/pipeline.rs` (lines 178–185, 198–206)
- **Issue:** `a_consumer_gets_empty_prompt_rather_than_a_billable_round_trip` and `a_blank_prompt_never_reaches_the_router` attempt to construct `Vault::open(&config)` and `QueryPlanner::new(&config)` with default auto-mode configuration (probing `localhost:8080` then requiring `ANTHROPIC_API_KEY`). In environments without MLX or API keys (such as standard CI), the `let Ok(...) = ... else { return; };` branches silently return early, bypassing the assertions without running them.

### 5. `apply_pragmas` fails open on WAL errors contrary to specification
- **File:** `src/store/schema.rs` (lines 140–146)
- **Issue:** The doc comments state that inability to set WAL mode (e.g. on read-only filesystems or unsupported network filesystems) should keep working in whatever fallback mode SQLite selects. However, line 144 executes `PRAGMA journal_mode=WAL` with `?` error propagation (`.map_err(|e| StoreError::Backend(e.to_string()))?`), causing `schema::open()` to fail immediately on any filesystem error during WAL setup instead of continuing.
