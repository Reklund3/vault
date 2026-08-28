use std::path::Path;
use std::sync::Once;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::store::traits::StoreError;

/// The v1 DDL every connection runs at open.
///
/// **`chunks_fts` has insert/update/delete triggers; `chunks_vec` has none, and
/// that asymmetry is deliberate (review B8).**
///
/// FTS5 is compiled into the bundled SQLite, so a trigger body referencing
/// `chunks_fts` can always be compiled. `chunks_vec` is a `vec0` virtual table
/// from sqlite-vec, which this process registers as a *runtime* auto-extension
/// (see `VEC_INIT` below). SQLite compiles a trigger body when the trigger
/// fires, so a trigger referencing `chunks_vec` would make **every** delete
/// from `chunks` fail in any process that has not loaded sqlite-vec — the
/// `sqlite3` CLI, a backup script, a future migration tool. That turns a
/// missing optional extension into a database that cannot be pruned or
/// repaired, including by the tooling you would reach for to fix it.
///
/// So vec rows are deleted explicitly instead, at the two places that remove
/// chunks: `SqliteStore::upsert_document` (replacing a document's chunks) and
/// `SqliteStore::prune_orphans` (reconciling deletions at sync time). Both are
/// covered by tests — `upsert_replaces_existing_chunks` asserts no vec row
/// leaks, and `prune_orphans_cleans_chunks_vec` covers the sync-time path.
///
/// The cost is that this is a hand-maintained invariant: a **third** path that
/// deletes from `chunks` would silently orphan its `chunks_vec` rows, since
/// nothing in the schema enforces the pairing. Any new delete path needs its own
/// `DELETE FROM chunks_vec` and its own test.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    repo_path  TEXT,
    -- domain assignment (NULL = unassigned -> hook falls back to
    -- defaults.context_tag). Interactive runtime state vault writes during sync;
    -- only the name is stored; it renders as the `domain` attribute on the block.
    domain     TEXT,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS documents (
    id           INTEGER PRIMARY KEY,
    project_id   INTEGER NOT NULL REFERENCES projects(id),
    doc_type     TEXT NOT NULL CHECK(doc_type IN ('contract','plan','convention','meta')),
    source_path  TEXT NOT NULL,
    title        TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE(project_id, source_path)
);

CREATE TABLE IF NOT EXISTS chunks (
    id           INTEGER PRIMARY KEY,
    document_id  INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    project_id   INTEGER NOT NULL,
    doc_type     TEXT NOT NULL,
    language     TEXT NOT NULL CHECK(language IN
                   ('go','rust','scala','proto','openapi','helm','markdown','unknown')),
    label        TEXT NOT NULL,
    content      TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    token_est    INTEGER NOT NULL,
    chunk_index  INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    UNIQUE(document_id, label)
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    label, content,
    content='chunks',
    content_rowid='id',
    tokenize='porter unicode61'
);

-- chunks_vec (the vec0 virtual table) is created separately in `migrate`,
-- parameterized by the configured embedding dim — vec0 bakes the dimension into
-- the column at creation, so it can't live in this fixed-text const.

CREATE TABLE IF NOT EXISTS retrieval_log (
    id               INTEGER PRIMARY KEY,
    prompt_hash      TEXT NOT NULL,
    query_plan       TEXT NOT NULL,
    chunks_returned  INTEGER NOT NULL,
    tokens_injected  INTEGER NOT NULL,
    created_at       INTEGER NOT NULL
);

-- Tracks what embedding stack the chunks_vec rows were produced by. Future
-- migrations to a new embedding model use this to detect mismatch and to drive
-- the re-embedding pass before swapping the vec table.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, label, content)
    VALUES (new.id, new.label, new.content);
END;

CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, label, content)
    VALUES ('delete', old.id, old.label, old.content);
    INSERT INTO chunks_fts(rowid, label, content)
    VALUES (new.id, new.label, new.content);
END;

CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, label, content)
    VALUES ('delete', old.id, old.label, old.content);
END;

