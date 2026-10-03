use std::rc::Rc;

use serde_json::{Map, Number, Value};
use tinyjoin_core::{
    ApplyOutcome, ChangedKeys, ColumnType, EngineError, ExecuteResult, IndexDefinition, Result,
    TableDefinition,
};
use wasm_bindgen::JsValue;

pub(crate) const VERSION: u32 = 5;

pub(crate) const OP_EXECUTE_SQL: u32 = 1;
pub(crate) const OP_EXEC_SQL: u32 = 2;
pub(crate) const OP_PREPARE_SQL: u32 = 3;
pub(crate) const OP_EXECUTE_PREPARED: u32 = 4;
pub(crate) const OP_CLOSE_PREPARED: u32 = 5;
pub(crate) const OP_BEGIN: u32 = 6;
pub(crate) const OP_COMMIT: u32 = 7;
pub(crate) const OP_ROLLBACK: u32 = 8;
pub(crate) const OP_IN_TRANSACTION: u32 = 9;
pub(crate) const OP_REVISION: u32 = 10;
pub(crate) const OP_CLOSE: u32 = 11;
pub(crate) const OP_CHECK: u32 = 12;
pub(crate) const OP_SCHEMA: u32 = 13;
pub(crate) const OP_SET_SCHEMA: u32 = 14;

const SUCCESS: u32 = 0;
const FAILURE: u32 = 1;
const SAFE_RESPONSE: u32 = 0;
const DURABLE_RESPONSE: u32 = 1;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

const TAG_NULL: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_INTEGER: u8 = 3;
const TAG_FLOAT: u8 = 5;
const TAG_STRING: u8 = 6;
const TAG_ARRAY: u8 = 7;
const TAG_OBJECT: u8 = 8;

/// A request, read from the bytes the Worker wrote for it. A number is little-endian, a string is
/// its UTF-8 length as a u32 and then its bytes, a list is its length as a u32 and then its items,
/// and a JSON value is a tag byte and then its content: nothing for null and the booleans, the
/// eight bytes of a float for a number, which is an integer where its tag says so, and a string,
/// or a list of values, or a list of keys each followed by its value.
pub(crate) struct Request<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Request<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(invalid)?;
        let bytes = &self.bytes[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    pub(crate) fn flag(&mut self) -> Result<bool> {
        match self.take(1)? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(invalid()),
        }
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn f64(&mut self) -> Result<f64> {
        let bytes = self.take(8)?;
        let mut raw = [0; 8];
        raw.copy_from_slice(bytes);
        Ok(f64::from_le_bytes(raw))
    }

    pub(crate) fn string(&mut self) -> Result<&'a str> {
        let length = self.u32()? as usize;
        std::str::from_utf8(self.take(length)?).map_err(|_| invalid())
    }

    /// A list of JSON values, such as a statement's parameters.
    pub(crate) fn values(&mut self) -> Result<Vec<Value>> {
        self.list(0)
    }

    fn list(&mut self, depth: usize) -> Result<Vec<Value>> {
        let count = self.u32()? as usize;
        // Every value takes at least its tag, so a count past the bytes left is malformed, and
        // never reserves more than the request holds.
        if count > self.bytes.len() - self.at {
            return Err(invalid());
        }
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.value(depth)?);
        }
        Ok(values)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(invalid());
        }
        Ok(match self.take(1)?[0] {
            TAG_NULL => Value::Null,
            TAG_FALSE => Value::Bool(false),
            TAG_TRUE => Value::Bool(true),
            TAG_INTEGER => {
                let value = self.f64()?;
                if value.fract() != 0.0 || value.abs() > MAX_SAFE_INTEGER as f64 {
                    return Err(invalid());
                }
                Value::from(value as i64)
            }
            TAG_FLOAT => Value::Number(Number::from_f64(self.f64()?).ok_or_else(invalid)?),
            TAG_STRING => Value::String(self.string()?.to_owned()),
            TAG_ARRAY => Value::Array(self.list(depth + 1)?),
            TAG_OBJECT => {
                let count = self.u32()?;
                let mut object = Map::new();
                for _ in 0..count {
                    let key = self.string()?.to_owned();
                    object.insert(key, self.value(depth + 1)?);
                }
                Value::Object(object)
            }
            _ => return Err(invalid()),
        })
    }

    /// Checks that nothing follows what was read.
    pub(crate) fn finish(self) -> Result<()> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}

