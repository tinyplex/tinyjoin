use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Row = Map<String, Value>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ColumnType {
    Boolean,
    Integer,
    Float,
    Text,
    Json,
}

// Stable PostgreSQL type OIDs used by the public SQL result metadata. These
// describe TinyGres's five normalized runtime types, not the spelling used in
// CREATE TABLE (for example, INTEGER and BIGINT normalize to the same type).
pub const PG_OID_BOOLEAN: u32 = 16;
pub const PG_OID_INTEGER: u32 = 20;
pub const PG_OID_TEXT: u32 = 25;
pub const PG_OID_JSON: u32 = 114;
pub const PG_OID_FLOAT: u32 = 701;
pub const PG_OID_UNKNOWN: u32 = 705;

impl ColumnType {
    pub const fn postgres_oid(self) -> u32 {
        match self {
            Self::Boolean => PG_OID_BOOLEAN,
            // TinyGres integers span JavaScript's safe-integer domain, which
            // exceeds PostgreSQL INT4 but remains a subset of INT8.
            Self::Integer => PG_OID_INTEGER,
            Self::Float => PG_OID_FLOAT,
            Self::Text => PG_OID_TEXT,
            // The normalized JSON type does not promise JSONB operators or
            // binary storage semantics, so JSON is the honest closest OID.
            Self::Json => PG_OID_JSON,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultField {
    pub name: String,
    pub data_type_id: u32,
}

impl ResultField {
    pub fn new(name: impl Into<String>, data_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            data_type_id: data_type.postgres_oid(),
        }
    }

    pub fn unknown(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type_id: PG_OID_UNKNOWN,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: ColumnType,
    #[serde(default = "default_nullable")]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableSchema {
    pub name: String,
    pub primary_key: Vec<String>,
    /// Empty when no typed column catalog is available. SQL-created tables always populate this
    /// collection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<ColumnDefinition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexDefinition {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
}

fn default_nullable() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeBatch {
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum Change {
    Upsert { table: String, row: Row },
    Delete { table: String, key: Row },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FilterOperator {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Filter {
    pub column: String,
    pub operator: FilterOperator,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum Predicate {
    Comparison {
        column: String,
        operator: FilterOperator,
        value: Value,
    },
    IsNull {
        column: String,
        negated: bool,
    },
    In {
        column: String,
        values: Vec<Value>,
    },
    And {
        predicates: Vec<Predicate>,
    },
    Or {
        predicates: Vec<Predicate>,
    },
    Not {
        predicate: Box<Predicate>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderDirection {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NullOrder {
    Default,
    First,
    Last,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBy {
    pub column: String,
    pub direction: OrderDirection,
    pub nulls: NullOrder,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPlan {
    pub table: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<Predicate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order_by: Vec<OrderBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOutcome {
    pub revision: u64,
    pub tables: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub revision: u64,
    pub fields: Vec<ResultField>,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteResult {
    pub command: String,
    pub revision: u64,
    pub row_count: usize,
    pub fields: Vec<ResultField>,
    pub rows: Vec<Row>,
    pub tables: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::ChangeBatch;

    #[test]
    fn change_batches_reject_unknown_metadata() {
        let error = serde_json::from_str::<ChangeBatch>(
            r#"{"changes":[],"sourceId":"removed-integration"}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `sourceId`"));
    }
}
