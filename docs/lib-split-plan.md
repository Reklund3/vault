# Plan: lib + CLI split

**Status:** Steps 1–9 done and committed — the migration is complete
**Tracking:** committed as the record of this branch · **Target:** single package, `src/lib.rs` + `src/main.rs`

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

- **`Sync(SyncError)` was deferred to Step 6** and landed there with
  `Vault::sync`. `Stage::of` did have to gain a `Stage::Sync` for totality; it is
  unreachable from the hook, which never indexes. An unused-but-truthful stage
  name beats folding a sync failure into `Stage::Query`, which would have put a
  wrong label in `hook.log` if it ever did occur.
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

**Done** — see the Step 6 detail below.

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
Step 4  Retrieval / Context / PlannedQuery — DONE: types in retrieve/, retrieve::search is phase 2
Step 5  QueryPlanner + VaultStore + Vault — DONE: src/vault.rs; hook::run runs over the facade
Step 6  Interaction policy               — DONE: SyncOptions.interaction; Vault::sync lands
Step 7  Config::from_path                — DONE: Config carries its own dir
Step 8  Curate lib.rs exports            — DONE: `cli` feature splits library from CLI
Step 9  tests/ integration suite         — DONE: pipeline.rs, no_stdout.rs, public_api.rs
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

*The code only actually honoured that decision from the post-migration review
onward.* `apply_pragmas` propagated the pragma's error with `?`, which covered the
case the paragraph above describes — SQLite declining and reporting another mode,
a row and no error — but not the case where the pragma itself fails. A read-only
`vault.db` is the reachable one: the switch rewrites the file header, so SQLite
answers "attempt to write a readonly database" even though every query the hook
runs is a read, and the open failed outright. Now best-effort, with
`open_succeeds_on_a_read_only_database_that_cannot_take_wal` holding it.

### Step 4 detail

**Done.** `PlannedQuery`, `Retrieval`, `Context` and `SkipReason` live in
`src/retrieve/mod.rs` and are re-exported from `lib.rs`. `SkipReason` moved out
of `hook/` — it was always a retrieval concept, not a hook one.

`retrieve::search(&PlannedQuery, &Config, &dyn Store)` is phase 2, and its
signature is the guarantee: no router, no embedder, no network. `retrieve_with`
in the hook is now visibly two phases with a `PlannedQuery` between them.
`resolve_tag` moved alongside `search` (it is store-bound); `render_block`
became `Context::render_block`, so the hook is the only caller of the renderer
and a consumer that wants the chunks uses `Context::hits`.

**One telemetry semantic change:** `query_ms` now spans the whole store phase
(hybrid search + budget trim + tag resolution) rather than the hybrid query
alone. This is deliberate — that span is exactly how long a concurrent caller
would hold the store, which is the number the lock-granularity design cares
about. The `stage` field and all error text are unchanged, verified against the
live binary.

### Step 5 detail

**Done.** `src/vault.rs` holds all three types. `hook/mod.rs` lost its private
`Services` struct and `open_services()` — introduced in Step 3 as a placeholder
for exactly this — and now runs over the facade.

`QueryPlanner` exposes `route` and `embed_query` separately as well as the
combined `plan`, because the hook times the two independently and a single
combined call would flatten that telemetry.

The `#[cfg(test)]`-gated `from_parts` / `from_store` constructors are the same
compiler-enforced boundary the router and classifier stubs rely on: production
can only reach the configured backends via `new` / `open`.

### Step 6 detail

**Done.** `Interaction` and `SyncOptions.interaction` live in `src/index/sync.rs`;
`VaultStore::sync` and `Vault::sync` are on the facade, and `VaultError::Sync`
closes the deferral from Step 3.

Three decisions worth recording:

- **`SyncOptions` has no `Default`, on purpose.** `Interaction` has no safe
  guess. Defaulting to `Terminal` blocks a service on a read of a stdin that is
  somebody else's protocol channel; defaulting to `NonInteractive` answers a
  billing question for the caller. Every construction site names it.
- **`RemoteBillingNotPermitted` is a separate variant from
  `DeclinedRemoteCost`.** Nobody declined — nobody was asked. A caller that
  meant to allow billing can tell the two apart and retry with consent instead
  of reporting a refusal that never happened. Pinned from outside the crate by
  `a_consumer_can_tell_a_missing_consent_from_a_refused_one`.
- **The gate stays auto-mode-only.** An explicit `mode = "haiku"` / `"openai"`
  is the user having already chosen a paid backend; the gate exists for the
  fallback they did *not* choose. Firing it on an explicit mode would strand a
  non-interactive caller who had configured exactly what they wanted.

The four call sites now go through `resolve_project_name`,
`resolve_domain_choice`, and `confirm_remote_classification`. Only the
`Terminal` arm of each reaches for stdin, so a non-interactive sync never locks
a stream it does not own — the branch happens before the lock, not after.

