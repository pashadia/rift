//! Common shared types

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareInfo {
    pub name: String,
    pub path: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            read: true,
            write: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_share_info_equality() {
        let s1 = ShareInfo {
            name: "test".to_string(),
            path: "/tmp".to_string(),
            readonly: false,
        };
        let s2 = s1.clone();
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_permissions_default() {
        let perms = Permissions::default();
        assert!(perms.read);
        assert!(!perms.write);
    }
}