-- There is deliberately NO matching trigger for chunks_vec. See the note in
-- Rust above `SCHEMA_V1` before adding one (review B8).
"#;

/// Schema v2: covering indexes for the three columns `Store::inventory` scans
/// and the plan filters bind against.
///
/// `inventory()` runs on every hook call, before the router can decide to skip,
/// and `chunks.content` is stored inline — so a bare `SELECT DISTINCT language
/// FROM chunks` pages in the whole corpus. Measured on a 120k-chunk / 91MB
/// database: the three inventory queries cost 226ms without these and 40ms
/// with them, and each becomes a covering index scan that never touches the
/// table.
///
/// Additive only. `CREATE INDEX` builds a new B-tree from one pass over the
/// table; it does not rewrite `chunks`, `chunks_fts`, or `chunks_vec`, so no
/// re-embed or re-sync is involved. One-time build cost on that same 120k
/// corpus was ~215ms total.
const SCHEMA_V2: &str = r#"
CREATE INDEX IF NOT EXISTS idx_chunks_language   ON chunks(language);
CREATE INDEX IF NOT EXISTS idx_chunks_doc_type   ON chunks(doc_type);
CREATE INDEX IF NOT EXISTS idx_chunks_project_id ON chunks(project_id);
"#;

#[cfg(test)]
mod b8 {
    use super::*;

    /// Pins the deliberate asymmetry documented on `SCHEMA_V1` (review B8).
    ///
    /// A trigger referencing `chunks_vec` compiles only where sqlite-vec is
    /// loaded, and SQLite compiles trigger bodies when they fire — so adding one
    /// would make every delete from `chunks` fail in the `sqlite3` CLI, a backup
    /// script, or any tool that has not registered the extension. The obvious
    /// "fix" for the missing trigger is the bug, which is why this is a test and
    /// not only a comment.
    #[test]
    fn no_trigger_references_the_vec_table() {
        let conn = open_in_memory().expect("open");
        migrate(&conn, 8).expect("migrate");

        let mut stmt = conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger'")
            .expect("prepare");
        let triggers: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();

        assert!(!triggers.is_empty(), "expected the chunks_fts triggers");
        for (name, sql) in &triggers {
            assert!(
                !sql.contains("chunks_vec"),
                "trigger {name} references chunks_vec; see the note on SCHEMA_V1 \
                 — vec cleanup is explicit in upsert_document and prune_orphans"
            );
        }
    }
}

static VEC_INIT: Once = Once::new();

type SqliteAutoExtensionFn = unsafe extern "C" fn(
    *mut rusqlite::ffi::sqlite3,
    *mut *mut std::os::raw::c_char,
    *const rusqlite::ffi::sqlite3_api_routines,
) -> std::os::raw::c_int;

fn register_vec_extension() {
    VEC_INIT.call_once(|| unsafe {
        let init_fn: SqliteAutoExtensionFn =
            std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(init_fn));
    });
}

/// How long a connection waits for a lock before giving up with `SQLITE_BUSY`.
///
/// With WAL enabled readers never block on the writer, so this only governs
/// writer-vs-writer contention — two concurrent `vault index sync` runs. Waiting
/// is the right answer there. The hook is a reader and never pays it.
///
/// Note this value matches what rusqlite already applies on `Connection::open`,
/// so setting it changes nothing today. It is here to pin the number vault
/// depends on rather than inherit a library default that could change without
/// notice — `open_sets_a_busy_timeout` guards it in either direction.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Connection-level setup applied to every vault connection.
///
/// `foreign_keys` and `busy_timeout` are per-connection in SQLite and must be
/// set on each open. `journal_mode` is persisted in the database file, but
/// setting it every time is idempotent and keeps a restored or hand-created
/// `vault.db` from silently running in rollback-journal mode.
///
/// WAL is what lets a `vault hook` read run while a `vault index sync` write is
/// in flight — under the default rollback journal the reader would block, and
/// the hook is on the prompt hot path. It is an availability property, not a
/// correctness one: if the filesystem can't support WAL (network homes are the
/// usual case) the connection keeps working in whatever mode SQLite chose, just
/// with less concurrency. That is why a non-WAL result is not an error.
fn apply_pragmas(conn: &Connection, wal: bool) -> Result<(), StoreError> {
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    if wal {
        // `PRAGMA journal_mode` reports the resulting mode as a result row, so
        // `pragma_update` (which expects no rows back) cannot be used here.
        //
        // The result is deliberately discarded, errors included. Two different
        // failures live here and neither should stop the open:
        //
        //   * SQLite declines the switch and reports the mode it kept — a row,
        //     no error. That was always tolerated.
        //   * SQLite errors outright. A read-only `vault.db` is the reachable
        //     case: the pragma has to write the file header, so it fails with
        //     `attempt to write a readonly database` even though every query
        //     the hook runs is a read.
        //
        // Propagating the second turned a *less concurrent* store into *no*
        // store, which contradicts the paragraph above — WAL is an availability
        // property, not a correctness one. A genuinely broken database still
        // fails, just at the first real query, with an error about that query.
        let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
    }
    Ok(())
}

