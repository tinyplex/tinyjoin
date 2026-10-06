use std::rc::Rc;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::expression::Expression;

pub type Row = Map<String, Value>;

/// One of the five runtime types a column holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ColumnType {
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

/// A column of a table, as its catalog holds it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: ColumnType,
    pub nullable: bool,
    /// The literal `DEFAULT`, where the column declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// The most characters a `VARCHAR(n)` column holds, its `n`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
}

/// A table, as its catalog holds it: its columns in declared order, and its primary key's
/// columns in key order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDefinition {
    pub name: String,
    pub primary_key: Vec<String>,
    pub columns: Vec<ColumnDefinition>,
    /// The table's foreign keys, in the order they were declared.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKeyDefinition>,
}

/// A foreign key: columns of a table whose values, unless one is NULL, must be the values of the
/// primary key or a unique index of the table they reference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyDefinition {
    pub name: String,
    pub columns: Vec<String>,
    /// The table the key references, which may be its own.
    pub references: String,
    /// The referenced table's columns, by position in `columns`.
    pub referenced_columns: Vec<String>,
    pub on_delete: ForeignKeyAction,
    pub on_update: ForeignKeyAction,
}

/// What deleting or updating a referenced row does to the rows that reference it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ForeignKeyAction {
    #[serde(rename = "no action")]
    NoAction,
    #[serde(rename = "restrict")]
    Restrict,
    #[serde(rename = "cascade")]
    Cascade,
    #[serde(rename = "set null")]
    SetNull,
    #[serde(rename = "set default")]
    SetDefault,
}

impl ForeignKeyAction {
    /// The action as SQL names it, in lower case.
    pub fn name(self) -> &'static str {
        match self {
            Self::NoAction => "no action",
            Self::Restrict => "restrict",
            Self::Cascade => "cascade",
            Self::SetNull => "set null",
            Self::SetDefault => "set default",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "no action" => Self::NoAction,
            "restrict" => Self::Restrict,
            "cascade" => Self::Cascade,
            "set null" => Self::SetNull,
            "set default" => Self::SetDefault,
            _ => return None,
        })
    }
}

/// An index on a table, as its catalog holds it, with its columns in index order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexDefinition {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// A catalog model read back from the JSON its [`Serialize`] implementation writes. Reading it by
/// hand, rather than through serde's derived deserializers, keeps their error formatting, and the
/// floating-point formatting it pulls in, out of the engine. A catalog item is only accepted when
/// the model read back serializes to exactly the JSON it was read from, so a field this ignores
/// cannot pass unnoticed.
pub(crate) trait CatalogModel: Serialize + Sized {
    fn from_catalog_json(value: &Value) -> Option<Self>;
}

impl CatalogModel for TableDefinition {
    fn from_catalog_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let mut columns = Vec::new();
        for column in object.get("columns")?.as_array()? {
            columns.push(ColumnDefinition::from_catalog_json(column)?);
        }
        let mut foreign_keys = Vec::new();
        if let Some(keys) = object.get("foreignKeys") {
            for key in keys.as_array()? {
                foreign_keys.push(ForeignKeyDefinition::from_json(key)?);
            }
        }
        Some(Self {
            name: catalog_string(object.get("name")?)?,
            primary_key: catalog_strings(object.get("primaryKey")?)?,
            columns,
            foreign_keys,
        })
    }
}

impl ForeignKeyDefinition {
    /// Reads a foreign key as its catalog, and the JavaScript API, write it.
    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let action = |key| ForeignKeyAction::from_name(object.get(key)?.as_str()?);
        Some(Self {
            name: catalog_string(object.get("name")?)?,
            columns: catalog_strings(object.get("columns")?)?,
            references: catalog_string(object.get("references")?)?,
            referenced_columns: catalog_strings(object.get("referencedColumns")?)?,
            on_delete: action("onDelete")?,
            on_update: action("onUpdate")?,
        })
    }
}

impl CatalogModel for IndexDefinition {
    fn from_catalog_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            name: catalog_string(object.get("name")?)?,
            table: catalog_string(object.get("table")?)?,
            columns: catalog_strings(object.get("columns")?)?,
            unique: match object.get("unique") {
                None => false,
                Some(unique) => unique.as_bool()?,
            },
        })
    }
}

