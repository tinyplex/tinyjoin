use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

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
const INDEX_PRIMARY_KEY_SEPARATOR: u8 = 0xff;

const COMPONENT_BOOLEAN: u8 = 0x01;
const COMPONENT_INTEGER: u8 = 0x02;
const COMPONENT_FLOAT: u8 = 0x03;
const COMPONENT_TEXT: u8 = 0x04;
const COMPONENT_JSON: u8 = 0x05;

const RECORD_VERSION: u8 = 1;
const RECORD_FLAGS: u8 = 0;
const RECORD_PREFIX_BYTES: usize = 4;
const ROW_HEADER_BYTES: usize = 8;
const CATALOG_HEADER_BYTES: usize = 20;
const CATALOG_ITEM_HEADER_BYTES: usize = 32;
const NO_PAGE_ID: PageId = u64::MAX;
pub(crate) const MAX_TREE_ID: TreeId = u64::MAX - 1;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NODES: usize = 1_000_000;
const MAX_COLUMNS: usize = 256;

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
    if entry.get(offset) != Some(&INDEX_PRIMARY_KEY_SEPARATOR) || offset + 1 == entry.len() {
        return Err(storage_corrupt(
            "A secondary-index entry does not contain its tuple boundary and primary key",
        ));
    }
    let primary_key = &entry[offset + 1..];
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
pub(crate) fn encode_primary_key(schema: &TableDefinition, row: &Row) -> Result<Vec<u8>> {
    validate_schema_shape(schema)?;
    let key = encode_columns(schema, &schema.primary_key, row, NullPolicy::Reject)?
        .expect("rejecting nulls always returns a tuple");
    validate_key_size(&key)?;
    Ok(key)
}

