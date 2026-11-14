//! Domain failures retain their category across UI and HTTP boundaries.
#[derive(Debug)]
pub enum Failure {
    Conflict(String),
    InvalidInput(String),
    NotFound(String),
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::InvalidInput(message) | Self::NotFound(message) => {
                f.write_str(message)
            }
        }
    }
}
impl std::error::Error for Failure {}
pub fn conflict(message: impl Into<String>) -> Failure {
    Failure::Conflict(message.into())
}
