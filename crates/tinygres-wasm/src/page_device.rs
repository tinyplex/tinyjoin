use js_sys::Reflect;
use tinygres_core::{EngineError, MAX_PAGE_COUNT, PAGE_SIZE, PageDevice, PageId, Result};
use wasm_bindgen::{JsCast, prelude::*};

#[wasm_bindgen]
extern "C" {
    /// The synchronous JavaScript page-device contract implemented by the
    /// worker's OPFS and in-memory devices.
    ///
    /// These methods are structural deliberately: applications pass an
    /// ordinary JavaScript object rather than an object created by wasm-bindgen.
    #[wasm_bindgen(typescript_type = "PageDevice")]
    type RawPageDevice;

    #[wasm_bindgen(method, structural, catch, js_name = pageCount)]
    fn raw_page_count(this: &RawPageDevice) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(method, structural, catch, js_name = readPage)]
    fn raw_read_page(
        this: &RawPageDevice,
        page_id_low: u32,
        page_id_high: u32,
        destination: &mut [u8],
    ) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(method, structural, catch, js_name = writePage)]
    fn raw_write_page(
        this: &RawPageDevice,
        page_id_low: u32,
        page_id_high: u32,
        source: &[u8],
    ) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(method, structural, catch, js_name = flush)]
    fn raw_flush(this: &RawPageDevice) -> std::result::Result<(), JsValue>;

    #[wasm_bindgen(method, structural, catch, js_name = close)]
    fn raw_close(this: &RawPageDevice) -> std::result::Result<(), JsValue>;
}

/// A Rust [`PageDevice`] backed by the worker's synchronous JavaScript
/// `PageDevice` implementation.
///
/// The adapter takes exclusive logical ownership of the device. JavaScript
/// must not append or truncate pages behind it: Rust's `PageDevice::page_count`
/// cannot report failures, so the count is validated once and then maintained
/// after successful dense appends.
///
/// Reads and writes pass borrowed views of WebAssembly memory to JavaScript.
/// The JavaScript method must consume each `Uint8Array` synchronously and must
/// never retain it after returning.
pub(crate) struct WasmPageDevice {
    device: RawPageDevice,
    page_count: PageId,
    closed: bool,
}

impl WasmPageDevice {
    /// Validates a JavaScript page device and captures its initial page count.
    pub(crate) fn new(device: JsValue) -> Result<Self> {
        let device = device.unchecked_into::<RawPageDevice>();
        let page_count = device
            .raw_page_count()
            .map_err(|error| js_device_error(error, "TinyGres could not read the page count"))
            .and_then(parse_page_count);
        let page_count = match page_count {
            Ok(page_count) => page_count,
            Err(error) => {
                // Construction takes ownership immediately. Preserve the open
                // error if cleanup also fails; it is the useful diagnosis.
                let _ = device.raw_close();
                return Err(error);
            }
        };
        Ok(Self {
            device,
            page_count,
            closed: false,
        })
    }

    /// Closes the JavaScript page device and reports a close failure.
    ///
    /// Consuming `self` prevents further I/O. `Drop` still runs afterwards but
    /// the close call is issued exactly once.
    pub(crate) fn close(mut self) -> Result<()> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        // Mark closed before entering JavaScript. If close throws, retrying
        // during Drop could invoke an access handle whose outcome is unknown.
        self.closed = true;
        self.device
            .raw_close()
            .map_err(|error| js_device_error(error, "TinyGres could not close its page device"))
    }
}

impl Drop for WasmPageDevice {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

impl PageDevice for WasmPageDevice {
    fn page_count(&self) -> PageId {
        self.page_count
    }

    fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
        validate_buffer(destination.len())?;
        if id >= self.page_count {
            return Err(out_of_range(id, self.page_count));
        }
        let (low, high) = split_page_id(id);
        let transferred = self
            .device
            .raw_read_page(low, high, destination)
            .map_err(|error| js_device_error(error, "TinyGres could not read a database page"))?;
        validate_transfer_count(transferred, "read")
    }

    fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
        validate_buffer(source.len())?;
        if id >= MAX_PAGE_COUNT {
            return Err(EngineError::new(
                "STORAGE_DATABASE_TOO_LARGE",
                format!("The TinyGres database cannot exceed {MAX_PAGE_COUNT} pages"),
            ));
        }
        if id > self.page_count {
            return Err(out_of_range(id, self.page_count));
        }
        let appending = id == self.page_count;
        let (low, high) = split_page_id(id);
        let transferred = self
            .device
            .raw_write_page(low, high, source)
            .map_err(|error| js_device_error(error, "TinyGres could not write a database page"))?;

        // A conforming JS device has completed the page write before it
        // returns. Keep the cached count coherent even if its byte-count return
        // value violates the bridge contract; callers must treat that error as
        // fatal and reopen the database.
        if appending {
            self.page_count += 1;
        }
        validate_write_transfer_count(transferred)
    }

    fn flush(&mut self) -> Result<()> {
        self.device
            .raw_flush()
            .map_err(|error| js_device_error(error, "TinyGres could not flush database pages"))
    }
}

