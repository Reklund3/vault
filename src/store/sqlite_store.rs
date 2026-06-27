use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, ToSql, params};

use crate::config::Config;
use crate::retrieve::QueryPlan;
use crate::store::schema;
use crate::store::traits::{Store, StoreError};
use crate::store::types::{ChunkWithEmbedding, Document, Hit, RetrievalLogEntry};
use crate::types::DocType;

pub struct SqliteStore {
    conn: Connection,
    embedding_dim: usize,
}

impl SqliteStore {
    pub fn open(path: &Path, config: &Config) -> Result<Self, StoreError> {
        let conn = schema::open(path)?;
        let mut store = SqliteStore {
            conn,
            embedding_dim: config.embedding_dim(),
        };
        store.migrate()?;
        schema::verify_or_init_embedding(
            &store.conn,
            config.embedding_model(),
            config.embedding_dim(),
        )?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory(config: &Config) -> Result<Self, StoreError> {
        let conn = schema::open_in_memory()?;
        let mut store = SqliteStore {
            conn,
            embedding_dim: config.embedding_dim(),
        };
        store.migrate()?;
        schema::verify_or_init_embedding(
            &store.conn,
            config.embedding_model(),
            config.embedding_dim(),
        )?;
        Ok(store)
    }

    /// Resolve router-supplied project names to the ids that actually exist in
    /// the store, matched case-insensitively (ASCII). Names with no matching
    /// project are silently dropped; an empty `names` (or all-unknown names)
    /// yields an empty vec, which `build_filter_clause` reads as "no project
    /// filter — search across all projects". Names are bound as parameters, not
    /// formatted into SQL.
    fn existing_project_ids(&self, names: &[String]) -> Result<Vec<i64>, StoreError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id FROM projects WHERE name COLLATE NOCASE IN ({})",
            placeholders(names.len())
        );
        let params: Vec<&dyn ToSql> = names.iter().map(|n| n as &dyn ToSql).collect();
        let mut stmt = self.conn.prepare(&sql).map_err(backend_err)?;
        let ids = stmt
            .query_map(params.as_slice(), |r| r.get::<_, i64>(0))
            .map_err(backend_err)?
            .collect::<Result<Vec<i64>, _>>()
            .map_err(backend_err)?;
        Ok(ids)
    }
}

impl Store for SqliteStore {
    fn migrate(&mut self) -> Result<(), StoreError> {
        schema::migrate(&self.conn)
    }

    fn get_or_create_project(&mut self, name: &str, repo_path: &str) -> Result<i64, StoreError> {
        let tx = self.conn.transaction().map_err(backend_err)?;

        let existing: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT id, repo_path FROM projects WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(backend_err)?;

        let id = match existing {
            None => {
                let new_id: i64 = tx
                    .query_row(
                        "INSERT INTO projects (name, repo_path, created_at)
                         VALUES (?1, ?2, ?3) RETURNING id",
                        params![name, repo_path, now_secs()],
                        |r| r.get(0),
                    )
                    .map_err(backend_err)?;
                new_id
            }
            Some((id, None)) => id, // NULL on existing row matches anything
            Some((id, Some(existing_path))) if existing_path == repo_path => id,
            Some((_, Some(existing_path))) => {
                return Err(StoreError::Conflict(format!(
                    "project name {name:?} already registered at {existing_path:?}; \
                     this sync targets {repo_path:?}. Pass `--name <unique>` to register a separate project."
                )));
            }
        };

        tx.commit().map_err(backend_err)?;
        Ok(id)
    }

    fn get_document_content_hash(
        &self,
        project_id: i64,
        source_path: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT content_hash FROM documents
                 WHERE project_id = ?1 AND source_path = ?2",
                params![project_id, source_path],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(backend_err)
    }

    fn resolve_domain(&self, project_names: &[String]) -> Result<Option<String>, StoreError> {
        // First named project (in router order) with a non-NULL domain wins —
        // mirrors the old config-side "first listed project" policy. The name
        // list is small (the router returns a handful), so a query per name is
        // fine and keeps the order-preserving logic trivial.
        for name in project_names {
            // Outer Option = row found?; inner Option = domain column NULL?
            let row: Option<Option<String>> = self
                .conn
                .query_row(
                    "SELECT domain FROM projects WHERE name = ?1 COLLATE NOCASE",
                    params![name],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(backend_err)?;
            if let Some(Some(domain)) = row {
                return Ok(Some(domain));
            }
        }
        Ok(None)
    }

    fn set_project_domain(&mut self, project_id: i64, domain: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "UPDATE projects SET domain = ?1 WHERE id = ?2",
                params![domain, project_id],
            )
            .map_err(backend_err)?;
        Ok(())
    }

