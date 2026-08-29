# PR 13 Code Review (`lib-cli-split`)

This review focuses strictly on defects, edge cases, API contract breaks, and required fixes in PR 13. Each identified issue includes a technical breakdown, reproduction steps or unit test, and the required fix.

---

## Summary of Required Fixes

| # | Severity | Component | Issue |
|---|---|---|---|
| 1 | **Critical** | `src/parse/rust_source.rs` | Rust parser drops item bodies when opening brace `{` is on a new line |
| 2 | **High** | `src/index/sync.rs` | `finish_sync` silently ignores `--domain` / `explicit_domain` on re-sync of existing projects |
| 3 | **High** | `src/retrieve/router/mod.rs`<br>`src/index/classify/mod.rs` | LLM `null` fields crash JSON response deserialization with `BadResponse` |
| 4 | **Medium** | `src/retrieve/budget.rs` | `select_within_budget` allows `NaN` scores through `min_score` gate |
| 5 | **Medium** | `src/diagnose/mod.rs` | `vault diagnose` discards CLI filter overrides when router returns `Skip` |
| 6 | **Medium** | `src/tei/launcher.rs` | `scrub_env` strips Docker socket, proxy (`HTTP_PROXY`), and CUDA environment variables |
| 7 | **Low** | `docs/runbook.md` | Outdated documentation regarding `vault tei status` TCP vs HTTP health probe |
| 8 | **Low** | `Cargo.toml` | Build warning on `toml` dependency semver build metadata |
| 9 | **Low** | `src/hook/log.rs`<br>`src/tei/launcher.rs` | Non-atomic file permission hardening on Unix |
| 10 | **Low** | `src/store/sqlite_store.rs` | `prune_orphans` SQL parameter explosion on repos with >999/32k files |

---

## Detailed Findings & Test Proofs

### 1. Critical: Rust Parser Drops Item Bodies When Opening Brace is on a New Line

**Location:** `src/parse/rust_source.rs:55-178`

#### Problem
In `RustParser::parse`, top-level items (`fn`, `struct`, `enum`) and inherent `impl` methods determine chunk boundaries by tracking delimiter depth `scanner.group_depth`. When an item declaration line has balanced parentheses (e.g. `fn helper()`, `pub fn get_x(&self)`, or a multi-line signature where `)` is on the line before `{`), `group_depth` is `0` (or `body_depth` for methods) at the end of that line.

Because `RustParser` immediately checks `if scanner.group_depth == item.close_depth` on the same line, the item is closed and emitted **before** the opening brace `{` on the next line is ever encountered. As a result:
1. The chunk emitted for the item contains only the header/signature; the entire function/struct body is dropped from the index.
2. The subsequent lines containing `{ ... }` are scanned with `open = None`, causing braces to be parsed out-of-context.

#### Proof Test
Add to `src/parse/rust_source.rs`:
```rust
#[test]
fn opening_brace_on_newline_includes_body() {
    let src = "\
fn helper()
{
    let x = 1;
}

pub struct Point
{
    pub x: i32,
}

impl Point {
    pub fn get_x(&self)
    {
        println!(\"x\");
    }
}
";
    let chunks = parse(src);
    assert_eq!(
        labels(&chunks),
        ["fn helper", "struct Point", "Point::get_x"]
    );
    assert!(
        chunks[0].content.contains("let x = 1;"),
        "fn body missing: {}",
        chunks[0].content
    );
    assert!(
        chunks[1].content.contains("pub x: i32,"),
        "struct body missing: {}",
        chunks[1].content
    );
    assert!(
        chunks[2].content.contains("println!(\"x\");"),
        "method body missing: {}",
        chunks[2].content
    );
}
```
**Test Failure Output:**
```
thread 'parse::rust_source::tests::opening_brace_on_newline_includes_body' panicked at src/parse/rust_source.rs:1231:9:
fn body missing: fn helper()
```

#### Required Fix
Track whether an opened item has either seen its opening body delimiter `{` or a terminal `;` (for unit structs, type aliases, consts, or extern/trait declarations) before allowing `group_depth == item.close_depth` to close the item.

---

### 2. High: `finish_sync` Silently Ignores Explicit `--domain` on Existing Projects

**Location:** `src/index/sync.rs:270-289`