pub(crate) fn unit(committed: bool) -> Result<JsValue> {
    let mut json = Json::success(committed);
    json.raw("null]");
    json.finish()
}

pub(crate) fn boolean(value: bool) -> Result<JsValue> {
    let mut json = Json::success(false);
    json.raw(if value { "true]" } else { "false]" });
    json.finish()
}

pub(crate) fn unsigned(value: u64) -> Result<JsValue> {
    let mut json = Json::success(false);
    json.unsigned(value)?;
    json.raw("]");
    json.finish()
}

pub(crate) fn prepared_statement_id(value: u32) -> Result<JsValue> {
    unsigned(value.into())
}

/// Writes the schema as the payload.
pub(crate) fn schema(
    version: u64,
    tables: &[(Rc<TableDefinition>, Vec<IndexDefinition>)],
) -> Result<JsValue> {
    let mut json = Json::success(false);
    json.schema(version, tables)?;
    json.raw("]");
    json.finish()
}

pub(crate) fn apply_outcome(outcome: &ApplyOutcome, committed: bool) -> Result<JsValue> {
    let mut json = Json::success(committed);
    json.raw("{\"revision\":");
    json.unsigned(outcome.revision)?;
    json.raw(",\"tables\":");
    json.strings(&outcome.tables);
    json.raw(",\"keys\":");
    json.changed_keys(&outcome.keys)?;
    json.raw("}]");
    json.finish()
}

pub(crate) fn execute_result(
    result: &ExecuteResult,
    committed: bool,
    array_rows: bool,
) -> Result<JsValue> {
    let mut json = Json::success(committed);
    json.results(std::slice::from_ref(result), array_rows, false)?;
    json.finish()
}

/// Writes every result of a script: an array of their headers as the payload, and then each
/// result's fields and rows on a line of its own.
pub(crate) fn execute_results(
    results: &[ExecuteResult],
    committed: bool,
    array_rows: bool,
) -> Result<JsValue> {
    let mut json = Json::success(committed);
    json.results(results, array_rows, true)?;
    json.finish()
}

pub(crate) fn error(error: &EngineError) -> JsValue {
    JsValue::from_str(&error_text(error))
}

/// Private generated-constructor ABI: `new WasmEngine(device)` throws the JSON text of a
/// `{code, message, retryable?}` object directly. It is not a response envelope because no
/// engine/call boundary exists yet. The worker's constructor normalizer validates this exact
/// serialized-error shape before exposing it as a public error.
pub(crate) fn constructor_error(error: EngineError) -> JsValue {
    let mut json = Json::default();
    json.error(&error);
    JsValue::from_str(&json.0)
}

/// A failure's response. A message too long to send is replaced by one that says so.
fn error_text(error: &EngineError) -> String {
    let mut json = Json::envelope(FAILURE, SAFE_RESPONSE);
    json.error(error);
    json.raw("]");
    if json.0.len() <= MAX_BYTES {
        return json.0;
    }
    error_text(
        &EngineError::new(
            "BRIDGE_SERIALIZATION_ERROR",
            "TinyJoin could not encode a structured bridge error",
        )
        .with_retryable(false),
    )
}

/// A response, written as JSON text. Its first line is the envelope, `[version, status,
/// disposition, payload]`. A statement's payload is its header, and its fields and rows follow on
/// a line of their own, which the worker passes to the page without reading. JSON text holds no
/// raw line breaks, so the lines separate unambiguously.
#[derive(Default)]
struct Json(String);

impl Json {
    fn envelope(status: u32, disposition: u32) -> Self {
        // Room for a statement's header and a few rows, so that most responses are written without
        // growing the text.
        let mut json = Self(String::with_capacity(512));
        json.raw("[");
        for value in [VERSION, status, disposition] {
            json.raw(itoa::Buffer::new().format(value));
            json.raw(",");
        }
        json
    }

    fn success(committed: bool) -> Self {
        Self::envelope(
            SUCCESS,
            if committed {
                DURABLE_RESPONSE
            } else {
                SAFE_RESPONSE
            },
        )
    }

    fn finish(self) -> Result<JsValue> {
        self.bounded()?;
        Ok(JsValue::from_str(&self.0))
    }

    fn bounded(&self) -> Result<()> {
        if self.0.len() > MAX_BYTES {
            return Err(limit());
        }
        Ok(())
    }

    fn raw(&mut self, text: &str) {
        self.0.push_str(text);
    }

