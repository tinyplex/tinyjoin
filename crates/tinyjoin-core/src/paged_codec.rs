use std::borrow::Cow;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::row::ValueRef;
use crate::{
    ColumnType, EngineError, FIRST_DATA_PAGE_ID, IndexDefinition, MAX_PAGE_COUNT, PageId, Result,
    Row, TableDefinition,
    btree::{MAX_BTREE_KEY_BYTES, MAX_BTREE_VALUE_BYTES, TreeId},
};

pub(crate) const CATALOG_TREE_ID: TreeId = 1;
pub(crate) const FIRST_USER_TREE_ID: TreeId = CATALOG_TREE_ID + 1;
pub(crate) const MAX_PAGED_VALUE_BYTES: usize = MAX_BTREE_VALUE_BYTES;
pub(crate) const MAX_CATALOG_TABLES: u32 = 4_096;
pub(crate) const MAX_CATALOG_INDEXES: u32 = 4_096;
pub(crate) const MAX_STORED_COUNT: u64 = u32::MAX as u64;

const CATALOG_HEADER_KEY: u8 = 0x00;
const CATALOG_TABLE_KEY: u8 = 0x10;
const CATALOG_INDEX_KEY: u8 = 0x20;
// Keys in page format 3: every component compares correctly as raw bytes and delimits itself, so
// a tuple is its components concatenated, and a B-tree orders keys as SQL orders their values.
// BOOLEAN is one byte, INTEGER and FLOAT eight sortable big-endian bytes, and TEXT its UTF-8
// bytes, with each 0x00 written as 0x00 0xff, followed by 0x00 0x01. Byte order is then Unicode
// code-point order. JSON cannot be a key column.
const TEXT_ESCAPED_ZERO: u8 = 0xff;
const TEXT_TERMINATOR: u8 = 0x01;

// Rows in page format 3: a flags byte, the stored column count, a null bitmap if any stored
// column is NULL, the end offset of every stored column but the last, then the packed values.
// Primary-key columns live only in the B-tree key, and trailing columns that equal their defaults
// are not stored.
const RECORD_OFFSET_WIDTH: u8 = 0b0000_0011;
const RECORD_HAS_NULLS: u8 = 0b0000_0100;
/// Reserved for sync metadata: a row HLC, a column exception list, and a tombstone. A record with
/// any of these bits, or an unknown one, is rejected until a sync protocol defines them.
const RECORD_RESERVED: u8 = 0b1111_1000;
const RECORD_HEADER_BYTES: usize = 2;

const RECORD_VERSION: u8 = 1;
const RECORD_FLAGS: u8 = 0;
const CATALOG_HEADER_BYTES: usize = 20;
const CATALOG_ITEM_HEADER_BYTES: usize = 40;
const NO_PAGE_ID: PageId = u64::MAX;
pub(crate) const MAX_TREE_ID: TreeId = u64::MAX - 1;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NODES: usize = 1_000_000;
const MAX_COLUMNS: usize = 256;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogHeader {
    pub next_tree_id: TreeId,
    pub table_count: u32,
    pub index_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogTableRecord {
    pub schema: TableDefinition,
    pub tree_id: TreeId,
    pub root_page_id: Option<PageId>,
    pub row_count: u64,
    /// The fingerprint of every row in this table, as reported by its B-tree root.
    pub hash: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogIndexRecord {
    pub definition: IndexDefinition,
    pub tree_id: TreeId,
    pub root_page_id: Option<PageId>,
    pub entry_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CatalogKey {
    Header,
    Table(String),
    Index(String),
}

/// Returns the primary-key portion of an entry without requiring its indexed values.
///
/// `visit_index` uses a fully encoded lookup tuple, but unique-index validation often has only an
/// existing entry and its schema. Parsing the component framing avoids searching for the separator
/// byte inside arbitrary text or JSON payloads.
pub(crate) fn secondary_index_primary_key_for_definition<'a>(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    entry: &'a [u8],
) -> Result<&'a [u8]> {
    validate_index_identity(schema, definition).map_err(as_storage_corruption)?;
    validate_key_size(entry).map_err(as_storage_corruption)?;
    let offset = validate_tuple_prefix(schema, &definition.columns, entry, 0)?;
    if offset == entry.len() {
        return Err(storage_corrupt(
            "A secondary-index entry does not contain a primary key after its tuple",
        ));
    }
    let primary_key = &entry[offset..];
    let end = validate_tuple_prefix(schema, &schema.primary_key, primary_key, 0)?;
    if end != primary_key.len() {
        return Err(storage_corrupt(
            "A secondary-index entry contains trailing bytes after its primary key",
        ));
    }
    Ok(primary_key)
}

/// Serializes JSON with recursively sorted object keys and no insignificant whitespace.
///
/// The one-mebibyte bound is shared by row and catalog overflow values. Keeping the encoder
/// bounded also prevents a malformed or adversarial in-memory value from consuming unbounded
/// temporary memory before the storage layer can reject it.
pub(crate) fn encode_canonical_json(value: &Value) -> Result<Vec<u8>> {
    let mut encoder = CanonicalJsonEncoder {
        bytes: Vec::new(),
        nodes: 0,
    };
    encoder.encode(value, 0)?;
    Ok(encoder.bytes)
}

/// Encodes the complete primary-key tuple for a row or lookup object.
///
/// The schema is one the catalog already validated, when it was created or read, so it is not
/// checked again for every key.
pub(crate) fn encode_primary_key(schema: &TableDefinition, row: &Row) -> Result<Vec<u8>> {
    let key = encode_columns(schema, &schema.primary_key, row, NullPolicy::Reject)?
        .expect("rejecting nulls always returns a tuple");
    validate_key_size(&key)?;
    Ok(key)
}

/// Encodes an index tuple, which prefixes every entry for that tuple.
///
/// `None` follows PostgreSQL's default index semantics for a tuple containing SQL NULL: it is not
/// represented in the index and cannot satisfy an equality lookup. As for primary keys, the schema
/// and index definition are ones the catalog already validated.
pub(crate) fn encode_secondary_index_prefix(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<Vec<u8>>> {
    let Some(key) = encode_columns(schema, &definition.columns, row, NullPolicy::Omit)? else {
        return Ok(None);
    };
    validate_key_size(&key)?;
    Ok(Some(key))
}

/// Encodes one secondary-index entry as `indexed tuple || primary-key tuple`.
pub(crate) fn encode_secondary_index_entry_key(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<Vec<u8>>> {
    Ok(encode_secondary_index_entry(schema, definition, row)?.map(|(key, _)| key))
}

/// Encodes one secondary-index entry, as [`encode_secondary_index_entry_key`] does, with the length
/// of its indexed tuple.
pub(crate) fn encode_secondary_index_entry(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<(Vec<u8>, usize)>> {
    let Some(mut key) = encode_secondary_index_prefix(schema, definition, row)? else {
        return Ok(None);
    };
    let tuple = key.len();
    key.extend_from_slice(&encode_primary_key(schema, row)?);
    validate_key_size(&key)?;
    Ok(Some((key, tuple)))
}

/// Where each of an index's columns is in its table's schema, to read them from stored records.
pub(crate) fn index_column_positions(
    schema: &TableDefinition,
    definition: &IndexDefinition,
) -> Result<Vec<usize>> {
    let mut positions = Vec::with_capacity(definition.columns.len());
    for column in &definition.columns {
        let position = schema
            .columns
            .iter()
            .position(|item| item.name == *column)
            .ok_or_else(|| {
                EngineError::invalid_change(format!(
                    "Row for `{}` is missing key column `{column}`",
                    schema.name
                ))
            })?;
        positions.push(position);
    }
    Ok(positions)
}

/// Encodes a stored row's secondary-index entry, as [`encode_secondary_index_entry`] encodes the
/// row the record decodes to, reading only the indexed columns, at `positions`. The entry ends with
/// the record's key, which is the row's encoded primary key.
pub(crate) fn encode_record_index_entry(
    positions: &[usize],
    record: &StoredRecord<'_>,
) -> Result<Option<(Vec<u8>, usize)>> {
    let schema = record.schema;
    let mut key = Vec::with_capacity(16 * positions.len() + record.key.len());
    for position in positions {
        let value = record.column(*position)?;
        if value.is_null() {
            return Ok(None);
        }
        let column = &schema.columns[*position];
        encode_component_ref(
            &mut key,
            column.data_type,
            &value,
            &schema.name,
            &column.name,
        )?;
        validate_key_size(&key)?;
    }
    let tuple = key.len();
    key.extend_from_slice(record.key);
    validate_key_size(&key)?;
    Ok(Some((key, tuple)))
}

/// Encodes one value as a key component of `data_type`, to bound a range of keys. Components compare
/// as their values do, so a key lies between two bounds exactly when its component does.
pub(crate) fn encode_key_bound(data_type: ColumnType, value: &Value) -> Result<Vec<u8>> {
    let mut bound = Vec::new();
    encode_component(&mut bound, data_type, value, "", "")?;
    Ok(bound)
}

/// Encodes the smallest and largest components of `data_type` text that start with `prefix`: the
/// prefix's escaped bytes, and those followed by `0xff`, which UTF-8 never contains.
pub(crate) fn encode_text_prefix_bounds(prefix: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut lower = encode_key_bound(ColumnType::Text, &Value::from(prefix))?;
    lower.truncate(lower.len() - 2);
    let mut upper = lower.clone();
    upper.push(0xff);
    Ok((lower, upper))
}

/// The first component of a key: a table key's leading primary-key column, or an index entry's
/// leading indexed column.
pub(crate) fn leading_key_component(key: &[u8], data_type: ColumnType) -> Result<&[u8]> {
    Ok(&key[..component_end(key, 0, data_type)?])
}

/// The primary key an index entry carries after its indexed components, whose types are
/// `indexed_types`.
pub(crate) fn index_entry_primary_key<'a>(
    entry: &'a [u8],
    indexed_types: &[ColumnType],
) -> Result<&'a [u8]> {
    let mut offset = 0;
    for data_type in indexed_types {
        offset = component_end(entry, offset, *data_type)?;
    }
    if offset == entry.len() {
        return Err(storage_corrupt(
            "A secondary-index entry does not contain a primary key after its tuple",
        ));
    }
    Ok(&entry[offset..])
}

/// Reports whether an index entry holds the tuple `prefix` encodes. Components delimit themselves,
/// so an entry for any other tuple cannot begin with these bytes.
pub(crate) fn secondary_index_entry_matches_prefix(entry: &[u8], prefix: &[u8]) -> bool {
    entry.len() > prefix.len() && entry.starts_with(prefix)
}

/// Returns the encoded primary key carried by an index entry already matched to `prefix`.
pub(crate) fn secondary_index_primary_key<'a>(entry: &'a [u8], prefix: &[u8]) -> Result<&'a [u8]> {
    validate_key_size(entry).map_err(as_storage_corruption)?;
    if !secondary_index_entry_matches_prefix(entry, prefix) || entry.len() == prefix.len() {
        return Err(storage_corrupt(
            "A secondary-index entry does not contain the expected tuple prefix and primary key",
        ));
    }
    Ok(&entry[prefix.len()..])
}

