---
name: parser-test
description: Use this skill when working on language parsers in src/parse/ for the vault project (proto.rs, go_source.rs, rust_source.rs, openapi.rs, helm.rs, markdown.rs). Runs a parser against a sample input file and pretty-prints the extracted chunks (label, doc_type, language, token estimate, content preview) so chunk boundary correctness can be validated without the full hook pipeline. Triggers when the user is implementing, debugging, or reviewing a parser, or asks to "test the parser", "check chunk boundaries", or similar.
---

# Parser Test

Parsers in `src/parse/` extract chunks from source files at definition boundaries. Boundary correctness matters — wrong chunking degrades retrieval quality and is hard to diagnose downstream once the hook pipeline is running. This skill exercises a parser against a sample file and surfaces the resulting chunks.

## When to invoke

- User is implementing or modifying a file under `src/parse/`
- User asks to "test the parser", "verify chunks", "check boundaries"
- User is reviewing parser output before wiring `index/sync`

## Boundary expectations

From `docs/vault-plan.md` § Chunking Strategy:

| doc_type | language | Boundary |
|----------|----------|----------|
| contract | proto | per top-level `message` / `service` / `enum` at column 0 |
| contract | openapi | per path+method, per `components/schemas` entry |
| plan | any | whole file, single chunk |
| convention | go | per exported symbol (`func [A-Z]`, `type [A-Z]`, `const [A-Z]`, `var [A-Z]`) including preceding `//` doc comment; interfaces as one whole unit |
| convention | rust | per `pub fn` / `pub struct` / `pub enum` / `pub trait` / `pub type` / `pub const`, including preceding `///` doc comment; `pub mod` shallow only |
| convention | scala | whole file (v1) |
| convention/meta | markdown | per `##` heading block (not `#` — that's the document title) |

## Procedure

1. **Identify the parser under test.** Confirm the file path in `src/parse/`.

2. **Locate or request a sample.** Look for fixtures under `tests/fixtures/` or `src/parse/<lang>/fixtures/`. If none exists, ask the user for a sample file path.

3. **Run the parser.** Either via a test (`cargo test parse::<lang> -- --nocapture`) or a debug binary if one exists. If neither, suggest adding a focused unit test that prints chunks via `dbg!` or `println!`.

4. **Print each extracted chunk:**
   - `chunk_index`
   - `label` (e.g. `message BuildRequest [build-service]`)
   - `doc_type` / `language`
   - `token_est`
   - First 200 chars of `content`
   - Boundary line numbers in source

5. **Validate against expectations:**
   - Does the chunk count match the number of definitions in the source?
   - Are doc comments included with their symbol (Go `//`, Rust `///`)?
   - Are unexported / non-public symbols correctly excluded?
   - For proto: are nested messages handled per the parser's documented behavior?
   - For markdown: are `##` blocks the unit, with `#` excluded as title?

6. **Report:**

```
Parser Test — src/parse/proto.rs against tests/fixtures/build.proto

Source: 47 lines, 3 messages, 1 service, 0 enums
Extracted: 4 chunks

[0] message BuildRequest [contract/proto] — 142 tokens, lines 5–18
    "message BuildRequest { string id = 1; ... }"
[1] message BuildResponse [contract/proto] — 89 tokens, lines 20–28
    ...
[2] message BuildStatus [contract/proto] — 64 tokens, lines 30–37
    ...
[3] service BuildService [contract/proto] — 51 tokens, lines 39–46
    ...

✓ Chunk count matches definition count (4 = 3 messages + 1 service)
✓ All chunks at column-0 boundaries
⚠ BuildRequest token count (142) approaches typical chunk size — flag for review
```

Be specific. If the parser is missing or panics, surface the exact error and suggest the next step.
