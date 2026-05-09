---
name: impl-status
description: Use this skill at the start of a vault development session, or when the user asks "where are we", "what's next", "what step am I on", or wants a progress check. Reads the current source tree against the 14-step implementation order in docs/olympus-vault-plan.md and reports which steps are complete, which is in progress, and what the next blocker is. Particularly useful because Step 0 (embedding dimension confirmation) is a hard prerequisite for Step 1 and the rest of the pipeline depends on lower layers being in place.
---

# Implementation Status

The `vault` project follows a strict bottom-up implementation order with hard dependencies. This skill reports the current position in that order so a session can pick up cleanly.

## When to invoke

- Start of a development session ("where are we?", "what's next?")
- After a hiatus where state may have drifted
- Before starting a new step, to confirm prerequisites are met

## The 14 steps

```
Step 0   Confirm TEI reachable + 768 dims         (curl localhost:8081, write vault.toml [embeddings])
Step 1   src/store/schema.rs                       (DDL, migration runner)
Step 2   src/store/writer.rs                       (upsert project/document/chunk/vec)
Step 3   src/store/query.rs                        (FTS5 + vec hybrid, score merge, budget)
Step 4   vault diagnose                            (CLI cmd to validate retrieval)
Step 5   src/parse/proto.rs
Step 6   src/parse/go_source.rs
Step 7   src/parse/rust_source.rs
Step 8   src/embed/tei.rs                          (HTTP client against TEI /embeddings)
Step 9   src/index/classify/{mod,gemma,haiku}.rs   (Classifier trait + Gemma + Haiku impls)
Step 10  src/retrieve/router/{mod,gemma,haiku}.rs  (Router trait + Gemma + Haiku impls)
Step 11  src/retrieve/hybrid.rs                    (score merge)
Step 12  src/retrieve/budget.rs                    (token budget selection)
Step 13  src/hook/mod.rs                           (full pipeline wired)
Step 14  First-run UX                              (domain + classification prompts)
```

## Procedure

1. **Check Step 0 status.**
   - Open `vault.toml` if it exists at project root or `~/.vault/vault.toml`. Look for an `[embeddings]` section with `endpoint`, `model`, and `dims = 768` (for nomic-embed-text-v1.5).
   - If `[embeddings]` is missing, or `dims` is absent/0 — Step 0 is **not done**, and Step 1 must not start.
   - Bonus check: `curl <endpoint>/health` (or `/embeddings` with a test payload) confirms TEI is actually reachable.

2. **Walk the source tree.** For each step, check whether the target file exists and has non-trivial content (more than scaffolding):
   - File missing → step not started
   - File exists but stub/empty/`todo!()` → step in progress
   - File exists with implementation + tests → step likely complete

3. **Check dependencies.** Steps 5–7 (parsers) depend on Step 4 (`vault diagnose`) being usable. Step 13 (hook) depends on Steps 1–12. Flag any out-of-order work.

4. **Surface blockers.** Common ones:
   - Step 0 not resolved before Step 1 began → schema may need migration
   - `Cargo.toml` missing critical deps (`rusqlite` with `bundled`, `sqlite-vec`, `reqwest` for TEI HTTP, `serde`, `serde_json`, `tiktoken-rs`, `tokio`)
   - TEI not running locally when index sync is attempted
   - Tests failing on `cargo test`

## Output format

```
Implementation Status

Step 0   Embedding stack            ⚠ NOT CONFIRMED — vault.toml missing [embeddings] or TEI unreachable
Step 1   store/schema.rs            ✗ not started (BLOCKED on Step 0)
Step 2   store/writer.rs            ✗ not started
...
Step 14  First-run UX               ✗ not started

Current position: Step 0 (blocking)
Next action: Stand up TEI and confirm 768-dim output for nomic-embed-text-v1.5.
  $ curl http://localhost:8081/embeddings \
      -H "Content-Type: application/json" \
      -d '{"input": "search_document: test"}'
  Then write [embeddings] block to vault.toml:
      [embeddings]
      endpoint = "http://localhost:8081"
      model    = "nomic-ai/nomic-embed-text-v1.5"
      dims     = 768

Open Cargo.toml dependencies needed before Step 1: rusqlite (bundled), sqlite-vec,
reqwest (for TEI HTTP), tiktoken-rs, serde, serde_json, tokio (for hook IO).
```

Be honest about partial state. A file with `fn upsert_chunk() { todo!() }` is in progress, not complete. Read the actual source — don't infer from filenames alone.