/// Encodes a normalized row as a page format 3 record. The primary key is not repeated: it is the
/// entry's B-tree key.
pub(crate) fn encode_row(schema: &TableDefinition, row: &Row) -> Result<Vec<u8>> {
    let stored = stored_columns(schema);
    let mut data = Vec::new();
    let mut ends = Vec::with_capacity(stored.len());
    let mut nulls = Vec::with_capacity(stored.len());
    for column in &stored {
        let value = row.get(&column.name).ok_or_else(|| {
            codec_argument(format!(
                "Row for `{}` is missing column `{}`",
                schema.name, column.name
            ))
        })?;
        nulls.push(value.is_null());
        if !value.is_null() {
            encode_value(column, value, &mut data, &schema.name)?;
        }
        ends.push(data.len());
    }

    // Omit trailing columns that equal their defaults. A column's default is a literal no later
    // statement can change, so this is canonical, and ADD COLUMN needs no rewrite.
    let mut count = stored.len();
    while count > 0 {
        let start = if count >= 2 { ends[count - 2] } else { 0 };
        let stored_value = (!nulls[count - 1]).then(|| &data[start..ends[count - 1]]);
        if !is_default(stored[count - 1], stored_value, &schema.name)? {
            break;
        }
        count -= 1;
    }
    data.truncate(if count == 0 { 0 } else { ends[count - 1] });

    let largest_offset = if count >= 2 { ends[count - 2] } else { 0 };
    let (width_code, width) = offset_width(largest_offset);
    let has_nulls = nulls[..count].iter().any(|null| *null);
    let bitmap_bytes = if has_nulls { count.div_ceil(8) } else { 0 };
    let total = RECORD_HEADER_BYTES + bitmap_bytes + count.saturating_sub(1) * width + data.len();
    if total > MAX_PAGED_VALUE_BYTES {
        return Err(value_too_large(format!(
            "An encoded row cannot exceed {MAX_PAGED_VALUE_BYTES} bytes"
        )));
    }
    let mut record = Vec::with_capacity(total);
    record.push(width_code | if has_nulls { RECORD_HAS_NULLS } else { 0 });
    record.push(u8::try_from(count).map_err(|_| {
        codec_argument(format!(
            "Table `{}` cannot store more than 255 non-key columns",
            schema.name
        ))
    })?);
    if has_nulls {
        let mut bitmap = vec![0u8; bitmap_bytes];
        for (index, _) in nulls[..count].iter().enumerate().filter(|(_, null)| **null) {
            bitmap[index / 8] |= 1 << (index % 8);
        }
        record.extend_from_slice(&bitmap);
    }
    for end in &ends[..count.saturating_sub(1)] {
        record.extend_from_slice(&(*end as u32).to_le_bytes()[..width]);
    }
    record.extend_from_slice(&data);
    Ok(record)
}

/// Decodes a whole stored table entry, as [`StoredRecord::to_row`] does.
#[cfg(test)]
pub(crate) fn decode_row(schema: &TableDefinition, key: &[u8], value: &[u8]) -> Result<Row> {
    StoredRecord::new(schema, &RecordLayout::new(schema)?, key, value)?.to_row()
}

/// Decodes a whole stored table entry, as [`StoredRecord::to_row_strictly`] does.
#[cfg(test)]
pub(crate) fn decode_row_strictly(
    schema: &TableDefinition,
    key: &[u8],
    value: &[u8],
) -> Result<Row> {
    StoredRecord::new(schema, &RecordLayout::new(schema)?, key, value)?.to_row_strictly()
}

/// Where each of a table's columns lives in its stored entries: in the B-tree key for a
/// primary-key column, and at a position in the record for any other. It is worked out once for
/// each version of a table's schema, rather than for every row.
#[derive(Clone, Debug)]
pub(crate) struct RecordLayout {
    /// Each column's place, in schema order.
    slots: Vec<ColumnSlot>,
    /// The type of each primary-key component, in key order.
    key_types: Vec<ColumnType>,
    /// The schema position of each stored column, in record order.
    stored: Vec<usize>,
}

#[derive(Clone, Copy, Debug)]
enum ColumnSlot {
    Key(usize),
    Stored(usize),
}

impl RecordLayout {
    /// The type of each primary-key column, in key order.
    pub(crate) fn key_types(&self) -> &[ColumnType] {
        &self.key_types
    }

    pub(crate) fn new(schema: &TableDefinition) -> Result<Self> {
        let key_types = schema
            .primary_key
            .iter()
            .map(|name| schema_column_type(schema, name).map_err(as_storage_corruption))
            .collect::<Result<Vec<_>>>()?;
        let mut slots = Vec::with_capacity(schema.columns.len());
        let mut stored = Vec::with_capacity(schema.columns.len().saturating_sub(key_types.len()));
        for (index, column) in schema.columns.iter().enumerate() {
            match schema
                .primary_key
                .iter()
                .position(|name| *name == column.name)
            {
                Some(position) => slots.push(ColumnSlot::Key(position)),
                None => {
                    slots.push(ColumnSlot::Stored(stored.len()));
                    stored.push(index);
                }
            }
        }
        Ok(Self {
            slots,
            key_types,
            stored,
        })
    }
}

/// A stored table entry read in place. Each column is decoded only when it is read, straight from
/// the entry's key and record, and a text value borrows its bytes.
///
/// Stored pages are verified as they are loaded, and rows were checked when written or when the
/// database was opened, so a read checks only what reading safely requires: opening a record
/// checks its header, and reading a column checks that column's bounds and encoding.
/// [`Self::to_row_strictly`] also rejects every encoding this engine would not have written.
#[derive(Clone, Copy)]
pub(crate) struct StoredRecord<'a> {
    schema: &'a TableDefinition,
    layout: &'a RecordLayout,
    key: &'a [u8],
    value: &'a [u8],
    count: usize,
    width: usize,
    bitmap: &'a [u8],
    offsets_start: usize,
    data: &'a [u8],
}

/// A stored row's entry copied out of its page, its encoded primary key and its record, which a
/// [`StoredRecord`] reads in place again.
#[derive(Clone, Debug)]
pub(crate) struct StoredEntry {
    bytes: Box<[u8]>,
    key_length: usize,
}

impl StoredEntry {
    /// The encoded primary key.
    pub(crate) fn key(&self) -> &[u8] {
        &self.bytes[..self.key_length]
    }

    /// The row record.
    pub(crate) fn value(&self) -> &[u8] {
        &self.bytes[self.key_length..]
    }
}

impl<'a> StoredRecord<'a> {
    /// Opens an entry of `schema`'s table, whose columns are placed as `layout` says.
    pub(crate) fn new(
        schema: &'a TableDefinition,
        layout: &'a RecordLayout,
        key: &'a [u8],
        value: &'a [u8],
    ) -> Result<Self> {
        debug_assert_eq!(layout.slots.len(), schema.columns.len());
        let [flags, count, ..] = *value else {
            return Err(storage_corrupt("A stored row record is truncated"));
        };
        if flags & RECORD_RESERVED != 0 || flags & RECORD_OFFSET_WIDTH == 3 {
            return Err(storage_version(format!(
                "Row record flags {flags:#04x} are not supported"
            )));
        }
        let count = count as usize;
        if count > layout.stored.len() {
            return Err(storage_corrupt(
                "A stored row record has more columns than its table",
            ));
        }
        let width = 1 << (flags & RECORD_OFFSET_WIDTH);
        let bitmap_bytes = if flags & RECORD_HAS_NULLS != 0 {
            count.div_ceil(8)
        } else {
            0
        };
        let offsets_start = RECORD_HEADER_BYTES + bitmap_bytes;
        let data_start = offsets_start + count.saturating_sub(1) * width;
        if data_start > value.len() {
            return Err(storage_corrupt("A stored row record is truncated"));
        }
        Ok(Self {
            schema,
            layout,
            key,
            value,
            count,
            width,
            bitmap: &value[RECORD_HEADER_BYTES..offsets_start],
            offsets_start,
            data: &value[data_start..],
        })
    }

    /// The entry's B-tree key, which is its encoded primary key.
    pub(crate) fn key(&self) -> &'a [u8] {
        self.key
    }

    /// The schema of the table the entry belongs to.
    pub(crate) fn schema(&self) -> &'a TableDefinition {
        self.schema
    }

    /// The length of the entry, its key and its record.
    pub(crate) fn entry_len(&self) -> usize {
        self.key.len() + self.value.len()
    }

    /// A copy of the entry, its key and record, to read in place again later.
    pub(crate) fn to_entry(self) -> StoredEntry {
        let mut bytes = Vec::with_capacity(self.key.len() + self.value.len());
        bytes.extend_from_slice(self.key);
        bytes.extend_from_slice(self.value);
        StoredEntry {
            bytes: bytes.into_boxed_slice(),
            key_length: self.key.len(),
        }
    }

    /// The value of the column at `index`, in schema order.
    pub(crate) fn column(&self, index: usize) -> Result<ValueRef<'a>> {
        match self.layout.slots.get(index) {
            Some(ColumnSlot::Key(position)) => self.key_component(*position),
            Some(ColumnSlot::Stored(position)) => {
                self.stored_column(*position, &self.schema.columns[index])
            }
            None => Err(codec_argument(format!(
                "Table `{}` has no column {index}",
                self.schema.name
            ))),
        }
    }

    /// Decodes every column, the primary key from the key and the rest from the record.
    pub(crate) fn to_row(self) -> Result<Row> {
        self.decode(false)
    }

    /// Decodes every column, rejecting any encoding this engine would not have written.
    pub(crate) fn to_row_strictly(self) -> Result<Row> {
        self.decode(true)
    }

    fn decode(&self, strict: bool) -> Result<Row> {
        let mut row = Row::new();
        let mut offset = 0;
        for (name, data_type) in self.schema.primary_key.iter().zip(&self.layout.key_types) {
            let end = component_end(self.key, offset, *data_type)?;
            row.insert(
                name.clone(),
                decode_component(&self.key[offset..end], *data_type)?.into_value(),
            );
            offset = end;
        }
        if offset != self.key.len() {
            return Err(storage_corrupt(
                "A stored row key contains trailing bytes after its primary key",
            ));
        }
        self.decode_columns(&mut row, strict)?;
        Ok(row)
    }

    fn key_component(&self, position: usize) -> Result<ValueRef<'a>> {
        let mut start = 0;
        for data_type in &self.layout.key_types[..position] {
            start = component_end(self.key, start, *data_type)?;
        }
        let data_type = self.layout.key_types[position];
        let end = component_end(self.key, start, data_type)?;
        decode_component(&self.key[start..end], data_type)
    }

    fn stored_column(
        &self,
        position: usize,
        column: &'a crate::ColumnDefinition,
    ) -> Result<ValueRef<'a>> {
        if position >= self.count {
            return Ok(stored_default(column));
        }
        let start = if position == 0 {
            0
        } else {
            self.end(position - 1)
        };
        let end = self.end(position);
        if end < start || end > self.data.len() {
            return Err(storage_corrupt(
                "A stored row record has invalid column offsets",
            ));
        }
        let bytes = &self.data[start..end];
        if self.is_null(position) {
            return if bytes.is_empty() {
                Ok(ValueRef::Null)
            } else {
                Err(storage_corrupt("A stored NULL column has a value"))
            };
        }
        decode_value(column.data_type, bytes, false)
    }

    /// Where the stored column at `position` ends within the data.
    fn end(&self, position: usize) -> usize {
        if position + 1 == self.count {
            return self.data.len();
        }
        // Read byte by byte: copying a slice of the offset's width would call memcpy.
        let at = self.offsets_start + position * self.width;
        let value = self.value;
        match self.width {
            1 => usize::from(value[at]),
            2 => usize::from(u16::from_le_bytes([value[at], value[at + 1]])),
            _ => u32::from_le_bytes([value[at], value[at + 1], value[at + 2], value[at + 3]])
                as usize,
        }
    }

    fn is_null(&self, position: usize) -> bool {
        self.bitmap
            .get(position / 8)
            .is_some_and(|byte| byte & (1 << (position % 8)) != 0)
    }

    fn decode_columns(&self, row: &mut Row, strict: bool) -> Result<()> {
        let schema = self.schema;
        let mut start = 0;
        for (position, index) in self.layout.stored.iter().enumerate() {
            let column = &schema.columns[*index];
            if position >= self.count {
                row.insert(column.name.clone(), stored_default(column).into_value());
                continue;
            }
            let end = self.end(position);
            if end < start || end > self.data.len() {
                return Err(storage_corrupt(
                    "A stored row record has invalid column offsets",
                ));
            }
            let bytes = &self.data[start..end];
            start = end;
            let decoded = if self.is_null(position) {
                if !bytes.is_empty() {
                    return Err(storage_corrupt("A stored NULL column has a value"));
                }
                Value::Null
            } else {
                decode_value(column.data_type, bytes, strict)?.into_value()
            };
            row.insert(column.name.clone(), decoded);
        }
        if strict {
            self.check_canonical(row)?;
        }
        Ok(())
    }

    fn check_canonical(&self, row: &Row) -> Result<()> {
        let count = self.count;
        let flags = self.value[0];
        let largest_offset = if count >= 2 { self.end(count - 2) } else { 0 };
        let has_nulls = (0..count).any(|position| self.is_null(position));
        let unused_bits = (count..self.bitmap.len() * 8).any(|position| self.is_null(position));
        if offset_width(largest_offset).0 != flags & RECORD_OFFSET_WIDTH
            || (flags & RECORD_HAS_NULLS != 0) != has_nulls
            || unused_bits
        {
            return Err(storage_corrupt("A stored row record is not canonical"));
        }
        if count > 0 {
            let schema = self.schema;
            let column = &schema.columns[self.layout.stored[count - 1]];
            let mut last = Vec::new();
            let last_value = match row.get(&column.name) {
                Some(value) if !value.is_null() => {
                    encode_value(column, value, &mut last, &schema.name)
                        .map_err(as_storage_corruption)?;
                    Some(last.as_slice())
                }
                _ => None,
            };
            if is_default(column, last_value, &schema.name).map_err(as_storage_corruption)? {
                return Err(storage_corrupt(
                    "A stored row record keeps a trailing default column",
                ));
            }
        }
        Ok(())
    }
}