impl ColumnDefinition {
    fn from_catalog_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let data_type = match object.get("dataType")?.as_str()? {
            "boolean" => ColumnType::Boolean,
            "integer" => ColumnType::Integer,
            "float" => ColumnType::Float,
            "text" => ColumnType::Text,
            "json" => ColumnType::Json,
            _ => return None,
        };
        Some(Self {
            name: catalog_string(object.get("name")?)?,
            data_type,
            nullable: match object.get("nullable") {
                None => true,
                Some(nullable) => nullable.as_bool()?,
            },
            // An absent default is none, and a JSON null is `DEFAULT NULL`, which the catalog
            // writes as it was declared.
            default: object.get("default").cloned(),
            max_length: match object.get("maxLength") {
                None => None,
                Some(length) => Some(u32::try_from(length.as_u64()?).ok()?),
            },
        })
    }
}

fn catalog_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn catalog_strings(value: &Value) -> Option<Vec<String>> {
    let mut strings = Vec::new();
    for value in value.as_array()? {
        strings.push(catalog_string(value)?);
    }
    Some(strings)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RowChange {
    Upsert {
        table: Rc<str>,
        row: Row,
    },
    Delete {
        table: Rc<str>,
        key: Row,
    },
    /// An upsert planned straight into the stored entry it writes: its encoded primary key and
    /// its record. Only a reader with record layouts plans rows this way.
    Put {
        table: Rc<str>,
        key: Vec<u8>,
        record: Vec<u8>,
    },
    /// A delete planned straight from the stored entry it removes: its encoded primary key. Only a
    /// reader with record layouts plans deletes this way.
    Remove {
        table: Rc<str>,
        key: Vec<u8>,
    },
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
    /// `LIKE`, or `ILIKE` when `case_insensitive`. `escape` is `None` when no `ESCAPE` clause was
    /// given, which makes backslash the escape character.
    Like {
        column: String,
        pattern: Value,
        escape: Option<Value>,
        case_insensitive: bool,
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
    /// A comparison `Comparison` cannot hold: of two columns, or of a value worked out from
    /// the row. It never narrows the rows read.
    Expressions {
        left: Expression,
        operator: ComparisonOperator,
        right: Expression,
    },
    /// `column IN (SELECT ...)`. Its query is run once, before the statement reads any row, and
    /// the predicate becomes the `In` of the values it returns.
    Subquery {
        column: String,
        query: Box<Subquery>,
    },
}

/// The query of an `IN (SELECT ...)`, which returns one column.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Subquery {
    Select(SelectPlan),
    Aggregate(crate::aggregate::AggregatePlan),
    Join(crate::join::JoinPlan),
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

/// One item of an explicit single-table projection: a source column and the name it is returned
/// under, which is the column's own name unless `AS` renamed it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectColumn {
    pub(crate) column: String,
    pub(crate) output: String,
    /// An output worked out from the row rather than read from `column`, which is then empty.
    pub(crate) expression: Option<Expression>,
    /// A `*` beside other outputs, which execution lists as the table's columns.
    pub(crate) star: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectPlan {
    pub(crate) table: String,
    pub(crate) columns: Option<Vec<SelectColumn>>,
    /// Whether the outputs are keyed by position because their names repeat.
    pub(crate) positional: bool,
    /// Whether rows are read as arrays, so that the outputs a `*` lists may repeat names.
    pub(crate) array_rows: bool,
    /// Whether `order_by` names outputs rather than the table's columns, because an output it
    /// orders by is worked out from the row.
    pub(crate) ordered_by_outputs: bool,
    pub(crate) predicate: Option<Predicate>,
    pub(crate) order_by: Vec<OrderBy>,
    pub(crate) limit: Option<usize>,
    pub(crate) offset: usize,
    /// Whether rows that stream in key order are returned as [`ValueRows`].
    pub(crate) value_rows: bool,
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
    pub keys: ChangedKeys,
}

/// The primary keys a write changed, table by table, in table name order.
///
/// Each key is its table's primary-key columns, which a result reports as an object of them.
/// They are kept as rows of values of a table's key columns, rather than as a map for each key.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChangedKeys(Vec<TableKeys>);

/// One table's changed primary keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableKeys {
    /// The table's schema, shared with the catalog rather than copied for every statement, whose
    /// primary-key columns, in key order, a key's object lists.
    pub schema: Rc<TableDefinition>,
    /// Each key's values in the order of the key's columns, one key after another.
    pub values: Vec<Value>,
}

impl ChangedKeys {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Each table's keys, in table name order.
    pub fn tables(&self) -> &[TableKeys] {
        &self.0
    }

    pub fn get(&self, table: &str) -> Option<&TableKeys> {
        self.0
            .binary_search_by(|keys| keys.table().cmp(table))
            .ok()
            .map(|at| &self.0[at])
    }

    /// Sets `keys` as its table's keys.
    pub fn insert(&mut self, keys: TableKeys) {
        match self
            .0
            .binary_search_by(|other| other.table().cmp(keys.table()))
        {
            Ok(at) => self.0[at] = keys,
            Err(at) => self.0.insert(at, keys),
        }
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, table: &str) -> bool {
        self.get(table).is_some()
    }

    /// Each of `table`'s keys as an object of its columns, as a result reports them.
    #[cfg(test)]
    pub(crate) fn rows(&self, table: &str) -> Option<Vec<Row>> {
        self.get(table).map(TableKeys::rows)
    }
}

impl TableKeys {
    /// The table's name.
    pub fn table(&self) -> &str {
        &self.schema.name
    }

    /// The table's primary-key columns, in key order, as a key's object lists them.
    pub fn columns(&self) -> &[String] {
        &self.schema.primary_key
    }

    /// Each key's values, in the order of the columns.
    pub fn keys(&self) -> std::slice::Chunks<'_, Value> {
        self.values.chunks(self.columns().len().max(1))
    }

    /// Each key as an object of its columns, as a result reports it.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> Vec<Row> {
        self.keys()
            .map(|values| {
                self.columns()
                    .iter()
                    .cloned()
                    .zip(values.iter().cloned())
                    .collect()
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.keys().len()
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> std::vec::IntoIter<Row> {
        self.rows().into_iter()
    }
}

#[cfg(test)]
impl std::ops::Index<&str> for ChangedKeys {
    type Output = TableKeys;

    fn index(&self, table: &str) -> &TableKeys {
        self.get(table).expect("the table reports its keys")
    }
}

#[cfg(test)]
impl PartialEq<Vec<Row>> for TableKeys {
    fn eq(&self, rows: &Vec<Row>) -> bool {
        self.rows() == *rows
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QueryResult {
    pub(crate) revision: u64,
    pub(crate) fields: Vec<ResultField>,
    pub(crate) rows: Vec<Row>,
    pub(crate) values: Option<ValueRows>,
}

impl QueryResult {
    pub(crate) fn row_count(&self) -> usize {
        self.values
            .as_ref()
            .map_or(self.rows.len(), |values| values.rows.len())
    }
}

/// A result's rows as their values in field order, which a single-table query returns in place of
/// [`ExecuteResult::rows`] once its engine is asked to. A row built as an object of its values
/// under their names costs far more than its values alone, and is written out in field order in
/// any case.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ValueRows {
    pub rows: Vec<Vec<Value>>,
    /// What the rows are estimated to take, as the objects they stand for would be.
    pub estimated_bytes: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecuteResult {
    /// The statement's command tag, one of a few fixed words.
    pub command: &'static str,
    pub revision: u64,
    pub row_count: usize,
    pub fields: Vec<ResultField>,
    /// Each row's values under their fields' names. Where field names repeat, which only an
    /// execution for array rows returns, each value is instead under its field's position as
    /// three digits and then its name, so that a row's values come in field order.
    pub rows: Vec<Row>,
    /// The rows, as their values in field order, of a single-table query whose engine was asked
    /// for them, which leaves `rows` empty.
    pub values: Option<ValueRows>,
    pub tables: Vec<String>,
    /// Changed primary keys per table, under the same bounded contract as [`ApplyOutcome::keys`].
    pub keys: ChangedKeys,
}

/// The most changed primary keys one table may report in a single change notification.
///
/// A write that exceeds this reports no keys for that table rather than a partial set, so a
/// subscriber never mistakes a truncated list for a complete one. The bound keeps a change
/// notification's size independent of how many rows a statement touched.
pub const MAX_CHANGED_KEYS_PER_TABLE: usize = 1_000;