    fn unsigned(&mut self, value: u64) -> Result<()> {
        if value > MAX_SAFE_INTEGER {
            return Err(serialization());
        }
        self.raw(itoa::Buffer::new().format(value));
        Ok(())
    }

    /// Writes a JSON number as the JS number a response carries: an integer within JavaScript's
    /// safe range, or a finite float.
    fn number(&mut self, number: &Number) -> Result<()> {
        if let Some(value) = number.as_i64() {
            if value.unsigned_abs() > MAX_SAFE_INTEGER {
                return Err(serialization());
            }
            self.raw(itoa::Buffer::new().format(value));
        } else if let Some(value) = number.as_u64() {
            self.unsigned(value)?;
        } else {
            let value = number
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(serialization)?;
            self.raw(zmij::Buffer::new().format_finite(value));
        }
        Ok(())
    }

    fn string(&mut self, value: &str) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.0.push('"');
        let mut start = 0;
        for (index, byte) in value.bytes().enumerate() {
            let escaped = match byte {
                b'"' => "\\\"",
                b'\\' => "\\\\",
                b'\n' => "\\n",
                b'\r' => "\\r",
                b'\t' => "\\t",
                0..0x20 => "",
                _ => continue,
            };
            self.0.push_str(&value[start..index]);
            if escaped.is_empty() {
                self.0.push_str("\\u00");
                self.0.push(char::from(HEX[usize::from(byte >> 4)]));
                self.0.push(char::from(HEX[usize::from(byte & 0xf)]));
            } else {
                self.0.push_str(escaped);
            }
            start = index + 1;
        }
        self.0.push_str(&value[start..]);
        self.0.push('"');
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(serialization());
        }
        match value {
            Value::Null => self.raw("null"),
            Value::Bool(true) => self.raw("true"),
            Value::Bool(false) => self.raw("false"),
            Value::Number(number) => self.number(number)?,
            Value::String(text) => self.string(text),
            Value::Array(values) => {
                self.raw("[");
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        self.raw(",");
                    }
                    self.value(value, depth + 1)?;
                }
                self.raw("]");
            }
            Value::Object(values) => self.object(values, depth)?,
        }
        Ok(())
    }

    fn object(&mut self, values: &Map<String, Value>, depth: usize) -> Result<()> {
        self.raw("{");
        for (index, (key, value)) in values.iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            self.string(key);
            self.raw(":");
            self.value(value, depth + 1)?;
        }
        self.raw("}");
        Ok(())
    }

    fn strings(&mut self, values: &[String]) {
        self.raw("[");
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            self.string(value);
        }
        self.raw("]");
    }

    /// Writes the per-table changed-key map, each key as an object of its columns. A table
    /// appears only when its complete key set is known, so an absent table means "changed, but
    /// re-read it" rather than "unchanged".
    fn changed_keys(&mut self, keys: &ChangedKeys) -> Result<()> {
        self.raw("{");
        for (index, table) in keys.tables().iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            self.string(&table.table);
            self.raw(":[");
            for (index, values) in table.keys().enumerate() {
                self.raw(if index > 0 { ",{" } else { "{" });
                for (index, (column, value)) in table.columns.iter().zip(values).enumerate() {
                    if index > 0 {
                        self.raw(",");
                    }
                    self.string(column);
                    self.raw(":");
                    self.value(value, 1)?;
                }
                self.raw("}");
            }
            self.raw("]");
        }
        self.raw("}");
        Ok(())
    }

    fn error(&mut self, error: &EngineError) {
        self.raw("{\"code\":");
        self.string(&error.code);
        self.raw(",\"message\":");
        self.string(&error.message);
        if let Some(retryable) = error.retryable {
            self.raw(if retryable {
                ",\"retryable\":true"
            } else {
                ",\"retryable\":false"
            });
        }
        self.raw("}");
    }

    /// Writes the schema as an object of its tables, each with its columns, the columns of its
    /// primary key, and its indexes, all in the order the engine gives them.
    fn schema(
        &mut self,
        version: u64,
        tables: &[(Rc<TableDefinition>, Vec<IndexDefinition>)],
    ) -> Result<()> {
        self.raw("{\"version\":");
        self.unsigned(version)?;
        self.raw(",\"tables\":[");
        for (position, (table, indexes)) in tables.iter().enumerate() {
            if position > 0 {
                self.raw(",");
            }
            self.raw("{\"name\":");
            self.string(&table.name);
            self.raw(",\"columns\":[");
            for (position, column) in table.columns.iter().enumerate() {
                if position > 0 {
                    self.raw(",");
                }
                self.raw("{\"name\":");
                self.string(&column.name);
                self.raw(match column.data_type {
                    ColumnType::Boolean => ",\"type\":\"boolean\"",
                    ColumnType::Integer => ",\"type\":\"integer\"",
                    ColumnType::Float => ",\"type\":\"float\"",
                    ColumnType::Text => ",\"type\":\"text\"",
                    ColumnType::Json => ",\"type\":\"json\"",
                });
                self.raw(if column.nullable {
                    ",\"nullable\":true"
                } else {
                    ",\"nullable\":false"
                });
                if let Some(default) = &column.default {
                    self.raw(",\"default\":");
                    self.value(default, 1)?;
                }
                if let Some(length) = column.max_length {
                    self.raw(",\"maxLength\":");
                    self.unsigned(length.into())?;
                }
                self.raw("}");
            }
            self.raw("],\"primaryKey\":");
            self.strings(&table.primary_key);
            self.raw(",\"indexes\":[");
            for (position, index) in indexes.iter().enumerate() {
                if position > 0 {
                    self.raw(",");
                }
                self.raw("{\"name\":");
                self.string(&index.name);
                self.raw(",\"columns\":");
                self.strings(&index.columns);
                self.raw(if index.unique {
                    ",\"unique\":true}"
                } else {
                    ",\"unique\":false}"
                });
            }
            self.raw("]}");
            self.bounded()?;
        }
        self.raw("]}");
        Ok(())
    }

    /// Writes each result's header as the payload, or with `list` an array of them, closes the
    /// envelope, and then writes each result's fields and rows on a line of its own.
    fn results(&mut self, results: &[ExecuteResult], array_rows: bool, list: bool) -> Result<()> {
        if list {
            self.raw("[");
        }
        for (index, result) in results.iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            self.raw("{\"command\":");
            self.string(&result.command);
            self.raw(",\"revision\":");
            self.unsigned(result.revision)?;
            self.raw(",\"rowCount\":");
            self.unsigned(u64::try_from(result.row_count).map_err(|_| serialization())?)?;
            self.raw(",\"tables\":");
            self.strings(&result.tables);
            self.raw(",\"keys\":");
            self.changed_keys(&result.keys)?;
            self.raw("}");
        }
        self.raw(if list { "]]" } else { "]" });
        for result in results {
            self.raw("\n");
            self.rows(result, array_rows)?;
        }
        Ok(())
    }

    /// Writes a result's fields, and its rows with their values in field order: as arrays, or as
    /// objects whose keys follow the fields.
    fn rows(&mut self, result: &ExecuteResult, array_rows: bool) -> Result<()> {
        let fields = &result.fields;
        // A row's values come in key order. Each field's position in that order is where its
        // value sits, and the key there must be the field's name.
        let mut positions: Vec<usize> = fields
            .iter()
            .map(|field| {
                fields
                    .iter()
                    .filter(|other| other.name < field.name)
                    .count()
            })
            .collect();
        // Fields of one name, which only array rows are returned, instead leave each row's
        // values keyed by field position, and so already in field order.
        let positional = (1..fields.len()).any(|index| {
            fields[..index]
                .iter()
                .any(|field| field.name == fields[index].name)
        });
        if positional {
            if !array_rows {
                return Err(serialization());
            }
            for (index, position) in positions.iter_mut().enumerate() {
                *position = index;
            }
        }
        let mut names = vec![""; fields.len()];
        for (field, position) in fields.iter().zip(&positions) {
            names[*position] = &field.name;
        }
        // Each field's escaped name and colon, as an object row writes it.
        let mut keys = Vec::with_capacity(if array_rows { 0 } else { fields.len() });
        self.raw("{\"fields\":[");
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            self.raw("{\"name\":");
            let start = self.0.len();
            self.string(&field.name);
            if !array_rows {
                let mut key = String::with_capacity(self.0.len() - start + 1);
                key.push_str(&self.0[start..]);
                key.push(':');
                keys.push(key);
            }
            self.raw(",\"dataTypeID\":");
            self.unsigned(field.data_type_id.into())?;
            self.raw("}");
        }
        self.raw("],\"rows\":[");
        let mut values = Vec::with_capacity(fields.len());
        for (index, row) in result.rows.iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            values.clear();
            for ((key, value), name) in row.iter().zip(&names) {
                // A positional key is three digits and then the field's name.
                let named = if positional {
                    key.get(3..)
                } else {
                    Some(&**key)
                };
                if named != Some(name) {
                    return Err(serialization());
                }
                values.push(value);
            }
            if row.len() != fields.len() || values.len() != fields.len() {
                return Err(serialization());
            }
            self.raw(if array_rows { "[" } else { "{" });
            for (index, position) in positions.iter().enumerate() {
                if index > 0 {
                    self.raw(",");
                }
                if !array_rows {
                    self.raw(&keys[index]);
                }
                self.value(values[*position], 1)?;
            }
            self.raw(if array_rows { "]" } else { "}" });
            self.bounded()?;
        }
        self.raw("]}");
        Ok(())
    }
}