`run_sync` split into `prepare_sync` (walk, name, consent gate, TEI, classifier)
and `finish_sync` (project id, domain, `sync_with`), with `Prepared::DryRun`
short-circuiting between them. `run_sync` opens its own `SqliteStore` between
the two halves; `run_sync_with_store` — what `VaultStore::sync` calls — passes
in the connection the consumer already holds. Ordering is otherwise byte-for-byte
what it was, verified against the live binary's dry-run output.

The consent gate was mutation-checked: replacing the
`allow_remote_billing: false` arm with `Ok(())` fails exactly
`a_non_interactive_sync_without_consent_refuses_a_remote_classifier` and
`refusing_for_lack_of_consent_is_not_the_same_error_as_declining`.

### Step 7 detail

`Config::load()` resolved `~/.vault/vault.toml` via `home_dir()`, and
`vault_dir()` / `db_path()` derived from it. A service in a container, or one
running as a different user, needs an explicit path.

**Done.** The root cause was not the missing constructor — it was that `Config`
did not *carry* its directory. `vault_dir()` re-derived it from `$HOME` on every
call, so even a config loaded from elsewhere would have written its database to
`~/.vault`. `Config` now holds a `#[serde(skip)] vault_dir: Option<PathBuf>`:

- `Config::from_path(dir)` reads `<dir>/vault.toml` and pins `dir`.
- `Config::load()` is now a one-liner over `from_path`, so the two cannot drift.
- `Config::with_vault_dir(dir)` pins a config that has no toml at all — the
  container-from-defaults case, which `from_path` alone does not cover.
- `None` (what `Config::default()` gets) keeps the historical `$HOME` fallback.

Two `$HOME` reads are **deliberately left alone**, because both run when no
`Config` exists to consult:

- `hook/log.rs` needs a log destination precisely when `vault.toml` failed to
  load, which is why it took `vault_dir_path()` directly in the first place.
- `configure/mod.rs` *creates* `~/.vault` and seeds the toml.

Both are CLI-only paths (see the Step 8 note on which modules are the library),
so a library consumer never reaches either. Revisit if `vault configure` ever
grows an explicit-directory flag.

`ConfigError::IoError` and `ParseError` now carry the path they failed on. This
is not cosmetic: the whole point of the step is a consumer juggling more than
one config directory, and "No such file or directory" does not say which one.
The change dropped `#[from]`, which had exactly one call site.

Mutation-checked: making `vault_dir()` ignore the stored field fails
`from_path_pins_vault_dir_and_db_path_away_from_home` and
`with_vault_dir_pins_a_config_that_has_no_toml`. The tests assert exact paths
rather than mutating `HOME`, which would race the rest of the suite.

---

### Step 8 detail

**Done.** The curation turned on one observation: the eight public modules split
cleanly in two, and the split is already visible in who prints.

| Module | stdout sites | `process::exit` | |
|---|---|---|---|
| `diagnose` | 24 | — | CLI |
| `tei` | 17 | — | CLI |
| `hook` | 2 | 1 | CLI |
| `configure` | 1 | — | CLI |
| `config`, `error`, `index`, `vault` | 0 | — | **library** |

So the four CLI modules moved behind a default-on `cli` feature, with `clap`
`optional = true` and the binary declaring `required-features = ["cli"]`. Nothing
moved on disk; `cargo build` / `test` / `clippy` behave exactly as before.

**What this does and does not buy.** It does not by itself make the pipelines
print-free — that is a property of the pipeline code. What it buys is that a
consumer building `default-features = false` cannot *reach* a printing entry
point, and that "library or CLI?" is a question the compiler answers. The
Step 9 rule-3 test still has to exercise the pipeline and watch stdout.

Also settled:

- `index::{classify, secrets, walk}` are now `pub(crate)` — a consumer drives
  `sync`, not the machinery underneath it. That made `SyncError` half-public
  (`BuildClassifier(ClassifyError)`, `Walk(WalkError)`), so both error types are
  re-exported at the root, the same pattern Step 3 set for `RouterError`.
- `Config` and `ConfigError` are re-exported at the root, closing the
  inconsistency `tests/public_api.rs` had been carrying a NOTE about.
- Four CLI-only helpers were gated alongside their callers so the library-only
  build is warning-clean: `StubEmbedder` and its constructor, the
  `ResolvedBackend` / `resolve_backend` re-export, and `probe::tei_reachable`.

**One documented invariant changed shape, deliberately.** CLAUDE.md said
`embed/stub.rs` is "deliberately not gated". Its gate is now
`#[cfg(any(feature = "cli", test))]`. The rule's substance is intact — it is
still not `#[cfg(test)]`, and it still ships in every release build that has a
CLI, which is exactly when `--stub` exists. It is absent only from a library-only
build, where `diagnose` does not exist either. CLAUDE.md is updated to say so.

A `lib-only` CI job now runs `cargo build --no-default-features --lib` and the
matching clippy. Without it the feature would rot silently, since no other job
builds without `cli`.

## Test split

Starting state: **364 tests**, all in inline `#[cfg(test)] mod tests` blocks
across 32 files, no `tests/` directory. At the close of Step 9: **404 inline + 21
across three `tests/` binaries**; the post-migration review pass took that to
**426 inline + 23**.

