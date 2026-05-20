use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ToSql, params};

use crate::config::Config;
use crate::retrieve::QueryPlan;
use crate::store::schema;
use crate::store::traits::{Store, StoreError};
use crate::store::types::{ChunkWithEmbedding, Document, Hit, RetrievalLogEntry};
use crate::types::DocType;

const TOP_K: usize = 50;

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
}

impl Store for SqliteStore {
    fn migrate(&mut self) -> Result<(), StoreError> {
        schema::migrate(&self.conn)
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
                format!(" AND source_path NOT IN ({})", placeholders(kept_paths.len())),
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

    fn hybrid_search(
        &self,
        plan: &QueryPlan,
        embedding: &[f32],
        alpha: f32,
    ) -> Result<Vec<Hit>, StoreError> {
        if embedding.len() != self.embedding_dim {
            return Err(StoreError::InvalidInput(format!(
                "embedding dim {}, expected {}",
                embedding.len(),
                self.embedding_dim
            )));
        }

        let doc_type_strs: Vec<&'static str> =
            plan.doc_types.iter().map(|d| d.as_str()).collect();
        let language_strs: Vec<&'static str> =
            plan.languages.iter().map(|l| l.as_str()).collect();

        let filter_sql = build_filter_clause(plan);
        let filter_params: Vec<&dyn ToSql> = {
            let mut p: Vec<&dyn ToSql> = Vec::new();
            for s in &plan.projects {
                p.push(s);
            }
            for s in &doc_type_strs {
                p.push(s);
            }
            for s in &language_strs {
                p.push(s);
            }
            p
        };

        let emb_json = embedding_to_json(embedding);
        let mut hits: HashMap<i64, Hit> = HashMap::new();

        // ---- FTS5 BM25 ----
        if let Some(match_q) = build_match_query(plan) {
            let sql = format!(
                "SELECT c.id, c.label, c.content, c.token_est, c.project_id, c.doc_type,
                        -rank AS bm25_score
                 FROM chunks_fts
                 JOIN chunks c ON c.id = chunks_fts.rowid
                 WHERE chunks_fts MATCH ?1{filter_sql}
                 ORDER BY rank LIMIT {TOP_K}"
            );
            let mut bm25_params: Vec<&dyn ToSql> = vec![&match_q];
            bm25_params.extend(filter_params.iter().copied());

            let mut stmt = self.conn.prepare(&sql).map_err(backend_err)?;
            let rows = stmt
                .query_map(bm25_params.as_slice(), map_hit_row)
                .map_err(backend_err)?;
            for row in rows {
                let (hit, score) = row.map_err(backend_err)?;
                let mut h = hit;
                h.bm25_score = score;
                hits.insert(h.chunk_id, h);
            }
        }

        // ---- Vec cosine ----
        let cos_sql = format!(
            "SELECT c.id, c.label, c.content, c.token_est, c.project_id, c.doc_type,
                    1.0 - vec_distance_cosine(v.embedding, ?1) AS cos
             FROM chunks_vec v
             JOIN chunks c ON c.id = v.chunk_id
             WHERE 1=1{filter_sql}
             ORDER BY vec_distance_cosine(v.embedding, ?1) LIMIT {TOP_K}"
        );
        let mut cos_params: Vec<&dyn ToSql> = vec![&emb_json];
        cos_params.extend(filter_params.iter().copied());

        let mut stmt = self.conn.prepare(&cos_sql).map_err(backend_err)?;
        let rows = stmt
            .query_map(cos_params.as_slice(), map_hit_row)
            .map_err(backend_err)?;
        for row in rows {
            let (hit, cos) = row.map_err(backend_err)?;
            hits.entry(hit.chunk_id)
                .and_modify(|h| h.cosine_score = cos)
                .or_insert(Hit {
                    cosine_score: cos,
                    ..hit
                });
        }

        // Merge: normalize BM25 across the result set, blend with cosine.
        let mut hits: Vec<Hit> = hits.into_values().collect();
        let max_bm25 = hits.iter().map(|h| h.bm25_score).fold(0.0_f32, f32::max);
        for h in &mut hits {
            let bm25_norm = if max_bm25 > 0.0 {
                h.bm25_score / max_bm25
            } else {
                0.0
            };
            h.final_score = alpha * bm25_norm + (1.0 - alpha) * h.cosine_score;
        }
        hits.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(TOP_K);
        Ok(hits)
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

fn build_filter_clause(plan: &QueryPlan) -> String {
    let mut s = String::new();
    if !plan.projects.is_empty() {
        s.push_str(&format!(
            " AND c.project_id IN (SELECT id FROM projects WHERE name IN ({}))",
            placeholders(plan.projects.len())
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
