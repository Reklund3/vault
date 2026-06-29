use std::path::Path;
use std::sync::Once;

use rusqlite::{Connection, OptionalExtension, params};

use crate::store::traits::StoreError;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    repo_path  TEXT,
    -- domain assignment (NULL = unassigned -> hook falls back to
    -- defaults.context_tag). Interactive runtime state vault writes during sync;
    -- only the name is stored, the context tag is derived as `{domain}-context`.
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
"#;

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

pub(crate) fn open(path: &Path) -> Result<Connection, StoreError> {
    register_vec_extension();
    let conn = Connection::open(path).map_err(|e| StoreError::Backend(e.to_string()))?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    Ok(conn)
}

#[allow(dead_code)]
pub(crate) fn open_in_memory() -> Result<Connection, StoreError> {
    register_vec_extension();
    let conn = Connection::open_in_memory().map_err(|e| StoreError::Backend(e.to_string()))?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|e| StoreError::Backend(e.to_string()))?;
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
        assert_eq!(version, 1);
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