The split follows from what each test actually exercises:

| Location | Holds | Why |
|---|---|---|
| inline `#[cfg(test)] mod tests` | unit tests reaching private internals | All 32 test-bearing files use `use super::*`; 85 tests depend on `#[cfg(test)]`-gated stubs (`sync.rs` 30, `router/mod.rs` 24, `classify/mod.rs` 19, `hook/mod.rs` 12) that integration tests cannot see |
| `tests/` | black-box tests against `Vault` / `QueryPlanner` / `VaultStore` | This is what integration tests are for |

**The existing inline tests mostly stay where they are.** `tests/` holds *new*
coverage of the public API, not relocated coverage. This is the same split the
standard library uses.

One test that belongs in `tests/` from day one: assert the library writes nothing
to stdout (rule 3), so a future stdio MCP server can't be broken by an
accidental `println!`.

**Written in Step 9** (`tests/no_stdout.rs`), after Step 8 settled what the rule
means: `hook`, `diagnose`, `configure` and `tei` are behind the `cli` feature, so
the reachability half is compiler-enforced and the test covers the behavioural
half. It cost more machinery than expected — see the Step 9 detail.

---

### Step 9 detail

**Done.** `tests/` now holds three binaries plus a shared fixture module:

| File | Holds |
|---|---|
| `public_api.rs` | 14 tests — the types a consumer needs are nameable and usable |
| `pipeline.rs` | 8 tests — the pipeline *works* when driven from outside the crate |
| `no_stdout.rs` | 1 test — design rule 3, alone in its binary by necessity |
| `common/mod.rs` | `TmpDir`, `config_in`, `offline_config_in`, `plan_for` — no tests |

Two of `pipeline.rs`'s tests were added at Step 9 but did not assert anything until
the review pass: they needed a `Vault`, auto mode probes `localhost:8080` and then
demands a key, and the `let Ok(..) else { return }` they used to skip on made them
silently vacuous on any machine without a backend — CI included.
`offline_config_in` is the fix: a real `vault.toml` pinning gemma mode, loaded
through `Config::from_path`, which constructs without a probe or a key.

`pipeline.rs` runs with no network and no services. That is possible because
`PlannedQuery`'s fields are public, so a consumer builds one directly instead of
going through a router and an embedder — design rule 4 paying off, and the reason
`VaultStore::search` is testable on its own. Step 7's `with_vault_dir` is what
keeps these tests off the developer's real `~/.vault/vault.db`.

Note what is *not* available out here: `StubRouter` and `StubClassifier` are
`#[cfg(test)]`-gated, so integration tests cannot see them. Every test in `tests/`
runs against the real types. That is the stub invariant working as designed,
observed from the outside.

**The rule-3 test took three attempts, and the first two were wrong in ways worth
recording:**

1. *In-process `dup2` on fd 1.* Passed, and proved nothing. `cargo test`'s
   default harness swaps out `print!`'s destination at the Rust level, so
   `println!` never reaches fd 1 — a stray print in `retrieve::search` sailed
   straight through. Caught only by mutation-testing the test.
2. *Same, in a single-test binary.* Fixed a real problem — the harness writes its
   own "test foo ... ok" progress lines to fd 1, and a parallel neighbour
   contaminates the capture — but not the capture problem above.
3. *Child process under `--nocapture`.* What shipped. The parent re-runs the test
   binary for this one test with capture disabled; the child points fd 1 at a
   file for the duration of the library calls and restores it; the parent reads
   the file back. `--nocapture` is what makes `println!` reach fd 1 at all, and it
   cannot be set from inside a test.

Mutation-checked both ways it can fail: a `println!` in `retrieve::search` and a
`print!` in the sync path each fail it with the offending bytes in the message.

`libc` is a new dev-dependency, for `dup`/`dup2` alone. It was already in the
graph transitively (ring → getrandom), so the lockfile gained one line and no
new download. The test is `#[cfg(unix)]`.

## Open decisions

- ~~Whether `Box<dyn Router>` / `Box<dyn Embedder>` can carry `+ Send + Sync`
  bounds.~~ **Resolved in Step 5: they can.** `build_router` now returns
  `Box<dyn Router + Send + Sync>`, and the bound is pinned from outside the crate
  by `the_concurrency_contract_holds_for_consumers`.
- Whether `diagnose`'s `Overrides` / plan-from-CLI-flags path becomes public. An
  MCP server would want the equivalent (bypass the router, supply a plan
  directly); the CLI already has it. **Still open, but the accident is gone:**
  `diagnose::Args` used to be reachable as `vault::diagnose::Args` without anyone
  deciding it should be. `diagnose` is now behind the `cli` feature, so a
  library-only consumer cannot see it. Exposing the capability deliberately means
  a `PlannedQuery`-from-overrides constructor on `QueryPlanner`, not re-exporting
  a clap struct.
- ~~Whether to feature-gate `clap` so library consumers skip the CLI dep tree.~~
  **Resolved in Step 8: yes.** `clap` is `optional = true` behind the default-on
  `cli` feature, and the binary declares `required-features = ["cli"]`.