#### Problem
In `finish_sync`, domain resolution is implemented as:
```rust
let domain = match store
    .resolve_domain(std::slice::from_ref(&project_name))
    .map_err(SyncError::Store)?
{
    Some(existing) => Some(existing),
    None => {
        let chosen = resolve_domain_choice(opts.explicit_domain.clone(), &opts.interaction)?;
        if let Some(ref d) = chosen {
            store
                .set_project_domain(project_id, d)
                .map_err(SyncError::Store)?;
            notify_domain_assigned(&opts.interaction, d);
        }
        chosen
    }
};
```
If a project was already indexed previously and has a domain in `vault.db`:
- When a user runs `vault index sync <repo> --domain finance` or a consumer passes `explicit_domain: Some("finance".into())`, `store.resolve_domain(...)` returns `Some("software")`.
- The `Some(existing)` branch executes, returning the existing domain.
- `opts.explicit_domain` is ignored, `store.set_project_domain` is not called, and the user has no way to reassign or update a project's domain.
- Furthermore, an invalid domain passed via `--domain` on an existing project bypasses `resolve_domain_choice` validation completely.

#### Proof Test
Add to `src/index/sync.rs`:
```rust
#[test]
fn an_explicit_domain_updates_existing_project_domain_on_resync() {
    let tmp = Tmp::new("resync-domain");
    let canonical = tmp.canonical();

    let config = Config::default();
    let mut store = SqliteStore::open_in_memory(&config).unwrap();
    let embedder = TeiEmbedder::from_config(&config).unwrap();

    let pid = store.get_or_create_project("test-project", canonical.to_str().unwrap()).unwrap();
    store.set_project_domain(pid, "software").unwrap();
    assert_eq!(
        store.resolve_domain(&["test-project".to_string()]).unwrap().as_deref(),
        Some("software")
    );

    let opts = SyncOptions {
        repo: canonical.clone(),
        explicit_name: Some("test-project".to_string()),
        explicit_domain: Some("finance".to_string()),
        dry_run: false,
        interaction: Interaction::NonInteractive {
            allow_remote_billing: false,
        },
    };

    let live = LiveSync {
        canonical,
        project_name: "test-project".to_string(),
        walked: vec![],
        embedder,
        classifier: Box::new(ExtClassifier),
    };

    let rep = finish_sync(live, &mut store, &opts).expect("finish_sync");
    assert_eq!(
        rep.domain.as_deref(),
        Some("finance"),
        "explicit domain must update the report"
    );
    assert_eq!(
        store.resolve_domain(&["test-project".to_string()]).unwrap().as_deref(),
        Some("finance"),
        "explicit domain must be saved in the database"
    );
}
```
**Test Failure Output:**
```
thread 'index::sync::tests::an_explicit_domain_updates_existing_project_domain_on_resync' panicked at src/index/sync.rs:2153:9:
assertion `left == right` failed: explicit domain must update the report
  left: Some("software")
 right: Some("finance")
```

#### Required Fix
Check `opts.explicit_domain` first:
```rust
let domain = match opts.explicit_domain {
    Some(explicit) => {
        let chosen = resolve_domain_choice(Some(explicit), &opts.interaction)?;
        if let Some(ref d) = chosen {
            store
                .set_project_domain(project_id, d)
                .map_err(SyncError::Store)?;
            notify_domain_assigned(&opts.interaction, d);
        }
        chosen
    }
    None => match store
        .resolve_domain(std::slice::from_ref(&project_name))
        .map_err(SyncError::Store)?
    {
        Some(existing) => Some(existing),
        None => {
            let chosen = resolve_domain_choice(None, &opts.interaction)?;
            if let Some(ref d) = chosen {
                store
                    .set_project_domain(project_id, d)
                    .map_err(SyncError::Store)?;
                notify_domain_assigned(&opts.interaction, d);
            }
            chosen
        }
    },
};
```

---

### 3. High: LLM `null` JSON Fields Crash Deserialization in Router and Classifier

**Location:**
- `src/retrieve/router/mod.rs:140-152`
- `src/index/classify/mod.rs:126-133`

#### Problem
LLMs (especially smaller local models or OpenAI-compatible backends) frequently emit `null` for empty fields rather than empty arrays or empty strings (e.g. `{"projects": null, "doc_types": null, "languages": null}` or `{"doc_type": "convention", "language": null}`).

