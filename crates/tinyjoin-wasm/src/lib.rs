#![deny(unreachable_pub)]

mod mem;
mod page_device;
mod structured;

use tinyjoin_core::{EngineError, ExecuteResult, PagedEngine, SchemaDefinition};
use wasm_bindgen::prelude::*;

use page_device::WasmPageDevice;

#[wasm_bindgen]
pub struct WasmEngine {
    engine: Option<PagedEngine<WasmPageDevice>>,
    poisoned: bool,
    /// Where a statement's result is written as a header, as [`structured::statement`] writes
    /// one. It is empty, and so holds no header, until the Worker asks where it is: an engine
    /// that nobody reads headers from writes none. It is then allocated once, so that its
    /// address holds for as long as the engine does.
    header: Box<[u8]>,
}

#[wasm_bindgen]
impl WasmEngine {
    #[wasm_bindgen(constructor)]
    pub fn new(device: JsValue) -> std::result::Result<WasmEngine, JsValue> {
        let device = WasmPageDevice::new(device).map_err(structured::constructor_error)?;
        let mut engine = PagedEngine::open(device).map_err(structured::constructor_error)?;
        // Rows are written out in field order, so they need not be built as objects first.
        engine.set_value_rows(true);
        Ok(Self {
            engine: Some(engine),
            poisoned: false,
            header: Box::default(),
        })
    }

    /// Where in the module's memory this engine writes a statement result's header, which the
    /// Worker asks once, and only when it can read that memory. Asking is what has the engine
    /// write headers at all: until then it answers every statement in JSON text, so that
    /// whatever calls it without reading its memory, as a tool that wraps it may, is never
    /// answered with a header it cannot see.
    ///
    /// The engine is borrowed as a call borrows it, which adds no glue of its own to the
    /// module, where a shared borrow would.
    #[wasm_bindgen(js_name = resultHeader)]
    pub fn result_header(&mut self) -> *const u8 {
        if self.header.is_empty() {
            self.header = vec![structured::NO_HEADER; structured::HEADER_BYTES].into_boxed_slice();
        }
        self.header.as_ptr()
    }

    /// The module's memory, in which the Worker reads a statement result's header. It comes
    /// from the engine, rather than from whoever loaded the module, so that every engine can be
    /// read, however it was made.
    pub fn memory(&mut self) -> JsValue {
        wasm_bindgen::memory()
    }

