use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineError {
    pub code: String,
    pub message: String,
}

impl EngineError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub(crate) fn invalid_schema(message: impl Into<String>) -> Self {
        Self::new("INVALID_SCHEMA", message)
    }

    pub(crate) fn invalid_change(message: impl Into<String>) -> Self {
        Self::new("INVALID_CHANGE", message)
    }

    pub(crate) fn invalid_query(message: impl Into<String>) -> Self {
        Self::new("INVALID_QUERY", message)
    }

    pub(crate) fn parse_error(message: impl Into<String>) -> Self {
        Self::new("SQL_PARSE_ERROR", message)
    }

    pub(crate) fn unsupported_sql(message: impl Into<String>) -> Self {
        Self::new("UNSUPPORTED_SQL", message)
    }

    pub(crate) fn table_not_found(table: &str) -> Self {
        Self::new("TABLE_NOT_FOUND", format!("Table `{table}` is not defined"))
    }

    pub(crate) fn column_not_found(column: &str, table: &str) -> Self {
        Self::new(
            "COLUMN_NOT_FOUND",
            format!("Column `{column}` does not exist in table `{table}`"),
        )
    }

    pub(crate) fn bind_error(message: impl Into<String>) -> Self {
        Self::new("BIND_ERROR", message)
    }
}

impl Display for EngineError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EngineError {}
