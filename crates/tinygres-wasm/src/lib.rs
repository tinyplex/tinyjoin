mod page_device;
mod structured;

use tinygres_core::{EngineError, PagedEngine};
use wasm_bindgen::prelude::*;

pub use page_device::WasmPageDevice;

#[wasm_bindgen]
pub struct WasmEngine {
    engine: Option<PagedEngine<WasmPageDevice>>,
    poisoned: bool,
}

#[wasm_bindgen]
impl WasmEngine {
    #[wasm_bindgen(constructor)]
    pub fn new(device: JsValue) -> std::result::Result<WasmEngine, JsValue> {
        let device = WasmPageDevice::new(device).map_err(structured::constructor_error)?;
        let engine = PagedEngine::open(device).map_err(structured::constructor_error)?;
        Ok(Self {
            engine: Some(engine),
            poisoned: false,
        })
    }

    /// Executes one versioned structured-clone request.
    #[wasm_bindgen(js_name = callStructured)]
    pub fn call_structured(
        &mut self,
        bridge_version: u32,
        operation: u32,
        payload: JsValue,
    ) -> std::result::Result<JsValue, JsValue> {
        match self.call_structured_inner(bridge_version, operation, payload) {
            Ok(response) => Ok(response),
            Err(error) => {
                if fatal_storage_error(&error) {
                    self.poison_and_close();
                }
                structured::error(&error)
            }
        }
    }
}

impl WasmEngine {
    fn call_structured_inner(
        &mut self,
        bridge_version: u32,
        operation: u32,
        payload: JsValue,
    ) -> tinygres_core::Result<JsValue> {
        if bridge_version != structured::VERSION {
            return Err(EngineError::new(
                "INVALID_BRIDGE_VALUE",
                "Invalid structured bridge request",
            ));
        }
        if operation == structured::OP_CLOSE {
            structured::unit_payload(&payload)?;
            let result = match self.engine.take() {
                Some(engine) => engine
                    .into_device()
                    .close()
                    .and_then(|()| structured::unit(false)),
                None => structured::unit(false),
            };
            self.poisoned = false;
            return result;
        }
        self.ensure_available()?;
        match operation {
            structured::OP_EXECUTE_SQL => {
                let request: structured::ExecuteSqlRequest = structured::decode(payload)?;
                let was_in_transaction = self.engine()?.in_transaction();
                let previous_revision = self.engine()?.revision();
                let result = self
                    .engine_mut()?
                    .execute_sql(&request.sql, &request.params)?;
                let committed = !was_in_transaction && result.revision != previous_revision;
                self.encode_committed(structured::execute_result(&result, committed), committed)
            }
            structured::OP_EXEC_SQL => {
                let sql: String = structured::decode(payload)?;
                let was_in_transaction = self.engine()?.in_transaction();
                let previous_revision = self.engine()?.revision();
                let results = self.engine_mut()?.exec_sql(&sql)?;
                let committed =
                    !was_in_transaction && self.engine()?.revision() != previous_revision;
                self.encode_committed(structured::execute_results(&results, committed), committed)
            }
            structured::OP_PREPARE_SQL => {
                let sql: String = structured::decode(payload)?;
                let id = self.engine_mut()?.prepare_sql(&sql)?;
                structured::prepared_statement_id(id)
            }
            structured::OP_EXECUTE_PREPARED => {
                let request: structured::ExecutePreparedRequest = structured::decode(payload)?;
                let was_in_transaction = self.engine()?.in_transaction();
                let previous_revision = self.engine()?.revision();
                let result = self
                    .engine_mut()?
                    .execute_prepared(request.statement_id, &request.params)?;
                let committed = !was_in_transaction && result.revision != previous_revision;
                self.encode_committed(structured::execute_result(&result, committed), committed)
            }
            structured::OP_CLOSE_PREPARED => {
                let id: u32 = structured::decode(payload)?;
                self.engine_mut()?.close_prepared(id)?;
                structured::unit(false)
            }
            structured::OP_BEGIN => {
                structured::unit_payload(&payload)?;
                self.engine_mut()?.begin_transaction()?;
                structured::unit(false)
            }
            structured::OP_COMMIT => {
                structured::unit_payload(&payload)?;
                let previous_revision = self.engine()?.revision();
                let outcome = self.engine_mut()?.commit_transaction()?;
                let committed = outcome.revision != previous_revision;
                self.encode_committed(structured::apply_outcome(&outcome, committed), committed)
            }
            structured::OP_ROLLBACK => {
                structured::unit_payload(&payload)?;
                self.engine_mut()?.rollback_transaction()?;
                structured::unit(false)
            }
            structured::OP_IN_TRANSACTION => {
                structured::unit_payload(&payload)?;
                structured::boolean(self.engine()?.in_transaction())
            }
            structured::OP_REVISION => {
                structured::unit_payload(&payload)?;
                let revision = self.engine()?.revision();
                self.engine()?.ensure_readiness()?;
                structured::unsigned(revision)
            }
            _ => Err(EngineError::new(
                "INVALID_BRIDGE_VALUE",
                "Invalid structured bridge request",
            )),
        }
    }

    fn ensure_available(&self) -> tinygres_core::Result<()> {
        if self.poisoned {
            return Err(EngineError::new(
                "STORAGE_ENGINE_POISONED",
                "The TinyGres engine cannot be used after an uncertain result",
            )
            .with_retryable(false));
        }
        if self.engine.is_none() {
            return Err(EngineError::new(
                "ENGINE_CLOSED",
                "The TinyGres engine is closed",
            ));
        }
        Ok(())
    }

    fn engine(&self) -> tinygres_core::Result<&PagedEngine<WasmPageDevice>> {
        self.ensure_available()?;
        Ok(self.engine.as_ref().expect("availability checked"))
    }

    fn engine_mut(&mut self) -> tinygres_core::Result<&mut PagedEngine<WasmPageDevice>> {
        self.ensure_available()?;
        Ok(self.engine.as_mut().expect("availability checked"))
    }

    fn encode_committed<T>(
        &mut self,
        encoded: tinygres_core::Result<T>,
        committed: bool,
    ) -> tinygres_core::Result<T> {
        match encoded {
            Ok(response) => Ok(response),
            Err(error) if committed => self.poison_after_commit(error),
            Err(error) => Err(error),
        }
    }

    fn poison_after_commit<T>(&mut self, error: EngineError) -> tinygres_core::Result<T> {
        self.poison_and_close();
        Err(EngineError::new(
            "STORAGE_COMMIT_OUTCOME_UNKNOWN",
            format!(
                "The database mutation may have committed, but TinyGres could not encode its result: {}",
                error.message
            ),
        )
        .with_retryable(false))
    }

    fn poison_and_close(&mut self) {
        self.poisoned = true;
        if let Some(engine) = self.engine.take() {
            let _ = engine.into_device().close();
        }
    }
}

fn fatal_storage_error(error: &EngineError) -> bool {
    matches!(
        error.code.as_str(),
        "RECOVERY_REQUIRED" | "STORAGE_COMMIT_OUTCOME_UNKNOWN" | "STORAGE_ENGINE_POISONED"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fatal_storage_outcomes_poison_the_raw_engine() {
        for code in [
            "RECOVERY_REQUIRED",
            "STORAGE_COMMIT_OUTCOME_UNKNOWN",
            "STORAGE_ENGINE_POISONED",
        ] {
            assert!(fatal_storage_error(&EngineError::new(code, "fatal")));
        }
        assert!(!fatal_storage_error(&EngineError::new(
            "PAGE_DEVICE_ERROR",
            "ordinary failure",
        )));
    }
}