fn invalid() -> EngineError {
    EngineError::new("INVALID_BRIDGE_VALUE", "Invalid structured bridge request")
}

fn limit() -> EngineError {
    EngineError::new(
        "RESOURCE_LIMIT",
        "Structured bridge resource limit exceeded",
    )
}

fn serialization() -> EngineError {
    EngineError::new(
        "BRIDGE_SERIALIZATION_ERROR",
        "TinyJoin could not encode the structured bridge result",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tinyjoin_core::{ColumnDefinition, ResultField, TableKeys};

    fn result(fields: &[(&str, u32)], rows: Vec<Value>) -> ExecuteResult {
        ExecuteResult {
            command: "SELECT".into(),
            revision: 7,
            row_count: rows.len(),
            fields: fields
                .iter()
                .map(|(name, data_type_id)| ResultField {
                    name: (*name).into(),
                    data_type_id: *data_type_id,
                })
                .collect(),
            rows: rows
                .into_iter()
                .map(|row| row.as_object().unwrap().clone())
                .collect(),
            tables: vec!["items".into()],
            keys: changed_keys("items", &["id"], vec![json!(1)]),
        }
    }

    fn changed_keys(table: &str, columns: &[&str], values: Vec<Value>) -> ChangedKeys {
        let mut keys = ChangedKeys::default();
        keys.insert(TableKeys {
            table: table.into(),
            columns: columns.iter().map(|column| (*column).into()).collect(),
            values,
        });
        keys
    }

    fn written(results: &[ExecuteResult], array_rows: bool, list: bool) -> Result<String> {
        let mut json = Json::success(true);
        json.results(results, array_rows, list)?;
        Ok(json.0)
    }

    #[test]
    fn operation_numbers_are_dense_and_stable() {
        assert_eq!(VERSION, 5);
        assert_eq!(
            [
                OP_EXECUTE_SQL,
                OP_EXEC_SQL,
                OP_PREPARE_SQL,
                OP_EXECUTE_PREPARED,
                OP_CLOSE_PREPARED,
                OP_BEGIN,
                OP_COMMIT,
                OP_ROLLBACK,
                OP_IN_TRANSACTION,
                OP_REVISION,
                OP_CLOSE,
                OP_CHECK,
                OP_SCHEMA,
                OP_SET_SCHEMA,
            ],
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]
        );
    }

    #[test]
    fn results_put_headers_in_the_envelope_and_rows_on_their_own_lines() {
        let rows = result(
            &[("title", 25), ("id", 20)],
            vec![
                json!({"id": 1, "title": "one"}),
                json!({"id": 2, "title": null}),
            ],
        );
        let header = r#"{"command":"SELECT","revision":7,"rowCount":2,"tables":["items"],"keys":{"items":[{"id":1}]}}"#;
        let fields =
            r#"{"fields":[{"name":"title","dataTypeID":25},{"name":"id","dataTypeID":20}]"#;
        assert_eq!(
            written(std::slice::from_ref(&rows), false, false).unwrap(),
            format!(
                "[5,0,1,{header}]\n{fields},\"rows\":[{{\"title\":\"one\",\"id\":1}},{{\"title\":null,\"id\":2}}]}}"
            )
        );
        assert_eq!(
            written(std::slice::from_ref(&rows), true, false).unwrap(),
            format!("[5,0,1,{header}]\n{fields},\"rows\":[[\"one\",1],[null,2]]}}")
        );

        let empty = ExecuteResult {
            command: "UPDATE".into(),
            revision: 8,
            row_count: 3,
            fields: vec![],
            rows: vec![],
            tables: vec![],
            keys: ChangedKeys::default(),
        };
        assert_eq!(
            written(&[empty.clone(), empty], false, true).unwrap(),
            [
                r#"[5,0,1,[{"command":"UPDATE","revision":8,"rowCount":3,"tables":[],"keys":{}},{"command":"UPDATE","revision":8,"rowCount":3,"tables":[],"keys":{}}]]"#,
                r#"{"fields":[],"rows":[]}"#,
                r#"{"fields":[],"rows":[]}"#,
            ]
            .join("\n")
        );
    }

    #[test]
    fn fields_of_one_name_are_written_as_array_rows_keyed_by_position() {
        let rows = result(
            &[("id", 20), ("title", 25), ("id", 20)],
            vec![json!({"000id": 1, "001title": "one", "002id": 7})],
        );
        let written_rows = written(std::slice::from_ref(&rows), true, false).unwrap();
        assert!(
            written_rows.ends_with(
                r#"{"fields":[{"name":"id","dataTypeID":20},{"name":"title","dataTypeID":25},{"name":"id","dataTypeID":20}],"rows":[[1,"one",7]]}"#
            ),
            "{written_rows}"
        );
        // An object cannot hold them, and the engine returns them only for array rows, under
        // keys that end in their names.
        assert_eq!(
            written(std::slice::from_ref(&rows), false, false)
                .unwrap_err()
                .code,
            "BRIDGE_SERIALIZATION_ERROR"
        );
        let misnamed = result(
            &[("id", 20), ("id", 20)],
            vec![json!({"000id": 1, "001title": 7})],
        );
        assert_eq!(
            written(&[misnamed], true, false).unwrap_err().code,
            "BRIDGE_SERIALIZATION_ERROR"
        );
    }

    #[test]
    fn a_schema_is_written_with_its_columns_keys_and_indexes() {
        let table = TableDefinition {
            name: "t\"1".into(),
            primary_key: vec!["id".into(), "k".into()],
            columns: vec![
                ColumnDefinition {
                    name: "id".into(),
                    data_type: ColumnType::Integer,
                    nullable: false,
                    default: None,
                    max_length: None,
                },
                ColumnDefinition {
                    name: "k".into(),
                    data_type: ColumnType::Text,
                    nullable: false,
                    default: Some(json!("x")),
                    max_length: Some(8),
                },
                ColumnDefinition {
                    name: "doc".into(),
                    data_type: ColumnType::Json,
                    nullable: true,
                    default: Some(Value::Null),
                    max_length: None,
                },
            ],
        };
        let index = IndexDefinition {
            name: "by_k".into(),
            table: "t\"1".into(),
            columns: vec!["k".into()],
            unique: true,
        };
        let mut json = Json::default();
        json.schema(7, &[(Rc::new(table), vec![index])]).unwrap();
        assert_eq!(
            json.0,
            concat!(
                r#"{"version":7,"tables":[{"name":"t\"1","columns":["#,
                r#"{"name":"id","type":"integer","nullable":false},"#,
                r#"{"name":"k","type":"text","nullable":false,"default":"x","maxLength":8},"#,
                r#"{"name":"doc","type":"json","nullable":true,"default":null}],"#,
                r#""primaryKey":["id","k"],"indexes":[{"name":"by_k","columns":["k"],"unique":true}]}]}"#,
            )
        );
        let mut json = Json::default();
        json.schema(0, &[]).unwrap();
        assert_eq!(json.0, r#"{"version":0,"tables":[]}"#);
    }

    #[test]
    fn changed_keys_are_written_as_objects_of_their_columns() {
        let mut keys = changed_keys(
            "pairs",
            &["a", "b"],
            vec![json!(1), json!("x"), json!(2), json!("y")],
        );
        keys.insert(TableKeys {
            table: "empty".into(),
            columns: vec!["id".into()],
            values: vec![],
        });
        let mut json = Json::default();
        json.changed_keys(&keys).unwrap();
        assert_eq!(
            json.0,
            r#"{"empty":[],"pairs":[{"a":1,"b":"x"},{"a":2,"b":"y"}]}"#
        );
    }

    #[test]
    fn values_are_written_as_the_json_a_page_parses_back() {
        let text = "quote \" backslash \\ line\nreturn\r tab\t bell\u{7} é 😀";
        let value = json!([text, 1.5, -0.0, 1e21, -9_007_199_254_740_991_i64, {"nested": [true, false, null]}]);
        let mut json = Json::default();
        json.value(&value, 0).unwrap();
        assert!(!json.0.contains('\n'));
        assert_eq!(serde_json::from_str::<Value>(&json.0).unwrap(), value);
        assert!(json.0.contains(r#"\u0007"#));
        assert!(json.0.contains("-0.0"));
    }

    #[test]
    fn unsafe_numbers_deep_values_and_mismatched_rows_are_refused() {
        let mut json = Json::default();
        assert_eq!(
            json.unsigned(MAX_SAFE_INTEGER + 1).unwrap_err().code,
            "BRIDGE_SERIALIZATION_ERROR"
        );
        let unsafe_integer = json!(-9_007_199_254_740_992_i64);
        assert_eq!(
            json.value(&unsafe_integer, 0).unwrap_err().code,
            "BRIDGE_SERIALIZATION_ERROR"
        );
        let mut deep = json!(null);
        for _ in 0..MAX_DEPTH {
            deep = json!([deep]);
        }
        assert!(json.value(&deep, 0).is_ok());
        assert_eq!(
            json.value(&json!([deep]), 0).unwrap_err().code,
            "BRIDGE_SERIALIZATION_ERROR"
        );

        for row in [
            json!({"id": 1}),
            json!({"id": 1, "title": "x", "extra": 2}),
            json!({"id": 1, "name": "x"}),
        ] {
            let mismatched = result(&[("id", 20), ("title", 25)], vec![row]);
            assert_eq!(
                written(&[mismatched], false, false).unwrap_err().code,
                "BRIDGE_SERIALIZATION_ERROR"
            );
        }
    }

    #[test]
    fn oversized_results_and_errors_are_bounded() {
        let oversized = result(
            &[("value", 25)],
            vec![json!({"value": "x".repeat(MAX_BYTES)})],
        );
        assert_eq!(
            written(&[oversized], false, false).unwrap_err().code,
            "RESOURCE_LIMIT"
        );

        let error = EngineError::new("CONSTRAINT", "a \"quoted\" failure").with_retryable(true);
        assert_eq!(
            error_text(&error),
            r#"[5,1,0,{"code":"CONSTRAINT","message":"a \"quoted\" failure","retryable":true}]"#
        );
        assert_eq!(
            error_text(&EngineError::new("HUGE", "x".repeat(MAX_BYTES))),
            r#"[5,1,0,{"code":"BRIDGE_SERIALIZATION_ERROR","message":"TinyJoin could not encode a structured bridge error","retryable":false}]"#
        );
    }

    #[test]
    fn requests_read_exactly_the_values_written() {
        let mut bytes = vec![1, 7, 0, 0, 0, 3, 0, 0, 0];
        bytes.push(TAG_INTEGER);
        bytes.extend_from_slice(&(-0.0_f64).to_le_bytes());
        bytes.push(TAG_FLOAT);
        bytes.extend_from_slice(&0.9856906946328695_f64.to_le_bytes());
        bytes.push(TAG_OBJECT);
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        for (key, value) in [("b", TAG_TRUE), ("a", TAG_NULL)] {
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.push(value);
        }
        let mut request = Request::new(&bytes);
        assert!(request.flag().unwrap());
        assert_eq!(request.u32().unwrap(), 7);
        let values = request.values().unwrap();
        request.finish().unwrap();
        assert_eq!(
            values,
            [
                json!(0),
                json!(0.9856906946328695),
                json!({"a": null, "b": true})
            ]
        );
        assert!(values[0].is_u64());

        // Truncated, trailing, unsafe, and unknown content is refused.
        for malformed in [
            &bytes[..bytes.len() - 1],
            &[bytes.as_slice(), &[0]].concat()[..],
            &[0, 1, 0, 0, 0, TAG_INTEGER, 0, 0, 0, 0, 0, 0, 0x40, 0x43][..],
            &[0, 1, 0, 0, 0, TAG_INTEGER, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f][..],
            &[0, 1, 0, 0, 0, 4][..],
            &[0, 255, 255, 255, 255][..],
            &[2][..],
        ] {
            let mut request = Request::new(malformed);
            let read = request
                .flag()
                .and_then(|_| request.values())
                .and_then(|_| request.finish());
            assert_eq!(read.unwrap_err().code, "INVALID_BRIDGE_VALUE");
        }
    }
}