In `RawQueryPlan` and `RawClassification`:
```rust
#[derive(serde::Deserialize)]
struct RawQueryPlan {
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    type_names: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    doc_types: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
}
```
In `serde`, `#[serde(default)]` only applies when a field is *absent*. If the key is present with a value of `null`, serde returns `invalid type: null, expected a sequence` (or `expected a string`). This turns a normal model response into `RouterError::BadResponse` / `ClassifyError::BadResponse`, causing hook passthrough or sync file skips.

#### Proof Tests
1. In `src/retrieve/router/mod.rs`:
```rust
#[test]
fn parse_response_null_fields_fall_back_to_empty() {
    let text = r#"{"projects": null, "type_names": null, "topics": null, "doc_types": null, "languages": null}"#;
    let out = parse_response(text).expect("null fields should deserialize as empty");
    match out {
        RouterOutput::Plan(plan) => {
            assert!(plan.projects.is_empty());
            assert!(plan.doc_types.is_empty());
        }
        RouterOutput::Skip => panic!("expected Plan"),
    }
}
```
**Failure:**
```
null fields should deserialize as empty: BadResponse("invalid JSON: invalid type: null, expected a sequence at line 1 column 17")
```

2. In `src/index/classify/mod.rs`:
```rust
#[test]
fn parse_response_null_fields_fall_back_to_unknown() {
    let c = parse_response(r#"{"doc_type":"convention","language":null}"#)
        .expect("null language should deserialize and map to Unknown");
    assert_eq!(c.doc_type, DocType::Convention);
    assert_eq!(c.language, Language::Unknown);
}
```
**Failure:**
```
null language should deserialize and map to Unknown: BadResponse("invalid JSON: invalid type: null, expected a string at line 1 column 40")
```

#### Required Fix
Use `Option<Vec<String>>` in `RawQueryPlan` and `Option<String>` in `RawClassification`, unwarpping to empty vectors / strings via `.unwrap_or_default()`.

---

### 4. Medium: `select_within_budget` Allows `NaN` Scores Past `min_score` Gate

**Location:** `src/retrieve/budget.rs:42-44`

#### Problem
In `select_within_budget`:
```rust
for hit in hits {
    if hit.final_score < min_score {
        continue;
    }
```
Under IEEE-754 floating-point comparison semantics, `NaN < min_score` is always `false`. If a hit has `final_score: f32::NAN` (which can occur if an embedding vector has zero magnitude resulting in 0/0 cosine distance, or from backend score anomalies), the hit bypasses `hit.final_score < min_score` and gets included in `chunks`.

#### Proof Test
Add to `src/retrieve/budget.rs`:
```rust
#[test]
fn min_score_gate_drops_nan_score() {
    let hits = vec![hit(1, f32::NAN, 50), hit(2, 0.9, 50)];
    let sel = select_within_budget(hits, 10_000, 0.15, None);
    assert_eq!(
        sel.chunks.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
        vec![2],
        "NaN score must be dropped by min_score gate"
    );
}
```
**Test Failure Output:**
```
thread 'retrieve::budget::tests::min_score_gate_drops_nan_score' panicked at src/retrieve/budget.rs:119:9:
assertion `left == right` failed: NaN score must be dropped by min_score gate
  left: [1, 2]
 right: [2]
```

#### Required Fix
Change the guard to:
```rust
if !(hit.final_score >= min_score) {
    continue;
}
```

---

### 5. Medium: `diagnose` Discards CLI Filter Overrides When Router Returns `Skip`

**Location:** `src/diagnose/mod.rs:88-101`

#### Problem
In `src/diagnose/mod.rs`:
```rust
match router.plan(&args.prompt, &inventory)? {
    RouterOutput::Skip => (RouterStatus::Skip { backend }, None),
    RouterOutput::Plan(mut p) => {
        p.retain_indexed(&inventory);
        (
            RouterStatus::Plan { backend },
            Some(merge_overrides(p, &cli)),
        )
    }
}
```
When an operator runs `vault diagnose "hi" --topics auth` to debug the store against the `auth` topic, the router judges `"hi"` to be a greeting and returns `RouterOutput::Skip`. Because the `Skip` branch sets `plan = None` unconditionally, the CLI overrides in `cli` are discarded, and lines 124–130 abort with:
```
(router judged no retrieval needed — no search ran)
```
The entire purpose of CLI overrides (`--topics`, `--type-names`, etc.) is to inspect retrieval behavior even when the router's prediction is unhelpful or skips.