fn parse_page_count(value: JsValue) -> Result<PageId> {
    let Some(value) = value.as_f64() else {
        return Err(device_error(
            "JavaScript PageDevice.pageCount() must return a number",
        ));
    };
    validate_page_count(value)
}

fn validate_page_count(value: f64) -> Result<PageId> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > MAX_PAGE_COUNT as f64 {
        return Err(device_error(format!(
            "JavaScript PageDevice.pageCount() returned {value}; expected an integer between 0 and {MAX_PAGE_COUNT}",
        )));
    }
    Ok(value as PageId)
}

fn validate_transfer_count(value: JsValue, operation: &str) -> Result<()> {
    if value.as_f64() == Some(PAGE_SIZE as f64) {
        return Ok(());
    }
    Err(device_error(format!(
        "JavaScript PageDevice.{operation}Page() must return exactly {PAGE_SIZE}",
    )))
}

fn validate_write_transfer_count(value: JsValue) -> Result<()> {
    if value.as_f64() == Some(PAGE_SIZE as f64) {
        return Ok(());
    }
    Err(invalid_write_transfer_count())
}

fn invalid_write_transfer_count() -> EngineError {
    EngineError::new(
        "STORAGE_COMMIT_OUTCOME_UNKNOWN",
        format!(
            "JavaScript PageDevice.writePage() returned an invalid byte count; expected exactly {PAGE_SIZE}",
        ),
    )
    .with_retryable(false)
}

fn validate_buffer(length: usize) -> Result<()> {
    if length != PAGE_SIZE {
        return Err(device_error(format!(
            "Page device buffers must be exactly {PAGE_SIZE} bytes, not {length}",
        )));
    }
    Ok(())
}

fn split_page_id(id: PageId) -> (u32, u32) {
    (id as u32, (id >> 32) as u32)
}

fn out_of_range(id: PageId, page_count: PageId) -> EngineError {
    device_error(format!(
        "Page ID {id} is outside a device containing {page_count} pages",
    ))
}

fn device_error(message: impl Into<String>) -> EngineError {
    EngineError::new("PAGE_DEVICE_ERROR", message)
}

fn js_device_error(error: JsValue, fallback_message: &str) -> EngineError {
    let code = string_property(&error, "code").unwrap_or_else(|| "PAGE_DEVICE_ERROR".into());
    let message = string_property(&error, "message")
        .or_else(|| error.as_string())
        .unwrap_or_else(|| fallback_message.into());
    with_retryability(
        EngineError::new(code, message),
        boolean_property(&error, "retryable"),
    )
}

fn string_property(value: &JsValue, property: &str) -> Option<String> {
    Reflect::get(value, &JsValue::from_str(property))
        .ok()
        .and_then(|value| value.as_string())
        .filter(|value| !value.is_empty())
}

fn boolean_property(value: &JsValue, property: &str) -> Option<bool> {
    Reflect::get(value, &JsValue::from_str(property))
        .ok()
        .and_then(|value| value.as_bool())
}

fn with_retryability(error: EngineError, retryable: Option<bool>) -> EngineError {
    match retryable {
        Some(retryable) => error.with_retryable(retryable),
        None => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_count_validation_accepts_only_the_bounded_integer_domain() {
        assert_eq!(validate_page_count(0.0).unwrap(), 0);
        assert_eq!(
            validate_page_count(MAX_PAGE_COUNT as f64).unwrap(),
            MAX_PAGE_COUNT,
        );
        for invalid in [
            -1.0,
            0.5,
            MAX_PAGE_COUNT as f64 + 1.0,
            f64::INFINITY,
            f64::NAN,
        ] {
            assert_eq!(
                validate_page_count(invalid).unwrap_err().code,
                "PAGE_DEVICE_ERROR"
            );
        }
    }

    #[test]
    fn page_ids_are_split_without_losing_high_words() {
        assert_eq!(split_page_id(0), (0, 0));
        assert_eq!(split_page_id(u32::MAX as u64 + 2), (1, 1));
    }

    #[test]
    fn bridge_buffer_validation_matches_physical_pages() {
        assert!(validate_buffer(PAGE_SIZE).is_ok());
        assert_eq!(
            validate_buffer(PAGE_SIZE - 1).unwrap_err().code,
            "PAGE_DEVICE_ERROR",
        );
    }

    #[test]
    fn native_bridge_helper_preserves_only_explicit_retryability() {
        for retryable in [None, Some(false), Some(true)] {
            let error = with_retryability(
                EngineError::new("STORAGE_READ_FAILED", "read failed"),
                retryable,
            );
            assert_eq!(error.retryable, retryable);
        }
    }

    #[test]
    fn invalid_write_counts_are_explicitly_fatal() {
        let error = invalid_write_transfer_count();
        assert_eq!(error.code, "STORAGE_COMMIT_OUTCOME_UNKNOWN");
        assert_eq!(error.retryable, Some(false));
    }
}