    /// Executes one versioned request, written as [`structured::Request`] reads it. A failure is
    /// a response too, so this returns no `Result`, whose glue would read each call's outcome back
    /// through the shadow stack.
    ///
    /// A response is JSON text, as [`structured`] writes it, with one exception, which an
    /// engine makes once it has been asked for [`Self::result_header`]. The result of a
    /// statement that published nothing, where it has the shape [`structured::statement`]
    /// describes, is written as a header there instead, and the call returns `true`, or for a
    /// `SELECT` the result's fields and rows alone as JSON text. The header's first byte says
    /// whether there is one: every call clears it before doing anything else, and only such a
    /// result sets it, as the last thing its call does. So a header never outlives its call, a
    /// failure never leaves one, and none is ever read for a result that published.
    #[wasm_bindgen(js_name = callStructured)]
    pub fn call_structured(
        &mut self,
        bridge_version: u32,
        operation: u32,
        payload: &[u8],
    ) -> JsValue {
        if let Some(first) = self.header.first_mut() {
            *first = structured::NO_HEADER;
        }
        match self.call_structured_inner(bridge_version, operation, payload) {
            Ok(response) => response,
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
        payload: &[u8],
    ) -> tinyjoin_core::Result<JsValue> {
        if bridge_version != structured::VERSION {
            return Err(EngineError::new(
                "INVALID_BRIDGE_VALUE",
                "Invalid structured bridge request",
            ));
        }
        let mut request = structured::Request::new(payload);
        if operation == structured::OP_CLOSE {
            request.finish()?;
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
        // Every other operation runs on the engine, which is borrowed once for it, with one
        // check that it is there to borrow. One that is closed or poisoned so refuses a request
        // before reading it.
        let engine = self.engine()?;
        match operation {
            structured::OP_EXECUTE_SQL => {
                let array_rows = request.flag()?;
                let sql = request.string()?;
                let params = request.values()?;
                request.finish()?;
                let was_in_transaction = engine.in_transaction();
                let previous_revision = engine.revision();
                let result = engine.execute_sql_rows(sql, &params, array_rows)?;
                let committed = !was_in_transaction && result.revision != previous_revision;
                self.statement_response(&result, committed, array_rows)
            }
            structured::OP_EXEC_SQL => {
                let array_rows = request.flag()?;
                let sql = request.string()?;
                request.finish()?;
                let was_in_transaction = engine.in_transaction();
                let previous_revision = engine.revision();
                let results = engine.exec_sql_rows(sql, array_rows)?;
                let committed = !was_in_transaction && engine.revision() != previous_revision;
                self.encode_committed(
                    structured::execute_results(&results, committed, array_rows),
                    committed,
                )
            }
            structured::OP_PREPARE_SQL => {
                let sql = request.string()?;
                request.finish()?;
                let id = engine.prepare_sql(sql)?;
                structured::prepared_statement_id(id)
            }
            structured::OP_EXECUTE_PREPARED => {
                let array_rows = request.flag()?;
                let statement_id = request.u32()?;
                let params = request.values()?;
                request.finish()?;
                let was_in_transaction = engine.in_transaction();
                let previous_revision = engine.revision();
                let result = engine.execute_prepared_rows(statement_id, &params, array_rows)?;
                let committed = !was_in_transaction && result.revision != previous_revision;
                self.statement_response(&result, committed, array_rows)
            }
            structured::OP_CLOSE_PREPARED => {
                let id = request.u32()?;
                request.finish()?;
                engine.close_prepared(id)?;
                structured::unit(false)
            }
            structured::OP_BEGIN => {
                request.finish()?;
                engine.begin_transaction()?;
                structured::unit(false)
            }
            structured::OP_COMMIT => {
                request.finish()?;
                let previous_revision = engine.revision();
                let outcome = engine.commit_transaction()?;
                let committed = outcome.revision != previous_revision;
                self.encode_committed(structured::apply_outcome(&outcome, committed), committed)
            }
            structured::OP_ROLLBACK => {
                request.finish()?;
                engine.rollback_transaction()?;
                structured::unit(false)
            }
            structured::OP_IN_TRANSACTION => {
                request.finish()?;
                structured::boolean(engine.in_transaction())
            }
            structured::OP_REVISION => {
                request.finish()?;
                let revision = engine.revision();
                engine.ensure_readiness()?;
                structured::unsigned(revision)
            }
            structured::OP_CHECK => {
                request.finish()?;
                engine.check()?;
                structured::unit(false)
            }
            structured::OP_SCHEMA => {
                request.finish()?;
                structured::schema(engine.schema_version(), &engine.schema()?)
            }
            structured::OP_SET_SCHEMA => {
                let drop = request.flag()?;
                let schema = request.values()?;
                request.finish()?;
                let [schema] = schema.as_slice() else {
                    return Err(EngineError::new(
                        "INVALID_BRIDGE_VALUE",
                        "Invalid structured bridge request",
                    ));
                };
                let target = SchemaDefinition::from_json(schema)?;
                let previous_revision = engine.revision();
                let outcome = engine.set_schema(&target, drop)?;
                let committed = outcome.revision != previous_revision;
                self.encode_committed(structured::apply_outcome(&outcome, committed), committed)
            }
            _ => Err(EngineError::new(
                "INVALID_BRIDGE_VALUE",
                "Invalid structured bridge request",
            )),
        }
    }

    /// The engine, for an operation to run on: there is none once it is poisoned, or closed.
    fn engine(&mut self) -> tinyjoin_core::Result<&mut PagedEngine<WasmPageDevice>> {
        if self.poisoned {
            return Err(EngineError::new(
                "STORAGE_ENGINE_POISONED",
                "The TinyJoin engine cannot be used after an uncertain result",
            )
            .with_retryable(false));
        }
        match &mut self.engine {
            Some(engine) => Ok(engine),
            None => Err(EngineError::new(
                "ENGINE_CLOSED",
                "The TinyJoin engine is closed",
            )),
        }
    }

    /// Answers a statement with its result: as a header where the result published nothing and
    /// has the shape of one, and as JSON text otherwise.
    fn statement_response(
        &mut self,
        result: &ExecuteResult,
        committed: bool,
        array_rows: bool,
    ) -> tinyjoin_core::Result<JsValue> {
        if !committed
            && let Some(response) = structured::statement(result, array_rows, &mut self.header)?
        {
            return Ok(response);
        }
        self.encode_committed(
            structured::execute_result(result, committed, array_rows),
            committed,
        )
    }

    fn encode_committed<T>(
        &mut self,
        encoded: tinyjoin_core::Result<T>,
        committed: bool,
    ) -> tinyjoin_core::Result<T> {
        match encoded {
            Ok(response) => Ok(response),
            Err(error) if committed => self.poison_after_commit(error),
            Err(error) => Err(error),
        }
    }

    fn poison_after_commit<T>(&mut self, error: EngineError) -> tinyjoin_core::Result<T> {
        self.poison_and_close();
        Err(EngineError::new(
            "STORAGE_COMMIT_OUTCOME_UNKNOWN",
            format!(
                "The database mutation may have committed, but TinyJoin could not encode its result: {}",
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
    fn a_header_has_a_place_only_once_it_is_asked_for() {
        let mut engine = WasmEngine {
            engine: None,
            poisoned: false,
            header: Box::default(),
        };
        // Until then there is nowhere to write one, which is what leaves every result as JSON.
        assert!(engine.header.is_empty());
        let place = engine.result_header();
        assert_eq!(engine.header.len(), structured::HEADER_BYTES);
        assert!(
            engine
                .header
                .iter()
                .all(|byte| *byte == structured::NO_HEADER)
        );
        // Asked again, it is where it was, with what it held.
        engine.header[0] = 3;
        assert_eq!(engine.result_header(), place);
        assert_eq!(engine.header.as_ptr(), place);
        assert_eq!(engine.header[0], 3);
    }

    #[test]
    fn an_engine_that_is_poisoned_or_closed_runs_nothing() {
        let mut engine = WasmEngine {
            engine: None,
            poisoned: false,
            header: Box::default(),
        };
        let closed = engine.engine().map(drop).unwrap_err();
        assert_eq!(
            (closed.code.as_str(), closed.retryable),
            ("ENGINE_CLOSED", None)
        );
        // One that is poisoned says so, though poisoning closed it too.
        engine.poisoned = true;
        let poisoned = engine.engine().map(drop).unwrap_err();
        assert_eq!(
            (poisoned.code.as_str(), poisoned.retryable),
            ("STORAGE_ENGINE_POISONED", Some(false))
        );
    }

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