    fn upsert_document(
        &mut self,
        doc: &Document,
        chunks: &[ChunkWithEmbedding],
    ) -> Result<(), StoreError> {
        for (i, c) in chunks.iter().enumerate() {
            if c.embedding.len() != self.embedding_dim {
                return Err(StoreError::InvalidInput(format!(
                    "chunk {i} embedding dim {}, expected {}",
                    c.embedding.len(),
                    self.embedding_dim
                )));
            }
        }

        let tx = self.conn.transaction().map_err(backend_err)?;
        let now = now_secs();

        let doc_id: i64 = tx
            .query_row(
                "INSERT INTO documents
                    (project_id, doc_type, source_path, title, content_hash, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(project_id, source_path) DO UPDATE SET
                    doc_type     = excluded.doc_type,
                    title        = excluded.title,
                    content_hash = excluded.content_hash,
                    updated_at   = excluded.updated_at
                 RETURNING id",
                params![
                    doc.project_id,
                    doc.doc_type.as_str(),
                    doc.source_path,
                    doc.title,
                    doc.content_hash,
                    now,
                ],
                |r| r.get(0),
            )
            .map_err(backend_err)?;

        // chunks_vec is a virtual table — no FK cascade. Wipe its rows first.
        tx.execute(
            "DELETE FROM chunks_vec WHERE chunk_id IN
                (SELECT id FROM chunks WHERE document_id = ?1)",
            [doc_id],
        )
        .map_err(backend_err)?;
        tx.execute("DELETE FROM chunks WHERE document_id = ?1", [doc_id])
            .map_err(backend_err)?;

        for c in chunks {
            let chunk_id: i64 = tx
                .query_row(
                    "INSERT INTO chunks
                        (document_id, project_id, doc_type, language, label, content,
                         content_hash, token_est, chunk_index, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     RETURNING id",
                    params![
                        doc_id,
                        doc.project_id,
                        doc.doc_type.as_str(),
                        c.chunk.language.as_str(),
                        c.chunk.label,
                        c.chunk.content,
                        c.chunk.content_hash,
                        c.chunk.token_est,
                        c.chunk.chunk_index,
                        now,
                    ],
                    |r| r.get(0),
                )
                .map_err(backend_err)?;

            tx.execute(
                "INSERT INTO chunks_vec (chunk_id, embedding) VALUES (?1, ?2)",
                params![chunk_id, embedding_to_json(&c.embedding)],
            )
            .map_err(backend_err)?;
        }

        tx.commit().map_err(backend_err)?;
        Ok(())
    }

    fn prune_orphans(
        &mut self,
        project_id: i64,
        kept_paths: &[String],
    ) -> Result<usize, StoreError> {
        let tx = self.conn.transaction().map_err(backend_err)?;

        // The placeholder string only contains '?' and ',' — no user data formatted into SQL.
        // Values are still parameter-bound below.
        let (kept_clause, kept_params): (String, Vec<&dyn ToSql>) = if kept_paths.is_empty() {
            (String::new(), Vec::new())
        } else {
            (
                format!(
                    " AND source_path NOT IN ({})",
                    placeholders(kept_paths.len())
                ),
                kept_paths.iter().map(|s| s as &dyn ToSql).collect(),
            )
        };

        // 1. Drop chunks_vec rows for orphan documents before the docs go (no FK cascade).
        let vec_sql = format!(
            "DELETE FROM chunks_vec WHERE chunk_id IN (
                SELECT c.id FROM chunks c
                JOIN documents d ON c.document_id = d.id
                WHERE d.project_id = ?1{kept_clause}
             )"
        );
        let mut vec_params: Vec<&dyn ToSql> = vec![&project_id];
        vec_params.extend(kept_params.iter().copied());
        tx.execute(&vec_sql, vec_params.as_slice())
            .map_err(backend_err)?;

        // 2. Delete documents. chunks cascade via FK; chunks_fts cascades via trigger.
        let doc_sql = format!("DELETE FROM documents WHERE project_id = ?1{kept_clause}");
        let mut doc_params: Vec<&dyn ToSql> = vec![&project_id];
        doc_params.extend(kept_params.iter().copied());
        let removed = tx
            .execute(&doc_sql, doc_params.as_slice())
            .map_err(backend_err)?;

        tx.commit().map_err(backend_err)?;
        Ok(removed)
    }

    fn bm25_search(&self, plan: &QueryPlan, top_k: usize) -> Result<Vec<Hit>, StoreError> {
        // No keyword tokens → no BM25 arm. Cosine still runs in the merge.
        let Some(match_q) = build_match_query(plan) else {
            return Ok(Vec::new());
        };

        // Resolve the router's project names to the subset that actually exists
        // (case-insensitive). The filter is applied only when this is non-empty,
        // so both "router named no project" and "router named only unknown
        // projects" collapse to the same no-filter path — see existing_project_ids.
        let project_ids = self.existing_project_ids(&plan.projects)?;
        let doc_type_strs: Vec<&'static str> = plan.doc_types.iter().map(|d| d.as_str()).collect();
        let language_strs: Vec<&'static str> = plan.languages.iter().map(|l| l.as_str()).collect();
        let filter_sql = build_filter_clause(&project_ids, plan);
        let filter = filter_bind_params(&project_ids, &doc_type_strs, &language_strs);

        let sql = format!(
            "SELECT c.id, c.label, c.content, c.token_est, c.project_id, c.doc_type,
                    -rank AS bm25_score
             FROM chunks_fts
             JOIN chunks c ON c.id = chunks_fts.rowid
             WHERE chunks_fts MATCH ?1{filter_sql}
             ORDER BY rank LIMIT {top_k}"
        );
        let mut params: Vec<&dyn ToSql> = vec![&match_q];
        params.extend(filter.iter().copied());

        let mut stmt = self.conn.prepare(&sql).map_err(backend_err)?;
        let rows = stmt
            .query_map(params.as_slice(), map_hit_row)
            .map_err(backend_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (mut hit, score) = row.map_err(backend_err)?;
            hit.bm25_score = score;
            out.push(hit);
        }
        Ok(out)
    }

    fn cosine_search(
        &self,
        plan: &QueryPlan,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<Hit>, StoreError> {
        if embedding.len() != self.embedding_dim {
            return Err(StoreError::InvalidInput(format!(
                "embedding dim {}, expected {}",
                embedding.len(),
                self.embedding_dim
            )));
        }

        // Resolve the router's project names to the subset that actually exists
        // (case-insensitive). The filter is applied only when this is non-empty,
        // so both "router named no project" and "router named only unknown
        // projects" collapse to the same no-filter path — see existing_project_ids.
        let project_ids = self.existing_project_ids(&plan.projects)?;
        let doc_type_strs: Vec<&'static str> = plan.doc_types.iter().map(|d| d.as_str()).collect();
        let language_strs: Vec<&'static str> = plan.languages.iter().map(|l| l.as_str()).collect();
        let filter_sql = build_filter_clause(&project_ids, plan);
        let filter = filter_bind_params(&project_ids, &doc_type_strs, &language_strs);
        let emb_json = embedding_to_json(embedding);

        let sql = format!(
            "SELECT c.id, c.label, c.content, c.token_est, c.project_id, c.doc_type,
                    1.0 - vec_distance_cosine(v.embedding, ?1) AS cos
             FROM chunks_vec v
             JOIN chunks c ON c.id = v.chunk_id
             WHERE 1=1{filter_sql}
             ORDER BY vec_distance_cosine(v.embedding, ?1) LIMIT {top_k}"
        );
        let mut params: Vec<&dyn ToSql> = vec![&emb_json];
        params.extend(filter.iter().copied());

        let mut stmt = self.conn.prepare(&sql).map_err(backend_err)?;
        let rows = stmt
            .query_map(params.as_slice(), map_hit_row)
            .map_err(backend_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (mut hit, cos) = row.map_err(backend_err)?;
            hit.cosine_score = cos;
            out.push(hit);
        }
        Ok(out)
    }

    fn log_retrieval(&mut self, entry: &RetrievalLogEntry) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO retrieval_log
                    (prompt_hash, query_plan, chunks_returned, tokens_injected, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    entry.prompt_hash,
                    entry.query_plan,
                    entry.chunks_returned,
                    entry.tokens_injected,
                    now_secs(),
                ],
            )
            .map_err(backend_err)?;
        Ok(())
    }
}

