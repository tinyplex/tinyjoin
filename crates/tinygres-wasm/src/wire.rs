use std::mem::size_of;

use serde_json::{Map, Number, Value};
use tinygres_core::{
    ApplyOutcome, Change, ChangeBatch, ColumnDefinition, ColumnType, EngineError, ExecuteResult,
    Filter, FilterOperator, NullOrder, OrderBy, OrderDirection, QueryPlan, QueryResult, Result,
    Row, SourceCursor, TableSchema,
};

const VERSION: u8 = 1;
const SUCCESS: u8 = 0;
const FAILURE: u8 = 1;
const SAFE_RESPONSE: u8 = 0;
const DURABLE_RESPONSE: u8 = 1;

pub(crate) const OP_DEFINE_TABLES: u32 = 1;
pub(crate) const OP_REPLACE_SNAPSHOT: u32 = 2;
pub(crate) const OP_APPLY_BATCH: u32 = 3;
pub(crate) const OP_QUERY: u32 = 4;
pub(crate) const OP_QUERY_SQL: u32 = 5;
pub(crate) const OP_EXECUTE_SQL: u32 = 6;
pub(crate) const OP_BEGIN: u32 = 7;
pub(crate) const OP_COMMIT: u32 = 8;
pub(crate) const OP_ROLLBACK: u32 = 9;
pub(crate) const OP_IN_TRANSACTION: u32 = 10;
pub(crate) const OP_REVISION: u32 = 11;
pub(crate) const OP_WATERMARK: u32 = 12;
pub(crate) const OP_CLOSE: u32 = 13;

const JSON_NULL: u8 = 0;
const JSON_FALSE: u8 = 1;
const JSON_TRUE: u8 = 2;
const JSON_I64: u8 = 3;
const JSON_F64: u8 = 5;
const JSON_STRING: u8 = 6;
const JSON_ARRAY: u8 = 7;
const JSON_OBJECT: u8 = 8;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_NODES: usize = 1_000_000;
const MAX_ITEMS: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const STRING_OVERHEAD: usize = size_of::<String>();
const VEC_OVERHEAD: usize = size_of::<Vec<Value>>();
const MAP_ENTRY_OVERHEAD: usize = 128;

pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    budget: Budget,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(limit());
        }
        let mut reader = Self {
            bytes,
            position: 0,
            // The copied binary transport and the decoded Rust model each have
            // an independent 16 MiB cap. This matches the structured bridge's
            // retained-model contract without double-charging the same data.
            budget: Budget::default(),
        };
        if reader.u8()? != VERSION {
            return Err(invalid("Unsupported binary bridge version"));
        }
        Ok(reader)
    }

    pub(crate) fn finish(&self) -> Result<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("Trailing bytes in binary bridge request"))
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| invalid("Binary bridge length overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid("Truncated binary bridge request"))?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(*self.take_array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(*self.take_array()?))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(*self.take_array()?))
    }

    fn take_array<const N: usize>(&mut self) -> Result<&'a [u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| invalid("Truncated binary bridge request"))
    }

    fn count(&mut self) -> Result<usize> {
        let length = self.u32()? as usize;
        if length > MAX_ITEMS {
            return Err(limit());
        }
        Ok(length)
    }

    pub(crate) fn string(&mut self) -> Result<String> {
        let length = self.u32()? as usize;
        let bytes = self.take(length)?;
        let value =
            std::str::from_utf8(bytes).map_err(|_| invalid("Binary bridge string is not UTF-8"))?;
        self.budget.string(length)?;
        Ok(value.to_owned())
    }

    fn optional_string(&mut self) -> Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.string().map(Some),
            _ => Err(invalid("Invalid optional string tag")),
        }
    }

    fn strings(&mut self) -> Result<Vec<String>> {
        let length = self.count()?;
        self.budget.vector::<String>(length)?;
        let mut values = Vec::with_capacity(length);
        for _ in 0..length {
            values.push(self.string()?);
        }
        Ok(values)
    }

    pub(crate) fn schemas(&mut self) -> Result<Vec<TableSchema>> {
        let length = self.count()?;
        self.budget.vector::<TableSchema>(length)?;
        let mut schemas = Vec::with_capacity(length);
        for _ in 0..length {
            schemas.push(self.schema()?);
        }
        Ok(schemas)
    }

    pub(crate) fn schema(&mut self) -> Result<TableSchema> {
        let name = self.string()?;
        let primary_key = self.strings()?;
        let length = self.count()?;
        self.budget.vector::<ColumnDefinition>(length)?;
        let mut columns = Vec::with_capacity(length);
        for _ in 0..length {
            let name = self.string()?;
            let data_type = match self.u8()? {
                0 => ColumnType::Boolean,
                1 => ColumnType::Integer,
                2 => ColumnType::Float,
                3 => ColumnType::Text,
                4 => ColumnType::Json,
                _ => return Err(invalid("Invalid column type tag")),
            };
            let nullable = match self.u8()? {
                0 => false,
                1 => true,
                _ => return Err(invalid("Invalid nullable tag")),
            };
            let default = match self.u8()? {
                0 => None,
                1 => Some(self.value(0)?),
                _ => return Err(invalid("Invalid default tag")),
            };
            columns.push(ColumnDefinition {
                name,
                data_type,
                nullable,
                default,
            });
        }
        Ok(TableSchema {
            name,
            primary_key,
            columns,
        })
    }

    pub(crate) fn rows(&mut self) -> Result<Vec<Row>> {
        let length = self.count()?;
        self.budget.vector::<Row>(length)?;
        let mut rows = Vec::with_capacity(length);
        for _ in 0..length {
            rows.push(self.row(0)?);
        }
        Ok(rows)
    }

    fn row(&mut self, depth: usize) -> Result<Row> {
        self.budget.node(depth)?;
        self.row_body(depth)
    }

    fn row_body(&mut self, depth: usize) -> Result<Row> {
        let length = self.count()?;
        let mut row = Map::new();
        for _ in 0..length {
            let key = self.string()?;
            self.budget.map_entry()?;
            let value = self.value(depth + 1)?;
            if row.insert(key, value).is_some() {
                return Err(invalid("Duplicate binary object key"));
            }
        }
        Ok(row)
    }

    pub(crate) fn values(&mut self, depth: usize) -> Result<Vec<Value>> {
        let length = self.count()?;
        self.budget.vector::<Value>(length)?;
        let mut values = Vec::with_capacity(length);
        for _ in 0..length {
            values.push(self.value(depth)?);
        }
        Ok(values)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        self.budget.operation()?;
        self.budget.node(depth)?;
        match self.u8()? {
            JSON_NULL => Ok(Value::Null),
            JSON_FALSE => Ok(Value::Bool(false)),
            JSON_TRUE => Ok(Value::Bool(true)),
            JSON_I64 => {
                let value = self.i64()?;
                if value.unsigned_abs() > MAX_SAFE_INTEGER {
                    return Err(invalid("Integer is not JavaScript-safe"));
                }
                Ok(Value::Number(Number::from(value)))
            }
            JSON_F64 => {
                let value = f64::from_bits(self.u64()?);
                Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| invalid("JSON number must be finite"))
            }
            JSON_STRING => self.string().map(Value::String),
            JSON_ARRAY => self.values(depth + 1).map(Value::Array),
            JSON_OBJECT => self.row_body(depth).map(Value::Object),
            _ => Err(invalid("Invalid binary JSON tag")),
        }
    }

    pub(crate) fn batch(&mut self) -> Result<ChangeBatch> {
        let source_id = self.optional_string()?;
        let cursor = match self.u8()? {
            0 => None,
            1 => Some(SourceCursor {
                kind: self.string()?,
                value: self.string()?,
            }),
            _ => return Err(invalid("Invalid cursor tag")),
        };
        let transaction_id = self.optional_string()?;
        let committed_at = self.optional_string()?;
        let length = self.count()?;
        self.budget.vector::<Change>(length)?;
        let mut changes = Vec::with_capacity(length);
        for _ in 0..length {
            let kind = self.u8()?;
            let table = self.string()?;
            let row = self.row(0)?;
            changes.push(match kind {
                0 => Change::Upsert { table, row },
                1 => Change::Delete { table, key: row },
                _ => return Err(invalid("Invalid change tag")),
            });
        }
        Ok(ChangeBatch {
            source_id,
            cursor,
            transaction_id,
            committed_at,
            changes,
        })
    }

    pub(crate) fn query(&mut self) -> Result<QueryPlan> {
        let table = self.string()?;
        let columns = match self.u8()? {
            0 => None,
            1 => Some(self.strings()?),
            _ => return Err(invalid("Invalid projection tag")),
        };
        let filter_count = self.count()?;
        self.budget.vector::<Filter>(filter_count)?;
        let mut filters = Vec::with_capacity(filter_count);
        for _ in 0..filter_count {
            let column = self.string()?;
            let operator = match self.u8()? {
                0 => FilterOperator::Eq,
                1 => FilterOperator::Neq,
                2 => FilterOperator::Lt,
                3 => FilterOperator::Lte,
                4 => FilterOperator::Gt,
                5 => FilterOperator::Gte,
                _ => return Err(invalid("Invalid filter operator")),
            };
            filters.push(Filter {
                column,
                operator,
                value: self.value(0)?,
            });
        }
        let order_count = self.count()?;
        self.budget.vector::<OrderBy>(order_count)?;
        let mut order_by = Vec::with_capacity(order_count);
        for _ in 0..order_count {
            let column = self.string()?;
            let direction = match self.u8()? {
                0 => OrderDirection::Asc,
                1 => OrderDirection::Desc,
                _ => return Err(invalid("Invalid order direction")),
            };
            let nulls = match self.u8()? {
                0 => NullOrder::Default,
                1 => NullOrder::First,
                2 => NullOrder::Last,
                _ => return Err(invalid("Invalid null ordering")),
            };
            order_by.push(OrderBy {
                column,
                direction,
                nulls,
            });
        }
        let limit = match self.u8()? {
            0 => None,
            1 => Some(self.u32()? as usize),
            _ => return Err(invalid("Invalid query limit tag")),
        };
        let offset = self.u32()? as usize;
        if limit
            .and_then(|limit| offset.checked_add(limit))
            .is_some_and(|end| end > u32::MAX as usize)
        {
            return Err(invalid("Query window exceeds u32"));
        }
        Ok(QueryPlan {
            table,
            columns,
            filters,
            predicate: None,
            order_by,
            limit,
            offset,
        })
    }
}

