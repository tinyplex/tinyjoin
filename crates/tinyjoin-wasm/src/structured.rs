use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Number, Value};
use tinyjoin_core::{ApplyOutcome, EngineError, ExecuteResult, Result, Row};
use wasm_bindgen::JsValue;

pub(crate) const VERSION: u32 = 3;

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

const SUCCESS: u32 = 0;
const FAILURE: u32 = 1;
const SAFE_RESPONSE: u32 = 0;
const DURABLE_RESPONSE: u32 = 1;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExecuteSqlRequest {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
    #[serde(default)]
    pub(crate) array_rows: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExecutePreparedRequest {
    pub(crate) statement_id: u32,
    pub(crate) params: Vec<Value>,
    #[serde(default)]
    pub(crate) array_rows: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExecSqlRequest {
    pub(crate) sql: String,
    #[serde(default)]
    pub(crate) array_rows: bool,
}

pub(crate) fn decode<T: for<'de> Deserialize<'de>>(payload: JsValue) -> Result<T> {
    serde_wasm_bindgen::from_value(payload).map_err(|_| invalid())
}

pub(crate) fn unit_payload(payload: &JsValue) -> Result<()> {
    if payload.is_undefined() {
        Ok(())
    } else {
        Err(invalid())
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
        let mut json = Self::default();
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

    /// Writes the per-table changed-key map. A table appears only when its complete key set is
    /// known, so an absent table means "changed, but re-read it" rather than "unchanged".
    fn changed_keys(&mut self, keys: &BTreeMap<String, Vec<Row>>) -> Result<()> {
        self.raw("{");
        for (index, (table, rows)) in keys.iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            self.string(table);
            self.raw(":[");
            for (index, row) in rows.iter().enumerate() {
                if index > 0 {
                    self.raw(",");
                }
                self.object(row, 0)?;
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
        let positions: Vec<usize> = fields
            .iter()
            .map(|field| {
                fields
                    .iter()
                    .filter(|other| other.name < field.name)
                    .count()
            })
            .collect();
        let mut names = vec![""; fields.len()];
        for (field, position) in fields.iter().zip(&positions) {
            names[*position] = &field.name;
        }
        let mut keys = Vec::new();
        self.raw("{\"fields\":[");
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                self.raw(",");
            }
            self.raw("{\"name\":");
            let start = self.0.len();
            self.string(&field.name);
            if !array_rows {
                let mut key = self.0[start..].to_owned();
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
                if key != name {
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
    use tinyjoin_core::ResultField;

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
            keys: BTreeMap::from([(
                "items".into(),
                vec![json!({"id": 1}).as_object().unwrap().clone()],
            )]),
        }
    }

    fn written(results: &[ExecuteResult], array_rows: bool, list: bool) -> Result<String> {
        let mut json = Json::success(true);
        json.results(results, array_rows, list)?;
        Ok(json.0)
    }

    #[test]
    fn operation_numbers_are_dense_and_stable() {
        assert_eq!(VERSION, 3);
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
            ],
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
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
                "[3,0,1,{header}]\n{fields},\"rows\":[{{\"title\":\"one\",\"id\":1}},{{\"title\":null,\"id\":2}}]}}"
            )
        );
        assert_eq!(
            written(std::slice::from_ref(&rows), true, false).unwrap(),
            format!("[3,0,1,{header}]\n{fields},\"rows\":[[\"one\",1],[null,2]]}}")
        );

        let empty = ExecuteResult {
            command: "UPDATE".into(),
            revision: 8,
            row_count: 3,
            fields: vec![],
            rows: vec![],
            tables: vec![],
            keys: BTreeMap::new(),
        };
        assert_eq!(
            written(&[empty.clone(), empty], false, true).unwrap(),
            [
                r#"[3,0,1,[{"command":"UPDATE","revision":8,"rowCount":3,"tables":[],"keys":{}},{"command":"UPDATE","revision":8,"rowCount":3,"tables":[],"keys":{}}]]"#,
                r#"{"fields":[],"rows":[]}"#,
                r#"{"fields":[],"rows":[]}"#,
            ]
            .join("\n")
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
            r#"[3,1,0,{"code":"CONSTRAINT","message":"a \"quoted\" failure","retryable":true}]"#
        );
        assert_eq!(
            error_text(&EngineError::new("HUGE", "x".repeat(MAX_BYTES))),
            r#"[3,1,0,{"code":"BRIDGE_SERIALIZATION_ERROR","message":"TinyJoin could not encode a structured bridge error","retryable":false}]"#
        );
    }

    #[test]
    fn operation_payload_structs_keep_exact_camel_case_shapes() {
        let request: ExecutePreparedRequest = serde_json::from_value(json!({
            "statementId": 7,
            "params": [1, "two"],
        }))
        .unwrap();
        assert_eq!(request.statement_id, 7);
        assert_eq!(request.params, [json!(1), json!("two")]);
        assert!(!request.array_rows);
        let request: ExecSqlRequest =
            serde_json::from_value(json!({"sql": "SELECT 1", "arrayRows": true})).unwrap();
        assert!(request.array_rows);
        assert!(
            serde_json::from_value::<ExecutePreparedRequest>(json!({
                "statementId": 7,
                "params": [],
                "extra": true,
            }))
            .is_err()
        );
    }
}
