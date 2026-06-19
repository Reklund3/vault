use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use crate::parse::{ParseError, Parser, disambiguate_labels, estimate_tokens, sha256_hex};
use crate::store::Chunk;
use crate::types::Language;

/// Chunks an OpenAPI / Swagger document: one chunk per path+method operation and
/// one per reusable schema (`components/schemas` for OpenAPI 3, top-level
/// `definitions` for Swagger 2). The spec is parsed as YAML — JSON is a YAML
/// subset, so `.yaml`, `.yml`, and `.json` specs all parse through the same
/// path. Each chunk's content is the re-serialized subtree prefixed with a
/// `# {label}` comment so the path/method/schema name is embedded alongside the
/// body.
///
/// Dispatch is by *classified language*, not extension — `.yaml` is shared with
/// non-spec files, so the classifier is what marks a document as OpenAPI (see
/// [`crate::parse::select_parser`]).
pub struct OpenApiParser;

const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

impl Parser for OpenApiParser {
    fn language(&self) -> Language {
        Language::OpenApi
    }

    fn parse(&self, source: &str) -> Result<Vec<Chunk>, ParseError> {
        let docs = YamlLoader::load_from_str(source).map_err(|e| ParseError::Structural {
            detail: e.to_string(),
        })?;
        let Some(Yaml::Hash(root)) = docs.first() else {
            // Empty document or a non-mapping top level — nothing structural to
            // chunk. Consistent with a proto file that holds only `syntax`.
            return Ok(Vec::new());
        };

        let mut chunks: Vec<Chunk> = Vec::new();
        let mut chunk_index: u32 = 0;

        if let Some(Yaml::Hash(paths)) = root.get(&key("paths")) {
            for (path_key, methods) in paths {
                let (Yaml::String(path), Yaml::Hash(methods)) = (path_key, methods) else {
                    continue;
                };
                for (method_key, operation) in methods {
                    let Yaml::String(method) = method_key else {
                        continue;
                    };
                    if !HTTP_METHODS.contains(&method.to_ascii_lowercase().as_str()) {
                        continue;
                    }
                    let label = format!("{} {}", method.to_ascii_uppercase(), path);
                    push(&mut chunks, &mut chunk_index, label, operation);
                }
            }
        }

        // OpenAPI 3: components/schemas. Swagger 2: top-level definitions.
        let oas3_schemas = match root.get(&key("components")) {
            Some(Yaml::Hash(components)) => components.get(&key("schemas")),
            _ => None,
        };
        for schemas in [oas3_schemas, root.get(&key("definitions"))]
            .into_iter()
            .flatten()
        {
            if let Yaml::Hash(schemas) = schemas {
                for (name_key, schema) in schemas {
                    let Yaml::String(name) = name_key else {
                        continue;
                    };
                    let label = format!("schema {name}");
                    push(&mut chunks, &mut chunk_index, label, schema);
                }
            }
        }

        disambiguate_labels(&mut chunks);
        Ok(chunks)
    }
}

fn key(name: &str) -> Yaml {
    Yaml::String(name.to_string())
}

fn push(chunks: &mut Vec<Chunk>, chunk_index: &mut u32, label: String, node: &Yaml) {
    let content = format!("# {}\n{}", label, emit(node));
    chunks.push(Chunk {
        language: Language::OpenApi,
        content_hash: sha256_hex(content.as_bytes()),
        token_est: estimate_tokens(&content),
        label,
        content,
        chunk_index: *chunk_index,
    });
    *chunk_index += 1;
}

/// Re-serialize a YAML subtree, stripping the emitter's leading `---` document
/// marker. Returns an empty string if the node cannot be emitted (e.g. an
/// alias) — the `# {label}` comment still carries the identifying header.
fn emit(node: &Yaml) -> String {
    let mut out = String::new();
    {
        let mut emitter = YamlEmitter::new(&mut out);
        if emitter.dump(node).is_err() {
            return String::new();
        }
    }
    let trimmed = out.trim();
    trimmed
        .strip_prefix("---")
        .map(str::trim)
        .unwrap_or(trimmed)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Chunk> {
        OpenApiParser.parse(src).expect("parse ok")
    }

    fn labels(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn chunks_paths_per_method() {
        let src = "\
openapi: 3.0.0
paths:
  /build:
    post:
      summary: Trigger a build
      responses:
        '200':
          description: ok
    get:
      summary: Build status
";
        let chunks = parse(src);
        let labels = labels(&chunks);
        assert!(labels.contains(&"POST /build"), "labels: {labels:?}");
        assert!(labels.contains(&"GET /build"), "labels: {labels:?}");
        let post = chunks.iter().find(|c| c.label == "POST /build").unwrap();
        assert_eq!(post.language, Language::OpenApi);
        assert!(post.content.contains("# POST /build"));
        assert!(post.content.contains("Trigger a build"));
        assert_eq!(post.content_hash.len(), 64);
    }

    #[test]
    fn chunks_oas3_component_schemas() {
        let src = "\
openapi: 3.0.0
components:
  schemas:
    BuildRequest:
      type: object
      properties:
        id:
          type: string
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), vec!["schema BuildRequest"]);
        assert!(chunks[0].content.contains("type: object"));
    }

    #[test]
    fn chunks_swagger2_definitions() {
        let src = "\
swagger: '2.0'
definitions:
  Pet:
    type: object
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), vec!["schema Pet"]);
    }

    #[test]
    fn ignores_non_method_keys_under_path() {
        let src = "\
openapi: 3.0.0
paths:
  /x:
    parameters: []
    summary: shared
    get:
      summary: read
";
        let chunks = parse(src);
        assert_eq!(labels(&chunks), vec!["GET /x"]);
    }

    #[test]
    fn json_spec_parses_as_yaml() {
        // JSON is a YAML subset — the same loader handles a JSON-bodied spec.
        let src = r#"{"openapi":"3.0.0","paths":{"/ping":{"get":{"summary":"ping"}}}}"#;
        let chunks = parse(src);
        assert_eq!(labels(&chunks), vec!["GET /ping"]);
    }

    #[test]
    fn non_spec_yaml_yields_no_chunks() {
        // A k8s-manifest-shaped YAML the classifier might mislabel: no paths, no
        // schemas → no structural chunks (caller keeps it as nothing, not a bad
        // whole-file dump).
        let src = "\
apiVersion: v1
kind: ConfigMap
data:
  key: value
";
        assert!(parse(src).is_empty());
    }

    #[test]
    fn malformed_yaml_is_structural_error() {
        let err = OpenApiParser
            .parse("paths:\n  /a:\n   - : :\n\t bad")
            .expect_err("should fail");
        assert!(matches!(err, ParseError::Structural { .. }), "got {err:?}");
    }
}