#[derive(Default)]
struct Budget {
    retained: usize,
    nodes: usize,
    operations: usize,
}

impl Budget {
    fn retain(&mut self, bytes: usize) -> Result<()> {
        let next = self.retained.checked_add(bytes).ok_or_else(limit)?;
        if next > MAX_BYTES {
            return Err(limit());
        }
        self.retained = next;
        Ok(())
    }

    fn operation(&mut self) -> Result<()> {
        self.operations = self.operations.checked_add(1).ok_or_else(limit)?;
        if self.operations > MAX_ITEMS {
            return Err(limit());
        }
        Ok(())
    }

    fn node(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(invalid("Binary JSON exceeds the depth limit"));
        }
        self.nodes = self.nodes.checked_add(1).ok_or_else(limit)?;
        if self.nodes > MAX_NODES {
            return Err(limit());
        }
        Ok(())
    }

    fn string(&mut self, bytes: usize) -> Result<()> {
        self.retain(bytes.checked_add(STRING_OVERHEAD).ok_or_else(limit)?)
    }

    fn vector<T>(&mut self, length: usize) -> Result<()> {
        self.retain(
            size_of::<T>()
                .checked_mul(length)
                .and_then(|bytes| bytes.checked_add(VEC_OVERHEAD))
                .ok_or_else(limit)?,
        )
    }

    fn map_entry(&mut self) -> Result<()> {
        self.retain(
            size_of::<Value>()
                .checked_add(MAP_ENTRY_OVERHEAD)
                .ok_or_else(limit)?,
        )
    }
}