// ---------- helpers ----------

fn backend_err(e: rusqlite::Error) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn embedding_to_json(emb: &[f32]) -> String {
    serde_json::to_string(emb).unwrap_or_else(|_| "[]".to_string())
}

fn placeholders(n: usize) -> String {
    (0..n).map(|_| "?").collect::<Vec<_>>().join(",")
}

fn escape_fts_token(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn build_match_query(plan: &QueryPlan) -> Option<String> {
    let mut tokens: Vec<String> = Vec::new();
    for t in &plan.type_names {
        tokens.push(escape_fts_token(t));
    }
    for t in &plan.topics {
        tokens.push(escape_fts_token(t));
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

/// Build the positional bind params matching `build_filter_clause`'s `?`
/// placeholders, in the same order: project ids, then doc_types, then languages.
/// Borrows the caller's resolved id slice and interned label slices, so all
/// three must outlive the query. Shared by `bm25_search` and `cosine_search` so
/// the clause and its params can never drift out of sync.
fn filter_bind_params<'a>(
    project_ids: &'a [i64],
    doc_type_strs: &'a [&'static str],
    language_strs: &'a [&'static str],
) -> Vec<&'a dyn ToSql> {
    let mut p: Vec<&dyn ToSql> = Vec::new();
    for id in project_ids {
        p.push(id);
    }
    for s in doc_type_strs {
        p.push(s);
    }
    for s in language_strs {
        p.push(s);
    }
    p
}