/// A table's non-key columns, in schema order: the columns a row record stores.
fn stored_columns(schema: &TableDefinition) -> Vec<&crate::ColumnDefinition> {
    schema
        .columns
        .iter()
        .filter(|column| !schema.primary_key.contains(&column.name))
        .collect()
}

/// The value a row holds for a column its record omits: the column's default, normalized as it
/// would be if it had been stored.
fn stored_default(column: &crate::ColumnDefinition) -> ValueRef<'_> {
    match &column.default {
        None => ValueRef::Null,
        Some(value) if column.data_type == ColumnType::Float => value.as_f64().map_or_else(
            || ValueRef::from_value(value, ColumnType::Float),
            ValueRef::Float,
        ),
        Some(value) => ValueRef::from_value(value, column.data_type),
    }
}

/// Reports whether a column's encoded value, or `None` for NULL, is its default's encoding. Encoded
/// bytes decide, so values SQL considers equal but stores differently, such as `-0.0` and `0.0`,
/// stay distinct.
fn is_default(
    column: &crate::ColumnDefinition,
    encoded: Option<&[u8]>,
    table: &str,
) -> Result<bool> {
    match (
        column.default.as_ref().filter(|value| !value.is_null()),
        encoded,
    ) {
        (None, encoded) => Ok(encoded.is_none()),
        (Some(_), None) => Ok(false),
        (Some(default), Some(encoded)) => {
            let mut bytes = Vec::new();
            encode_value(column, default, &mut bytes, table)?;
            Ok(bytes == encoded)
        }
    }
}

/// The smallest offset width, and its flag code, that can hold `largest` bytes.
fn offset_width(largest: usize) -> (u8, usize) {
    if largest <= 0xff {
        (0, 1)
    } else if largest <= 0xffff {
        (1, 2)
    } else {
        (2, 4)
    }
}

fn encode_value(
    column: &crate::ColumnDefinition,
    value: &Value,
    data: &mut Vec<u8>,
    table: &str,
) -> Result<()> {
    match column.data_type {
        ColumnType::Boolean => {
            let value = value
                .as_bool()
                .ok_or_else(|| value_type_mismatch(table, &column.name, "boolean"))?;
            data.push(u8::from(value));
        }
        ColumnType::Integer => {
            let value = safe_integer(value)
                .ok_or_else(|| value_type_mismatch(table, &column.name, "integer"))?;
            let length = integer_length(value);
            data.extend_from_slice(&value.to_le_bytes()[..length]);
        }
        ColumnType::Float => {
            let value = value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| value_type_mismatch(table, &column.name, "finite float"))?;
            data.extend_from_slice(&value.to_le_bytes());
        }
        ColumnType::Text => {
            let value = value
                .as_str()
                .ok_or_else(|| value_type_mismatch(table, &column.name, "text"))?;
            data.extend_from_slice(value.as_bytes());
        }
        ColumnType::Json => data.extend_from_slice(&encode_canonical_json(value)?),
    }
    Ok(())
}

/// The fewest little-endian bytes whose sign extension is `value`; zero needs none.
fn integer_length(value: i64) -> usize {
    if value == 0 {
        return 0;
    }
    (1..=8)
        .find(|length| {
            let shift = 64 - 8 * length;
            (value << shift) >> shift == value
        })
        .expect("eight bytes hold every integer")
}

fn decode_value(data_type: ColumnType, bytes: &[u8], strict: bool) -> Result<ValueRef<'_>> {
    let value = match data_type {
        ColumnType::Boolean => match bytes {
            [0] => ValueRef::Boolean(false),
            [1] => ValueRef::Boolean(true),
            _ => return Err(storage_corrupt("A stored boolean is not canonical")),
        },
        ColumnType::Integer => {
            if bytes.len() > 7 {
                return Err(storage_corrupt("A stored integer is too long"));
            }
            // Gathered byte by byte: copying a slice of the value's length would call memcpy.
            let mut raw = 0_u64;
            for (index, byte) in bytes.iter().enumerate() {
                raw |= u64::from(*byte) << (8 * index);
            }
            let shift = 64 - 8 * bytes.len() as u32;
            let value = if bytes.is_empty() {
                0
            } else {
                ((raw as i64) << shift) >> shift
            };
            if value.unsigned_abs() > MAX_SAFE_INTEGER
                || strict && integer_length(value) != bytes.len()
            {
                return Err(storage_corrupt("A stored integer is not canonical"));
            }
            ValueRef::Integer(value)
        }
        ColumnType::Float => {
            let bytes = <[u8; 8]>::try_from(bytes)
                .map_err(|_| storage_corrupt("A stored float is not eight bytes"))?;
            let value = f64::from_le_bytes(bytes);
            if !value.is_finite() {
                return Err(storage_corrupt("A stored float is not finite"));
            }
            ValueRef::Float(value)
        }
        ColumnType::Text => {
            ValueRef::Text(Cow::Borrowed(std::str::from_utf8(bytes).map_err(|_| {
                storage_corrupt("A stored text value is not valid UTF-8")
            })?))
        }
        ColumnType::Json if strict => {
            ValueRef::Json(Cow::Owned(decode_canonical_json(bytes, "JSON column")?))
        }
        ColumnType::Json => ValueRef::Json(Cow::Owned(serde_json::from_slice(bytes).map_err(
            |error| storage_corrupt(format!("A stored JSON value is invalid: {error}")),
        )?)),
    };
    Ok(value)
}

pub(crate) fn encode_catalog_header_record(header: &CatalogHeader) -> Result<(Vec<u8>, Vec<u8>)> {
    validate_catalog_header(header)?;
    let mut value = Vec::with_capacity(CATALOG_HEADER_BYTES);
    append_record_prefix(&mut value);
    value.extend_from_slice(&header.next_tree_id.to_le_bytes());
    value.extend_from_slice(&header.table_count.to_le_bytes());
    value.extend_from_slice(&header.index_count.to_le_bytes());
    Ok((vec![CATALOG_HEADER_KEY], value))
}

pub(crate) fn decode_catalog_header_record(key: &[u8], value: &[u8]) -> Result<CatalogHeader> {
    if decode_catalog_key(key)? != CatalogKey::Header {
        return Err(storage_corrupt(
            "A catalog header value must use the catalog header key",
        ));
    }
    if value.len() != CATALOG_HEADER_BYTES {
        return Err(storage_corrupt(format!(
            "A catalog header value must contain exactly {CATALOG_HEADER_BYTES} bytes"
        )));
    }
    validate_record_prefix(value, "catalog header")?;
    let header = CatalogHeader {
        next_tree_id: read_u64(value, 4),
        table_count: read_u32(value, 12),
        index_count: read_u32(value, 16),
    };
    validate_catalog_header(&header).map_err(as_storage_corruption)?;
    Ok(header)
}

#[cfg(test)]
pub(crate) fn encode_catalog_table_record(
    record: &CatalogTableRecord,
) -> Result<(Vec<u8>, Vec<u8>)> {
    validate_schema_shape(&record.schema)?;
    encode_catalog_table_record_with_schema(
        &record.schema.name,
        record.tree_id,
        record.root_page_id,
        record.row_count,
        record.hash,
        &encode_catalog_schema(&record.schema)?,
    )
}

/// Encodes a table's schema as its catalog record holds it.
pub(crate) fn encode_catalog_schema(schema: &TableDefinition) -> Result<Vec<u8>> {
    let model = serde_json::to_value(schema).map_err(|error| {
        codec_argument(format!("Could not encode catalog table schema: {error}"))
    })?;
    encode_canonical_json(&model)
}

/// Encodes a table's catalog record around a schema that [`encode_catalog_schema`] encoded and the
/// catalog already validated.
pub(crate) fn encode_catalog_table_record_with_schema(
    name: &str,
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    row_count: u64,
    hash: u64,
    schema: &[u8],
) -> Result<(Vec<u8>, Vec<u8>)> {
    validate_catalog_name(name)?;
    validate_catalog_item(tree_id, root_page_id, row_count, "table")?;
    let key = encode_catalog_key(CATALOG_TABLE_KEY, name)?;
    let value = encode_catalog_item_body(tree_id, root_page_id, row_count, hash, schema)?;
    Ok((key, value))
}

pub(crate) fn encode_catalog_table_key(name: &str) -> Result<Vec<u8>> {
    encode_catalog_key(CATALOG_TABLE_KEY, name)
}

pub(crate) fn decode_catalog_table_record(key: &[u8], value: &[u8]) -> Result<CatalogTableRecord> {
    let CatalogKey::Table(name) = decode_catalog_key(key)? else {
        return Err(storage_corrupt(
            "A catalog table value must use a table key",
        ));
    };
    let (tree_id, root_page_id, row_count, hash, schema) =
        decode_catalog_item_value::<TableDefinition>(value, "table")?;
    if schema.name != name {
        return Err(storage_corrupt(format!(
            "Catalog table key `{name}` does not match schema name `{}`",
            schema.name
        )));
    }
    validate_schema_shape(&schema).map_err(as_storage_corruption)?;
    Ok(CatalogTableRecord {
        schema,
        tree_id,
        root_page_id,
        row_count,
        hash,
    })
}

pub(crate) fn encode_catalog_index_record(
    record: &CatalogIndexRecord,
) -> Result<(Vec<u8>, Vec<u8>)> {
    validate_catalog_name(&record.definition.name)?;
    validate_catalog_name(&record.definition.table)?;
    validate_index_shape(&record.definition)?;
    validate_catalog_item(
        record.tree_id,
        record.root_page_id,
        record.entry_count,
        "index",
    )?;
    let key = encode_catalog_key(CATALOG_INDEX_KEY, &record.definition.name)?;
    // A secondary index is derived from the rows of its table, so it carries no fingerprint of
    // its own; comparing the table's rows already covers everything the index represents.
    let value = encode_catalog_item_value(
        record.tree_id,
        record.root_page_id,
        record.entry_count,
        crate::hash::EMPTY_HASH,
        &record.definition,
        "index definition",
    )?;
    Ok((key, value))
}

pub(crate) fn encode_catalog_index_key(name: &str) -> Result<Vec<u8>> {
    encode_catalog_key(CATALOG_INDEX_KEY, name)
}

