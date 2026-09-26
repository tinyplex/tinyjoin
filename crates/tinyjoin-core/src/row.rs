//! Rows as executors read them.
//!
//! Storage hands each visited row to its visitor as a [`RowRef`]: either a map the engine already
//! holds, such as a row staged in a transaction, or a stored record read in place from its leaf.
//! A record decodes a column only when it is read, so a scan that tests one column of each row
//! decodes one value per row, and allocates nothing for it. Executors resolve the columns they
//! read to schema positions once per statement, and read rows by position.

use std::borrow::Cow;

use serde_json::Value;

use crate::paged_codec::{StoredEntry, StoredRecord, encode_primary_key};
use crate::storage::estimated_row_bytes;
use crate::{ColumnType, EngineError, Result, Row, TableDefinition};

/// One column's value as its row holds it, borrowed where the row allows.
///
/// Each typed variant holds exactly the values its column type admits. A `JSON` column's value,
/// and any value whose shape does not match its column's type, stays JSON, so that reading it
/// behaves exactly as reading the JSON value does.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ValueRef<'a> {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(Cow<'a, str>),
    Json(Cow<'a, Value>),
}

impl<'a> ValueRef<'a> {
    /// Reads a value from a map, as a column of `data_type`.
    pub(crate) fn from_value(value: &'a Value, data_type: ColumnType) -> Self {
        let typed = match (data_type, value) {
            (_, Value::Null) => return Self::Null,
            (ColumnType::Boolean, Value::Bool(value)) => Some(Self::Boolean(*value)),
            (ColumnType::Integer, Value::Number(number)) => number.as_i64().map(Self::Integer),
            (ColumnType::Float, Value::Number(number)) if number.is_f64() => {
                number.as_f64().map(Self::Float)
            }
            (ColumnType::Text, Value::String(text)) => Some(Self::Text(Cow::Borrowed(text))),
            _ => None,
        };
        typed.unwrap_or(Self::Json(Cow::Borrowed(value)))
    }

    pub(crate) fn is_null(&self) -> bool {
        match self {
            Self::Null => true,
            Self::Json(value) => value.is_null(),
            _ => false,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Boolean(value) => Value::Bool(value),
            Self::Integer(value) => Value::from(value),
            Self::Float(value) => Value::from(value),
            Self::Text(text) => Value::String(text.into_owned()),
            Self::Json(value) => value.into_owned(),
        }
    }

    /// The estimated bytes of an owned copy, which is the estimate for the equivalent JSON value:
    /// a scalar takes 16 bytes, text 24 more than its length, and `json` estimates JSON.
    pub(crate) fn owned_bytes(&self, json: impl FnOnce(&Value) -> Result<usize>) -> Result<usize> {
        match self {
            Self::Null | Self::Boolean(_) | Self::Integer(_) | Self::Float(_) => Ok(16),
            // A string's length is at most `isize::MAX`, so this cannot overflow.
            Self::Text(text) => Ok(24 + text.len()),
            Self::Json(value) => json(value),
        }
    }
}

/// A row whose columns a filter reads by the positions it resolved them to.
pub(crate) trait Columns {
    fn column(&self, index: usize) -> Result<ValueRef<'_>>;
}

/// A row as a visitor reads it.
pub(crate) struct RowRef<'a>(Source<'a>);

/// A row a writer keeps from reading it: a map, or the stored entry a record was read from, which
/// the writer reads in place rather than decoding whole.
#[derive(Clone, Debug)]
pub(crate) enum HeldRow {
    Map(Row),
    Stored(StoredEntry),
}

enum Source<'a> {
    Map {
        row: &'a Row,
        schema: &'a TableDefinition,
    },
    Record(StoredRecord<'a>),
}

impl<'a> RowRef<'a> {
    /// A row held as a map, whose columns are those of `schema`.
    pub(crate) fn map(row: &'a Row, schema: &'a TableDefinition) -> Self {
        Self(Source::Map { row, schema })
    }

    /// A stored entry, read in place.
    pub(crate) fn record(record: StoredRecord<'a>) -> Self {
        Self(Source::Record(record))
    }

    /// The value of the column at `index`, in schema order.
    pub(crate) fn get(&self, index: usize) -> Result<ValueRef<'a>> {
        match &self.0 {
            Source::Map { row, schema } => {
                let column = schema.columns.get(index).ok_or_else(|| {
                    EngineError::invalid_query(format!(
                        "Table `{}` has no column {index}",
                        schema.name
                    ))
                })?;
                row.get(&column.name)
                    .map(|value| ValueRef::from_value(value, column.data_type))
                    .ok_or_else(|| EngineError::column_not_found(&column.name, &schema.name))
            }
            Source::Record(record) => record.column(index),
        }
    }

    /// The whole row as an owned map.
    pub(crate) fn to_row(&self) -> Result<Row> {
        match &self.0 {
            Source::Map { row, .. } => Ok((*row).clone()),
            Source::Record(record) => record.to_row(),
        }
    }

    /// The row as a writer keeps it: a copy of a record's stored entry, or of a map.
    pub(crate) fn hold(&self) -> HeldRow {
        match &self.0 {
            Source::Map { row, .. } => HeldRow::Map((*row).clone()),
            Source::Record(record) => HeldRow::Stored(record.to_entry()),
        }
    }

    /// What [`Self::hold`] keeps, in bytes: a record's entry, or a map's estimated bytes.
    pub(crate) fn held_bytes(&self) -> Result<usize> {
        match &self.0 {
            Source::Map { row, .. } => estimated_row_bytes(row),
            Source::Record(record) => Ok(record.entry_len()),
        }
    }

    /// The row's primary-key columns and their values, as the key of a change to the row.
    pub(crate) fn primary_key(&self) -> Result<Row> {
        let schema = match &self.0 {
            Source::Map { row, schema } => return crate::statement::primary_key_row(schema, row),
            Source::Record(record) => record.schema(),
        };
        let mut key = Row::new();
        for (position, column) in schema.columns.iter().enumerate() {
            if schema.primary_key.contains(&column.name) {
                key.insert(column.name.clone(), self.get(position)?.into_value());
            }
        }
        Ok(key)
    }

    /// The row's primary key, encoded as its B-tree key.
    pub(crate) fn encoded_key(&self) -> Result<Cow<'a, [u8]>> {
        match &self.0 {
            Source::Map { row, schema } => encode_primary_key(schema, row).map(Cow::Owned),
            Source::Record(record) => Ok(Cow::Borrowed(record.key())),
        }
    }
}

impl Columns for RowRef<'_> {
    fn column(&self, index: usize) -> Result<ValueRef<'_>> {
        self.get(index)
    }
}