pub(crate) fn unit(committed: bool) -> Vec<u8> {
    vec![VERSION, SUCCESS, disposition(committed)]
}

pub(crate) fn boolean(value: bool) -> Vec<u8> {
    vec![VERSION, SUCCESS, SAFE_RESPONSE, u8::from(value)]
}

pub(crate) fn unsigned(value: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(11);
    bytes.extend_from_slice(&[VERSION, SUCCESS, SAFE_RESPONSE]);
    bytes.extend_from_slice(&value.to_le_bytes());
    bytes
}

pub(crate) fn apply_outcome(outcome: &ApplyOutcome, committed: bool) -> Result<Vec<u8>> {
    safe_revision(outcome.revision)?;
    encode_success(committed, |sink| {
        write_u64(sink, outcome.revision)?;
        write_strings(sink, &outcome.tables)
    })
}

pub(crate) fn query_result(result: &QueryResult, committed: bool) -> Result<Vec<u8>> {
    safe_revision(result.revision)?;
    encode_success(committed, |sink| {
        write_u64(sink, result.revision)?;
        write_rows(sink, &result.rows)
    })
}

pub(crate) fn execute_result(result: &ExecuteResult, committed: bool) -> Result<Vec<u8>> {
    safe_revision(result.revision)?;
    let row_count = u64::try_from(result.row_count).map_err(|_| serialization())?;
    if row_count > MAX_SAFE_INTEGER {
        return Err(serialization());
    }
    encode_success(committed, |sink| {
        write_string(sink, &result.command)?;
        write_u64(sink, result.revision)?;
        write_u64(sink, row_count)?;
        write_rows(sink, &result.rows)?;
        write_strings(sink, &result.tables)
    })
}