pub(crate) fn open(path: &Path) -> Result<Connection, StoreError> {
    register_vec_extension();
    let conn = Connection::open(path).map_err(|e| StoreError::Backend(e.to_string()))?;
    apply_pragmas(&conn, true)?;
    Ok(conn)
}

/// Test-only: the sole caller, `SqliteStore::open_in_memory`, is `#[cfg(test)]`.
/// Gated rather than `allow(dead_code)`-ed so the compiler keeps that true.
#[cfg(test)]
pub(crate) fn open_in_memory() -> Result<Connection, StoreError> {
    register_vec_extension();
    let conn = Connection::open_in_memory().map_err(|e| StoreError::Backend(e.to_string()))?;
    // An in-memory database has no journal file; WAL does not apply to it.
    apply_pragmas(&conn, false)?;
    Ok(conn)
}

pub(crate) fn migrate(conn: &Connection, dim: usize) -> Result<(), StoreError> {
    if dim == 0 {
        return Err(StoreError::Migration(
            "embedding dim must be non-zero (chunks_vec FLOAT[0] is invalid DDL)".to_string(),
        ));
    }
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| StoreError::Migration(e.to_string()))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)
            .map_err(|e| StoreError::Migration(e.to_string()))?;
        // chunks_vec is built here, not in SCHEMA_V1: vec0 bakes the dimension
        // into the column at creation and offers no dimensionless mode, so the
        // configured `dim` must be formatted in. `dim` is a config u16 widened to
        // usize, never user text — no injection surface; the `dim == 0` guard
        // above keeps the DDL well-formed. `IF NOT EXISTS` leaves an existing
        // table untouched, so a dim change on a populated DB is caught later by
        // `verify_or_init_embedding` rather than silently re-created here.
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(chunk_id INTEGER PRIMARY KEY, embedding FLOAT[{dim}]);"
        ))
        .map_err(|e| StoreError::Migration(e.to_string()))?;
        conn.pragma_update(None, "user_version", 1)
            .map_err(|e| StoreError::Migration(e.to_string()))?;
    }
    // `version` is read once above, so a fresh database (0) runs both steps in
    // this call while an existing v1 database runs only this one.
    if version < 2 {
        conn.execute_batch(SCHEMA_V2)
            .map_err(|e| StoreError::Migration(e.to_string()))?;
        conn.pragma_update(None, "user_version", 2)
            .map_err(|e| StoreError::Migration(e.to_string()))?;
    }
    Ok(())
}