pub(crate) fn decode_catalog_index_record(key: &[u8], value: &[u8]) -> Result<CatalogIndexRecord> {
    let CatalogKey::Index(name) = decode_catalog_key(key)? else {
        return Err(storage_corrupt(
            "A catalog index value must use an index key",
        ));
    };
    let (tree_id, root_page_id, entry_count, hash, definition) =
        decode_catalog_item_value::<IndexDefinition>(value, "index")?;
    if hash != crate::hash::EMPTY_HASH {
        return Err(storage_corrupt(
            "A catalog index record must not carry a fingerprint",
        ));
    }
    if definition.name != name {
        return Err(storage_corrupt(format!(
            "Catalog index key `{name}` does not match definition name `{}`",
            definition.name
        )));
    }
    validate_catalog_name(&definition.table).map_err(as_storage_corruption)?;
    validate_index_shape(&definition).map_err(as_storage_corruption)?;
    Ok(CatalogIndexRecord {
        definition,
        tree_id,
        root_page_id,
        entry_count,
    })
}

pub(crate) fn decode_catalog_key(key: &[u8]) -> Result<CatalogKey> {
    if key.is_empty() || key.len() > MAX_BTREE_KEY_BYTES {
        return Err(storage_corrupt(format!(
            "A catalog key must contain between 1 and {MAX_BTREE_KEY_BYTES} bytes"
        )));
    }
    match key[0] {
        CATALOG_HEADER_KEY if key.len() == 1 => Ok(CatalogKey::Header),
        CATALOG_HEADER_KEY => Err(storage_corrupt(
            "The catalog header key contains trailing bytes",
        )),
        kind @ (CATALOG_TABLE_KEY | CATALOG_INDEX_KEY) => {
            let name = std::str::from_utf8(&key[1..])
                .map_err(|_| storage_corrupt("A catalog name is not valid UTF-8"))?
                .to_owned();
            validate_catalog_name(&name).map_err(as_storage_corruption)?;
            Ok(if kind == CATALOG_TABLE_KEY {
                CatalogKey::Table(name)
            } else {
                CatalogKey::Index(name)
            })
        }
        kind => Err(storage_version(format!(
            "Catalog key kind {kind:#04x} is not supported"
        ))),
    }
}

#[derive(Clone, Copy)]
enum NullPolicy {
    Reject,
    Omit,
}

fn encode_columns(
    schema: &TableDefinition,
    columns: &[String],
    row: &Row,
    null_policy: NullPolicy,
) -> Result<Option<Vec<u8>>> {
    if columns.is_empty() {
        return Err(codec_argument("A key tuple must name at least one column"));
    }
    // Room for a typical tuple, so that encoding it allocates once.
    let mut key = Vec::with_capacity(16 * columns.len());
    for column in columns {
        let value = row.get(column).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing key column `{column}`",
                schema.name
            ))
        })?;
        if value == &Value::Null {
            return match null_policy {
                NullPolicy::Reject => Err(EngineError::invalid_change(format!(
                    "Primary-key column `{column}` in `{}` cannot be null",
                    schema.name
                ))),
                NullPolicy::Omit => Ok(None),
            };
        }
        let data_type = schema_column_type(schema, column)?;
        encode_component(&mut key, data_type, value, &schema.name, column)?;
        validate_key_size(&key)?;
    }
    Ok(Some(key))
}

fn encode_component(
    key: &mut Vec<u8>,
    data_type: ColumnType,
    value: &Value,
    table: &str,
    column: &str,
) -> Result<()> {
    match data_type {
        ColumnType::Boolean => {
            let value = value
                .as_bool()
                .ok_or_else(|| key_type_mismatch(table, column, "boolean"))?;
            key.push(u8::from(value));
        }
        ColumnType::Integer => {
            let value =
                safe_integer(value).ok_or_else(|| key_type_mismatch(table, column, "integer"))?;
            push_integer_component(key, value);
        }
        ColumnType::Float => {
            let Value::Number(number) = value else {
                return Err(key_type_mismatch(table, column, "float"));
            };
            let value = number
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| key_type_mismatch(table, column, "finite float"))?;
            push_float_component(key, value);
        }
        ColumnType::Text => {
            let value = value
                .as_str()
                .ok_or_else(|| key_type_mismatch(table, column, "text"))?;
            push_text_component(key, value);
        }
        ColumnType::Json => {
            return Err(codec_argument(format!(
                "JSON column `{column}` in `{table}` cannot be part of a key"
            )));
        }
    }
    Ok(())
}

/// Encodes a value read from a row as a key component, exactly as [`encode_component`] encodes the
/// equivalent JSON value, without converting a typed value to JSON first.
fn encode_component_ref(
    key: &mut Vec<u8>,
    data_type: ColumnType,
    value: &ValueRef<'_>,
    table: &str,
    column: &str,
) -> Result<()> {
    match (data_type, value) {
        (ColumnType::Boolean, ValueRef::Boolean(value)) => key.push(u8::from(*value)),
        (ColumnType::Integer, ValueRef::Integer(value))
            if value.unsigned_abs() <= MAX_SAFE_INTEGER =>
        {
            push_integer_component(key, *value);
        }
        (ColumnType::Float, ValueRef::Float(value)) if value.is_finite() => {
            push_float_component(key, *value);
        }
        (ColumnType::Text, ValueRef::Text(value)) => push_text_component(key, value),
        _ => return encode_component(key, data_type, &value.clone().into_value(), table, column),
    }
    Ok(())
}

fn push_integer_component(key: &mut Vec<u8>, value: i64) {
    let sortable = (value as u64) ^ (1_u64 << 63);
    key.extend_from_slice(&sortable.to_be_bytes());
}

fn push_float_component(key: &mut Vec<u8>, value: f64) {
    // SQL comparison already operates in f64 space. Normalize signed zero before deriving
    // lexicographically sortable IEEE-754 bytes, so a B-tree cannot admit two primary keys which
    // SQL considers equal.
    let normalized = if value == 0.0 { 0.0 } else { value };
    let bits = normalized.to_bits();
    let sortable = if bits & (1_u64 << 63) == 0 {
        bits ^ (1_u64 << 63)
    } else {
        !bits
    };
    key.extend_from_slice(&sortable.to_be_bytes());
}

fn push_text_component(key: &mut Vec<u8>, value: &str) {
    for byte in value.bytes() {
        key.push(byte);
        if byte == 0 {
            key.push(TEXT_ESCAPED_ZERO);
        }
    }
    key.extend_from_slice(&[0, TEXT_TERMINATOR]);
}

/// Returns where the key component starting at `offset` ends.
fn component_end(key: &[u8], offset: usize, data_type: ColumnType) -> Result<usize> {
    let fixed = |length: usize| {
        offset
            .checked_add(length)
            .filter(|end| *end <= key.len())
            .ok_or_else(|| storage_corrupt("A storage key component is truncated"))
    };
    match data_type {
        ColumnType::Boolean => fixed(1),
        ColumnType::Integer | ColumnType::Float => fixed(8),
        ColumnType::Text => {
            let mut index = offset;
            loop {
                let zero = key
                    .get(index..)
                    .and_then(|rest| rest.iter().position(|byte| *byte == 0))
                    .ok_or_else(|| storage_corrupt("A text key component is not terminated"))?
                    + index;
                match key.get(zero + 1) {
                    Some(&TEXT_TERMINATOR) => return Ok(zero + 2),
                    Some(&TEXT_ESCAPED_ZERO) => index = zero + 2,
                    _ => {
                        return Err(storage_corrupt(
                            "A text key component contains an invalid escape",
                        ));
                    }
                }
            }
        }
        ColumnType::Json => Err(storage_corrupt("A storage key cannot contain JSON")),
    }
}

/// Decodes one complete key component, rejecting any encoding the encoder would not produce. Text
/// borrows the key's bytes unless it contains an escaped zero.
fn decode_component(bytes: &[u8], data_type: ColumnType) -> Result<ValueRef<'_>> {
    let sortable = || {
        <[u8; 8]>::try_from(bytes)
            .map(u64::from_be_bytes)
            .map_err(|_| storage_corrupt("A storage key component has the wrong length"))
    };
    match data_type {
        ColumnType::Boolean => match bytes {
            [0] => Ok(ValueRef::Boolean(false)),
            [1] => Ok(ValueRef::Boolean(true)),
            _ => Err(storage_corrupt("A boolean key component is not canonical")),
        },
        ColumnType::Integer => {
            let value = (sortable()? ^ (1_u64 << 63)) as i64;
            if value.unsigned_abs() > MAX_SAFE_INTEGER {
                return Err(storage_corrupt("An integer key component is out of range"));
            }
            Ok(ValueRef::Integer(value))
        }
        ColumnType::Float => {
            if !valid_encoded_float_component(bytes) {
                return Err(storage_corrupt("A float key component is not canonical"));
            }
            let sortable = sortable()?;
            let bits = if sortable & (1_u64 << 63) != 0 {
                sortable ^ (1_u64 << 63)
            } else {
                !sortable
            };
            Ok(ValueRef::Float(f64::from_bits(bits)))
        }
        ColumnType::Text => {
            let payload = &bytes[..bytes.len() - 2];
            let invalid = || storage_corrupt("A text key component is not valid UTF-8");
            if !payload.contains(&0) {
                return std::str::from_utf8(payload)
                    .map(|text| ValueRef::Text(Cow::Borrowed(text)))
                    .map_err(|_| invalid());
            }
            let mut text = Vec::with_capacity(payload.len());
            let mut escaped = false;
            for byte in payload {
                if escaped {
                    escaped = false;
                } else {
                    text.push(*byte);
                    escaped = *byte == 0;
                }
            }
            String::from_utf8(text)
                .map(|text| ValueRef::Text(Cow::Owned(text)))
                .map_err(|_| invalid())
        }
        ColumnType::Json => Err(storage_corrupt("A storage key cannot contain JSON")),
    }
}

fn validate_tuple_prefix(
    schema: &TableDefinition,
    columns: &[String],
    key: &[u8],
    mut offset: usize,
) -> Result<usize> {
    for column in columns {
        let data_type = schema_column_type(schema, column)?;
        let end = component_end(key, offset, data_type)?;
        decode_component(&key[offset..end], data_type)?;
        offset = end;
    }
    Ok(offset)
}

fn valid_encoded_float_component(payload: &[u8]) -> bool {
    let Ok(sortable) = <[u8; 8]>::try_from(payload).map(u64::from_be_bytes) else {
        return false;
    };
    let bits = if sortable & (1_u64 << 63) != 0 {
        sortable ^ (1_u64 << 63)
    } else {
        !sortable
    };
    let value = f64::from_bits(bits);
    value.is_finite() && (value != 0.0 || bits == 0)
}

fn schema_column_type(schema: &TableDefinition, column: &str) -> Result<ColumnType> {
    schema
        .columns
        .iter()
        .find(|definition| definition.name == column)
        .map(|definition| definition.data_type)
        .ok_or_else(|| EngineError::column_not_found(column, &schema.name))
}

fn safe_integer(value: &Value) -> Option<i64> {
    value
        .as_u64()
        .filter(|number| *number <= MAX_SAFE_INTEGER)
        .map(|number| number as i64)
        .or_else(|| {
            value.as_i64().filter(|number| {
                *number >= -(MAX_SAFE_INTEGER as i64) && *number <= MAX_SAFE_INTEGER as i64
            })
        })
}

fn validate_index_identity(schema: &TableDefinition, definition: &IndexDefinition) -> Result<()> {
    validate_schema_shape(schema)?;
    if definition.table != schema.name {
        return Err(codec_argument(format!(
            "Index `{}` belongs to `{}`, not `{}`",
            definition.name, definition.table, schema.name
        )));
    }
    validate_index_shape(definition)?;
    for name in &definition.columns {
        let column = schema
            .columns
            .iter()
            .find(|column| column.name == *name)
            .ok_or_else(|| EngineError::column_not_found(name, &schema.name))?;
        if matches!(column.data_type, ColumnType::Float | ColumnType::Json) {
            return Err(codec_argument(format!(
                "Index `{}` cannot use float or JSON column `{name}`",
                definition.name
            )));
        }
    }
    Ok(())
}