/// Encodes an index tuple followed by the boundary byte used for prefix seeks.
///
/// `None` follows PostgreSQL's default index semantics for a tuple containing SQL NULL: it is not
/// represented in the index and cannot satisfy an equality lookup.
pub(crate) fn encode_secondary_index_prefix(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<Vec<u8>>> {
    validate_index_identity(schema, definition)?;
    let Some(mut key) = encode_columns(schema, &definition.columns, row, NullPolicy::Omit)? else {
        return Ok(None);
    };
    key.push(INDEX_PRIMARY_KEY_SEPARATOR);
    validate_key_size(&key)?;
    Ok(Some(key))
}

/// Encodes one secondary-index entry as `indexed tuple || 0xff || primary-key tuple`.
pub(crate) fn encode_secondary_index_entry_key(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<Vec<u8>>> {
    let Some(mut key) = encode_secondary_index_prefix(schema, definition, row)? else {
        return Ok(None);
    };
    key.extend_from_slice(&encode_primary_key(schema, row)?);
    validate_key_size(&key)?;
    Ok(Some(key))
}

pub(crate) fn secondary_index_entry_matches_prefix(entry: &[u8], prefix: &[u8]) -> bool {
    prefix.last() == Some(&INDEX_PRIMARY_KEY_SEPARATOR) && entry.starts_with(prefix)
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

pub(crate) fn encode_row(row: &Row) -> Result<Vec<u8>> {
    let body = encode_canonical_json(&Value::Object(row.clone()))?;
    let total = ROW_HEADER_BYTES
        .checked_add(body.len())
        .ok_or_else(|| value_too_large("Encoded row length overflowed"))?;
    if total > MAX_PAGED_VALUE_BYTES {
        return Err(value_too_large(format!(
            "An encoded row cannot exceed {MAX_PAGED_VALUE_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(total);
    append_record_prefix(&mut bytes);
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

pub(crate) fn decode_row(bytes: &[u8]) -> Result<Row> {
    if bytes.len() < ROW_HEADER_BYTES || bytes.len() > MAX_PAGED_VALUE_BYTES {
        return Err(storage_corrupt(format!(
            "An encoded row must contain between {ROW_HEADER_BYTES} and {MAX_PAGED_VALUE_BYTES} bytes"
        )));
    }
    validate_record_prefix(bytes, "row")?;
    let body_length = read_u32(bytes, RECORD_PREFIX_BYTES) as usize;
    if ROW_HEADER_BYTES.checked_add(body_length) != Some(bytes.len()) {
        return Err(storage_corrupt(
            "Encoded row length does not match its header",
        ));
    }
    let value = decode_canonical_json(&bytes[ROW_HEADER_BYTES..], "row")?;
    match value {
        Value::Object(row) => Ok(row),
        _ => Err(storage_corrupt("Encoded row JSON must be an object")),
    }
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

pub(crate) fn encode_catalog_table_record(
    record: &CatalogTableRecord,
) -> Result<(Vec<u8>, Vec<u8>)> {
    validate_catalog_name(&record.schema.name)?;
    validate_schema_shape(&record.schema)?;
    validate_catalog_item(
        record.tree_id,
        record.root_page_id,
        record.row_count,
        "table",
    )?;
    let key = encode_catalog_key(CATALOG_TABLE_KEY, &record.schema.name)?;
    let value = encode_catalog_item_value(
        record.tree_id,
        record.root_page_id,
        record.row_count,
        &record.schema,
        "table schema",
    )?;
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
    let (tree_id, root_page_id, row_count, schema) =
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
    let value = encode_catalog_item_value(
        record.tree_id,
        record.root_page_id,
        record.entry_count,
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
    let (tree_id, root_page_id, entry_count, definition) =
        decode_catalog_item_value::<IndexDefinition>(value, "index")?;
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
    let mut key = Vec::new();
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
            append_component(key, COMPONENT_BOOLEAN, &[u8::from(value)])
        }
        ColumnType::Integer => {
            let value =
                safe_integer(value).ok_or_else(|| key_type_mismatch(table, column, "integer"))?;
            let sortable = (value as u64) ^ (1_u64 << 63);
            append_component(key, COMPONENT_INTEGER, &sortable.to_be_bytes())
        }
        ColumnType::Float => {
            let Value::Number(number) = value else {
                return Err(key_type_mismatch(table, column, "float"));
            };
            let value = number
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| key_type_mismatch(table, column, "finite float"))?;
            // SQL comparison already operates in f64 space. Normalize both the JSON spelling and
            // signed zero before deriving lexicographically sortable IEEE-754 bytes, so a B-tree
            // cannot admit two primary keys which SQL considers equal.
            let normalized = if value == 0.0 { 0.0 } else { value };
            let bits = normalized.to_bits();
            let sortable = if bits & (1_u64 << 63) == 0 {
                bits ^ (1_u64 << 63)
            } else {
                !bits
            };
            append_component(key, COMPONENT_FLOAT, &sortable.to_be_bytes())
        }
        ColumnType::Text => {
            let value = value
                .as_str()
                .ok_or_else(|| key_type_mismatch(table, column, "text"))?;
            append_component(key, COMPONENT_TEXT, value.as_bytes())
        }
        ColumnType::Json => append_component(key, COMPONENT_JSON, &encode_canonical_json(value)?),
    }
}

fn append_component(key: &mut Vec<u8>, tag: u8, payload: &[u8]) -> Result<()> {
    let length = u16::try_from(payload.len()).map_err(|_| {
        value_too_large("A single key component cannot exceed 65,535 encoded bytes")
    })?;
    key.push(tag);
    key.extend_from_slice(&length.to_be_bytes());
    key.extend_from_slice(payload);
    Ok(())
}

fn component_end(key: &[u8], offset: usize) -> Result<usize> {
    let header_end = offset
        .checked_add(3)
        .filter(|end| *end <= key.len())
        .ok_or_else(|| storage_corrupt("A storage key component header is truncated"))?;
    if !matches!(
        key[offset],
        COMPONENT_BOOLEAN | COMPONENT_INTEGER | COMPONENT_FLOAT | COMPONENT_TEXT | COMPONENT_JSON
    ) {
        return Err(storage_version(format!(
            "Storage key component tag {:#04x} is not supported",
            key[offset]
        )));
    }
    let length = u16::from_be_bytes([key[offset + 1], key[offset + 2]]) as usize;
    header_end
        .checked_add(length)
        .filter(|end| *end <= key.len())
        .ok_or_else(|| storage_corrupt("A storage key component payload is truncated"))
}

fn validate_tuple_prefix(
    schema: &TableDefinition,
    columns: &[String],
    key: &[u8],
    mut offset: usize,
) -> Result<usize> {
    for column in columns {
        let start = offset;
        offset = component_end(key, offset)?;
        let payload = &key[start + 3..offset];
        validate_encoded_component(key[start], payload, schema_column_type(schema, column)?)?;
    }
    Ok(offset)
}

fn validate_encoded_component(tag: u8, payload: &[u8], data_type: ColumnType) -> Result<()> {
    let valid = match data_type {
        ColumnType::Boolean => tag == COMPONENT_BOOLEAN && matches!(payload, [0] | [1]),
        ColumnType::Integer => tag == COMPONENT_INTEGER && payload.len() == 8,
        ColumnType::Float => tag == COMPONENT_FLOAT && valid_encoded_float_component(payload),
        ColumnType::Text => tag == COMPONENT_TEXT && std::str::from_utf8(payload).is_ok(),
        ColumnType::Json => {
            tag == COMPONENT_JSON && decode_canonical_json(payload, "JSON key component").is_ok()
        }
    };
    if valid {
        Ok(())
    } else {
        Err(storage_corrupt(
            "A storage key component does not match its schema or canonical encoding",
        ))
    }
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
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
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
    model: &T,
    description: &str,
) -> Result<Vec<u8>> {
    let model = serde_json::to_value(model).map_err(|error| {
        codec_argument(format!("Could not encode catalog {description}: {error}"))
    })?;
    let body = encode_canonical_json(&model)?;
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
    value.extend_from_slice(&(body.len() as u32).to_le_bytes());
    value.extend_from_slice(&body);
    Ok(value)
}

fn decode_catalog_item_value<T: DeserializeOwned + Serialize>(
    value: &[u8],
    description: &str,
) -> Result<(TreeId, Option<PageId>, u64, T)> {
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
    validate_catalog_item(tree_id, root_page_id, count, description)
        .map_err(as_storage_corruption)?;
    let body_length = read_u32(value, 28) as usize;
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
    Ok((tree_id, root_page_id, count, model))
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
                self.append(b"{")?;
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for (index, key) in keys.into_iter().enumerate() {
                    if index != 0 {
                        self.append(b",")?;
                    }
                    self.append_json_string(key)?;
                    self.append(b":")?;
                    self.encode(&values[key], depth + 1)?;
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
                &[
                    0x02, 0x00, 0x08, 0x7f, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01
                ][..],
                &[
                    0x02, 0x00, 0x08, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
                ],
                &[
                    0x02, 0x00, 0x08, 0x80, 0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
                ],
                &[0x01, 0x00, 0x01, 0x00],
                &[0x01, 0x00, 0x01, 0x01],
            ]
            .concat()
        );
    }

    #[test]
    fn text_components_are_utf8_length_delimited_and_composites_do_not_collide() {
        let schema = typed_schema(
            vec!["a", "b"],
            vec![("a", ColumnType::Text), ("b", ColumnType::Text)],
        );
        let left = encode_primary_key(&schema, &row(json!({"a": "ab", "b": "c"}))).unwrap();
        let right = encode_primary_key(&schema, &row(json!({"a": "a", "b": "bc"}))).unwrap();
        assert_ne!(left, right);

        let unicode = encode_primary_key(&schema, &row(json!({"a": "a\u{0}é", "b": "λ"}))).unwrap();
        assert_eq!(&unicode[..7], &[COMPONENT_TEXT, 0, 4, b'a', 0, 0xc3, 0xa9]);
        assert_eq!(&unicode[7..], &[COMPONENT_TEXT, 0, 2, 0xce, 0xbb]);
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
        assert_eq!(
            integer,
            [
                vec![COMPONENT_FLOAT, 0, 8],
                0xbff0_0000_0000_0000_u64.to_be_bytes().to_vec()
            ]
            .concat()
        );
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
        assert_eq!(prefix.last(), Some(&INDEX_PRIMARY_KEY_SEPARATOR));
        assert!(secondary_index_entry_matches_prefix(&entry, &prefix));
        assert_eq!(
            secondary_index_primary_key(&entry, &prefix).unwrap(),
            encode_primary_key(&schema, &item).unwrap()
        );
        assert_eq!(
            secondary_index_primary_key_for_definition(&schema, &definition, &entry).unwrap(),
            encode_primary_key(&schema, &item).unwrap()
        );

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
    fn index_entry_parser_does_not_confuse_separator_bytes_inside_payloads() {
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
        let item = row(json!({"id": "primary", "value": "insideÿpayload"}));
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
    }

    #[test]
    fn key_and_integer_bounds_are_exact() {
        let schema = typed_schema(vec!["id"], vec![("id", ColumnType::Text)]);
        let accepted = "a".repeat(MAX_BTREE_KEY_BYTES - 3);
        assert_eq!(
            encode_primary_key(&schema, &row(json!({"id": accepted})))
                .unwrap()
                .len(),
            MAX_BTREE_KEY_BYTES
        );
        let rejected = "a".repeat(MAX_BTREE_KEY_BYTES - 2);
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

    #[test]
    fn row_codec_is_canonical_and_accepts_exactly_one_mebibyte() {
        let content = "a".repeat(MAX_PAGED_VALUE_BYTES - ROW_HEADER_BYTES - 8);
        let encoded = encode_row(&row(json!({"x": content}))).unwrap();
        assert_eq!(encoded.len(), MAX_PAGED_VALUE_BYTES);
        assert_eq!(
            decode_row(&encoded).unwrap()["x"].as_str().unwrap().len(),
            content.len()
        );

        let too_large = "a".repeat(content.len() + 1);
        assert_eq!(
            encode_row(&row(json!({"x": too_large}))).unwrap_err().code,
            "PAGED_VALUE_TOO_LARGE"
        );

        let first = row(json!({"z": {"b": 2, "a": 1}, "a": true}));
        let second = row(json!({"a": true, "z": {"a": 1, "b": 2}}));
        assert_eq!(encode_row(&first).unwrap(), encode_row(&second).unwrap());
    }

    #[test]
    fn row_decoder_rejects_noncanonical_non_object_and_header_corruption() {
        let mut encoded = encode_row(&row(json!({"a": 1, "b": 2}))).unwrap();
        let body = br#"{"b":2,"a":1}"#;
        encoded.truncate(ROW_HEADER_BYTES);
        encoded[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        encoded.extend_from_slice(body);
        assert_eq!(
            decode_row(&encoded).unwrap_err().code,
            "PAGED_STORAGE_CORRUPT"
        );

        let mut scalar = Vec::new();
        append_record_prefix(&mut scalar);
        scalar.extend_from_slice(&4_u32.to_le_bytes());
        scalar.extend_from_slice(b"null");
        assert_eq!(
            decode_row(&scalar).unwrap_err().code,
            "PAGED_STORAGE_CORRUPT"
        );

        for (offset, replacement, code) in [
            (0, 2, "PAGED_STORAGE_VERSION_UNSUPPORTED"),
            (1, 1, "PAGED_STORAGE_VERSION_UNSUPPORTED"),
            (2, 1, "PAGED_STORAGE_CORRUPT"),
        ] {
            let mut corrupt = encode_row(&row(json!({"id": 1}))).unwrap();
            corrupt[offset] = replacement;
            assert_eq!(decode_row(&corrupt).unwrap_err().code, code);
        }
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

    #[test]
    fn secondary_indexes_cross_validate_their_resolved_table_schema() {
        let schema = typed_schema(
            vec!["id"],
            vec![("id", ColumnType::Integer), ("rating", ColumnType::Float)],
        );
        for columns in [vec!["missing".to_owned()], vec!["rating".to_owned()]] {
            let definition = IndexDefinition {
                name: "items_lookup".to_owned(),
                table: "items".to_owned(),
                columns,
                unique: false,
            };
            assert!(
                encode_secondary_index_prefix(
                    &schema,
                    &definition,
                    &row(json!({"id": 1, "rating": 1.0})),
                )
                .is_err()
            );
        }
    }
}
