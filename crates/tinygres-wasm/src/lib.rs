mod page_device;
mod wire;

use js_sys::Uint8Array;
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
        let device = WasmPageDevice::new(device).map_err(constructor_error)?;
        let engine = PagedEngine::open(device).map_err(constructor_error)?;
        Ok(Self {
            engine: Some(engine),
            poisoned: false,
        })
    }

    /// Executes one versioned binary request and always returns a binary success/error envelope.
    pub fn call(&mut self, operation: u32, payload: &[u8]) -> Vec<u8> {
        match self.call_inner(operation, payload) {
            Ok(response) => response,
            Err(error) => {
                if fatal_storage_error(&error) {
                    self.poison_and_close();
                }
                wire::error(&error)
            }
        }
    }
}

impl WasmEngine {
    fn call_inner(&mut self, operation: u32, payload: &[u8]) -> tinygres_core::Result<Vec<u8>> {
        if operation == wire::OP_CLOSE {
            let reader = wire::Reader::new(payload)?;
            reader.finish()?;
            let result = match self.engine.take() {
                Some(engine) => engine.into_device().close().map(|()| wire::unit(false)),
                None => Ok(wire::unit(false)),
            };
            self.poisoned = false;
            return result;
        }
        self.ensure_available()?;
        let mut reader = wire::Reader::new(payload)?;
        match operation {
            wire::OP_DEFINE_TABLES => {
                let schemas = reader.schemas()?;
                reader.finish()?;
                let committed = self.engine_mut()?.define_tables_with_publication(schemas)?;
                Ok(wire::unit(committed))
            }
            wire::OP_REPLACE_SNAPSHOT => {
                let schema = reader.schema()?;
                let rows = reader.rows()?;
                reader.finish()?;
                let previous_revision = self.engine()?.revision();
                let outcome = self.engine_mut()?.replace_table_snapshot(schema, rows)?;
                let committed = outcome.revision != previous_revision;
                self.encode_committed(wire::apply_outcome(&outcome, committed), committed)
            }
            wire::OP_APPLY_BATCH => {
                let batch = reader.batch()?;
                reader.finish()?;
                let previous_revision = self.engine()?.revision();
                let outcome = self.engine_mut()?.apply_batch(&batch)?;
                drop(batch);
                let committed = outcome.revision != previous_revision;
                self.encode_committed(wire::apply_outcome(&outcome, committed), committed)
            }
            wire::OP_QUERY => {
                let plan = reader.query()?;
                reader.finish()?;
                let result = self.engine()?.query(&plan)?;
                drop(plan);
                wire::query_result(&result, false)
            }
            wire::OP_EXECUTE_SQL => {
                let sql = reader.string()?;
                let params = reader.values(0)?;
                reader.finish()?;
                let was_in_transaction = self.engine()?.in_transaction();
                let previous_revision = self.engine()?.revision();
                let result = self.engine_mut()?.execute_sql(&sql, &params)?;
                drop((sql, params));
                let committed = !was_in_transaction && result.revision != previous_revision;
                match wire::execute_result(&result, committed) {
                    Ok(response) => Ok(response),
                    Err(error) if !committed => Err(error),
                    Err(error) => self.poison_after_commit(error),
                }
            }
            wire::OP_EXEC_SQL => {
                let sql = reader.string()?;
                reader.finish()?;
                let was_in_transaction = self.engine()?.in_transaction();
                let previous_revision = self.engine()?.revision();
                let results = self.engine_mut()?.exec_sql(&sql)?;
                drop(sql);
                let committed =
                    !was_in_transaction && self.engine()?.revision() != previous_revision;
                match wire::execute_results(&results, committed) {
                    Ok(response) => Ok(response),
                    Err(error) if !committed => Err(error),
                    Err(error) => self.poison_after_commit(error),
                }
            }
            wire::OP_BEGIN => {
                reader.finish()?;
                self.engine_mut()?.begin_transaction()?;
                Ok(wire::unit(false))
            }
            wire::OP_COMMIT => {
                reader.finish()?;
                let previous_revision = self.engine()?.revision();
                let outcome = self.engine_mut()?.commit_transaction()?;
                let committed = outcome.revision != previous_revision;
                self.encode_committed(wire::apply_outcome(&outcome, committed), committed)
            }
            wire::OP_ROLLBACK => {
                reader.finish()?;
                self.engine_mut()?.rollback_transaction()?;
                Ok(wire::unit(false))
            }
            wire::OP_IN_TRANSACTION => {
                reader.finish()?;
                Ok(wire::boolean(self.engine()?.in_transaction()))
            }
            wire::OP_REVISION => {
                reader.finish()?;
                let revision = self.engine()?.revision();
                self.engine()?.ensure_readiness()?;
                Ok(wire::unsigned(revision))
            }
            _ => Err(EngineError::new(
                "INVALID_BRIDGE_VALUE",
                "Invalid binary bridge request",
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

    fn encode_committed(
        &mut self,
        encoded: tinygres_core::Result<Vec<u8>>,
        committed: bool,
    ) -> tinygres_core::Result<Vec<u8>> {
        match encoded {
            Ok(response) => Ok(response),
            Err(error) if committed => self.poison_after_commit(error),
            Err(error) => Err(error),
        }
    }

    fn poison_after_commit(&mut self, error: EngineError) -> tinygres_core::Result<Vec<u8>> {
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

fn constructor_error(error: EngineError) -> JsValue {
    Uint8Array::from(wire::error(&error).as_slice()).into()
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