/// Builds the `AND ...` filter suffix. `project_ids` are pre-resolved to ids
/// that exist in the store (see `existing_project_ids`), so the project clause
/// is a plain `IN (<ids>)` — casing is handled at resolution time, not here.
/// An empty `project_ids` emits no project clause: that is the deliberate
/// graceful-degradation path. When the router names projects but none of them
/// exist (e.g. it emitted a prompt phrase instead of an indexed project name),
/// the resolved set is empty and retrieval searches **across all projects**
/// rather than filtering everything out — strictly better than the silent total
/// context loss it used to cause (the same failure the `COLLATE NOCASE` fix
/// addressed for casing; this closes the wider phantom-name gap).
fn build_filter_clause(project_ids: &[i64], plan: &QueryPlan) -> String {
    let mut s = String::new();
    if !project_ids.is_empty() {
        s.push_str(&format!(
            " AND c.project_id IN ({})",
            placeholders(project_ids.len())
        ));
    }
    if !plan.doc_types.is_empty() {
        s.push_str(&format!(
            " AND c.doc_type IN ({})",
            placeholders(plan.doc_types.len())
        ));
    }
    if !plan.languages.is_empty() {
        s.push_str(&format!(
            " AND c.language IN ({})",
            placeholders(plan.languages.len())
        ));
    }
    s
}

fn doc_type_from_str(s: &str) -> Result<DocType, rusqlite::Error> {
    match s {
        "contract" => Ok(DocType::Contract),
        "plan" => Ok(DocType::Plan),
        "convention" => Ok(DocType::Convention),
        "meta" => Ok(DocType::Meta),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            format!("unknown doc_type: {other}").into(),
        )),
    }
}