/// On a fresh DB, record the embedding model + dim into `meta`. On subsequent
/// opens, verify the stored values match what the caller passed. Mismatch
/// returns `IncompatibleEmbedding` with both pairs so the user sees exactly
/// what changed.
///
/// This `meta` record **is** the per-DB embedding lock. `chunks_vec` is created
/// at the configured dim in `migrate` (vec0 fixes the dimension at table
/// creation), so once a DB is initialized at a given `(model, dim)` it must keep
/// using it — re-opening against a different dim would mismatch the vec0 column
/// and silently corrupt retrieval. Well-formedness of the dim (non-zero) is
/// enforced in `migrate`, not here.
pub(crate) fn verify_or_init_embedding(
    conn: &Connection,
    model: &str,
    dim: usize,
) -> Result<(), StoreError> {
    let stored_model: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'embedding_model'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    let stored_dim: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'embedding_dim'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    match (stored_model, stored_dim) {
        (Some(m), Some(d)) => {
            let parsed_dim: usize = d
                .parse()
                .map_err(|e: std::num::ParseIntError| StoreError::Backend(e.to_string()))?;
            if m != model || parsed_dim != dim {
                return Err(StoreError::IncompatibleEmbedding {
                    stored_model: m,
                    stored_dim: parsed_dim,
                    expected_model: model.to_string(),
                    expected_dim: dim,
                });
            }
            Ok(())
        }
        _ => {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('embedding_model', ?1)",
                params![model],
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('embedding_dim', ?1)",
                params![dim.to_string()],
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline dim the non-dynamic tests run at (nomic-embed-text-v1.5). The
    /// production default lives in `Config` (`[embeddings].dims`); this mirrors
    /// it so the schema tests don't depend on the config module.
    const DEFAULT_DIM: usize = 768;

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE name = ?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Unique temp path per test run; the file DB tests need a real file
    /// because WAL does not apply to `:memory:`.
    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "vault-pragma-{tag}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    /// A connection set up the way vault opened databases *before* WAL and
    /// `busy_timeout` were applied: SQLite's defaults. Used to reproduce the
    /// contention the pragmas fix, so the tests below fail for the original
    /// reason rather than passing vacuously.
    fn legacy_open(path: &std::path::Path) -> Connection {
        register_vec_extension();
        let conn = Connection::open(path).expect("legacy open");
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        // Explicit rather than implied: these two are exactly what the fix changed.
        conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))
            .unwrap();
        conn.busy_timeout(Duration::from_millis(0)).unwrap();
        conn
    }

    fn is_busy(e: &rusqlite::Error) -> bool {
        matches!(
            e,
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::DatabaseBusy
                    || f.code == rusqlite::ErrorCode::DatabaseLocked
        )
    }

    /// The defect, reproduced. Under the rollback journal a reader holding a
    /// transaction keeps a SHARED lock, and the writer's COMMIT needs EXCLUSIVE
    /// — so the commit is refused. With no busy timeout that surfaces
    /// immediately as SQLITE_BUSY.
    ///
    /// This is precisely the production case: `vault hook` reads on the prompt
    /// hot path while `vault index sync` writes.
    #[test]
    fn legacy_setup_lets_a_reader_break_a_writers_commit() {
        let path = temp_db_path("legacy-contention");
        let w = legacy_open(&path);
        migrate(&w, DEFAULT_DIM).expect("migrate");
        let r = legacy_open(&path);

        // Reader opens a transaction and touches a table -> holds SHARED.
        r.execute_batch("BEGIN; SELECT count(*) FROM projects;")
            .expect("reader begins");

        // Writer gets RESERVED fine; it is the COMMIT (EXCLUSIVE) that fails.
        w.execute_batch(
            "BEGIN IMMEDIATE; INSERT INTO projects (name, created_at) VALUES ('p', 0);",
        )
        .expect("writer begins");
        let err = w
            .execute_batch("COMMIT;")
            .expect_err("commit must be refused");
        assert!(is_busy(&err), "expected SQLITE_BUSY/LOCKED, got: {err}");

        let _ = r.execute_batch("ROLLBACK;");
        drop(r);
        drop(w);
        let _ = std::fs::remove_file(&path);
    }

    /// The fix. Same shape as the test above, but through `open()`: in WAL the
    /// reader keeps a snapshot instead of a lock, so the writer commits.
    #[test]
    fn wal_lets_a_writer_commit_while_a_reader_holds_a_transaction() {
        let path = temp_db_path("wal-contention");
        let w = open(&path).expect("open writer");
        migrate(&w, DEFAULT_DIM).expect("migrate");
        let r = open(&path).expect("open reader");

        r.execute_batch("BEGIN; SELECT count(*) FROM projects;")
            .expect("reader begins");

        w.execute_batch(
            "BEGIN IMMEDIATE; INSERT INTO projects (name, created_at) VALUES ('p', 0); COMMIT;",
        )
        .expect("writer must commit despite the open reader");

        let _ = r.execute_batch("ROLLBACK;");
        drop(r);
        drop(w);
        let _ = std::fs::remove_file(&path);
    }

    /// Property test, not a fix test. WAL still serializes writer-vs-writer (the
    /// two concurrent `vault index sync` case), so the second writer must *wait*
    /// rather than fail. This holds via rusqlite's default timeout as much as
    /// ours; it pins the behaviour vault relies on.
    #[test]
    fn busy_timeout_makes_a_second_writer_wait_rather_than_fail() {
        use std::sync::mpsc;

        let path = temp_db_path("writer-contention");
        let setup = open(&path).expect("open");
        migrate(&setup, DEFAULT_DIM).expect("migrate");
        drop(setup);

        let (locked_tx, locked_rx) = mpsc::channel();
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || {
            let c = open(&holder_path).expect("holder open");
            c.execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO projects (name, created_at) VALUES ('held', 0);",
            )
            .expect("holder takes the write lock");
            locked_tx.send(()).unwrap();
            // Held briefly — far under the 5s busy timeout, so the waiter wins.
            std::thread::sleep(Duration::from_millis(150));
            c.execute_batch("COMMIT;").expect("holder commits");
        });

        locked_rx.recv().expect("holder signalled");

        // With busy_timeout the contended write waits for the holder and succeeds.
        let waiter = open(&path).expect("waiter open");
        waiter
            .execute_batch("INSERT INTO projects (name, created_at) VALUES ('waited', 0);")
            .expect("must wait for the holder, not fail");

        holder.join().unwrap();
        drop(waiter);
        let _ = std::fs::remove_file(&path);
    }

    /// The same contention with `busy_timeout = 0` fails immediately — proving
    /// the timeout above is what made the difference.
    #[test]
    fn without_busy_timeout_a_contended_write_fails_immediately() {
        let path = temp_db_path("no-timeout");
        let setup = open(&path).expect("open");
        migrate(&setup, DEFAULT_DIM).expect("migrate");

        let holder = open(&path).expect("holder");
        holder
            .execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO projects (name, created_at) VALUES ('held', 0);",
            )
            .expect("holder takes the write lock");

        let impatient = open(&path).expect("impatient");
        impatient.busy_timeout(Duration::from_millis(0)).unwrap();
        let err = impatient
            .execute_batch("INSERT INTO projects (name, created_at) VALUES ('nope', 0);")
            .expect_err("must fail without a busy timeout");
        assert!(is_busy(&err), "expected SQLITE_BUSY/LOCKED, got: {err}");

        let _ = holder.execute_batch("ROLLBACK;");
        drop(holder);
        drop(impatient);
        drop(setup);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_enables_wal_on_a_file_db() {
        // WAL is what lets a `vault hook` read proceed while a `vault index
        // sync` write is in flight. Without it the hook blocks on the prompt
        // hot path.
        let path = temp_db_path("wal");
        let conn = open(&path).expect("open");

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("read journal_mode");
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// Guards the value, not our code path: rusqlite already applies a 5s busy
    /// timeout on open, so this passes with or without `apply_pragmas` setting
    /// it. It is a regression guard against that default changing underneath us.
    #[test]
    fn open_sets_a_busy_timeout() {
        let path = temp_db_path("busy");
        let conn = open(&path).expect("open");

        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("read busy_timeout");
        assert_eq!(timeout, BUSY_TIMEOUT.as_millis() as i64);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// WAL is an availability property, so failing to get it must not fail the
    /// open — the paragraph on `apply_pragmas` says so, and the code used to
    /// contradict it by propagating the pragma's error.
    ///
    /// A read-only database file is the reachable case: `PRAGMA journal_mode=WAL`
    /// rewrites the file header, so SQLite answers "attempt to write a readonly
    /// database" even though every query `vault hook` issues is a read. Under
    /// the old code that turned a *less concurrent* store into *no* store, and
    /// the hook silently injected nothing.
    #[cfg(unix)]
    #[test]
    fn open_succeeds_on_a_read_only_database_that_cannot_take_wal() {
        use std::os::unix::fs::PermissionsExt;

        // root ignores the permission bits, so the premise would not hold.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: running as root, read-only mode is not enforced");
            return;
        }

        let path = temp_db_path("readonly-wal");
        {
            let conn = open(&path).expect("initial open");
            conn.execute("CREATE TABLE probe (x INTEGER)", []).unwrap();
            conn.execute("INSERT INTO probe (x) VALUES (42)", [])
                .unwrap();
            // Land the WAL back in the main file, then leave rollback-journal
            // mode behind so the reopen below genuinely attempts the switch.
            conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))
                .unwrap();
        }
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&path, perms).unwrap();

        let conn = open(&path).expect("a read-only db must still open");
        let x: i64 = conn
            .query_row("SELECT x FROM probe", [], |r| r.get(0))
            .expect("reads must work without WAL");
        assert_eq!(x, 42);

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("read journal_mode");
        assert_ne!(
            mode.to_lowercase(),
            "wal",
            "premise check: the switch has to have actually failed"
        );

        drop(conn);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(&path, perms);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_in_memory_sets_busy_timeout_and_skips_wal() {
        // Covers the `wal: false` branch: an in-memory db has no journal file,
        // so WAL is not requested, but the timeout still applies.
        let conn = open_in_memory().expect("open");

        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("read busy_timeout");
        assert_eq!(timeout, BUSY_TIMEOUT.as_millis() as i64);

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("read journal_mode");
        assert_ne!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn migrate_creates_all_tables() {
        let conn = open_in_memory().expect("open");
        migrate(&conn, DEFAULT_DIM).expect("migrate");

        for t in [
            "projects",
            "documents",
            "chunks",
            "chunks_fts",
            "chunks_vec",
            "retrieval_log",
            "chunks_ai",
            "chunks_au",
            "chunks_ad",
        ] {
            assert!(table_exists(&conn, t), "missing {t}");
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = open_in_memory().expect("open");
        migrate(&conn, DEFAULT_DIM).expect("first migrate");
        migrate(&conn, DEFAULT_DIM).expect("second migrate should be a no-op");

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    fn index_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index' AND tbl_name = 'chunks'
                   AND name LIKE 'idx_chunks_%' ORDER BY name",
            )
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// The three columns `Store::inventory` reads and the plan filters bind
    /// against. Without these, every hook call scans `chunks` — and `content`
    /// is inline, so the scan pages in the whole corpus.
    #[test]
    fn migrate_creates_the_inventory_indexes() {
        let conn = open_in_memory().expect("open");
        migrate(&conn, DEFAULT_DIM).expect("migrate");

        assert_eq!(
            index_names(&conn),
            vec![
                "idx_chunks_doc_type".to_string(),
                "idx_chunks_language".to_string(),
                "idx_chunks_project_id".to_string(),
            ]
        );
    }

    /// The upgrade path, which the fresh-database test cannot cover: a store
    /// created before v2 must gain the indexes on its next open, without the
    /// table being rebuilt.
    #[test]
    fn an_existing_v1_database_gains_the_indexes_without_losing_rows() {
        let conn = open_in_memory().expect("open");
        // Stop at v1: apply exactly what a pre-v2 vault would have.
        conn.execute_batch(SCHEMA_V1).expect("v1 schema");
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(chunk_id INTEGER PRIMARY KEY, embedding FLOAT[{DEFAULT_DIM}]);"
        ))
        .expect("vec table");
        conn.pragma_update(None, "user_version", 1).unwrap();

        conn.execute(
            "INSERT INTO projects (name, repo_path, created_at) VALUES ('p', '/p', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (project_id, doc_type, source_path, title, content_hash, created_at, updated_at)
             VALUES (1, 'plan', 'a.md', 'a', 'h', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks (document_id, project_id, doc_type, language, label, content, content_hash, token_est, chunk_index, created_at)
             VALUES (1, 1, 'plan', 'markdown', 'l', 'c', 'ch', 1, 0, 0)",
            [],
        )
        .unwrap();

        assert!(index_names(&conn).is_empty(), "precondition: v1 has none");

        migrate(&conn, DEFAULT_DIM).expect("upgrade to v2");

        assert_eq!(index_names(&conn).len(), 3, "indexes added on upgrade");
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunks, 1, "adding an index must not touch the rows");
    }

    /// The whole point of the indexes: the inventory queries must stop reading
    /// the table. A covering index keeps `content` — stored inline — off the
    /// page cache entirely.
    #[test]
    fn the_inventory_queries_are_served_by_a_covering_index() {
        let conn = open_in_memory().expect("open");
        migrate(&conn, DEFAULT_DIM).expect("migrate");

        use crate::store::sqlite_store::{
            INVENTORY_DOC_TYPES_SQL, INVENTORY_LANGUAGES_SQL, INVENTORY_PROJECTS_SQL,
        };

        for (sql, idx, col) in [
            (INVENTORY_LANGUAGES_SQL, "idx_chunks_language", "language"),
            (INVENTORY_DOC_TYPES_SQL, "idx_chunks_doc_type", "doc_type"),
        ] {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let steps: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();

            // `SEARCH ... (col>?)` is the loose index scan seeking past each
            // value it has already seen. A plain `SELECT DISTINCT` plans to
            // `SCAN ... USING COVERING INDEX` instead — also index-only, but it
            // visits every entry to find a handful of answers. Asserting on
            // "COVERING INDEX" alone cannot tell those apart.
            let seek = format!("({col}>?)");
            assert!(
                steps
                    .iter()
                    .any(|s| s.contains("SEARCH") && s.contains(idx) && s.contains(&seek)),
                "expected a seek per distinct value on {idx}, got: {steps:?}"
            );
        }

        // The projects query must probe the index per project, not scan every
        // chunk row and deduplicate — that rewrite is the bulk of its win.
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {INVENTORY_PROJECTS_SQL}"))
            .unwrap();
        let steps: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        // The `EXISTS` marker is the load-bearing part: it means SQLite stops
        // at the first chunk of each project. A `JOIN ... DISTINCT` plans to
        // the same join order once statistics exist, but emits every matching
        // row into a temp B-tree to deduplicate — 31ms against 0.26ms on the
        // 120k corpus, *after* ANALYZE. Asserting only on the index name
        // cannot tell the two apart.
        assert!(
            steps
                .iter()
                .any(|s| s.contains("EXISTS") && s.contains("idx_chunks_project_id")),
            "expected an early-terminating EXISTS probe, got: {steps:?}"
        );
    }

    #[test]
    fn projects_table_has_domain_column() {
        let conn = open_in_memory().expect("open");
        migrate(&conn, DEFAULT_DIM).expect("migrate");

        // Column exists, is selectable, and a row inserted without it is NULL
        // (= unassigned; the hook falls back to defaults.context_tag).
        conn.execute(
            "INSERT INTO projects (name, repo_path, created_at) VALUES ('p', '/tmp/p', 0)",
            [],
        )
        .expect("insert project");
        let domain: Option<String> = conn
            .query_row("SELECT domain FROM projects WHERE name = 'p'", [], |r| {
                r.get(0)
            })
            .expect("select domain");
        assert_eq!(domain, None);
    }

    #[test]
    fn vec_extension_loaded() {
        let conn = open_in_memory().expect("open");
        migrate(&conn, DEFAULT_DIM).expect("migrate");
        let dim: i64 = conn
            .query_row("SELECT count(*) FROM chunks_vec", [], |r| r.get(0))
            .expect("query vec table");
        assert_eq!(dim, 0);
    }

    fn fresh_db() -> Connection {
        let conn = open_in_memory().expect("open");
        migrate(&conn, DEFAULT_DIM).expect("migrate");
        conn
    }

    #[test]
    fn verify_or_init_embedding_initializes_on_fresh_db() {
        let conn = fresh_db();
        verify_or_init_embedding(&conn, "nomic-v1.5", DEFAULT_DIM).expect("init");

        let model: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'embedding_model'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let dim: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'embedding_dim'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(model, "nomic-v1.5");
        assert_eq!(dim, DEFAULT_DIM.to_string());
    }

    #[test]
    fn verify_or_init_embedding_is_idempotent_when_matching() {
        let conn = fresh_db();
        verify_or_init_embedding(&conn, "nomic-v1.5", DEFAULT_DIM).expect("first");
        verify_or_init_embedding(&conn, "nomic-v1.5", DEFAULT_DIM).expect("second");
    }

    #[test]
    fn verify_or_init_embedding_rejects_model_mismatch() {
        let conn = fresh_db();
        verify_or_init_embedding(&conn, "nomic-v1.5", DEFAULT_DIM).expect("init");

        let err = verify_or_init_embedding(&conn, "different-model", DEFAULT_DIM)
            .expect_err("should reject");
        match err {
            StoreError::IncompatibleEmbedding {
                stored_model,
                expected_model,
                ..
            } => {
                assert_eq!(stored_model, "nomic-v1.5");
                assert_eq!(expected_model, "different-model");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn verify_or_init_embedding_rejects_reopen_at_different_dim() {
        // The lock is now per-DB, recorded in `meta` — not a schema constant.
        // A DB initialized at one dim must reject a reopen at another, because
        // the vec0 column is fixed at the dim it was created with. (A fresh DB at
        // 1024 is perfectly valid now; that's the whole point of this change.)
        let conn = fresh_db();
        verify_or_init_embedding(&conn, "nomic-v1.5", DEFAULT_DIM).expect("init at default dim");

        let err = verify_or_init_embedding(&conn, "nomic-v1.5", 1024)
            .expect_err("reopen at a different dim must reject");
        match err {
            StoreError::IncompatibleEmbedding {
                stored_dim,
                expected_dim,
                ..
            } => {
                assert_eq!(stored_dim, DEFAULT_DIM);
                assert_eq!(expected_dim, 1024);
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn migrate_builds_vec_table_at_configured_dim() {
        // Empirical proof that sqlite-vec accepts an arbitrary FLOAT[N] — the one
        // assumption code can't confirm. Build chunks_vec at 1024, round-trip a
        // 1024-element vector through insert + vec_distance_cosine. JSON-array
        // insert mirrors the production path (`embedding_to_json` in
        // sqlite_store), kept inline here since that helper is private there.
        let dim = 1024usize;
        let conn = open_in_memory().expect("open");
        migrate(&conn, dim).expect("migrate at 1024");

        let vec_json = format!("[{}]", vec!["0.5"; dim].join(","));
        conn.execute(
            "INSERT INTO chunks_vec (chunk_id, embedding) VALUES (1, ?1)",
            params![vec_json],
        )
        .expect("insert 1024-dim vector");

        // Identical query vector → cosine distance ~0. The query running at all
        // proves the column is FLOAT[1024], not FLOAT[768].
        let dist: f64 = conn
            .query_row(
                "SELECT vec_distance_cosine(embedding, ?1) FROM chunks_vec WHERE chunk_id = 1",
                params![vec_json],
                |r| r.get(0),
            )
            .expect("cosine query at dim 1024");
        assert!(dist.abs() < 1e-6, "expected ~0 distance, got {dist}");
    }

    #[test]
    fn migrate_rejects_zero_dim() {
        let conn = open_in_memory().expect("open");
        let err = migrate(&conn, 0).expect_err("dim 0 is invalid DDL");
        assert!(matches!(err, StoreError::Migration(_)));
    }
}
