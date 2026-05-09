
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("no such table: {0}")]
    NoSuchTable(String),
    #[error("fts5 error: {0}")]
    Fts5Error(String),
    #[error("vec error: {0}")]
    VecError(String),
    #[error("sqlite error: {0}")]
    SqliteError(String),
    #[error("unknown error: {0}")]
    Unknown(String),
}


fn fts5() -> Result<(), QueryError> {
    Ok(())
}

fn vec() -> Result<(), QueryError> {
    Ok(())
}