pub(crate) fn error(error: &EngineError) -> Vec<u8> {
    let retryable = match error.retryable {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    let payload = 4usize
        .checked_add(error.code.len())
        .and_then(|value| value.checked_add(4 + error.message.len() + 1));
    let (code, message, retryable, payload) = match payload {
        Some(payload) if payload + 3 <= MAX_BYTES => {
            (&*error.code, &*error.message, retryable, payload)
        }
        _ => (
            "BRIDGE_SERIALIZATION_ERROR",
            "TinyGres could not encode an error",
            1,
            4 + "BRIDGE_SERIALIZATION_ERROR".len()
                + 4
                + "TinyGres could not encode an error".len()
                + 1,
        ),
    };
    let mut bytes = Vec::with_capacity(payload + 3);
    bytes.extend_from_slice(&[VERSION, FAILURE, SAFE_RESPONSE]);
    write_raw_string(&mut bytes, code);
    write_raw_string(&mut bytes, message);
    bytes.push(retryable);
    bytes
}

struct Sink {
    output: Option<Vec<u8>>,
    bytes: usize,
    nodes: usize,
}

impl Sink {
    fn measure() -> Self {
        Self {
            output: None,
            bytes: 3,
            nodes: 0,
        }
    }

    fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(serialization)?;
        if next > MAX_BYTES {
            return Err(limit());
        }
        self.bytes = next;
        if let Some(output) = &mut self.output {
            output.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn node(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(serialization());
        }
        self.nodes = self.nodes.checked_add(1).ok_or_else(serialization)?;
        if self.nodes > MAX_NODES {
            return Err(limit());
        }
        Ok(())
    }
}

fn encode_success(committed: bool, write: impl Fn(&mut Sink) -> Result<()>) -> Result<Vec<u8>> {
    let mut sink = Sink::measure();
    write(&mut sink)?;
    let size = sink.bytes;
    sink.output = Some(Vec::with_capacity(size));
    sink.bytes = 0;
    sink.nodes = 0;
    sink.raw(&[VERSION, SUCCESS, disposition(committed)])?;
    write(&mut sink)?;
    Ok(sink.output.unwrap())
}

fn disposition(committed: bool) -> u8 {
    if committed {
        DURABLE_RESPONSE
    } else {
        SAFE_RESPONSE
    }
}

fn write_u8(sink: &mut Sink, value: u8) -> Result<()> {
    sink.raw(&[value])
}

fn write_u32(sink: &mut Sink, value: u32) -> Result<()> {
    sink.raw(&value.to_le_bytes())
}

fn write_u64(sink: &mut Sink, value: u64) -> Result<()> {
    sink.raw(&value.to_le_bytes())
}

fn write_i64(sink: &mut Sink, value: i64) -> Result<()> {
    sink.raw(&value.to_le_bytes())
}

fn write_string(sink: &mut Sink, value: &str) -> Result<()> {
    write_length(sink, value.len())?;
    sink.raw(value.as_bytes())
}

fn write_strings(sink: &mut Sink, values: &[String]) -> Result<()> {
    write_length(sink, values.len())?;
    for value in values {
        write_string(sink, value)?;
    }
    Ok(())
}

fn write_rows(sink: &mut Sink, rows: &[Row]) -> Result<()> {
    write_length(sink, rows.len())?;
    for row in rows {
        write_row(sink, row, 0)?;
    }
    Ok(())
}

fn write_row(sink: &mut Sink, row: &Row, depth: usize) -> Result<()> {
    sink.node(depth)?;
    write_row_body(sink, row, depth)
}

fn write_row_body(sink: &mut Sink, row: &Row, depth: usize) -> Result<()> {
    write_length(sink, row.len())?;
    for (key, value) in row {
        write_string(sink, key)?;
        write_value(sink, value, depth + 1)?;
    }
    Ok(())
}

fn write_value(sink: &mut Sink, value: &Value, depth: usize) -> Result<()> {
    sink.node(depth)?;
    match value {
        Value::Null => write_u8(sink, JSON_NULL),
        Value::Bool(false) => write_u8(sink, JSON_FALSE),
        Value::Bool(true) => write_u8(sink, JSON_TRUE),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            checked_number(number)?;
            write_u8(sink, JSON_I64)?;
            let value = number
                .as_i64()
                .unwrap_or_else(|| number.as_u64().expect("checked number") as i64);
            write_i64(sink, value)
        }
        Value::Number(number) => {
            checked_number(number)?;
            write_u8(sink, JSON_F64)?;
            write_u64(sink, number.as_f64().expect("checked number").to_bits())
        }
        Value::String(value) => {
            write_u8(sink, JSON_STRING)?;
            write_string(sink, value)
        }
        Value::Array(values) => {
            write_u8(sink, JSON_ARRAY)?;
            write_length(sink, values.len())?;
            for value in values {
                write_value(sink, value, depth + 1)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            write_u8(sink, JSON_OBJECT)?;
            write_row_body(sink, values, depth)
        }
    }
}

fn write_length(sink: &mut Sink, length: usize) -> Result<()> {
    if length > u32::MAX as usize {
        return Err(limit());
    }
    write_u32(sink, length as u32)
}

fn write_raw_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn checked_number(number: &Number) -> Result<()> {
    if let Some(value) = number.as_i64() {
        if value.unsigned_abs() <= MAX_SAFE_INTEGER {
            return Ok(());
        }
    } else if let Some(value) = number.as_u64() {
        if value <= MAX_SAFE_INTEGER {
            return Ok(());
        }
    } else if number.as_f64().is_some_and(f64::is_finite) {
        return Ok(());
    }
    Err(serialization())
}

fn safe_revision(value: u64) -> Result<()> {
    if value <= MAX_SAFE_INTEGER {
        Ok(())
    } else {
        Err(serialization())
    }
}

fn invalid(_message: &str) -> EngineError {
    EngineError::new("INVALID_BRIDGE_VALUE", "Invalid binary bridge request")
}

fn limit() -> EngineError {
    EngineError::new("RESOURCE_LIMIT", "Binary bridge resource limit exceeded")
}

fn serialization() -> EngineError {
    EngineError::new(
        "BRIDGE_SERIALIZATION_ERROR",
        "TinyGres could not encode the binary bridge result",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TYPED_SCHEMA_GOLDEN: &[u8] = &[
        1, 5, 0, 0, 0, 116, 121, 112, 101, 100, 1, 0, 0, 0, 2, 0, 0, 0, 105, 100, 2, 0, 0, 0, 2, 0,
        0, 0, 105, 100, 1, 0, 0, 4, 0, 0, 0, 109, 101, 116, 97, 4, 1, 1, 8, 1, 0, 0, 0, 1, 0, 0, 0,
        120, 3, 7, 0, 0, 0, 0, 0, 0, 0,
    ];

    const BATCH_GOLDEN: &[u8] = &[
        1, 1, 1, 0, 0, 0, 115, 1, 3, 0, 0, 0, 108, 115, 110, 2, 0, 0, 0, 52, 50, 1, 2, 0, 0, 0,
        116, 120, 1, 3, 0, 0, 0, 110, 111, 119, 2, 0, 0, 0, 0, 5, 0, 0, 0, 105, 116, 101, 109, 115,
        2, 0, 0, 0, 2, 0, 0, 0, 105, 100, 3, 7, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 110, 97, 109, 101,
        6, 3, 0, 0, 0, 65, 100, 97, 1, 5, 0, 0, 0, 105, 116, 101, 109, 115, 1, 0, 0, 0, 2, 0, 0, 0,
        105, 100, 3, 8, 0, 0, 0, 0, 0, 0, 0,
    ];

    const QUERY_GOLDEN: &[u8] = &[
        1, 5, 0, 0, 0, 105, 116, 101, 109, 115, 1, 2, 0, 0, 0, 2, 0, 0, 0, 105, 100, 5, 0, 0, 0,
        116, 105, 116, 108, 101, 1, 0, 0, 0, 2, 0, 0, 0, 105, 100, 0, 3, 7, 0, 0, 0, 0, 0, 0, 0, 1,
        0, 0, 0, 5, 0, 0, 0, 116, 105, 116, 108, 101, 1, 2, 1, 10, 0, 0, 0, 2, 0, 0, 0,
    ];

    #[test]
    fn values_preserve_integer_float_and_negative_zero_identity() {
        let payload = [
            VERSION, 4, 0, 0, 0, JSON_I64, 42, 0, 0, 0, 0, 0, 0, 0, JSON_F64, 0, 0, 0, 0, 0, 0, 0,
            128, JSON_NULL, JSON_TRUE,
        ];
        let mut reader = Reader::new(&payload).unwrap();
        let values = reader.values(0).unwrap();
        reader.finish().unwrap();
        assert_eq!(values[0], json!(42));
        assert!(values[1].as_f64().unwrap().is_sign_negative());
        assert_eq!(values[2], Value::Null);
        assert_eq!(values[3], Value::Bool(true));
    }

    #[test]
    fn output_preflight_and_error_envelope_are_bounded() {
        let result = QueryResult {
            revision: 1,
            rows: vec![Map::from_iter([(
                "payload".into(),
                Value::String("x".repeat(MAX_BYTES)),
            )])],
        };
        assert_eq!(
            query_result(&result, false).unwrap_err().code,
            "RESOURCE_LIMIT"
        );
        let encoded = error(&EngineError::new("TEST", "message").with_retryable(false));
        assert_eq!(&encoded[..3], &[VERSION, FAILURE, SAFE_RESPONSE]);
        assert!(encoded.len() <= MAX_BYTES);
    }

    #[test]
    fn request_goldens_decode_typed_schema_batch_and_query() {
        let mut reader = Reader::new(TYPED_SCHEMA_GOLDEN).unwrap();
        let schema = reader.schema().unwrap();
        reader.finish().unwrap();
        assert_eq!(schema.name, "typed");
        assert_eq!(schema.primary_key, ["id"]);
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].data_type, ColumnType::Integer);
        assert!(!schema.columns[0].nullable);
        assert_eq!(schema.columns[1].data_type, ColumnType::Json);
        assert_eq!(schema.columns[1].default, Some(json!({"x": 7})));

        let mut reader = Reader::new(BATCH_GOLDEN).unwrap();
        let batch = reader.batch().unwrap();
        reader.finish().unwrap();
        assert_eq!(batch.source_id.as_deref(), Some("s"));
        assert_eq!(
            batch.cursor.as_ref().map(|cursor| &*cursor.kind),
            Some("lsn")
        );
        assert_eq!(batch.transaction_id.as_deref(), Some("tx"));
        assert_eq!(batch.committed_at.as_deref(), Some("now"));
        assert_eq!(batch.changes.len(), 2);
        assert!(matches!(
            &batch.changes[0],
            Change::Upsert { table, row }
                if table == "items" && row.get("name") == Some(&json!("Ada"))
        ));
        assert!(matches!(
            &batch.changes[1],
            Change::Delete { table, key }
                if table == "items" && key.get("id") == Some(&json!(8))
        ));

        let mut reader = Reader::new(QUERY_GOLDEN).unwrap();
        let query = reader.query().unwrap();
        reader.finish().unwrap();
        assert_eq!(query.table, "items");
        assert_eq!(
            query.columns.as_deref(),
            Some(&["id".into(), "title".into()][..])
        );
        assert_eq!(query.filters.len(), 1);
        assert_eq!(query.filters[0].value, json!(7));
        assert_eq!(query.order_by.len(), 1);
        assert_eq!(query.order_by[0].direction, OrderDirection::Desc);
        assert_eq!(query.order_by[0].nulls, NullOrder::Last);
        assert_eq!(query.limit, Some(10));
        assert_eq!(query.offset, 2);
    }

    #[test]
    fn response_goldens_preserve_disposition_and_retryability() {
        let outcome = ApplyOutcome {
            revision: 3,
            tables: vec!["items".into()],
        };
        assert_eq!(
            apply_outcome(&outcome, true).unwrap(),
            [
                1, 0, 1, 3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0, 105, 116, 101, 109, 115,
            ]
        );
        assert_eq!(unit(false), [1, 0, 0]);
        assert_eq!(boolean(true), [1, 0, 0, 1]);
        assert_eq!(
            error(&EngineError::new("TEST", "message").with_retryable(false)),
            [
                1, 1, 0, 4, 0, 0, 0, 84, 69, 83, 84, 7, 0, 0, 0, 109, 101, 115, 115, 97, 103, 101,
                1,
            ]
        );
    }

    #[test]
    fn transport_and_decoded_model_have_independent_prospective_caps() {
        let mut exact_transport = vec![0; MAX_BYTES];
        exact_transport[0] = VERSION;
        assert!(Reader::new(&exact_transport).is_ok());
        drop(exact_transport);

        let mut oversized_transport = vec![0; MAX_BYTES + 1];
        oversized_transport[0] = VERSION;
        assert_eq!(
            Reader::new(&oversized_transport).err().unwrap().code,
            "RESOURCE_LIMIT"
        );
        drop(oversized_transport);

        let item_count = 700_000u32;
        let mut model_heavy = Vec::with_capacity(5 + item_count as usize * 4);
        model_heavy.push(VERSION);
        model_heavy.extend_from_slice(&item_count.to_le_bytes());
        model_heavy.resize(model_heavy.capacity(), 0);
        assert!(model_heavy.len() < MAX_BYTES);
        let mut reader = Reader::new(&model_heavy).unwrap();
        assert_eq!(reader.strings().unwrap_err().code, "RESOURCE_LIMIT");
    }
}
