//! Rows as executors read them.
//!
//! Storage hands each visited row to its visitor as a [`RowRef`]: either a map the engine already
//! holds, such as a row staged in a transaction, or a stored record read in place from its leaf.
//! A query that reads only columns an index holds can instead visit the index's entries, as rows
//! holding just those columns.
//! A record decodes a column only when it is read, so a scan that tests one column of each row
//! decodes one value per row, and allocates nothing for it. Executors resolve the columns they
//! read to schema positions once per statement, and read rows by position.

use std::borrow::Cow;

use serde_json::Value;

use crate::paged_codec::{IndexEntry, StoredEntry, StoredRecord, encode_primary_key};
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
/// the writer reads in place rather than decoding whole, or only what a stored row is charged as,
/// when the writer needs nothing else of it.
#[derive(Clone, Debug)]
pub(crate) enum HeldRow {
    Map(Row),
    Stored(StoredEntry),
    /// A stored row planning read and measured as [`crate::storage::estimated_record_bytes`] but
    /// did not copy, because its writer needs nothing of it but that it exists and what it is
    /// charged as: a row a script deletes from a table with no index and no foreign key.
    Measured(usize),
}

/// The error for a row planning measured without keeping it reaching a reader of it, which
/// planning avoids by measuring only the rows no writer reads.
pub(crate) fn unkept_row() -> EngineError {
    EngineError::new(
        "INTERNAL_ERROR",
        "A row planning measured without keeping it was read",
    )
}

enum Source<'a> {
    Map {
        row: &'a Row,
        schema: &'a TableDefinition,
    },
    Record(StoredRecord<'a>),
    Index(IndexEntry<'a>),
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

    /// A secondary-index entry, which holds only the index's columns and the primary key. Only
    /// the columns it holds can be read.
    pub(crate) fn index(entry: IndexEntry<'a>) -> Self {
        Self(Source::Index(entry))
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
            Source::Index(entry) => entry.column(index),
        }
    }

    /// The stored record the row is read from, if it is one.
    pub(crate) fn stored(&self) -> Option<&StoredRecord<'a>> {
        match &self.0 {
            Source::Record(record) => Some(record),
            _ => None,
        }
    }

    /// The whole row as an owned map.
    pub(crate) fn to_row(&self) -> Result<Row> {
        match &self.0 {
            Source::Map { row, .. } => Ok((*row).clone()),
            Source::Record(record) => record.to_row(),
            Source::Index(_) => Err(partial_row()),
        }
    }

    /// The whole row's values, in the order of `schema`'s columns.
    pub(crate) fn values(&self, schema: &TableDefinition) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(schema.columns.len());
        for index in 0..schema.columns.len() {
            values.push(self.get(index)?.into_value());
        }
        Ok(values)
    }

    /// The row as a writer keeps it: a copy of a record's stored entry, or of a map.
    pub(crate) fn hold(&self) -> Result<HeldRow> {
        match &self.0 {
            Source::Map { row, .. } => Ok(HeldRow::Map((*row).clone())),
            Source::Record(record) => Ok(HeldRow::Stored(record.to_entry())),
            Source::Index(_) => Err(partial_row()),
        }
    }

    /// What [`Self::hold`] keeps, in bytes: a record's entry, or a map's estimated bytes.
    pub(crate) fn held_bytes(&self) -> Result<usize> {
        match &self.0 {
            Source::Map { row, .. } => estimated_row_bytes(row),
            Source::Record(record) => Ok(record.entry_len()),
            Source::Index(_) => Err(partial_row()),
        }
    }

    /// The row's primary-key columns and their values, as the key of a change to the row.
    pub(crate) fn primary_key(&self) -> Result<Row> {
        match &self.0 {
            Source::Map { row, schema } => crate::statement::primary_key_row(schema, row),
            Source::Record(record) => record.key_row(),
            Source::Index(_) => Err(partial_row()),
        }
    }

    /// The row's primary key, encoded as its B-tree key.
    pub(crate) fn encoded_key(&self) -> Result<Cow<'a, [u8]>> {
        match &self.0 {
            Source::Map { row, schema } => encode_primary_key(schema, row).map(Cow::Owned),
            Source::Record(record) => Ok(Cow::Borrowed(record.key())),
            Source::Index(_) => Err(partial_row()),
        }
    }
}

/// The failure of reading a whole row from an index entry, which only a read of the columns the
/// entry holds can use.
fn partial_row() -> EngineError {
    EngineError::new(
        "INVALID_PAGED_ARGUMENT",
        "An index entry holds only its index's columns and the primary key",
    )
}

impl Columns for RowRef<'_> {
    fn column(&self, index: usize) -> Result<ValueRef<'_>> {
        self.get(index)
    }
}
