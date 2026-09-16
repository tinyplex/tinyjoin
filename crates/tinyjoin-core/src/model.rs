use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Row = Map<String, Value>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ColumnType {
    Boolean,
    Integer,
    Float,
    Text,
    Json,
}

// Stable PostgreSQL type OIDs used by the public SQL result metadata. These
// describe TinyJoin's five normalized runtime types, not the spelling used in
// CREATE TABLE (for example, INTEGER and BIGINT normalize to the same type).
pub(crate) const PG_OID_BOOLEAN: u32 = 16;
pub(crate) const PG_OID_INTEGER: u32 = 20;
pub(crate) const PG_OID_TEXT: u32 = 25;
pub(crate) const PG_OID_JSON: u32 = 114;
pub(crate) const PG_OID_FLOAT: u32 = 701;

impl ColumnType {
    pub(crate) const fn postgres_oid(self) -> u32 {
        match self {
            Self::Boolean => PG_OID_BOOLEAN,
            // TinyJoin integers span JavaScript's safe-integer domain, which
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultField {
    pub name: String,
    pub data_type_id: u32,
}

impl ResultField {
    pub(crate) fn new(name: impl Into<String>, data_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            data_type_id: data_type.postgres_oid(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ColumnDefinition {
    pub(crate) name: String,
    pub(crate) data_type: ColumnType,
    #[serde(default = "default_nullable")]
    pub(crate) nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TableDefinition {
    pub(crate) name: String,
    pub(crate) primary_key: Vec<String>,
    pub(crate) columns: Vec<ColumnDefinition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IndexDefinition {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) columns: Vec<String>,
    #[serde(default)]
    pub(crate) unique: bool,
}

fn default_nullable() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RowChange {
    Upsert { table: String, row: Row },
    Delete { table: String, key: Row },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComparisonOperator {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Predicate {
    Comparison {
        column: String,
        operator: ComparisonOperator,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OrderDirection {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NullOrder {
    Default,
    First,
    Last,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OrderBy {
    pub(crate) column: String,
    pub(crate) direction: OrderDirection,
    pub(crate) nulls: NullOrder,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectPlan {
    pub(crate) table: String,
    pub(crate) columns: Option<Vec<String>>,
    pub(crate) predicate: Option<Predicate>,
    pub(crate) order_by: Vec<OrderBy>,
    pub(crate) limit: Option<usize>,
    pub(crate) offset: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApplyOutcome {
    pub revision: u64,
    pub tables: Vec<String>,
    /// The primary keys changed in each table, for the tables whose complete set is known.
    ///
    /// `tables` remains the authoritative list of what changed. A table is present here only when
    /// every one of its changed keys fits within [`MAX_CHANGED_KEYS_PER_TABLE`]; a table that
    /// changed more rows than that is absent, and a subscriber must re-read it instead. Reporting
    /// keys is therefore a bounded best effort that can never grow with the size of a write.
    pub keys: BTreeMap<String, Vec<Row>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QueryResult {
    pub(crate) revision: u64,
    pub(crate) fields: Vec<ResultField>,
    pub(crate) rows: Vec<Row>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecuteResult {
    pub command: String,
    pub revision: u64,
    pub row_count: usize,
    pub fields: Vec<ResultField>,
    pub rows: Vec<Row>,
    pub tables: Vec<String>,
    /// Changed primary keys per table, under the same bounded contract as [`ApplyOutcome::keys`].
    pub keys: BTreeMap<String, Vec<Row>>,
}

/// The most changed primary keys one table may report in a single change notification.
///
/// A write that exceeds this reports no keys for that table rather than a partial set, so a
/// subscriber never mistakes a truncated list for a complete one. The bound keeps a change
/// notification's size independent of how many rows a statement touched.
pub const MAX_CHANGED_KEYS_PER_TABLE: usize = 1_000;
