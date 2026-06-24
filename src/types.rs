use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocType {
    Contract,
    Plan,
    Convention,
    Meta,
}

impl DocType {
    pub fn as_str(self) -> &'static str {
        match self {
            DocType::Contract => "contract",
            DocType::Plan => "plan",
            DocType::Convention => "convention",
            DocType::Meta => "meta",
        }
    }
}

impl FromStr for DocType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "contract" => Ok(DocType::Contract),
            "plan" => Ok(DocType::Plan),
            "convention" => Ok(DocType::Convention),
            "meta" => Ok(DocType::Meta),
            other => Err(format!(
                "unknown doc_type '{other}' (expected: contract|plan|convention|meta)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Go,
    Rust,
    Scala,
    Proto,
    OpenApi,
    Helm,
    Markdown,
    Unknown,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Go => "go",
            Language::Rust => "rust",
            Language::Scala => "scala",
            Language::Proto => "proto",
            Language::OpenApi => "openapi",
            Language::Helm => "helm",
            Language::Markdown => "markdown",
            Language::Unknown => "unknown",
        }
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "go" => Ok(Language::Go),
            "rust" => Ok(Language::Rust),
            "scala" => Ok(Language::Scala),
            "proto" => Ok(Language::Proto),
            "openapi" => Ok(Language::OpenApi),
            "helm" => Ok(Language::Helm),
            "markdown" => Ok(Language::Markdown),
            "unknown" => Ok(Language::Unknown),
            other => Err(format!(
                "unknown language '{other}' (expected: go|rust|scala|proto|openapi|helm|markdown|unknown)"
            )),
        }
    }
}
