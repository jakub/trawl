//! Role definitions (admin, analyst, reader) and permission model.

use std::fmt;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// The four roles in the trawl permission model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Full access — key management, server config, queries, schema.
    Admin,
    /// Power user — query execution, schema, saved queries, export, streaming.
    Analyst,
    /// Basic access — query execution, schema, cancel own queries.
    Reader,
    /// Write-only log ingestion (used by vector/agents).
    Ingest,
}

/// Discrete permissions that can be checked against a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Execute search queries, view history, list running queries.
    Query,
    /// Read schema and field catalog.
    SchemaRead,
    /// Validate DSL syntax without executing.
    Validate,
    /// CRUD operations on saved queries.
    SavedQuery,
    /// Export query results to file formats.
    Export,
    /// Subscribe to live SSE event streams.
    Stream,
    /// Cancel running queries (own queries; admin can cancel any via `ServerManage`).
    QueryCancel,
    /// Manage API keys (create, list, revoke).
    KeyManage,
    /// Manage server configuration and view stats.
    ServerManage,
    /// Write events via the ingest endpoint.
    Ingest,
}

impl Permission {
    /// Snake-case string representation for wire formats.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::SchemaRead => "schema_read",
            Self::Validate => "validate",
            Self::SavedQuery => "saved_query",
            Self::Export => "export",
            Self::Stream => "stream",
            Self::QueryCancel => "query_cancel",
            Self::KeyManage => "key_manage",
            Self::ServerManage => "server_manage",
            Self::Ingest => "ingest",
        }
    }
}

impl Role {
    /// All defined roles.
    pub const ALL: &[Self] = &[Self::Admin, Self::Analyst, Self::Reader, Self::Ingest];

    /// Check whether this role grants the given permission.
    pub fn has_permission(self, perm: Permission) -> bool {
        self.permissions().contains(&perm)
    }

    /// Return all permissions granted by this role.
    pub fn permissions(self) -> &'static [Permission] {
        match self {
            Self::Admin => &[
                Permission::Query,
                Permission::SchemaRead,
                Permission::Validate,
                Permission::SavedQuery,
                Permission::Export,
                Permission::Stream,
                Permission::QueryCancel,
                Permission::KeyManage,
                Permission::ServerManage,
            ],
            Self::Analyst => &[
                Permission::Query,
                Permission::SchemaRead,
                Permission::Validate,
                Permission::SavedQuery,
                Permission::Export,
                Permission::Stream,
                Permission::QueryCancel,
            ],
            Self::Reader => &[
                Permission::Query,
                Permission::SchemaRead,
                Permission::QueryCancel,
            ],
            Self::Ingest => &[Permission::Ingest],
        }
    }

    /// The string representation used in the database and CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Analyst => "analyst",
            Self::Reader => "reader",
            Self::Ingest => "ingest",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admin" => Ok(Self::Admin),
            "analyst" => Ok(Self::Analyst),
            "reader" => Ok(Self::Reader),
            "ingest" => Ok(Self::Ingest),
            other => Err(format!("unknown role: {other}")),
        }
    }
}

impl ToSql for Role {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Role {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse::<Self>()
            .map_err(|e| rusqlite::types::FromSqlError::Other(e.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_has_all_non_ingest_permissions() {
        assert!(Role::Admin.has_permission(Permission::Query));
        assert!(Role::Admin.has_permission(Permission::SchemaRead));
        assert!(Role::Admin.has_permission(Permission::Validate));
        assert!(Role::Admin.has_permission(Permission::SavedQuery));
        assert!(Role::Admin.has_permission(Permission::Export));
        assert!(Role::Admin.has_permission(Permission::Stream));
        assert!(Role::Admin.has_permission(Permission::QueryCancel));
        assert!(Role::Admin.has_permission(Permission::KeyManage));
        assert!(Role::Admin.has_permission(Permission::ServerManage));
        // admin and ingest are orthogonal
        assert!(!Role::Admin.has_permission(Permission::Ingest));
    }

    #[test]
    fn analyst_has_power_user_permissions() {
        assert!(Role::Analyst.has_permission(Permission::Query));
        assert!(Role::Analyst.has_permission(Permission::SchemaRead));
        assert!(Role::Analyst.has_permission(Permission::Validate));
        assert!(Role::Analyst.has_permission(Permission::SavedQuery));
        assert!(Role::Analyst.has_permission(Permission::Export));
        assert!(Role::Analyst.has_permission(Permission::Stream));
        assert!(Role::Analyst.has_permission(Permission::QueryCancel));
    }

    #[test]
    fn analyst_cannot_manage_keys_or_server() {
        assert!(!Role::Analyst.has_permission(Permission::KeyManage));
        assert!(!Role::Analyst.has_permission(Permission::ServerManage));
        assert!(!Role::Analyst.has_permission(Permission::Ingest));
    }

    #[test]
    fn reader_has_only_basic_permissions() {
        assert!(Role::Reader.has_permission(Permission::Query));
        assert!(Role::Reader.has_permission(Permission::SchemaRead));
        assert!(Role::Reader.has_permission(Permission::QueryCancel));
        // reader must NOT have analyst-tier permissions
        assert!(!Role::Reader.has_permission(Permission::Validate));
        assert!(!Role::Reader.has_permission(Permission::SavedQuery));
        assert!(!Role::Reader.has_permission(Permission::Export));
        assert!(!Role::Reader.has_permission(Permission::Stream));
        assert!(!Role::Reader.has_permission(Permission::KeyManage));
        assert!(!Role::Reader.has_permission(Permission::ServerManage));
        assert!(!Role::Reader.has_permission(Permission::Ingest));
    }

    #[test]
    fn ingest_has_only_ingest_permission() {
        assert!(Role::Ingest.has_permission(Permission::Ingest));
        assert!(!Role::Ingest.has_permission(Permission::Query));
        assert!(!Role::Ingest.has_permission(Permission::SchemaRead));
        assert!(!Role::Ingest.has_permission(Permission::Validate));
        assert!(!Role::Ingest.has_permission(Permission::SavedQuery));
        assert!(!Role::Ingest.has_permission(Permission::Export));
        assert!(!Role::Ingest.has_permission(Permission::Stream));
        assert!(!Role::Ingest.has_permission(Permission::QueryCancel));
        assert!(!Role::Ingest.has_permission(Permission::KeyManage));
        assert!(!Role::Ingest.has_permission(Permission::ServerManage));
    }

    #[test]
    fn display_roundtrip() {
        for &role in Role::ALL {
            let s = role.to_string();
            let parsed: Role = s.parse().unwrap();
            assert_eq!(role, parsed);
        }
    }

    #[test]
    fn fromstr_invalid() {
        let result = "superuser".parse::<Role>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown role"));
    }
}
