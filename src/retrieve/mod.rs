mod router;

use crate::types::{DocType, Language};
use serde::{Deserialize, Serialize};

pub enum RouterOutput {
    Skip,
    Plan(QueryPlan),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlan {
    pub projects: Vec<String>,
    pub type_names: Vec<String>,
    pub topics: Vec<String>,
    pub doc_types: Vec<DocType>,
    pub languages: Vec<Language>,
}