fn validate_schema_shape(schema: &TableDefinition) -> Result<()> {
    validate_catalog_name(&schema.name)?;
    if schema.columns.is_empty() {
        return Err(codec_argument(format!(
            "Table `{}` must declare at least one column",
            schema.name
        )));
    }
    if schema.primary_key.is_empty() {
        return Err(codec_argument(format!(
            "Table `{}` must declare a primary key",
            schema.name
        )));
    }
    for (index, column) in schema.primary_key.iter().enumerate() {
        validate_catalog_name(column)?;
        if schema.primary_key[..index].contains(column) {
            return Err(codec_argument(format!(
                "Table `{}` names primary-key column `{column}` more than once",
                schema.name
            )));
        }
    }
    if schema.columns.len() > MAX_COLUMNS {
        return Err(codec_argument(format!(
            "Table `{}` cannot contain more than {MAX_COLUMNS} columns",
            schema.name
        )));
    }
    for (index, column) in schema.columns.iter().enumerate() {
        validate_catalog_name(&column.name)?;
        if schema.columns[..index]
            .iter()
            .any(|previous| previous.name == column.name)
        {
            return Err(codec_argument(format!(
                "Table `{}` declares column `{}` more than once",
                schema.name, column.name
            )));
        }
        if schema.primary_key.contains(&column.name) {
            if column.nullable {
                return Err(codec_argument(format!(
                    "Primary-key column `{}` in `{}` cannot be nullable",
                    column.name, schema.name
                )));
            }
            if column.data_type == ColumnType::Json {
                return Err(codec_argument(format!(
                    "Page storage does not support JSON primary-key column `{}` in `{}`",
                    column.name, schema.name
                )));
            }
        }
        if let Some(default) = &column.default {
            validate_catalog_default(schema, column, default)?;
        }
    }
    if schema.primary_key.iter().any(|primary_key| {
        !schema
            .columns
            .iter()
            .any(|column| column.name == *primary_key)
    }) {
        return Err(codec_argument(format!(
            "Table `{}` has a primary-key column outside its catalog",
            schema.name
        )));
    }
    Ok(())
}

fn validate_catalog_default(
    schema: &TableDefinition,
    column: &crate::ColumnDefinition,
    value: &Value,
) -> Result<()> {
    if value == &Value::Null {
        return if column.nullable {
            Ok(())
        } else {
            Err(codec_argument(format!(
                "Default for non-null column `{}` in `{}` cannot be null",
                column.name, schema.name
            )))
        };
    }
    let valid = match column.data_type {
        ColumnType::Boolean => value.is_boolean(),
        ColumnType::Integer => safe_integer(value).is_some(),
        ColumnType::Float => value.is_number(),
        ColumnType::Text => value.is_string(),
        ColumnType::Json => true,
    };
    if valid {
        Ok(())
    } else {
        Err(codec_argument(format!(
            "Default for column `{}` in `{}` has the wrong type",
            column.name, schema.name
        )))
    }
}

fn validate_index_shape(definition: &IndexDefinition) -> Result<()> {
    validate_catalog_name(&definition.name)?;
    validate_catalog_name(&definition.table)?;
    if definition.columns.is_empty() {
        return Err(codec_argument(format!(
            "Index `{}` must name at least one column",
            definition.name
        )));
    }
    for (index, column) in definition.columns.iter().enumerate() {
        validate_catalog_name(column)?;
        if definition.columns[..index].contains(column) {
            return Err(codec_argument(format!(
                "Index `{}` names column `{column}` more than once",
                definition.name
            )));
        }
    }
    Ok(())
}

fn validate_key_size(key: &[u8]) -> Result<()> {
    if key.len() > MAX_BTREE_KEY_BYTES {
        Err(value_too_large(format!(
            "An encoded storage key cannot exceed {MAX_BTREE_KEY_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

fn encode_catalog_key(kind: u8, name: &str) -> Result<Vec<u8>> {
    validate_catalog_name(name)?;
    let mut key = Vec::with_capacity(1 + name.len());
    key.push(kind);
    key.extend_from_slice(name.as_bytes());
    validate_key_size(&key)?;
    Ok(key)
}

fn validate_catalog_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(codec_argument("A catalog name cannot be empty"));
    }
    if name.len() + 1 > MAX_BTREE_KEY_BYTES {
        return Err(value_too_large(format!(
            "A catalog name cannot exceed {} UTF-8 bytes",
            MAX_BTREE_KEY_BYTES - 1
        )));
    }
    Ok(())
}

fn validate_catalog_header(header: &CatalogHeader) -> Result<()> {
    if !(FIRST_USER_TREE_ID..=MAX_TREE_ID).contains(&header.next_tree_id) {
        return Err(codec_argument(format!(
            "The next catalog tree ID must be between {FIRST_USER_TREE_ID} and {MAX_TREE_ID}"
        )));
    }
    if header.table_count > MAX_CATALOG_TABLES {
        return Err(value_too_large(format!(
            "A catalog cannot contain more than {MAX_CATALOG_TABLES} tables"
        )));
    }
    if header.index_count > MAX_CATALOG_INDEXES {
        return Err(value_too_large(format!(
            "A catalog cannot contain more than {MAX_CATALOG_INDEXES} indexes"
        )));
    }
    let tree_count = u64::from(header.table_count) + u64::from(header.index_count);
    let minimum_next_tree_id = FIRST_USER_TREE_ID
        .checked_add(tree_count)
        .ok_or_else(|| value_too_large("Catalog tree ID range overflowed"))?;
    if header.next_tree_id < minimum_next_tree_id {
        return Err(codec_argument(format!(
            "Catalog next tree ID {} cannot cover {} table and index trees",
            header.next_tree_id, tree_count
        )));
    }
    Ok(())
}

fn validate_catalog_item(
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    count: u64,
    description: &str,
) -> Result<()> {
    if !(FIRST_USER_TREE_ID..=MAX_TREE_ID).contains(&tree_id) {
        return Err(codec_argument(format!(
            "A catalog {description} tree ID must be between {FIRST_USER_TREE_ID} and {MAX_TREE_ID}"
        )));
    }
    if count > MAX_STORED_COUNT {
        return Err(value_too_large(format!(
            "A catalog {description} count cannot exceed {MAX_STORED_COUNT}"
        )));
    }
    match (root_page_id, count) {
        (None, 0) => Ok(()),
        (Some(root), count) if count > 0 => {
            if !(FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT).contains(&root) {
                Err(codec_argument(format!(
                    "Catalog {description} root page {root} is outside the data-page range"
                )))
            } else {
                Ok(())
            }
        }
        (None, _) => Err(codec_argument(format!(
            "A non-empty catalog {description} must name a root page"
        ))),
        (Some(_), 0) => Err(codec_argument(format!(
            "An empty catalog {description} must not name a root page"
        ))),
        _ => unreachable!(),
    }
}

fn encode_catalog_item_value<T: Serialize>(
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    count: u64,
    hash: u64,
    model: &T,
    description: &str,
) -> Result<Vec<u8>> {
    let model = serde_json::to_value(model).map_err(|error| {
        codec_argument(format!("Could not encode catalog {description}: {error}"))
    })?;
    let body = encode_canonical_json(&model)?;
    encode_catalog_item_body(tree_id, root_page_id, count, hash, &body)
}

fn encode_catalog_item_body(
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    count: u64,
    hash: u64,
    body: &[u8],
) -> Result<Vec<u8>> {
    let total = CATALOG_ITEM_HEADER_BYTES
        .checked_add(body.len())
        .ok_or_else(|| value_too_large("Catalog value length overflowed"))?;
    if total > MAX_PAGED_VALUE_BYTES {
        return Err(value_too_large(format!(
            "An encoded catalog value cannot exceed {MAX_PAGED_VALUE_BYTES} bytes"
        )));
    }
    let mut value = Vec::with_capacity(total);
    append_record_prefix(&mut value);
    value.extend_from_slice(&tree_id.to_le_bytes());
    value.extend_from_slice(&root_page_id.unwrap_or(NO_PAGE_ID).to_le_bytes());
    value.extend_from_slice(&count.to_le_bytes());
    value.extend_from_slice(&hash.to_le_bytes());
    value.extend_from_slice(&(body.len() as u32).to_le_bytes());
    value.extend_from_slice(body);
    Ok(value)
}

fn decode_catalog_item_value<T: DeserializeOwned + Serialize>(
    value: &[u8],
    description: &str,
) -> Result<(TreeId, Option<PageId>, u64, u64, T)> {
    if value.len() < CATALOG_ITEM_HEADER_BYTES || value.len() > MAX_PAGED_VALUE_BYTES {
        return Err(storage_corrupt(format!(
            "An encoded catalog {description} has an invalid byte length"
        )));
    }
    validate_record_prefix(value, &format!("catalog {description}"))?;
    let tree_id = read_u64(value, 4);
    let root_page_id = match read_u64(value, 12) {
        NO_PAGE_ID => None,
        page_id => Some(page_id),
    };
    let count = read_u64(value, 20);
    let hash = read_u64(value, 28);
    validate_catalog_item(tree_id, root_page_id, count, description)
        .map_err(as_storage_corruption)?;
    let body_length = read_u32(value, 36) as usize;
    if CATALOG_ITEM_HEADER_BYTES.checked_add(body_length) != Some(value.len()) {
        return Err(storage_corrupt(format!(
            "Catalog {description} length does not match its header"
        )));
    }
    let body = &value[CATALOG_ITEM_HEADER_BYTES..];
    let json = decode_canonical_json(body, &format!("catalog {description}"))?;
    let model: T = serde_json::from_value(json).map_err(|error| {
        storage_corrupt(format!(
            "Catalog {description} JSON does not match its model: {error}"
        ))
    })?;
    let normalized = serde_json::to_value(&model).map_err(|error| {
        storage_corrupt(format!(
            "Catalog {description} could not be normalized: {error}"
        ))
    })?;
    if encode_canonical_json(&normalized).map_err(as_storage_corruption)? != body {
        return Err(storage_corrupt(format!(
            "Catalog {description} JSON is not the canonical model representation"
        )));
    }
    Ok((tree_id, root_page_id, count, hash, model))
}

fn append_record_prefix(bytes: &mut Vec<u8>) {
    bytes.push(RECORD_VERSION);
    bytes.push(RECORD_FLAGS);
    bytes.extend_from_slice(&0_u16.to_le_bytes());
}

fn validate_record_prefix(bytes: &[u8], description: &str) -> Result<()> {
    if bytes[0] != RECORD_VERSION {
        return Err(storage_version(format!(
            "Encoded {description} version {} is not supported",
            bytes[0]
        )));
    }
    if bytes[1] != RECORD_FLAGS {
        return Err(storage_version(format!(
            "Encoded {description} flags {:#04x} are not supported",
            bytes[1]
        )));
    }
    if bytes[2..4] != [0, 0] {
        return Err(storage_corrupt(format!(
            "Encoded {description} reserved bytes must be zero"
        )));
    }
    Ok(())
}

fn decode_canonical_json(bytes: &[u8], description: &str) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        storage_corrupt(format!("Encoded {description} JSON is invalid: {error}"))
    })?;
    if encode_canonical_json(&value).map_err(as_storage_corruption)? != bytes {
        return Err(storage_corrupt(format!(
            "Encoded {description} JSON is not canonical"
        )));
    }
    Ok(value)
}

struct CanonicalJsonEncoder {
    bytes: Vec<u8>,
    nodes: usize,
}

