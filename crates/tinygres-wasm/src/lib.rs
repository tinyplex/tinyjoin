use serde::Serialize;
use tinygres_core::{ChangeBatch, Engine, EngineError, QueryPlan, Row};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct WasmEngine {
    engine: Engine,
}

#[wasm_bindgen]
impl WasmEngine {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            engine: Engine::default(),
        }
    }

    pub fn define_table(&mut self, schema: JsValue) -> std::result::Result<(), JsValue> {
        self.engine
            .define_table(from_js(schema)?)
            .map_err(error_to_js)
    }

    pub fn replace_table(
        &mut self,
        table: &str,
        rows: JsValue,
    ) -> std::result::Result<JsValue, JsValue> {
        let rows: Vec<Row> = from_js(rows)?;
        let outcome = self
            .engine
            .replace_table(table, rows)
            .map_err(error_to_js)?;
        to_js(&outcome)
    }

    pub fn apply_batch(&mut self, batch: JsValue) -> std::result::Result<JsValue, JsValue> {
        let batch: ChangeBatch = from_js(batch)?;
        let outcome = self.engine.apply_batch(&batch).map_err(error_to_js)?;
        to_js(&outcome)
    }

    pub fn query(&self, plan: JsValue) -> std::result::Result<JsValue, JsValue> {
        let plan: QueryPlan = from_js(plan)?;
        let result = self.engine.query(&plan).map_err(error_to_js)?;
        to_js(&result)
    }

    pub fn query_sql(&self, sql: &str, params: JsValue) -> std::result::Result<JsValue, JsValue> {
        let params: Vec<serde_json::Value> = from_js(params)?;
        let result = self.engine.query_sql(sql, &params).map_err(error_to_js)?;
        to_js(&result)
    }

    pub fn revision(&self) -> u64 {
        self.engine.revision()
    }

    pub fn export_snapshot(&self) -> std::result::Result<Vec<u8>, JsValue> {
        self.engine.export_snapshot().map_err(error_to_js)
    }

    pub fn import_snapshot(&mut self, bytes: &[u8]) -> std::result::Result<(), JsValue> {
        self.engine.import_snapshot(bytes).map_err(error_to_js)
    }
}

impl Default for WasmEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn from_js<T: serde::de::DeserializeOwned>(value: JsValue) -> std::result::Result<T, JsValue> {
    serde_wasm_bindgen::from_value(value)
        .map_err(|error| error_to_js(EngineError::new("INVALID_BRIDGE_VALUE", error.to_string())))
}

fn to_js<T: Serialize>(value: &T) -> std::result::Result<JsValue, JsValue> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|error| {
            error_to_js(EngineError::new(
                "BRIDGE_SERIALIZATION_ERROR",
                error.to_string(),
            ))
        })
}

fn error_to_js(error: EngineError) -> JsValue {
    error
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .unwrap_or_else(|_| JsValue::from_str(&error.to_string()))
}
