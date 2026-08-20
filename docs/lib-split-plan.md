# Plan: lib + CLI split

**Status:** Steps 1–3 done; 4–9 pending · **Target:** single package, `src/lib.rs` + `src/main.rs`

Vault is a binary-only crate today (no `src/lib.rs`, no `[lib]` in `Cargo.toml`).
This plan turns it into a library with a thin CLI over it, so the retrieval and
indexing pipelines can be consumed from another process — a service now, an MCP
server later.

For the broader architecture see `vault-plan.md`; for where that spec has drifted
from the code see `plan-review-2026-06-11.md`.

---

## Goal

One package, two targets. The module tree does not move; only the manifest,
visibility, and the CLI/library boundary change.

```
vault/
  Cargo.toml          # unchanged deps; cargo auto-detects both targets
  src/
    lib.rs            # NEW — curated public API
    main.rs           # thin CLI over vault::*
    hook/ store/ index/ parse/ retrieve/ embed/ tei/ util/   # unmoved
  tests/              # NEW — black-box integration tests against the public API
```

A workspace split (`vault-core` + `vault-cli`) stays available later without
touching module paths — only the manifest. The cost of staying single-package is
that consumers pull `clap` transitively.

---

## Design rules

These are the constraints the API must satisfy. Each one is load-bearing for a
current or planned consumer.

1. **The library propagates errors; the CLI decides to fail open.**
   Fail-open is `vault hook` policy, not library policy. `hook::run()` keeps the
   exit-0 contract; every library entry point returns a `Result` with a typed
   error. An MCP tool call that fails must surface the failure — silently
   returning nothing is a bug in that context, even though it is correct for the
   hook.

2. **Retrieval returns structured data; rendering is a separate step.**
   The `<{domain}-context>` block is hook-shaped output. Consumers want the
   `Vec<Hit>`. `render_block()` becomes a method the hook calls, not something
   baked into the pipeline's return value.

3. **The library never writes to stdout and never reads stdin.**
   Today this holds by accident (pipeline code is print-free; all CLI output is
   in `main.rs`, `diagnose`, `configure`, `tei/launcher`). It becomes
   load-bearing under stdio MCP, where stdout *is* the JSON-RPC channel and stdin
   *is* the request stream. Enforce it with a test, not a convention.

4. **The stateless pipeline stages are separable from the store-bound ones.**
   `router.plan()` and `embed_query()` are network-bound with a 3s timeout and
   touch no store. Only `hybrid_search()` and `resolve_tag()` need the
   connection. Keeping these separable means a concurrent consumer holds its lock
   for milliseconds of SQLite work instead of across a 3-second network timeout.

5. **Public surface is curated, not blanket `pub`.**
   The 71 `pub(crate)` declarations stay `pub(crate)` unless a consumer needs
   them. This is what preserves the stub invariant — see below.

---

## The stub invariant survives

`retrieve/router/stub.rs` and `index/classify/stub.rs` are `#[cfg(test)]`-gated so
the compiler enforces that a stub can never become a silent production fallback.

A blanket `pub` export would destroy that: `#[cfg(test)]` is not active when the
crate is built as a dependency, so making those tests reachable from `tests/`
would require un-gating the stubs and shipping them in release builds.

A curated API avoids the problem entirely — the stubs are never part of the public
surface, so they stay gated and stay test-only. `embed/stub.rs` remains
deliberately un-gated because `vault diagnose --stub` exposes `StubEmbedder` in
release builds.

**This asymmetry is intentional. Do not "resolve" it in either direction.**

---

## Public API surface

Shape, not final signatures. The split into three types is what makes rule 4
enforceable at the type level.

```rust
// Stateless. Send + Sync — share freely across threads/tasks.
pub struct QueryPlanner { /* router + embedder + config */ }

impl QueryPlanner {
    pub fn new(config: &Config) -> Result<Self, VaultError>;
    /// Router plan + query embedding. Network-bound; no store access.
    /// Ok(None) when the router returns `{ skip: true }`.
    pub fn plan(&self, prompt: &str) -> Result<Option<PlannedQuery>, VaultError>;
}

// Owns the SQLite connection. Send + !Sync — one per worker, or behind a lock.
pub struct VaultStore { /* SqliteStore + config */ }

impl VaultStore {
    pub fn open(config: &Config) -> Result<Self, VaultError>;
    /// Hybrid search + budget trim + tag resolution. Store-bound; milliseconds.
    pub fn search(&self, planned: &PlannedQuery) -> Result<Retrieval, VaultError>;
    pub fn sync(&mut self, opts: SyncOptions) -> Result<SyncReport, VaultError>;
}

// Convenience facade for single-threaded consumers (the CLI uses this).
pub struct Vault { /* QueryPlanner + VaultStore */ }

impl Vault {
    pub fn open(config: &Config) -> Result<Self, VaultError>;
    pub fn retrieve(&self, prompt: &str) -> Result<Retrieval, VaultError>;
    pub fn sync(&mut self, opts: SyncOptions) -> Result<SyncReport, VaultError>;
}

pub enum Retrieval {
    /// Deliberate no-injection. Not an error.
    Skip(SkipReason),
    Context(Context),
}

pub struct Context {
    pub tag: String,
    pub hits: Vec<Hit>,
    pub tokens: u32,
}

impl Context {
    /// Render the `<{tag}>...</{tag}>` block. Only `vault hook` needs this.
    pub fn render_block(&self) -> String;
}
```

