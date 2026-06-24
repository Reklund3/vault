---
name: schema-check
description: Use this skill before writing or modifying SQL schema files in src/store/schema.rs (or anywhere SQLite DDL is defined for the vault project). Verifies that implemented schema matches the canonical spec in docs/vault-plan.md. Critical because chunks_vec FLOAT[N] dimension is locked at creation and FTS5 triggers must stay in sync with the chunks table. Triggers when the user is creating, editing, or reviewing CREATE TABLE / CREATE VIRTUAL TABLE / CREATE TRIGGER statements for the vault store.
---

# Schema Check

The `vault` project's SQLite schema is locked once the database is created. Vector dimensions on `chunks_vec`, FTS5 sync triggers, and `doc_type` / `language` CHECK constraints cannot change without a migration. This skill verifies a schema implementation against the canonical spec.

## When to invoke

- User is writing `src/store/schema.rs` or any SQL DDL for the vault store
- User asks to "check the schema", "verify schema", or similar
- User is about to commit changes that touch SQL definitions in the store layer

## Procedure

1. **Read the spec.** Open `docs/vault-plan.md` and locate the `## Storage Schema` section. The canonical tables are: `projects`, `documents`, `chunks`, `chunks_fts` (virtual, FTS5), `chunks_vec` (virtual, vec0), `retrieval_log`. Also note the three FTS5 sync triggers: `chunks_ai`, `chunks_au`, `chunks_ad`.

2. **Read the implementation.** Open the SQL file(s) under review.

3. **Compare against this checklist:**
   - `projects`: `id` PK, `name` UNIQUE NOT NULL, `repo_path`, `created_at` NOT NULL
   - `documents`: `doc_type` CHECK in `('contract','plan','convention','meta')`, `content_hash` NOT NULL (sha256), `UNIQUE(project_id, source_path)`
   - `chunks`: `language` CHECK in `('go','rust','scala','proto','openapi','helm','markdown','unknown')`, `project_id` and `doc_type` denormalized, `content_hash` NOT NULL (sha256 of chunk body, distinct from `documents.content_hash`), `token_est` NOT NULL, `ON DELETE CASCADE` from documents, `UNIQUE(document_id, label)` for sync-time prune diffing
   - `chunks_fts`: `USING fts5(label, content, content='chunks', content_rowid='id', tokenize='porter unicode61')` — must be content-table linked, must use porter stemming
   - `chunks_vec`: `USING vec0(chunk_id INTEGER PRIMARY KEY, embedding FLOAT[N])` — N must match `[embeddings].dims` in `vault.toml` (currently 768 for `nomic-ai/nomic-embed-text-v1.5`). **Flag if N is unset or doesn't match.**
   - Triggers: `chunks_ai` (insert), `chunks_au` (update — must delete then insert in FTS5), `chunks_ad` (delete) all present and correct
   - `retrieval_log` present with `prompt_hash`, `query_plan` (TEXT for JSON), `chunks_returned`, `tokens_injected`

4. **Report findings:**
   - ✓ for each item that matches
   - ✗ with a specific diff for any deviation
   - ⚠ for any field present in the spec but not in the implementation (or vice versa)

5. **Embedding dimension gate.** If `chunks_vec` uses a placeholder dimension (e.g. `FLOAT[0]`), or `vault.toml` lacks an `[embeddings]` block with `dims`, block with a clear message: "Step 0 not complete — stand up TEI on `localhost:8081` with `nomic-ai/nomic-embed-text-v1.5` (768 dims) and write `[embeddings]` block in `vault.toml` before locking schema."

## Output format

```
Schema Check — src/store/schema.rs

✓ projects table
✓ documents table
✗ chunks.language CHECK missing 'helm' value
✓ chunks_fts virtual table
⚠ chunks_vec uses FLOAT[768] but vault.toml [embeddings].dims unset — confirm Step 0
✓ chunks_ai trigger
✗ chunks_au trigger missing — required for FTS5/chunks sync on update
✓ chunks_ad trigger
✓ retrieval_log table

Blockers: 2 deviations, 1 warning. Fix before committing.
```

Be precise. Do not assume — read both files.
