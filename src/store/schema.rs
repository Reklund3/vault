use std::path::Path;
use std::sync::Once;

use rusqlite::{Connection, OptionalExtension, params};

use crate::store::traits::StoreError;

/// chunks_vec is locked at FLOAT[768] in the DDL above. Runtime code reads dim
/// from config; this constant exists so verify_or_init_embedding can refuse to
/// set up a DB whose configured dim doesn't match the schema lock — failing
/// fast at startup beats silently producing vec0 errors later.
pub(crate) const SCHEMA_LOCKED_DIM: usize = 768;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    repo_path  TEXT,
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

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    chunk_id  INTEGER PRIMARY KEY,
    embedding FLOAT[768]
);

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

pub(crate) fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| StoreError::Migration(e.to_string()))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)
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
/// Also refuses configured dims that don't match the schema lock — `chunks_vec
/// FLOAT[768]` is fixed at table creation, so 1024-in-config + 768-in-schema
/// can only end in tears later.
pub(crate) fn verify_or_init_embedding(
    conn: &Connection,
    model: &str,
    dim: usize,
) -> Result<(), StoreError> {
    if dim != SCHEMA_LOCKED_DIM {
        return Err(StoreError::IncompatibleEmbedding {
            stored_model: format!("schema-locked at dim {SCHEMA_LOCKED_DIM}"),
            stored_dim: SCHEMA_LOCKED_DIM,
            expected_model: model.to_string(),
            expected_dim: dim,
        });
    }

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
        migrate(&conn).expect("migrate");

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
        migrate(&conn).expect("first migrate");
        migrate(&conn).expect("second migrate should be a no-op");

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn vec_extension_loaded() {
        let conn = open_in_memory().expect("open");
        migrate(&conn).expect("migrate");
        let dim: i64 = conn
            .query_row("SELECT count(*) FROM chunks_vec", [], |r| r.get(0))
            .expect("query vec table");
        assert_eq!(dim, 0);
    }

    fn fresh_db() -> Connection {
        let conn = open_in_memory().expect("open");
        migrate(&conn).expect("migrate");
        conn
    }

    #[test]
    fn verify_or_init_embedding_initializes_on_fresh_db() {
        let conn = fresh_db();
        verify_or_init_embedding(&conn, "nomic-v1.5", SCHEMA_LOCKED_DIM).expect("init");

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
        assert_eq!(dim, SCHEMA_LOCKED_DIM.to_string());
    }

    #[test]
    fn verify_or_init_embedding_is_idempotent_when_matching() {
        let conn = fresh_db();
        verify_or_init_embedding(&conn, "nomic-v1.5", SCHEMA_LOCKED_DIM).expect("first");
        verify_or_init_embedding(&conn, "nomic-v1.5", SCHEMA_LOCKED_DIM).expect("second");
    }

    #[test]
    fn verify_or_init_embedding_rejects_model_mismatch() {
        let conn = fresh_db();
        verify_or_init_embedding(&conn, "nomic-v1.5", SCHEMA_LOCKED_DIM).expect("init");

        let err = verify_or_init_embedding(&conn, "different-model", SCHEMA_LOCKED_DIM)
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
    fn verify_or_init_embedding_rejects_dim_not_matching_schema_lock() {
        let conn = fresh_db();
        let err = verify_or_init_embedding(&conn, "nomic-v1.5", 1024).expect_err("should reject");
        match err {
            StoreError::IncompatibleEmbedding {
                stored_dim,
                expected_dim,
                ..
            } => {
                assert_eq!(stored_dim, SCHEMA_LOCKED_DIM);
                assert_eq!(expected_dim, 1024);
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }
}