Also exported: `Config`, `SyncOptions`, `SyncReport`, `Hit`, `QueryPlan`,
`DocType`, `Language`, `SkipReason`, `VaultError`.

Everything else stays `pub(crate)`.

### Errors

`Outcome::Failed { stage, detail: String }` truncates and stringifies for
`hook.log`. That is telemetry policy and stays in `hook/`. The library gets a
typed error carrying the real source:

```rust
pub enum VaultError {
    Config(ConfigError),
    RouterBuild(..), RouterPlan(..),
    EmbedderBuild(..), EmbedQuery(..),
    DbOpen(StoreError), Query(StoreError),
}
```

**Done** (`src/error.rs`). Two deviations from the sketch above, both deliberate:

- **`Sync(SyncError)` is deferred to Step 6.** The hook's retrieval path cannot
  produce it, so including it now would force `Stage::of` to invent a telemetry
  stage for a case that cannot occur. It lands when `Vault::sync` does.
- **`RouterError`, `EmbedError`, and `StoreError` are now re-exported from
  `lib.rs`.** A public error type cannot name private ones. The modules stay
  private at the crate root; only the error types are re-exported.
  `retrieve::RouterError` was `#[cfg(test)]`-gated on the rationale that
  production code never named it — `VaultError` does, so that gating is gone. The
  `StubRouter` gating, which is the one that matters, is untouched.

The hook now derives its telemetry `Stage` from the error variant
(`Stage::of`) instead of naming one at each of eight call sites, and
`pipeline_with` splits into a library-shaped `retrieve_with` returning
`Result<_, VaultError>` plus a thin fail-open adapter. Step 4 replaces the
`Outcome` in that `Ok` arm with a `Retrieval`.

One regression caught during the work and now pinned by a test: logging
`VaultError`'s own `Display` made records read
`"router-build failed: router construction failed: ..."`, since `stage` already
encodes the position. `from_vault_error` logs the *source* message instead, so
`hook.log` output is byte-identical to before the refactor.

`hook::run()` maps `VaultError` to the existing `Stage` for its log record, so
telemetry is unchanged. `Outcome`, `Stage`, and `SkipReason`'s hook-specific
handling stay `pub(crate)` in `hook/`.

### Interaction policy

`run_sync` currently hardcodes `std::io::stdin().lock()` at four call sites
(`src/index/sync.rs:112,126,131,165`). The prompt functions themselves are already
generic over `R: BufRead, W: Write` (`:593,622,647,669`), so the seam exists — only
the call sites need to branch.

```rust
pub enum Interaction {
    /// Read stdin / write stderr. CLI behavior; the default for `vault index sync`.
    Terminal,
    /// Never prompt. Missing name falls back to the derived name; missing domain
    /// resolves to None.
    NonInteractive { allow_remote_billing: bool },
}
```

`SyncOptions` gains an `interaction` field.

**`allow_remote_billing: false` must return an error, never silently proceed,**
when auto-mode resolves to a remote classifier backend. The one-time cost prompt
is a consent gate; a non-interactive caller has to opt in explicitly rather than
have the gate skipped for it.

---

## Future consumers

