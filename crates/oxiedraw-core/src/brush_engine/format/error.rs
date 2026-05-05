use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrushError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("png: {0}")]
    Png(String),
    #[error("unsupported schema {found}, expected {expected}")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("missing entry: {0}")]
    MissingEntry(String),
    #[error("manifest kind '{0}' is not a brush")]
    NotABrush(String),
    #[error("textured brush references pattern '{0}' which is not in the archive")]
    MissingPattern(String),
}
