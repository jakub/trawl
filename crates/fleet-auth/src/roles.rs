//! Role definitions (admin, analyst, reader) and permission model.

use std::fmt;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// The four roles in the fleet permission model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Full access — key management, server config, queries, schema.
    Admin,
    /// Query execution and schema inspection.
    Analyst,
    /// Query execution and schema inspection (same as analyst for now).
    Reader,
    /// Write-only log ingestion (used by vector/agents).
    Ingest,
}

/// Discrete permissions that can be checked against a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Execute search queries.
    Query,
    /// Read schema and field catalog.
    SchemaRead,
    /// Manage API keys (create, list, revoke).
    KeyManage,
    /// Manage server configuration.
    ServerManage,
    /// Write events via the ingest endpoint.
    Ingest,
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
                Permission::KeyManage,
                Permission::ServerManage,
            ],
            // analyst and reader are identical for now — differentiated in phase 7+
            Self::Analyst | Self::Reader => &[Permission::Query, Permission::SchemaRead],
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
    fn admin_has_all_permissions() {
        assert!(Role::Admin.has_permission(Permission::Query));
        assert!(Role::Admin.has_permission(Permission::SchemaRead));
        assert!(Role::Admin.has_permission(Permission::KeyManage));
        assert!(Role::Admin.has_permission(Permission::ServerManage));
    }

    #[test]
    fn analyst_has_query_and_schema() {
        assert!(Role::Analyst.has_permission(Permission::Query));
        assert!(Role::Analyst.has_permission(Permission::SchemaRead));
    }

    #[test]
    fn analyst_cannot_manage_keys_or_server() {
        assert!(!Role::Analyst.has_permission(Permission::KeyManage));
        assert!(!Role::Analyst.has_permission(Permission::ServerManage));
    }

    #[test]
    fn ingest_has_only_ingest_permission() {
        assert!(Role::Ingest.has_permission(Permission::Ingest));
        assert!(!Role::Ingest.has_permission(Permission::Query));
        assert!(!Role::Ingest.has_permission(Permission::SchemaRead));
        assert!(!Role::Ingest.has_permission(Permission::KeyManage));
        assert!(!Role::Ingest.has_permission(Permission::ServerManage));
    }

    #[test]
    fn reader_matches_analyst_permissions() {
        for perm in [
            Permission::Query,
            Permission::SchemaRead,
            Permission::KeyManage,
            Permission::ServerManage,
        ] {
            assert_eq!(
                Role::Reader.has_permission(perm),
                Role::Analyst.has_permission(perm),
                "reader and analyst should have identical permissions for {perm:?}"
            );
        }
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