fn map_hit_row(r: &rusqlite::Row) -> rusqlite::Result<(Hit, f32)> {
    let doc_type_s: String = r.get(5)?;
    let hit = Hit {
        chunk_id: r.get(0)?,
        label: r.get(1)?,
        content: r.get(2)?,
        token_est: r.get::<_, i64>(3)? as u32,
        project_id: r.get(4)?,
        doc_type: doc_type_from_str(&doc_type_s)?,
        bm25_score: 0.0,
        cosine_score: 0.0,
        final_score: 0.0,
    };
    let score: f64 = r.get(6)?;
    Ok((hit, score as f32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::types::Chunk;
    use crate::types::Language;

    fn unit_embedding(idx: usize) -> Vec<f32> {
        let dim = Config::default().embedding_dim();
        let mut v = vec![0.0; dim];
        v[idx] = 1.0;
        v
    }

    fn create_project(store: &SqliteStore, name: &str) -> i64 {
        store
            .conn
            .execute(
                "INSERT INTO projects (name, created_at) VALUES (?1, ?2)",
                params![name, now_secs()],
            )
            .unwrap();
        store.conn.last_insert_rowid()
    }

    fn proto_chunk(label: &str, content: &str, idx: u32) -> Chunk {
        Chunk {
            language: Language::Proto,
            label: label.to_string(),
            content: content.to_string(),
            content_hash: format!("hash-{label}"),
            token_est: 10,
            chunk_index: idx,
        }
    }

    fn set_domain(store: &SqliteStore, project_id: i64, domain: &str) {
        store
            .conn
            .execute(
                "UPDATE projects SET domain = ?1 WHERE id = ?2",
                params![domain, project_id],
            )
            .unwrap();
    }

    #[test]
    fn resolve_domain_returns_first_assigned_project_in_order() {
        let store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        create_project(&store, "docs-site"); // left unassigned (domain NULL)
        let assigned = create_project(&store, "build-service");
        set_domain(&store, assigned, "software");

        // The unassigned project is listed first but skipped; the assigned one wins.
        let domain = store
            .resolve_domain(&["docs-site".to_string(), "build-service".to_string()])
            .unwrap();
        assert_eq!(domain, Some("software".to_string()));
    }

    #[test]
    fn resolve_domain_is_case_insensitive_on_project_name() {
        let store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let id = create_project(&store, "Build-Service");
        set_domain(&store, id, "software");

        let domain = store
            .resolve_domain(&["BUILD-SERVICE".to_string()])
            .unwrap();
        assert_eq!(domain, Some("software".to_string()));
    }

    #[test]
    fn set_project_domain_persists_and_is_resolvable() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let id = create_project(&store, "build-service");
        store.set_project_domain(id, "software").unwrap();
        assert_eq!(
            store
                .resolve_domain(&["build-service".to_string()])
                .unwrap(),
            Some("software".to_string())
        );
    }

    #[test]
    fn set_project_domain_overwrites_existing() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let id = create_project(&store, "build-service");
        store.set_project_domain(id, "software").unwrap();
        store.set_project_domain(id, "finance").unwrap();
        assert_eq!(
            store
                .resolve_domain(&["build-service".to_string()])
                .unwrap(),
            Some("finance".to_string())
        );
    }

    #[test]
    fn resolve_domain_returns_none_when_unassigned_or_unknown() {
        let store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        create_project(&store, "build-service"); // domain stays NULL

        // Unassigned project, unknown name, and empty list all resolve to None
        // (the hook then uses defaults.context_tag).
        assert_eq!(
            store
                .resolve_domain(&["build-service".to_string()])
                .unwrap(),
            None
        );
        assert_eq!(
            store.resolve_domain(&["nonexistent".to_string()]).unwrap(),
            None
        );
        assert_eq!(store.resolve_domain(&[]).unwrap(), None);
    }

    #[test]
    fn upsert_inserts_chunks_and_embeddings() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let project_id = create_project(&store, "build-service");

        let doc = Document {
            project_id,
            doc_type: DocType::Contract,
            source_path: "build.proto".into(),
            title: "build.proto".into(),
            content_hash: "doc-hash-1".into(),
        };
        let chunks = vec![
            ChunkWithEmbedding {
                chunk: proto_chunk("BuildRequest", "message BuildRequest { string id = 1; }", 0),
                embedding: unit_embedding(0),
            },
            ChunkWithEmbedding {
                chunk: proto_chunk("BuildResponse", "message BuildResponse { bool ok = 1; }", 1),
                embedding: unit_embedding(1),
            },
        ];

        store.upsert_document(&doc, &chunks).unwrap();

        let chunk_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        let vec_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        let fts_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunk_count, 2);
        assert_eq!(vec_count, 2);
        assert_eq!(fts_count, 2);
    }

    #[test]
    fn upsert_replaces_existing_chunks() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let project_id = create_project(&store, "p");

        let mut doc = Document {
            project_id,
            doc_type: DocType::Contract,
            source_path: "f.proto".into(),
            title: "f".into(),
            content_hash: "h1".into(),
        };
        store
            .upsert_document(
                &doc,
                &[ChunkWithEmbedding {
                    chunk: proto_chunk("A", "x", 0),
                    embedding: unit_embedding(0),
                }],
            )
            .unwrap();

        doc.content_hash = "h2".into();
        store
            .upsert_document(
                &doc,
                &[ChunkWithEmbedding {
                    chunk: proto_chunk("B", "y", 0),
                    embedding: unit_embedding(2),
                }],
            )
            .unwrap();

        let chunk_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        let vec_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunk_count, 1);
        assert_eq!(vec_count, 1, "chunks_vec must not leak after replace");
    }

    #[test]
    fn hybrid_search_ranks_keyword_and_semantic_match() {
        let config = Config::default();
        let mut store = SqliteStore::open_in_memory(&config).unwrap();
        let project_id = create_project(&store, "build-service");

        let doc = Document {
            project_id,
            doc_type: DocType::Contract,
            source_path: "build.proto".into(),
            title: "build.proto".into(),
            content_hash: "h".into(),
        };
        store
            .upsert_document(
                &doc,
                &[
                    ChunkWithEmbedding {
                        chunk: proto_chunk(
                            "BuildRequest",
                            "message BuildRequest { string id = 1; }",
                            0,
                        ),
                        embedding: unit_embedding(0),
                    },
                    ChunkWithEmbedding {
                        chunk: proto_chunk(
                            "BuildResponse",
                            "message BuildResponse { bool ok = 1; }",
                            1,
                        ),
                        embedding: unit_embedding(1),
                    },
                ],
            )
            .unwrap();

        let plan = QueryPlan {
            projects: vec!["build-service".into()],
            type_names: vec!["BuildRequest".into()],
            topics: vec![],
            doc_types: vec![DocType::Contract],
            languages: vec![Language::Proto],
        };
        let hits = store
            .hybrid_search(&plan, &unit_embedding(0), config.alpha())
            .unwrap();

        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0].label, "BuildRequest");
        assert!(hits[0].bm25_score > 0.0);
        assert!(hits[0].cosine_score > 0.99);
    }

    #[test]
    fn hybrid_search_falls_back_to_cosine_when_no_keywords() {
        let config = Config::default();
        let mut store = SqliteStore::open_in_memory(&config).unwrap();
        let project_id = create_project(&store, "p");

        store
            .upsert_document(
                &Document {
                    project_id,
                    doc_type: DocType::Convention,
                    source_path: "a.go".into(),
                    title: "a".into(),
                    content_hash: "h".into(),
                },
                &[ChunkWithEmbedding {
                    chunk: Chunk {
                        language: Language::Go,
                        label: "Foo".into(),
                        content: "func Foo() {}".into(),
                        content_hash: "ch".into(),
                        token_est: 5,
                        chunk_index: 0,
                    },
                    embedding: unit_embedding(0),
                }],
            )
            .unwrap();

        let plan = QueryPlan {
            projects: vec![],
            type_names: vec![],
            topics: vec![],
            doc_types: vec![],
            languages: vec![],
        };
        let hits = store
            .hybrid_search(&plan, &unit_embedding(0), config.alpha())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].bm25_score, 0.0);
        assert!(hits[0].cosine_score > 0.99);
    }

    #[test]
    fn hybrid_search_matches_project_name_case_insensitively() {
        // P2 path 1: the router may emit a different casing than the stored
        // project name ("Build-Service" vs "build-service"). The project filter
        // must match case-insensitively (ASCII) — otherwise every chunk is
        // filtered out and the hook silently injects nothing.
        let config = Config::default();
        let mut store = SqliteStore::open_in_memory(&config).unwrap();
        let project_id = create_project(&store, "build-service");

        store
            .upsert_document(
                &Document {
                    project_id,
                    doc_type: DocType::Contract,
                    source_path: "build.proto".into(),
                    title: "build.proto".into(),
                    content_hash: "h".into(),
                },
                &[ChunkWithEmbedding {
                    chunk: proto_chunk(
                        "BuildRequest",
                        "message BuildRequest { string id = 1; }",
                        0,
                    ),
                    embedding: unit_embedding(0),
                }],
            )
            .unwrap();

        // Router-supplied casing differs from the stored name.
        let plan = QueryPlan {
            projects: vec!["Build-Service".into()],
            type_names: vec![],
            topics: vec![],
            doc_types: vec![],
            languages: vec![],
        };
        let hits = store
            .hybrid_search(&plan, &unit_embedding(0), config.alpha())
            .unwrap();

        assert_eq!(
            hits.len(),
            1,
            "case-mismatched project name must still match"
        );
        assert_eq!(hits[0].label, "BuildRequest");
    }

    #[test]
    fn hybrid_search_unknown_project_name_searches_all_not_nothing() {
        // Regression: the router (Gemma or Gemini, observed on both) can emit a
        // prompt phrase into `projects` that matches no indexed project. That
        // used to filter out every chunk — silent total context loss. It must
        // now degrade to searching across all projects.
        let config = Config::default();
        let mut store = SqliteStore::open_in_memory(&config).unwrap();
        let project_id = create_project(&store, "vault");
        store
            .upsert_document(
                &Document {
                    project_id,
                    doc_type: DocType::Contract,
                    source_path: "build.proto".into(),
                    title: "build.proto".into(),
                    content_hash: "h".into(),
                },
                &[ChunkWithEmbedding {
                    chunk: proto_chunk("BuildRequest", "message BuildRequest { string id = 1; }", 0),
                    embedding: unit_embedding(0),
                }],
            )
            .unwrap();

        let plan = QueryPlan {
            projects: vec!["vault project router".into()], // phantom — no such project
            type_names: vec!["BuildRequest".into()],
            topics: vec![],
            doc_types: vec![],
            languages: vec![],
        };
        let hits = store
            .hybrid_search(&plan, &unit_embedding(0), config.alpha())
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "unknown project name must degrade to search-all, not filter everything out"
        );
        assert_eq!(hits[0].label, "BuildRequest");
    }

    #[test]
    fn hybrid_search_known_project_filter_excludes_other_projects() {
        // The degradation must be surgical: a project that DOES exist still
        // filters, excluding chunks from other projects. Partial matches
        // (one known name + one phantom) behave like the known name alone.
        let config = Config::default();
        let mut store = SqliteStore::open_in_memory(&config).unwrap();
        let alpha = create_project(&store, "alpha");
        let beta = create_project(&store, "beta");
        store
            .upsert_document(
                &Document {
                    project_id: alpha,
                    doc_type: DocType::Contract,
                    source_path: "a.proto".into(),
                    title: "a".into(),
                    content_hash: "ha".into(),
                },
                &[ChunkWithEmbedding {
                    chunk: proto_chunk("AlphaFoo", "message AlphaFoo { string Widget = 1; }", 0),
                    embedding: unit_embedding(0),
                }],
            )
            .unwrap();
        store
            .upsert_document(
                &Document {
                    project_id: beta,
                    doc_type: DocType::Contract,
                    source_path: "b.proto".into(),
                    title: "b".into(),
                    content_hash: "hb".into(),
                },
                &[ChunkWithEmbedding {
                    chunk: proto_chunk("BetaFoo", "message BetaFoo { string Widget = 1; }", 0),
                    embedding: unit_embedding(1),
                }],
            )
            .unwrap();

        // Exact filter: only alpha's chunk comes back.
        let exact = QueryPlan {
            projects: vec!["alpha".into()],
            type_names: vec!["Widget".into()],
            topics: vec![],
            doc_types: vec![],
            languages: vec![],
        };
        let hits = store
            .hybrid_search(&exact, &unit_embedding(0), config.alpha())
            .unwrap();
        assert!(!hits.is_empty(), "expected alpha's chunk");
        assert!(
            hits.iter().all(|h| h.project_id == alpha),
            "a real project filter must exclude other projects' chunks"
        );

        // Partial: known + phantom resolves to the known one, same exclusion.
        let partial = QueryPlan {
            projects: vec!["alpha".into(), "ghost".into()],
            ..exact.clone()
        };
        let hits = store
            .hybrid_search(&partial, &unit_embedding(0), config.alpha())
            .unwrap();
        assert!(
            !hits.is_empty() && hits.iter().all(|h| h.project_id == alpha),
            "partial match must filter to the known project only"
        );
    }

    #[test]
    fn prune_orphans_removes_missing_paths() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let project_id = create_project(&store, "p");

        for path in ["a.md", "b.md", "c.md"] {
            store
                .upsert_document(
                    &Document {
                        project_id,
                        doc_type: DocType::Plan,
                        source_path: path.into(),
                        title: path.into(),
                        content_hash: "h".into(),
                    },
                    &[],
                )
                .unwrap();
        }

        let removed = store
            .prune_orphans(project_id, &["a.md".into(), "b.md".into()])
            .unwrap();
        assert_eq!(removed, 1);

        let remaining: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 2);
    }

    #[test]
    fn prune_orphans_empty_kept_removes_everything_in_project() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let project_id = create_project(&store, "p");
        let other_id = create_project(&store, "other");

        for (pid, path) in [(project_id, "a.md"), (other_id, "b.md")] {
            store
                .upsert_document(
                    &Document {
                        project_id: pid,
                        doc_type: DocType::Plan,
                        source_path: path.into(),
                        title: path.into(),
                        content_hash: "h".into(),
                    },
                    &[],
                )
                .unwrap();
        }

        let removed = store.prune_orphans(project_id, &[]).unwrap();
        assert_eq!(removed, 1);

        let remaining: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn prune_orphans_cleans_chunks_vec() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let project_id = create_project(&store, "p");

        store
            .upsert_document(
                &Document {
                    project_id,
                    doc_type: DocType::Contract,
                    source_path: "x.proto".into(),
                    title: "x".into(),
                    content_hash: "h".into(),
                },
                &[ChunkWithEmbedding {
                    chunk: proto_chunk("X", "msg X {}", 0),
                    embedding: unit_embedding(0),
                }],
            )
            .unwrap();

        store.prune_orphans(project_id, &[]).unwrap();

        let vec_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_count, 0);
    }

    #[test]
    fn get_or_create_inserts_new_project() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let id = store
            .get_or_create_project("vault", "/a/b/vault")
            .expect("insert");
        assert!(id > 0);
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE name = 'vault'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn get_or_create_fetches_existing_with_matching_path() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let first = store.get_or_create_project("vault", "/a/b/vault").unwrap();
        let second = store.get_or_create_project("vault", "/a/b/vault").unwrap();
        assert_eq!(first, second);
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "second call must not insert a duplicate row");
    }

    #[test]
    fn get_or_create_conflicts_on_different_repo_path() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        store.get_or_create_project("vault", "/a/b/vault").unwrap();
        let err = store
            .get_or_create_project("vault", "/c/d/vault")
            .unwrap_err();
        match err {
            StoreError::Conflict(msg) => {
                assert!(
                    msg.contains("/a/b/vault"),
                    "msg missing existing path: {msg}"
                );
                assert!(
                    msg.contains("/c/d/vault"),
                    "msg missing incoming path: {msg}"
                );
                assert!(msg.contains("--name"), "msg missing the --name hint: {msg}");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn get_or_create_treats_null_repo_path_as_match() {
        // Existing legacy rows (and the test helper at create_project) insert
        // projects with a NULL repo_path. New get_or_create calls should fetch
        // those rows without conflict and without overwriting the NULL.
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let legacy_id = create_project(&store, "legacy");
        let id = store
            .get_or_create_project("legacy", "/x/y/z")
            .expect("must match NULL row");
        assert_eq!(id, legacy_id);
        let stored_path: Option<String> = store
            .conn
            .query_row(
                "SELECT repo_path FROM projects WHERE id = ?1",
                params![legacy_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            stored_path.is_none(),
            "NULL must be preserved on existing legacy rows"
        );
    }

    #[test]
    fn get_or_create_multiple_distinct_projects_coexist() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let vault = store.get_or_create_project("vault", "/a/b/vault").unwrap();
        let other = store.get_or_create_project("other", "/a/b/other").unwrap();
        assert_ne!(vault, other);
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn get_document_content_hash_returns_hash_for_known_doc() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let project_id = create_project(&store, "p");
        store
            .upsert_document(
                &Document {
                    project_id,
                    doc_type: DocType::Plan,
                    source_path: "design.md".into(),
                    title: "design".into(),
                    content_hash: "abc123".into(),
                },
                &[],
            )
            .unwrap();

        let got = store
            .get_document_content_hash(project_id, "design.md")
            .unwrap();
        assert_eq!(got.as_deref(), Some("abc123"));
    }

    #[test]
    fn get_document_content_hash_returns_none_for_unknown_path() {
        let store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let got = store.get_document_content_hash(1, "missing.md").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn get_document_content_hash_scopes_to_project() {
        // Same source_path in a different project must NOT leak across.
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        let pa = create_project(&store, "a");
        let pb = create_project(&store, "b");
        store
            .upsert_document(
                &Document {
                    project_id: pa,
                    doc_type: DocType::Plan,
                    source_path: "x.md".into(),
                    title: "x".into(),
                    content_hash: "in-a".into(),
                },
                &[],
            )
            .unwrap();

        assert_eq!(
            store
                .get_document_content_hash(pa, "x.md")
                .unwrap()
                .as_deref(),
            Some("in-a")
        );
        assert!(
            store
                .get_document_content_hash(pb, "x.md")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn log_retrieval_inserts_row() {
        let mut store = SqliteStore::open_in_memory(&Config::default()).unwrap();
        store
            .log_retrieval(&RetrievalLogEntry {
                prompt_hash: "h".into(),
                query_plan: r#"{"projects":["a"]}"#.into(),
                chunks_returned: 5,
                tokens_injected: 1234,
            })
            .unwrap();

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM retrieval_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
