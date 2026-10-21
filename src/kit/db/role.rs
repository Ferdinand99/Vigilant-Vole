//! User roles.

use std::fmt;

use serde::{Deserialize, Serialize};

/// User role. Ordered: `Viewer < Editor < Admin`, so `role >= Role::Editor`
/// means "may edit".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Editor,
    Admin,
}

impl Role {
    pub fn all() -> &'static [Role] {
        &[Role::Viewer, Role::Editor, Role::Admin]
    }

    /// The value stored in `users.role`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Role::Viewer),
            "editor" => Some(Role::Editor),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Viewer => crate::kit::i18n::t("role.viewer"),
            Role::Editor => crate::kit::i18n::t("role.editor"),
            Role::Admin => crate::kit::i18n::t("role.admin"),
        }
    }

    pub fn is_admin(&self) -> bool {
        *self == Role::Admin
    }

    /// Editors and admins may change monitors, status pages, maintenance,
    /// notifications and tags.
    pub fn can_edit(&self) -> bool {
        *self >= Role::Editor
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