The MCP server subcommand remains **out of v1 scope** (see CLAUDE.md, "v1 Scope
Boundaries"). This section records what the API must not preclude, so the option
stays cheap.

| Requirement | Why | Covered by |
|---|---|---|
| Structured hits, not a pre-rendered block | The model consuming a tool result doesn't need XML framing | Rule 2 |
| Errors visible to the caller | A silently-empty tool result is indistinguishable from "no context found" | Rule 1 |
| Never touch stdin/stdout | Stdio MCP uses both for JSON-RPC framing | Rule 3 |
| No interactive prompts | Same — and `sync` as a tool must be callable unattended | `Interaction::NonInteractive` |
| Short lock hold under concurrency | Concurrent tool calls must not serialize behind a 3s router timeout | Rule 4 / the three-type split |
| Bypassable skip decision | Under MCP the model already decided it wants context; the router's `{skip:true}` is push-model logic | `QueryPlanner::plan` returning `Option`, plus a plan-from-overrides path |

Two things are **deliberately deferred**, and both are documented consumer
contracts rather than blockers:

- **Blocking HTTP.** `reqwest::blocking::Client` is used in all seven backend
  files (both routers, both classifiers, TEI). The Rust MCP ecosystem is
  tokio-based, so a future server wraps `Vault` calls in `spawn_blocking`. An
  async API is a later decision, not a prerequisite.
- **Process-lifetime backend probe cache.** `resolve_backend` caches the
  auto-mode decision for the process lifetime. Correct for a short-lived CLI;
  in a long-lived server, Gemma coming back up is never noticed. Revisit with a
  TTL when a long-lived consumer actually exists.

---

## Migration steps

Dependency order. Each step should leave `cargo build`, `cargo test`,
`cargo fmt --check`, and `cargo clippy -- -D warnings` green.

```
Step 1  src/lib.rs + main.rs shim        — DONE: both targets compile; no API curation yet
Step 2  WAL + busy_timeout               — DONE: schema::apply_pragmas
Step 3  VaultError                       — DONE: src/error.rs; hook derives Stage from it
Step 4  Retrieval / Context / PlannedQuery — split pipeline_with into stateless + store phases
Step 5  QueryPlanner + VaultStore + Vault — rewire hook::run over the facade
Step 6  Interaction policy               — thread through SyncOptions; branch the 4 call sites
Step 7  Config::from_path                — explicit vault dir for non-home consumers
Step 8  Curate lib.rs exports            — confirm stubs still #[cfg(test)]; check the public surface
Step 9  tests/ integration suite         — black-box against the public API
```

### Step 2 detail

`src/store/schema.rs` sets `foreign_keys` but no `journal_mode`. Under the
default rollback journal, a reader holding a transaction keeps a SHARED lock and
the writer's COMMIT (which needs EXCLUSIVE) is refused — i.e. a `vault hook` read
can break a `vault index sync` commit.

`busy_timeout` turned out **not** to be part of the defect: rusqlite already
applies a 5s timeout on `Connection::open`. Setting it explicitly changes no
behaviour and is kept only to pin the value vault depends on. WAL was the whole
fix.

This is invisible today — one CLI process, one connection — but it blocks both
"one `VaultStore` per worker" and any concurrent server. Set `journal_mode=WAL`
and a non-zero `busy_timeout` at open.

**Done.** `schema::apply_pragmas(conn, wal)` is now the single place connection
setup happens, called by both `open` (WAL on) and `open_in_memory` (WAL off — an
in-memory db has no journal file).

Four contention tests cover it, and the fix was mutation-checked: disabling WAL
makes `open_enables_wal_on_a_file_db` and
`wal_lets_a_writer_commit_while_a_reader_holds_a_transaction` fail.
`legacy_setup_lets_a_reader_break_a_writers_commit` reproduces the original
defect against a deliberately pre-fix connection and passes either way by design
— it characterises the bug rather than guarding the fix.

Decision made during implementation: **a non-WAL result is not an error.** WAL is
unavailable on some filesystems (network home directories being the usual case).
Since WAL buys concurrency rather than correctness, failing the open there would
make vault unusable for a benefit the current single-process CLI doesn't need.
The connection keeps working in whatever mode SQLite chose; `busy_timeout` still
applies. Revisit if a concurrent consumer ever needs to *depend* on WAL.

### Step 7 detail

`Config::load()` resolves `~/.vault/vault.toml` via `home_dir()`
(`src/config.rs:157`), and `vault_dir()` / `db_path()` (`:302,306`) derive from
it. A service in a container, or one running as a different user, needs an
explicit path. Keep `load()` as the convenience constructor.

---

## Test split

Current state: **364 tests**, all in inline `#[cfg(test)] mod tests` blocks across
32 files. No `tests/` directory.

The split follows from what each test actually exercises:

| Location | Holds | Why |
|---|---|---|
| inline `#[cfg(test)] mod tests` | unit tests reaching private internals | All 32 test-bearing files use `use super::*`; 85 tests depend on `#[cfg(test)]`-gated stubs (`sync.rs` 30, `router/mod.rs` 24, `classify/mod.rs` 19, `hook/mod.rs` 12) that integration tests cannot see |
| `tests/` | black-box tests against `Vault` / `QueryPlanner` / `VaultStore` | This is what integration tests are for |

**The existing 364 tests mostly stay where they are.** `tests/` holds *new*
coverage of the public API, not relocated coverage. This is the same split the
standard library uses.

One test that belongs in `tests/` from day one: assert the library writes nothing
to stdout (rule 3), so a future stdio MCP server can't be broken by an
accidental `println!`.

---

## Open decisions

- Whether `Box<dyn Router>` / `Box<dyn Embedder>` can carry `+ Send + Sync`
  bounds. Both hold a `reqwest::blocking::Client` (which is `Send + Sync`) plus
  owned config, so this should hold — confirm at compile time in Step 5 rather
  than assuming it.
- Whether `diagnose`'s `Overrides` / plan-from-CLI-flags path becomes public. An
  MCP server would want the equivalent (bypass the router, supply a plan
  directly); the CLI already has it. Cheap to expose, so probably yes.
- Whether to feature-gate `clap` so library consumers skip the CLI dep tree, or
  accept it until a workspace split.