#### Required Fix
When `RouterOutput::Skip` is returned, if `!cli.is_empty()`, apply overrides onto an empty plan:
```rust
RouterOutput::Skip => {
    if !cli.is_empty() {
        (
            RouterStatus::Skip { backend },
            Some(cli.clone().into_plan()),
        )
    } else {
        (RouterStatus::Skip { backend }, None)
    }
}
```

---

### 6. Medium: `scrub_env` in TEI Launcher Drops Proxy, Docker Socket, and CUDA Variables

**Location:** `src/tei/launcher.rs:329-348`

#### Problem
`scrub_env` wipes all environment variables via `command.env_clear()`, re-adding only:
`PATH`, `HOME`, `HF_HUB_CACHE`, `HF_HOME`, `HUGGINGFACE_HUB_CACHE`, `LANG`, `LC_ALL`, `LC_CTYPE`.

This strips:
1. **HTTP Proxies:** `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` (and lowercase variants). TEI fails to download models from HuggingFace in firewalled/proxied environments.
2. **Container Sockets:** `DOCKER_HOST`, `DOCKER_CONFIG`, `DOCKER_CERT_PATH`, `XDG_RUNTIME_DIR`. Breaks rootless Docker and custom Docker socket paths.
3. **CUDA / GPU Variables:** `CUDA_VISIBLE_DEVICES`, `NVIDIA_VISIBLE_DEVICES`, `LD_LIBRARY_PATH`. Breaks native TEI binary execution with CUDA on Linux.

#### Required Fix
Extend `PASSTHROUGH` in `src/tei/launcher.rs`:
```rust
const PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "HF_HUB_CACHE",
    "HF_HOME",
    "HUGGINGFACE_HUB_CACHE",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "XDG_RUNTIME_DIR",
    "CUDA_VISIBLE_DEVICES",
    "NVIDIA_VISIBLE_DEVICES",
    "LD_LIBRARY_PATH",
];
```

---

### 7. Low: Documentation Drift in `docs/runbook.md`

**Location:** `docs/runbook.md:149, 276`

#### Problem
Commit `6cbfa82` updated `vault tei status` to use an HTTP `GET /health` probe (reporting `serving: yes/no`) instead of a TCP connect probe. However, `docs/runbook.md` still states:
- Line 149: *"Note that `vault tei status` reports `reachable: yes` as soon as the port is bound, which happens before the model finishes loading."*
- Line 276: *"`vault tei status` says reachable but embeds fail \| Port is bound before the model finishes loading — status uses a TCP probe"*

#### Required Fix
Update `docs/runbook.md` to reflect that `vault tei status` checks `/health` and reports `serving: yes/no`.

---

### 8. Low: `Cargo.toml` Dependency Warning

**Location:** `Cargo.toml:11`

#### Problem
`Cargo.toml` specifies:
```toml
toml = "1.1.2+spec-1.1.0"
```
Cargo emits a compiler warning during every cargo command:
```
warning: version requirement `1.1.2+spec-1.1.0` for dependency `toml` includes semver metadata which will be ignored, removing the metadata is recommended to avoid confusion
```

#### Required Fix
Change line 11 to `toml = "0.8"` or `toml = "1.1.2"`.

---

### 9. Low: Non-Atomic File Permission Hardening

**Location:**
- `src/hook/log.rs:160-164`
- `src/tei/launcher.rs:110-111`

#### Problem
In `append_to` (`hook.log`) and `start` (`tei.pid`), files are created with standard `OpenOptions::create(true)` or `fs::write` (subject to process umask, often `0644`), and permissions are changed afterwards via `harden_file(path)` (`chmod 0600`). On multi-user systems, there is a race condition where the log/pid file is readable by other users before `chmod` executes.

#### Required Fix
On Unix, use `std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600)` when creating files.

---

### 10. Low: `prune_orphans` Parameter Limits on Repositories with Thousands of Files

**Location:** `src/store/sqlite_store.rs:379-389`

#### Problem
`prune_orphans` formats `AND source_path NOT IN (?, ?, ...)` using one positional parameter per kept file. In SQLite, the maximum number of query host parameters is bounded (`SQLITE_MAX_VARIABLE_NUMBER`, default 999 in older SQLite versions, 32,766 in SQLite 3.32.0+). Syncing a repository containing more files than the parameter limit will fail during the prune phase with `too many SQL variables`.

#### Required Fix
Chunk the kept paths into batches (or insert them into a temporary table) when `kept_paths.len() > 999`.
