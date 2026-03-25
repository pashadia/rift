//! Common error types

use thiserror::Error;

#[derive(Error, Debug)]
pub enum RiftError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),
}

pub type Result<T> = std::result::Result<T, RiftError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = RiftError::Config("test error".to_string());
        assert_eq!(format!("{}", err), "Configuration error: test error");
    }

    #[test]
    fn test_error_debug() {
        let err = RiftError::NotFound("file.txt".to_string());
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("NotFound"));
    }
}