impl CanonicalJsonEncoder {
    fn encode(&mut self, value: &Value, depth: usize) -> Result<()> {
        if depth > MAX_JSON_DEPTH {
            return Err(value_too_large(format!(
                "Canonical JSON cannot nest more than {MAX_JSON_DEPTH} levels"
            )));
        }
        self.nodes += 1;
        if self.nodes > MAX_JSON_NODES {
            return Err(value_too_large(format!(
                "Canonical JSON cannot contain more than {MAX_JSON_NODES} nodes"
            )));
        }
        match value {
            Value::Null => self.append(b"null")?,
            Value::Bool(false) => self.append(b"false")?,
            Value::Bool(true) => self.append(b"true")?,
            Value::Number(value) => self.append(value.to_string().as_bytes())?,
            Value::String(value) => self.append_json_string(value)?,
            Value::Array(values) => {
                self.append(b"[")?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        self.append(b",")?;
                    }
                    self.encode(value, depth + 1)?;
                }
                self.append(b"]")?;
            }
            Value::Object(values) => {
                // Without serde_json's preserve_order feature a Map is a BTreeMap, so its
                // entries come in the canonical order of their keys.
                self.append(b"{")?;
                for (index, (key, value)) in values.iter().enumerate() {
                    if index != 0 {
                        self.append(b",")?;
                    }
                    self.append_json_string(key)?;
                    self.append(b":")?;
                    self.encode(value, depth + 1)?;
                }
                self.append(b"}")?;
            }
        }
        Ok(())
    }

    fn append_json_string(&mut self, value: &str) -> Result<()> {
        let encoded = serde_json::to_vec(value).map_err(|error| {
            codec_argument(format!("Could not encode canonical JSON string: {error}"))
        })?;
        self.append(&encoded)
    }

    fn append(&mut self, value: &[u8]) -> Result<()> {
        let next = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or_else(|| value_too_large("Canonical JSON length overflowed"))?;
        if next > MAX_PAGED_VALUE_BYTES {
            return Err(value_too_large(format!(
                "Canonical JSON cannot exceed {MAX_PAGED_VALUE_BYTES} bytes"
            )));
        }
        self.bytes.extend_from_slice(value);
        Ok(())
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn value_type_mismatch(table: &str, column: &str, expected: &str) -> EngineError {
    EngineError::type_mismatch(format!("Column `{column}` in `{table}` expects {expected}"))
}

fn key_type_mismatch(table: &str, column: &str, expected: &str) -> EngineError {
    EngineError::type_mismatch(format!(
        "Key column `{column}` in `{table}` expects {expected}"
    ))
}

fn codec_argument(message: impl Into<String>) -> EngineError {
    EngineError::new("INVALID_PAGED_ARGUMENT", message)
}

fn value_too_large(message: impl Into<String>) -> EngineError {
    EngineError::new("PAGED_VALUE_TOO_LARGE", message)
}

fn storage_corrupt(message: impl Into<String>) -> EngineError {
    EngineError::new("PAGED_STORAGE_CORRUPT", message)
}

fn storage_version(message: impl Into<String>) -> EngineError {
    EngineError::new("PAGED_STORAGE_VERSION_UNSUPPORTED", message)
}

fn as_storage_corruption(error: EngineError) -> EngineError {
    storage_corrupt(error.message)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Number, json};

    use super::*;

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    fn typed_schema(primary_key: Vec<&str>, columns: Vec<(&str, ColumnType)>) -> TableDefinition {
        TableDefinition {
            name: "items".to_owned(),
            primary_key: primary_key.into_iter().map(str::to_owned).collect(),
            columns: columns
                .into_iter()
                .map(|(name, data_type)| crate::ColumnDefinition {
                    name: name.to_owned(),
                    data_type,
                    nullable: false,
                    default: None,
                })
                .collect(),
        }
    }

    fn table_record() -> CatalogTableRecord {
        CatalogTableRecord {
            schema: typed_schema(vec!["id"], vec![("id", ColumnType::Integer)]),
            tree_id: 2,
            root_page_id: Some(FIRST_DATA_PAGE_ID),
            row_count: 1,
            hash: 0x0123_4567_89ab_cdef,
        }
    }

    fn index_record() -> CatalogIndexRecord {
        CatalogIndexRecord {
            definition: IndexDefinition {
                name: "items_name".to_owned(),
                table: "items".to_owned(),
                columns: vec!["name".to_owned()],
                unique: false,
            },
            tree_id: 3,
            root_page_id: Some(FIRST_DATA_PAGE_ID + 1),
            entry_count: 2,
        }
    }

    fn table_value_with_schema(schema: &TableDefinition) -> Vec<u8> {
        let (_, mut value) = encode_catalog_table_record(&table_record()).unwrap();
        let body = encode_canonical_json(&serde_json::to_value(schema).unwrap()).unwrap();
        value.truncate(CATALOG_ITEM_HEADER_BYTES);
        value[28..32].copy_from_slice(&(body.len() as u32).to_le_bytes());
        value.extend_from_slice(&body);
        value
    }

    #[test]
    fn canonical_json_sorts_every_object_and_preserves_number_identity() {
        let mut inner = Map::new();
        inner.insert("z".to_owned(), json!(1));
        inner.insert("a".to_owned(), json!([{"y": 2, "x": 1}]));
        let mut outer = Map::new();
        outer.insert("second".to_owned(), Value::Object(inner));
        outer.insert("first".to_owned(), json!(true));
        assert_eq!(
            encode_canonical_json(&Value::Object(outer)).unwrap(),
            br#"{"first":true,"second":{"a":[{"x":1,"y":2}],"z":1}}"#
        );

        let integer = Value::Number(Number::from(1));
        let float = Value::Number(Number::from_f64(1.0).unwrap());
        assert_eq!(encode_canonical_json(&integer).unwrap(), b"1");
        assert_eq!(encode_canonical_json(&float).unwrap(), b"1.0");
    }

    #[test]
    fn typed_key_components_have_stable_golden_encodings() {
        let schema = typed_schema(
            vec!["low", "zero", "high", "off", "on"],
            vec![
                ("low", ColumnType::Integer),
                ("zero", ColumnType::Integer),
                ("high", ColumnType::Integer),
                ("off", ColumnType::Boolean),
                ("on", ColumnType::Boolean),
            ],
        );
        let encoded = encode_primary_key(
            &schema,
            &row(json!({
                "low": -9_007_199_254_740_991_i64,
                "zero": 0,
                "high": 9_007_199_254_740_991_i64,
                "off": false,
                "on": true
            })),
        )
        .unwrap();
        assert_eq!(
            encoded,
            [
                &[0x7f, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01][..],
                &[0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
                &[0x80, 0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                &[0x00],
                &[0x01],
            ]
            .concat()
        );
    }

    #[test]
    fn text_components_are_escaped_terminated_and_ordered_by_code_point() {
        let schema = typed_schema(
            vec!["a", "b"],
            vec![("a", ColumnType::Text), ("b", ColumnType::Text)],
        );
        let key =
            |a: &str, b: &str| encode_primary_key(&schema, &row(json!({"a": a, "b": b}))).unwrap();
        assert_ne!(key("ab", "c"), key("a", "bc"));

        let unicode = key("a\u{0}é", "λ");
        assert_eq!(&unicode[..7], &[b'a', 0x00, 0xff, 0xc3, 0xa9, 0x00, 0x01]);
        assert_eq!(&unicode[7..], &[0xce, 0xbb, 0x00, 0x01]);

        // Byte order is SQL's code-point order, with shorter prefixes first, and a composite
        // orders by its first column before its second.
        let ordered = [
            key("", "z"),
            key("a", "z"),
            key("a\u{0}", "a"),
            key("a\u{1}", "a"),
            key("ab", "a"),
            key("b", ""),
            key("b", "a"),
            key("é", "a"),
            key("λ", "a"),
        ];
        assert!(ordered.windows(2).all(|pair| pair[0] < pair[1]));

        let decoded = decode_row(
            &schema,
            &unicode,
            &encode_row(&schema, &row(json!({"a": "a\u{0}é", "b": "λ"}))).unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, row(json!({"a": "a\u{0}é", "b": "λ"})));
    }

    #[test]
    fn float_keys_follow_sql_equality() {
        let float_schema = typed_schema(vec!["id"], vec![("id", ColumnType::Float)]);
        let integer = encode_primary_key(&float_schema, &row(json!({"id": 1}))).unwrap();
        let mut float_row = Row::new();
        float_row.insert(
            "id".to_owned(),
            Value::Number(Number::from_f64(1.0).unwrap()),
        );
        let float = encode_primary_key(&float_schema, &float_row).unwrap();
        assert_eq!(integer, 0xbff0_0000_0000_0000_u64.to_be_bytes().to_vec());
        assert_eq!(integer, float);

        let positive_zero = encode_primary_key(&float_schema, &row(json!({"id": 0.0}))).unwrap();
        let negative_zero = encode_primary_key(&float_schema, &row(json!({"id": -0.0}))).unwrap();
        assert_eq!(positive_zero, negative_zero);
        let negative = encode_primary_key(&float_schema, &row(json!({"id": -2.0}))).unwrap();
        assert!(negative < positive_zero);
        assert!(positive_zero < float);
    }

    #[test]
    fn secondary_index_prefixes_are_seekable_and_carry_the_primary_key() {
        let schema = typed_schema(
            vec!["id"],
            vec![("id", ColumnType::Integer), ("tenant", ColumnType::Text)],
        );
        let definition = IndexDefinition {
            name: "items_tenant".to_owned(),
            table: "items".to_owned(),
            columns: vec!["tenant".to_owned()],
            unique: false,
        };
        let item = row(json!({"id": 7, "tenant": "tiny"}));
        let prefix = encode_secondary_index_prefix(&schema, &definition, &item)
            .unwrap()
            .unwrap();
        let entry = encode_secondary_index_entry_key(&schema, &definition, &item)
            .unwrap()
            .unwrap();
        let primary_key = encode_primary_key(&schema, &item).unwrap();
        assert_eq!(entry, [prefix.clone(), primary_key.clone()].concat());
        assert!(secondary_index_entry_matches_prefix(&entry, &prefix));
        assert_eq!(
            secondary_index_primary_key(&entry, &prefix).unwrap(),
            primary_key
        );
        assert_eq!(
            secondary_index_primary_key_for_definition(&schema, &definition, &entry).unwrap(),
            primary_key
        );

        // A longer tuple beginning with the same text is a different tuple.
        let longer = encode_secondary_index_entry_key(
            &schema,
            &definition,
            &row(json!({"id": 7, "tenant": "tinyjoin"})),
        )
        .unwrap()
        .unwrap();
        assert!(!secondary_index_entry_matches_prefix(&longer, &prefix));

        let mut null = item;
        null.insert("tenant".to_owned(), Value::Null);
        assert_eq!(
            encode_secondary_index_prefix(&schema, &definition, &null).unwrap(),
            None
        );
        assert_eq!(
            encode_secondary_index_entry_key(&schema, &definition, &null).unwrap(),
            None
        );
    }

    #[test]
    fn index_entry_parser_finds_the_primary_key_after_any_tuple_payload() {
        let schema = typed_schema(
            vec!["id"],
            vec![("id", ColumnType::Text), ("value", ColumnType::Text)],
        );
        let definition = IndexDefinition {
            name: "items_value".to_owned(),
            table: "items".to_owned(),
            columns: vec!["value".to_owned()],
            unique: true,
        };
        let item = row(json!({"id": "primary", "value": "inside\u{0}\u{ff}payload"}));
        let entry = encode_secondary_index_entry_key(&schema, &definition, &item)
            .unwrap()
            .unwrap();
        assert_eq!(
            secondary_index_primary_key_for_definition(&schema, &definition, &entry).unwrap(),
            encode_primary_key(&schema, &item).unwrap()
        );

        let mut truncated = entry;
        truncated.truncate(4);
        assert_eq!(
            secondary_index_primary_key_for_definition(&schema, &definition, &truncated)
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );

        let mut trailing = encode_secondary_index_entry_key(
            &schema,
            &definition,
            &row(json!({"id": "primary", "value": "value"})),
        )
        .unwrap()
        .unwrap();
        trailing.push(0);
        assert_eq!(
            secondary_index_primary_key_for_definition(&schema, &definition, &trailing)
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );

        let mut bad_escape = encode_primary_key(&schema, &row(json!({"id": "a\u{0}b"}))).unwrap();
        bad_escape[2] = 0x02;
        let index_prefix = encode_secondary_index_prefix(
            &schema,
            &definition,
            &row(json!({"id": "x", "value": "v"})),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            secondary_index_primary_key_for_definition(
                &schema,
                &definition,
                &[index_prefix, bad_escape].concat()
            )
            .unwrap_err()
            .code,
            "PAGED_STORAGE_CORRUPT"
        );
    }

    #[test]
    fn key_and_integer_bounds_are_exact() {
        let schema = typed_schema(vec!["id"], vec![("id", ColumnType::Text)]);
        let accepted = "a".repeat(MAX_BTREE_KEY_BYTES - 2);
        assert_eq!(
            encode_primary_key(&schema, &row(json!({"id": accepted})))
                .unwrap()
                .len(),
            MAX_BTREE_KEY_BYTES
        );
        let rejected = "a".repeat(MAX_BTREE_KEY_BYTES - 1);
        assert_eq!(
            encode_primary_key(&schema, &row(json!({"id": rejected})))
                .unwrap_err()
                .code,
            "PAGED_VALUE_TOO_LARGE"
        );
        assert_eq!(
            encode_primary_key(
                &typed_schema(vec!["id"], vec![("id", ColumnType::Integer)]),
                &row(json!({"id": 9_007_199_254_740_992_u64})),
            )
            .unwrap_err()
            .code,
            "TYPE_MISMATCH"
        );
    }

    fn benchmark_schema() -> TableDefinition {
        typed_schema(
            vec!["id"],
            vec![
                ("id", ColumnType::Integer),
                ("a", ColumnType::Integer),
                ("b", ColumnType::Integer),
                ("c", ColumnType::Text),
                ("g", ColumnType::Integer),
            ],
        )
    }

    #[test]
    fn row_records_pack_columns_behind_an_offset_table() {
        let schema = benchmark_schema();
        let stored = row(json!({
            "id": 18,
            "a": 17,
            "b": 8493,
            "c": "eight thousand four hundred ninety three",
            "g": 94
        }));
        let record = encode_row(&schema, &stored).unwrap();
        // Flags (one-byte offsets, no nulls), four stored columns, the ends of `a`, `b` and `c`,
        // then 17, 8493 and 94 as minimal little-endian integers around the text.
        assert_eq!(
            record,
            [
                &[0x00, 4, 1, 3, 43, 0x11, 0x2d, 0x21][..],
                b"eight thousand four hundred ninety three",
                &[0x5e],
            ]
            .concat()
        );
        assert_eq!(record.len(), 49);
        let key = encode_primary_key(&schema, &stored).unwrap();
        assert_eq!(decode_row(&schema, &key, &record).unwrap(), stored);
        assert_eq!(decode_row_strictly(&schema, &key, &record).unwrap(), stored);

        // Integers take the fewest bytes that sign-extend to them; zero takes none.
        for (value, length) in [
            (0_i64, 0),
            (1, 1),
            (-1, 1),
            (127, 1),
            (128, 2),
            (-128, 1),
            (-129, 2),
            (9_007_199_254_740_991, 7),
            (-9_007_199_254_740_991, 7),
        ] {
            let row = row(json!({"id": 1, "a": value, "b": 0, "c": "", "g": 1}));
            let record = encode_row(&schema, &row).unwrap();
            assert_eq!(record[2] as usize, length, "{value}");
            let key = encode_primary_key(&schema, &row).unwrap();
            assert_eq!(decode_row_strictly(&schema, &key, &record).unwrap(), row);
        }
    }

    #[test]
    fn stored_records_read_each_column_as_the_whole_row_decodes_it() {
        // Key columns come first in the key but not in the schema, and text keys escape zeros.
        let schema = typed_schema(
            vec!["name", "at"],
            vec![
                ("count", ColumnType::Integer),
                ("at", ColumnType::Float),
                ("label", ColumnType::Text),
                ("name", ColumnType::Text),
                ("flag", ColumnType::Boolean),
            ],
        );
        let layout = RecordLayout::new(&schema).unwrap();
        for stored in [
            row(json!({"name": "", "at": 0.0, "count": 0, "label": "", "flag": false})),
            row(json!({"name": "a\u{0}b", "at": -2.5, "count": -1, "label": "x", "flag": true})),
            row(json!({"name": "\u{0}", "at": 1e300, "count": 1, "label": "é🦀", "flag": false})),
        ] {
            let key = encode_primary_key(&schema, &stored).unwrap();
            let record = encode_row(&schema, &stored).unwrap();
            let entry = StoredRecord::new(&schema, &layout, &key, &record).unwrap();
            assert_eq!(entry.to_row().unwrap(), stored);
            for (index, column) in schema.columns.iter().enumerate() {
                assert_eq!(
                    entry.column(index).unwrap().into_value(),
                    stored[&column.name]
                );
            }
            // Text borrows the entry's bytes unless its key form escapes a zero.
            let name = schema
                .columns
                .iter()
                .position(|column| column.name == "name");
            let borrowed = matches!(
                entry.column(name.unwrap()).unwrap(),
                ValueRef::Text(Cow::Borrowed(_))
            );
            assert_eq!(borrowed, !stored["name"].as_str().unwrap().contains('\0'));
        }
        assert_eq!(
            StoredRecord::new(&schema, &layout, &[], &[0x00])
                .map(|_| ())
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );
        assert_eq!(
            StoredRecord::new(&schema, &layout, &[], &[0x08, 0])
                .map(|_| ())
                .unwrap_err()
                .code,
            "PAGED_STORAGE_VERSION_UNSUPPORTED"
        );
    }

    #[test]
    fn stored_records_read_offsets_and_integers_of_every_width() {
        let schema = typed_schema(
            vec!["id"],
            vec![
                ("id", ColumnType::Integer),
                ("first", ColumnType::Text),
                ("second", ColumnType::Integer),
                ("third", ColumnType::Text),
            ],
        );
        let layout = RecordLayout::new(&schema).unwrap();
        // The offset width grows with the largest offset, from one byte to two and then four.
        for (length, width) in [(3, 1), (300, 2), (70_000, 4)] {
            for second in [0_i64, -1, 255, -70_000, 1 << 40, -9_007_199_254_740_991] {
                let stored = row(json!({
                    "id": 1,
                    "first": "f".repeat(length),
                    "second": second,
                    "third": "t"
                }));
                let key = encode_primary_key(&schema, &stored).unwrap();
                let value = encode_row(&schema, &stored).unwrap();
                assert_eq!(1 << (value[0] & RECORD_OFFSET_WIDTH), width);
                let record = StoredRecord::new(&schema, &layout, &key, &value).unwrap();
                for (index, column) in schema.columns.iter().enumerate() {
                    assert_eq!(
                        record.column(index).unwrap().into_value(),
                        stored[&column.name]
                    );
                }
                assert_eq!(decode_row_strictly(&schema, &key, &value).unwrap(), stored);
            }
        }
    }

    #[test]
    fn stored_records_estimate_key_and_index_as_the_rows_they_decode_to() {
        // Writers read the rows they hold in place: a record's estimate, key, and index entries
        // must be exactly those of the row it decodes to.
        let mut schema = typed_schema(
            vec!["name", "at"],
            vec![
                ("count", ColumnType::Integer),
                ("at", ColumnType::Float),
                ("label", ColumnType::Text),
                ("name", ColumnType::Text),
                ("flag", ColumnType::Boolean),
                ("doc", ColumnType::Json),
                ("extra", ColumnType::Text),
            ],
        );
        for column in &mut schema.columns {
            column.nullable = !schema.primary_key.contains(&column.name);
        }
        // A trailing column holding its default is left out of the record.
        schema.columns[6].default = Some(json!("fallback"));
        let layout = RecordLayout::new(&schema).unwrap();
        let indexes = [
            vec!["label"],
            vec!["count", "flag"],
            vec!["flag", "label", "count"],
            vec!["at", "extra"],
            vec!["doc"],
        ]
        .map(|columns| IndexDefinition {
            name: columns.join("_"),
            table: "items".to_owned(),
            columns: columns.into_iter().map(str::to_owned).collect(),
            unique: false,
        });
        for stored in [
            json!({"name": "", "at": 0.0, "count": 0, "label": "", "flag": false, "doc": null,
                "extra": "fallback"}),
            json!({"name": "a\u{0}b", "at": -2.5, "count": -9_007_199_254_740_991_i64,
                "label": "x\u{0}", "flag": true, "doc": {"k": [1, "two"]}, "extra": "é🦀"}),
            json!({"name": "\u{0}", "at": 1e300, "count": null, "label": null, "flag": null,
                "doc": [true], "extra": null}),
            json!({"name": "z", "at": 3.0, "count": 9_007_199_254_740_991_i64, "label": "é",
                "flag": false, "doc": "text", "extra": "fallback"}),
        ] {
            let stored = row(stored);
            let key = encode_primary_key(&schema, &stored).unwrap();
            let value = encode_row(&schema, &stored).unwrap();
            let record = StoredRecord::new(&schema, &layout, &key, &value).unwrap();
            let decoded = record.to_row().unwrap();
            assert_eq!(
                crate::storage::estimated_record_bytes(&record).unwrap(),
                crate::storage::estimated_row_bytes(&decoded).unwrap()
            );
            assert_eq!(
                crate::row::RowRef::record(record).primary_key().unwrap(),
                crate::statement::primary_key_row(&schema, &decoded).unwrap()
            );
            for definition in &indexes {
                let positions = index_column_positions(&schema, definition).unwrap();
                assert_eq!(
                    encode_record_index_entry(&positions, &record).map_err(|error| error.code),
                    encode_secondary_index_entry(&schema, definition, &decoded)
                        .map_err(|error| error.code),
                    "{}",
                    definition.name
                );
            }
            let entry = record.to_entry();
            assert_eq!((entry.key(), entry.value()), (&key[..], &value[..]));
        }
    }

    #[test]
    fn row_records_omit_trailing_defaults_and_mark_nulls() {
        let mut schema = typed_schema(
            vec!["id"],
            vec![
                ("id", ColumnType::Integer),
                ("note", ColumnType::Text),
                ("score", ColumnType::Float),
                ("done", ColumnType::Boolean),
            ],
        );
        for column in &mut schema.columns[1..] {
            column.nullable = true;
        }
        schema.columns[2].default = Some(json!(0.0));
        schema.columns[3].default = Some(json!(false));
        let key = encode_primary_key(&schema, &row(json!({"id": 1}))).unwrap();
        let round_trip = |value: Value| {
            let row = row(value);
            let record = encode_row(&schema, &row).unwrap();
            assert_eq!(decode_row_strictly(&schema, &key, &record).unwrap(), row);
            record
        };

        // Every trailing column equals its default, so nothing but the header is stored.
        assert_eq!(
            round_trip(json!({"id": 1, "note": null, "score": 0.0, "done": false})),
            [0x00, 0]
        );
        // A NULL before a stored column is marked in the bitmap and takes no bytes.
        assert_eq!(
            round_trip(json!({"id": 1, "note": null, "score": 2.5, "done": false})),
            [&[RECORD_HAS_NULLS, 2, 0b01, 0][..], &2.5_f64.to_le_bytes()].concat()
        );
        // Negative zero is a different stored value from a zero default.
        assert_eq!(
            round_trip(json!({"id": 1, "note": "x", "score": -0.0, "done": false})),
            [&[0x00, 2, 1, b'x'][..], &(-0.0_f64).to_le_bytes()].concat()
        );
        round_trip(json!({"id": 1, "note": null, "score": null, "done": true}));

        // An omitted column reads as its default, normalized as a stored value would be.
        schema.columns[2].default = Some(json!(1));
        assert_eq!(
            decode_row(&schema, &key, &[0x00, 0]).unwrap()["score"],
            json!(1.0)
        );
    }

    #[test]
    fn row_records_are_bounded_at_one_mebibyte() {
        let schema = typed_schema(
            vec!["id"],
            vec![("id", ColumnType::Integer), ("x", ColumnType::Text)],
        );
        let content = "a".repeat(MAX_PAGED_VALUE_BYTES - RECORD_HEADER_BYTES);
        let encoded = encode_row(&schema, &row(json!({"id": 1, "x": content}))).unwrap();
        assert_eq!(encoded.len(), MAX_PAGED_VALUE_BYTES);
        let key = encode_primary_key(&schema, &row(json!({"id": 1}))).unwrap();
        assert_eq!(
            decode_row(&schema, &key, &encoded).unwrap()["x"]
                .as_str()
                .unwrap()
                .len(),
            content.len()
        );
        let too_large = "a".repeat(content.len() + 1);
        assert_eq!(
            encode_row(&schema, &row(json!({"id": 1, "x": too_large})))
                .unwrap_err()
                .code,
            "PAGED_VALUE_TOO_LARGE"
        );
    }

    #[test]
    fn strict_row_decoding_rejects_every_noncanonical_record() {
        let schema = benchmark_schema();
        let stored = row(json!({"id": 1, "a": 5, "b": 300, "c": "text", "g": 1}));
        let key = encode_primary_key(&schema, &stored).unwrap();
        let record = encode_row(&schema, &stored).unwrap();
        assert_eq!(record[..5], [0x00, 4, 1, 3, 7]);

        let variant = |edit: &dyn Fn(&mut Vec<u8>)| {
            let mut bytes = record.clone();
            edit(&mut bytes);
            bytes
        };
        for (description, corrupt, code) in [
            (
                "a reserved sync flag",
                variant(&|bytes| bytes[0] |= 0b1000),
                "PAGED_STORAGE_VERSION_UNSUPPORTED",
            ),
            (
                "an unknown offset width",
                variant(&|bytes| bytes[0] |= 0b11),
                "PAGED_STORAGE_VERSION_UNSUPPORTED",
            ),
            (
                "more columns than the table",
                variant(&|bytes| bytes[1] = 5),
                "PAGED_STORAGE_CORRUPT",
            ),
            (
                "offsets past the data",
                variant(&|bytes| bytes[4] = 200),
                "PAGED_STORAGE_CORRUPT",
            ),
            (
                "decreasing offsets",
                variant(&|bytes| bytes[3] = 0),
                "PAGED_STORAGE_CORRUPT",
            ),
            ("a truncated header", vec![0x00], "PAGED_STORAGE_CORRUPT"),
            (
                "a truncated offset table",
                vec![0x00, 4, 1],
                "PAGED_STORAGE_CORRUPT",
            ),
        ] {
            assert_eq!(
                decode_row(&schema, &key, &corrupt).unwrap_err().code,
                code,
                "{description}"
            );
        }

        // Readable but not what the encoder writes: only strict decoding refuses these.
        let wide_offsets = [&[0x01, 4, 1, 0, 3, 0, 7, 0][..], &record[5..]].concat();
        let padded_integer = [&[0x00, 4, 2, 4, 8, 0x05, 0x00][..], &record[6..]].concat();
        let needless_bitmap = [&[RECORD_HAS_NULLS, 4, 0][..], &record[2..]].concat();
        let kept_default = {
            let mut schema = schema.clone();
            schema.columns[4].default = Some(json!(1));
            (schema, record.clone())
        };
        for (description, corrupt) in [
            ("oversized offsets", wide_offsets),
            ("a padded integer", padded_integer),
            ("an empty null bitmap", needless_bitmap),
        ] {
            decode_row(&schema, &key, &corrupt).unwrap();
            assert_eq!(
                decode_row_strictly(&schema, &key, &corrupt)
                    .unwrap_err()
                    .code,
                "PAGED_STORAGE_CORRUPT",
                "{description}"
            );
        }
        let (defaulted, record) = kept_default;
        assert_eq!(
            decode_row_strictly(&defaulted, &key, &record)
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );
    }

    #[test]
    fn catalog_records_round_trip_with_stable_keys() {
        let header = CatalogHeader {
            next_tree_id: 4,
            table_count: 1,
            index_count: 1,
        };
        let (key, value) = encode_catalog_header_record(&header).unwrap();
        assert_eq!(key, [CATALOG_HEADER_KEY]);
        assert_eq!(decode_catalog_header_record(&key, &value).unwrap(), header);

        let table = table_record();
        let (key, value) = encode_catalog_table_record(&table).unwrap();
        assert_eq!(key, [vec![CATALOG_TABLE_KEY], b"items".to_vec()].concat());
        assert_eq!(decode_catalog_table_record(&key, &value).unwrap(), table);

        let index = index_record();
        let (key, value) = encode_catalog_index_record(&index).unwrap();
        assert_eq!(
            key,
            [vec![CATALOG_INDEX_KEY], b"items_name".to_vec()].concat()
        );
        assert_eq!(decode_catalog_index_record(&key, &value).unwrap(), index);
    }

    #[test]
    fn catalog_name_tree_root_and_count_bounds_are_strict() {
        let mut table = table_record();
        for name in [
            "".to_owned(),
            " ".to_owned(),
            "x".repeat(MAX_BTREE_KEY_BYTES),
        ] {
            table.schema.name = name;
            assert!(encode_catalog_table_record(&table).is_err());
        }
        table = table_record();
        for tree_id in [0, CATALOG_TREE_ID, u64::MAX] {
            table.tree_id = tree_id;
            assert_eq!(
                encode_catalog_table_record(&table).unwrap_err().code,
                "INVALID_PAGED_ARGUMENT"
            );
        }
        table = table_record();
        for root in [FIRST_DATA_PAGE_ID - 1, MAX_PAGE_COUNT] {
            table.root_page_id = Some(root);
            assert_eq!(
                encode_catalog_table_record(&table).unwrap_err().code,
                "INVALID_PAGED_ARGUMENT"
            );
        }
        table = table_record();
        table.row_count = MAX_STORED_COUNT + 1;
        assert_eq!(
            encode_catalog_table_record(&table).unwrap_err().code,
            "PAGED_VALUE_TOO_LARGE"
        );
        for (root, count) in [(None, 1), (Some(FIRST_DATA_PAGE_ID), 0)] {
            table = table_record();
            table.root_page_id = root;
            table.row_count = count;
            assert_eq!(
                encode_catalog_table_record(&table).unwrap_err().code,
                "INVALID_PAGED_ARGUMENT"
            );
        }
        let oversized = CatalogHeader {
            next_tree_id: 2,
            table_count: MAX_CATALOG_TABLES + 1,
            index_count: 0,
        };
        assert_eq!(
            encode_catalog_header_record(&oversized).unwrap_err().code,
            "PAGED_VALUE_TOO_LARGE"
        );
        let insufficient_tree_range = CatalogHeader {
            next_tree_id: 3,
            table_count: 1,
            index_count: 1,
        };
        assert_eq!(
            encode_catalog_header_record(&insufficient_tree_range)
                .unwrap_err()
                .code,
            "INVALID_PAGED_ARGUMENT"
        );
    }

    #[test]
    fn catalog_decode_rejects_unknown_kinds_versions_reserved_fields_and_name_mismatches() {
        assert_eq!(
            decode_catalog_key(&[0x77]).unwrap_err().code,
            "PAGED_STORAGE_VERSION_UNSUPPORTED"
        );
        assert_eq!(
            decode_catalog_key(&[CATALOG_TABLE_KEY, 0xff])
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );

        let table = table_record();
        let (key, value) = encode_catalog_table_record(&table).unwrap();
        for (offset, replacement, code) in [
            (0, 2, "PAGED_STORAGE_VERSION_UNSUPPORTED"),
            (1, 1, "PAGED_STORAGE_VERSION_UNSUPPORTED"),
            (3, 1, "PAGED_STORAGE_CORRUPT"),
        ] {
            let mut corrupt = value.clone();
            corrupt[offset] = replacement;
            assert_eq!(
                decode_catalog_table_record(&key, &corrupt)
                    .unwrap_err()
                    .code,
                code
            );
        }

        let wrong_key = [vec![CATALOG_TABLE_KEY], b"other".to_vec()].concat();
        assert_eq!(
            decode_catalog_table_record(&wrong_key, &value)
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );
    }

    #[test]
    fn catalog_decode_rejects_noncanonical_or_lossy_model_json() {
        let table = table_record();
        let (key, mut value) = encode_catalog_table_record(&table).unwrap();
        let mut model = serde_json::to_value(&table.schema).unwrap();
        model
            .as_object_mut()
            .unwrap()
            .insert("futureField".to_owned(), json!(true));
        let body = encode_canonical_json(&model).unwrap();
        value.truncate(CATALOG_ITEM_HEADER_BYTES);
        value[28..32].copy_from_slice(&(body.len() as u32).to_le_bytes());
        value.extend_from_slice(&body);
        assert_eq!(
            decode_catalog_table_record(&key, &value).unwrap_err().code,
            "PAGED_STORAGE_CORRUPT"
        );
    }

    #[test]
    fn catalog_records_reject_invalid_schema_and_index_shapes() {
        let mut table = table_record();
        table.schema.columns.clear();
        assert_eq!(
            encode_catalog_table_record(&table).unwrap_err().code,
            "INVALID_PAGED_ARGUMENT"
        );

        let mut table = table_record();
        table.schema.primary_key.push("id".to_owned());
        assert_eq!(
            encode_catalog_table_record(&table).unwrap_err().code,
            "INVALID_PAGED_ARGUMENT"
        );

        let mut table = table_record();
        table.schema.primary_key = vec!["missing".to_owned()];
        assert_eq!(
            encode_catalog_table_record(&table).unwrap_err().code,
            "INVALID_PAGED_ARGUMENT"
        );

        let mut index = index_record();
        index.definition.columns.push("name".to_owned());
        assert_eq!(
            encode_catalog_index_record(&index).unwrap_err().code,
            "INVALID_PAGED_ARGUMENT"
        );

        let mut nullable_primary_key = table_record().schema;
        nullable_primary_key.columns[0].nullable = true;
        let key = [vec![CATALOG_TABLE_KEY], b"items".to_vec()].concat();
        assert_eq!(
            decode_catalog_table_record(&key, &table_value_with_schema(&nullable_primary_key))
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );

        let mut wrong_default = table_record().schema;
        wrong_default.columns[0].default = Some(json!("not an integer"));
        assert_eq!(
            decode_catalog_table_record(&key, &table_value_with_schema(&wrong_default))
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );

        let mut too_many_columns = table_record().schema;
        too_many_columns
            .columns
            .extend((1..=MAX_COLUMNS).map(|index| crate::ColumnDefinition {
                name: format!("column_{index}"),
                data_type: ColumnType::Text,
                nullable: true,
                default: None,
            }));
        assert_eq!(too_many_columns.columns.len(), MAX_COLUMNS + 1);
        assert_eq!(
            decode_catalog_table_record(&key, &table_value_with_schema(&too_many_columns))
                .unwrap_err()
                .code,
            "PAGED_STORAGE_CORRUPT"
        );

        let json_primary_key = typed_schema(vec!["id"], vec![("id", ColumnType::Json)]);
        assert_eq!(
            encode_primary_key(&json_primary_key, &row(json!({"id": {"a": 1}})))
                .unwrap_err()
                .code,
            "INVALID_PAGED_ARGUMENT"
        );
    }
}
