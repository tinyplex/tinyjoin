use std::{collections::BTreeMap, mem::size_of};

use js_sys::{Array, Object, Reflect};
use serde::Deserialize;
use serde_json::{Map, Number, Value};
use tinyjoin_core::{ApplyOutcome, EngineError, ExecuteResult, Result, ResultField, Row};
use wasm_bindgen::{JsCast, JsValue};

pub(crate) const VERSION: u32 = 2;

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
const MAX_NODES: usize = 1_000_000;
const MAX_ITEMS: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const STRING_OVERHEAD: usize = size_of::<String>();
const JS_ARRAY_OVERHEAD: usize = 12;
const JS_VALUE_BYTES: usize = 4;
const JS_OBJECT_OVERHEAD: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecuteSqlRequest {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExecutePreparedRequest {
    pub(crate) statement_id: u32,
    pub(crate) params: Vec<Value>,
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
    success(committed, JsValue::UNDEFINED)
}

pub(crate) fn boolean(value: bool) -> Result<JsValue> {
    let mut measure = Measure::default();
    measure.raw(1)?;
    success(false, JsValue::from_bool(value))
}

pub(crate) fn unsigned(value: u64) -> Result<JsValue> {
    safe_number(value)?;
    let mut measure = Measure::default();
    measure.raw(8)?;
    success(false, JsValue::from_f64(value as f64))
}

pub(crate) fn prepared_statement_id(value: u32) -> Result<JsValue> {
    let mut measure = Measure::default();
    measure.raw(4)?;
    success(false, JsValue::from_f64(f64::from(value)))
}

pub(crate) fn apply_outcome(outcome: &ApplyOutcome, committed: bool) -> Result<JsValue> {
    let mut measure = Measure::default();
    measure.apply_outcome(outcome)?;
    success(committed, build_apply_outcome(outcome)?)
}

pub(crate) fn execute_result(result: &ExecuteResult, committed: bool) -> Result<JsValue> {
    let mut measure = Measure::default();
    measure.execute_result(result)?;
    success(committed, build_execute_result(result)?)
}

pub(crate) fn execute_results(results: &[ExecuteResult], committed: bool) -> Result<JsValue> {
    let mut measure = Measure::default();
    measure.array(results.len())?;
    for result in results {
        measure.operation()?;
        measure.execute_result(result)?;
    }
    let values = array(results.len())?;
    for (index, result) in results.iter().enumerate() {
        values.set(index as u32, build_execute_result(result)?);
    }
    success(committed, values.into())
}

pub(crate) fn error(error: &EngineError) -> std::result::Result<JsValue, JsValue> {
    let payload = serialized_error(error)?;
    envelope(FAILURE, SAFE_RESPONSE, payload).map_err(|_| terminal_error())
}

/// Private generated-constructor ABI: `new WasmEngine(device)` throws this
/// null-prototype `{code, message, retryable?}` object directly. It is not a
/// response envelope because no engine/call boundary exists yet. The worker's
/// structured constructor normalizer validates this exact serialized-error
/// shape before exposing it as a public error.
pub(crate) fn constructor_error(error: EngineError) -> JsValue {
    serialized_error(&error).unwrap_or_else(|error| error)
}

fn serialized_error(error: &EngineError) -> std::result::Result<JsValue, JsValue> {
    match error_payload(error) {
        Ok(payload) => Ok(payload),
        Err(_) => error_payload(
            &EngineError::new(
                "BRIDGE_SERIALIZATION_ERROR",
                "TinyJoin could not encode a structured bridge error",
            )
            .with_retryable(false),
        )
        .map_err(|_| terminal_error()),
    }
}

fn error_payload(error: &EngineError) -> Result<JsValue> {
    let mut measure = Measure::default();
    let keys = if error.retryable.is_some() {
        &["code", "message", "retryable"][..]
    } else {
        &["code", "message"][..]
    };
    measure.result_object(keys)?;
    measure.string(&error.code)?;
    measure.string(&error.message)?;
    measure.raw(1)?;
    let payload = record();
    set(&payload, "code", &JsValue::from_str(&error.code))?;
    set(&payload, "message", &JsValue::from_str(&error.message))?;
    if let Some(retryable) = error.retryable {
        set(&payload, "retryable", &JsValue::from_bool(retryable))?;
    }
    Ok(payload.into())
}

fn terminal_error() -> JsValue {
    JsValue::from_str("TinyJoin could not encode a structured bridge error")
}

fn success(committed: bool, payload: JsValue) -> Result<JsValue> {
    envelope(
        SUCCESS,
        if committed {
            DURABLE_RESPONSE
        } else {
            SAFE_RESPONSE
        },
        payload,
    )
}

fn envelope(status: u32, disposition: u32, payload: JsValue) -> Result<JsValue> {
    let response = Array::new_with_length(4);
    response.set(0, JsValue::from_f64(f64::from(VERSION)));
    response.set(1, JsValue::from_f64(f64::from(status)));
    response.set(2, JsValue::from_f64(f64::from(disposition)));
    response.set(3, payload);
    Ok(response.into())
}

fn build_apply_outcome(outcome: &ApplyOutcome) -> Result<JsValue> {
    safe_number(outcome.revision)?;
    let value = record();
    set(
        &value,
        "revision",
        &JsValue::from_f64(outcome.revision as f64),
    )?;
    set(&value, "tables", &build_strings(&outcome.tables)?.into())?;
    set(&value, "keys", &build_changed_keys(&outcome.keys)?)?;
    Ok(value.into())
}

fn build_execute_result(result: &ExecuteResult) -> Result<JsValue> {
    safe_number(result.revision)?;
    let row_count = u64::try_from(result.row_count).map_err(|_| serialization())?;
    safe_number(row_count)?;
    let value = record();
    set(&value, "command", &JsValue::from_str(&result.command))?;
    set(
        &value,
        "revision",
        &JsValue::from_f64(result.revision as f64),
    )?;
    set(&value, "rowCount", &JsValue::from_f64(row_count as f64))?;
    set(&value, "fields", &build_fields(&result.fields)?.into())?;
    set(&value, "rows", &build_rows(&result.rows)?.into())?;
    set(&value, "tables", &build_strings(&result.tables)?.into())?;
    set(&value, "keys", &build_changed_keys(&result.keys)?)?;
    Ok(value.into())
}

fn build_fields(fields: &[ResultField]) -> Result<Array> {
    let values = array(fields.len())?;
    for (index, field) in fields.iter().enumerate() {
        let value = record();
        set(&value, "name", &JsValue::from_str(&field.name))?;
        set(
            &value,
            "dataTypeID",
            &JsValue::from_f64(f64::from(field.data_type_id)),
        )?;
        values.set(index as u32, value.into());
    }
    Ok(values)
}

/// Builds the per-table changed-key map. A table appears only when its complete key set is known,
/// so an absent table means "changed, but re-read it" rather than "unchanged".
fn build_changed_keys(keys: &BTreeMap<String, Vec<Row>>) -> Result<JsValue> {
    let value = record();
    for (table, rows) in keys {
        set(&value, table, &build_rows(rows)?.into())?;
    }
    Ok(value.into())
}

fn build_rows(rows: &[Row]) -> Result<Array> {
    let values = array(rows.len())?;
    for (index, row) in rows.iter().enumerate() {
        values.set(index as u32, build_row(row, 0)?.into());
    }
    Ok(values)
}

fn build_row(row: &Row, depth: usize) -> Result<Object> {
    if depth > MAX_DEPTH {
        return Err(serialization());
    }
    let value = record();
    for (key, child) in row {
        set(&value, key, &build_value(child, depth + 1)?)?;
    }
    Ok(value)
}

fn build_value(value: &Value, depth: usize) -> Result<JsValue> {
    if depth > MAX_DEPTH {
        return Err(serialization());
    }
    match value {
        Value::Null => Ok(JsValue::NULL),
        Value::Bool(value) => Ok(JsValue::from_bool(*value)),
        Value::Number(number) => number_value(number),
        Value::String(value) => Ok(JsValue::from_str(value)),
        Value::Array(values) => {
            let output = array(values.len())?;
            for (index, value) in values.iter().enumerate() {
                output.set(index as u32, build_value(value, depth + 1)?);
            }
            Ok(output.into())
        }
        Value::Object(values) => Ok(build_row(values, depth)?.into()),
    }
}

fn number_value(number: &Number) -> Result<JsValue> {
    checked_number(number)?;
    if let Some(value) = number.as_i64() {
        Ok(JsValue::from_f64(value as f64))
    } else if let Some(value) = number.as_u64() {
        Ok(JsValue::from_f64(value as f64))
    } else {
        Ok(JsValue::from_f64(
            number.as_f64().ok_or_else(serialization)?,
        ))
    }
}

fn build_strings(values: &[String]) -> Result<Array> {
    let output = array(values.len())?;
    for (index, value) in values.iter().enumerate() {
        output.set(index as u32, JsValue::from_str(value));
    }
    Ok(output)
}

fn array(length: usize) -> Result<Array> {
    let length = u32::try_from(length).map_err(|_| limit())?;
    Ok(Array::new_with_length(length))
}

fn record() -> Object {
    Object::create(JsValue::NULL.unchecked_ref::<Object>())
}

fn set(target: &Object, key: &str, value: &JsValue) -> Result<()> {
    match Reflect::set(target.as_ref(), &JsValue::from_str(key), value) {
        Ok(true) => Ok(()),
        _ => Err(serialization()),
    }
}

struct Measure {
    bytes: usize,
    retained: usize,
    nodes: usize,
    operations: usize,
}

impl Default for Measure {
    fn default() -> Self {
        Self {
            // Charge the version/status/disposition prefix before measuring
            // the structured response payload's canonical logical size.
            bytes: 3,
            retained: 0,
            nodes: 0,
            operations: 0,
        }
    }
}

impl Measure {
    fn raw(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(limit)?;
        if self.bytes > MAX_BYTES {
            return Err(limit());
        }
        Ok(())
    }

    fn retain(&mut self, bytes: usize) -> Result<()> {
        self.retained = self.retained.checked_add(bytes).ok_or_else(limit)?;
        if self.retained > MAX_BYTES {
            return Err(limit());
        }
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
            return Err(serialization());
        }
        self.nodes = self.nodes.checked_add(1).ok_or_else(limit)?;
        if self.nodes > MAX_NODES {
            return Err(limit());
        }
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<()> {
        self.raw(4usize.checked_add(value.len()).ok_or_else(limit)?)?;
        self.retain(STRING_OVERHEAD.checked_add(value.len()).ok_or_else(limit)?)
    }

    fn array(&mut self, length: usize) -> Result<()> {
        if length > MAX_ITEMS {
            return Err(limit());
        }
        self.raw(4)?;
        self.retain(
            JS_ARRAY_OVERHEAD
                .checked_add(JS_VALUE_BYTES.checked_mul(length).ok_or_else(limit)?)
                .ok_or_else(limit)?,
        )
    }

    fn result_object(&mut self, keys: &[&str]) -> Result<()> {
        self.retain(JS_OBJECT_OVERHEAD)?;
        for key in keys {
            self.operation()?;
            self.retain(STRING_OVERHEAD.checked_add(key.len()).ok_or_else(limit)?)?;
        }
        Ok(())
    }

    fn strings(&mut self, values: &[String]) -> Result<()> {
        self.array(values.len())?;
        for value in values {
            self.operation()?;
            self.string(value)?;
        }
        Ok(())
    }

    fn fields(&mut self, fields: &[ResultField]) -> Result<()> {
        self.array(fields.len())?;
        for field in fields {
            self.operation()?;
            self.result_object(&["name", "dataTypeID"])?;
            self.string(&field.name)?;
            self.raw(4)?;
        }
        Ok(())
    }

    fn changed_keys(&mut self, keys: &BTreeMap<String, Vec<Row>>) -> Result<()> {
        self.retain(JS_OBJECT_OVERHEAD)?;
        for (table, rows) in keys {
            self.operation()?;
            self.string(table)?;
            self.rows(rows)?;
        }
        Ok(())
    }

    fn rows(&mut self, rows: &[Row]) -> Result<()> {
        self.array(rows.len())?;
        for row in rows {
            self.operation()?;
            self.row(row, 0)?;
        }
        Ok(())
    }

    fn row(&mut self, row: &Map<String, Value>, depth: usize) -> Result<()> {
        self.node(depth)?;
        self.retain(JS_OBJECT_OVERHEAD)?;
        self.raw(4)?;
        if row.len() > MAX_ITEMS {
            return Err(limit());
        }
        for (key, value) in row {
            self.operation()?;
            self.string(key)?;
            self.value(value, depth + 1)?;
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<()> {
        self.node(depth)?;
        self.raw(1)?;
        match value {
            Value::Null | Value::Bool(_) => Ok(()),
            Value::Number(number) => {
                checked_number(number)?;
                self.raw(8)
            }
            Value::String(value) => self.string(value),
            Value::Array(values) => {
                self.array(values.len())?;
                for value in values {
                    self.operation()?;
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
            Value::Object(values) => {
                self.retain(JS_OBJECT_OVERHEAD)?;
                self.raw(4)?;
                if values.len() > MAX_ITEMS {
                    return Err(limit());
                }
                for (key, value) in values {
                    self.operation()?;
                    self.string(key)?;
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
        }
    }

    fn apply_outcome(&mut self, outcome: &ApplyOutcome) -> Result<()> {
        safe_number(outcome.revision)?;
        self.result_object(&["revision", "tables", "keys"])?;
        self.raw(8)?;
        self.strings(&outcome.tables)?;
        self.changed_keys(&outcome.keys)
    }

    fn execute_result(&mut self, result: &ExecuteResult) -> Result<()> {
        safe_number(result.revision)?;
        let row_count = u64::try_from(result.row_count).map_err(|_| serialization())?;
        safe_number(row_count)?;
        self.result_object(&[
            "command", "revision", "rowCount", "fields", "rows", "tables", "keys",
        ])?;
        self.string(&result.command)?;
        self.raw(16)?;
        self.fields(&result.fields)?;
        self.rows(&result.rows)?;
        self.strings(&result.tables)?;
        self.changed_keys(&result.keys)
    }
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

fn safe_number(value: u64) -> Result<()> {
    if value <= MAX_SAFE_INTEGER {
        Ok(())
    } else {
        Err(serialization())
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

    #[test]
    fn operation_numbers_are_dense_and_stable() {
        assert_eq!(VERSION, 2);
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
    fn output_measure_preserves_safe_numbers_depth_and_resource_bounds() {
        let valid = ExecuteResult {
            command: "SELECT".into(),
            revision: MAX_SAFE_INTEGER,
            row_count: 1,
            fields: vec![ResultField {
                name: "value".into(),
                data_type_id: 25,
            }],
            rows: vec![Map::from_iter([("value".into(), json!([true, null, 1.5]))])],
            tables: vec![],
            keys: BTreeMap::new(),
        };
        let mut measure = Measure::default();
        measure.execute_result(&valid).unwrap();

        let invalid = ExecuteResult {
            command: "SELECT".into(),
            revision: MAX_SAFE_INTEGER + 1,
            row_count: 0,
            fields: vec![],
            rows: vec![],
            tables: vec![],
            keys: BTreeMap::new(),
        };
        assert_eq!(
            Measure::default()
                .execute_result(&invalid)
                .unwrap_err()
                .code,
            "BRIDGE_SERIALIZATION_ERROR"
        );

        let oversized = ExecuteResult {
            command: "SELECT".into(),
            revision: 1,
            row_count: 1,
            fields: vec![],
            rows: vec![Map::from_iter([(
                "value".into(),
                Value::String("x".repeat(MAX_BYTES)),
            )])],
            tables: vec![],
            keys: BTreeMap::new(),
        };
        assert_eq!(
            Measure::default()
                .execute_result(&oversized)
                .unwrap_err()
                .code,
            "RESOURCE_LIMIT"
        );
    }

    #[test]
    fn output_measure_keeps_the_canonical_logical_byte_cap() {
        let result = ExecuteResult {
            command: "SELECT".into(),
            revision: 7,
            row_count: 1,
            fields: vec![ResultField {
                name: "payload".into(),
                data_type_id: 25,
            }],
            rows: vec![Map::from_iter([(
                "payload".into(),
                json!({"nested": [true, null, 1.5, "value"]}),
            )])],
            tables: vec!["items".into()],
            keys: BTreeMap::new(),
        };
        let mut measure = Measure::default();
        measure.execute_result(&result).unwrap();

        // Version/status/disposition plus the canonical lengths, scalar widths,
        // collections, keys, and JSON tags for this result.
        assert_eq!(measure.bytes, 121);
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
