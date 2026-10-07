use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use serde_json::{Map, Number, Value};
use tinyjoin_core::{
    ApplyOutcome, ChangedKeys, ColumnType, EngineError, ExecuteResult, IndexDefinition,
    MAX_CHANGED_KEYS_PER_TABLE, Result, TableDefinition, TableKeys,
};
use wasm_bindgen::JsValue;

pub(crate) const VERSION: u32 = 6;

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

    #[inline(always)]
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        // Matched, rather than mapped through `Option`'s methods, which this build leaves as
        // calls: every statement takes its bytes a dozen times, and nearly always has them.
        match self.at.checked_add(count) {
            Some(end) if end <= self.bytes.len() => {
                let bytes = &self.bytes[self.at..end];
                self.at = end;
                Ok(bytes)
            }
            _ => Err(invalid()),
        }
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
    #[inline(always)]
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
            // Both kinds of number are read in the one place, which leaves reading a number
            // no call of its own.
            tag @ (TAG_INTEGER | TAG_FLOAT) => {
                let value = self.f64()?;
                if tag == TAG_INTEGER {
                    if value.fract() != 0.0 || value.abs() > MAX_SAFE_INTEGER as f64 {
                        return Err(invalid());
                    }
                    Value::from(value as i64)
                } else {
                    let Some(number) = Number::from_f64(value) else {
                        return Err(invalid());
                    };
                    Value::Number(number)
                }
            }
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

/// How many bytes an engine keeps for a statement result's header. A table reports at most a
/// thousand keys, which of one integer column take nine thousand bytes, so a write of any size to
/// such a table has room for them beside the names of the table and the column.
pub(crate) const HEADER_BYTES: usize = 16 * 1024;

/// A header's first byte while it holds no result.
pub(crate) const NO_HEADER: u8 = 0;

/// A header's first byte for a `SELECT`: as for each command, one more than its number.
const SELECT_HEADER: u8 = 4;

/// The bytes of a header that every result has, before the table a write changed.
const HEADER_FIXED_BYTES: usize = 16;

/// The most bytes of a header's text that the Worker reads as character codes, where they are
/// all ASCII. Reading a short name that way costs it less than a decoder does.
const PLAIN_TEXT_BYTES: usize = 64;

/// Set in the length of a header's text that the Worker is to decode as UTF-8, being longer
/// than that or not all ASCII. No text a header holds is long enough to set it otherwise.
const DECODED_TEXT: u16 = 0x8000;

/// The bytes a `SELECT`'s JSON response takes before its fields and rows, apart from the digits
/// of its revision and row count: the envelope, the header of a result that names no table and
/// no key, and the line break.
const SELECT_PREAMBLE_BYTES: usize =
    r#"[6,0,0,{"command":"SELECT","revision":,"rowCount":,"tables":[],"keys":{}}]"#.len() + 1;

/// The commands a header can name, each as its six letters read as one number, which compares
/// in a step where comparing the names as strings is a call for each name tried.
const INSERT: u64 = command_word(b"INSERT");
const UPDATE: u64 = command_word(b"UPDATE");
const DELETE: u64 = command_word(b"DELETE");
const SELECT: u64 = command_word(b"SELECT");

const fn command_word(name: &[u8; 6]) -> u64 {
    u64::from_le_bytes([name[0], name[1], name[2], name[3], name[4], name[5], 0, 0])
}

/// Writes the result of a statement that published nothing as a header in `out`, and returns
/// what its call then answers: `true`, or for a `SELECT` its fields and rows as JSON text.
/// Returns `None`, leaving `out` without a header, for a result of any other shape, which is
/// answered as JSON text whole.
///
/// A header says in a few bytes what the header of a JSON response says in text, so that the
/// Worker reads a statement's result out of the module's memory, with nothing to decode and
/// nothing to parse. It holds a `SELECT` that names no table and no key, and an `INSERT`,
/// `UPDATE` or `DELETE` without fields or rows that changed at most one table, whose keys are
/// the only keys reported and are all scalars. Any other result, and one whose header would not
/// fit, or whose revision or row count is more than a u32 holds, has not the shape of one.
///
/// Its numbers are little-endian. The first byte is the command's number plus one (`INSERT` 1,
/// `UPDATE` 2, `DELETE` 3, `SELECT` 4), which is zero while no header stands, as each call
/// leaves it on starting, and is written last. Then come the number of columns in the changed
/// table's primary key as a byte, zero where the table's keys are not reported; the number of
/// keys reported as a u16; the header's whole length as a u16; two bytes of zero; and the
/// revision and the row count as u32s, which are counts whatever their bits, so that the Worker
/// has nothing to check in them. A write that changed a table follows those 16 bytes with the
/// table's name, then the names of its key's columns, and then each key's values in the
/// columns' order, one key after another: each value tagged as a request's values are, a number
/// as its tag and the eight bytes of a float, a string as its tag and a text. A text is its
/// length in bytes as a u16, and then its UTF-8 bytes. The length's top bit is set unless the
/// text is plain, which is at most 64 bytes, all of them ASCII, so that the Worker knows
/// without looking at them that it may read those as character codes.
pub(crate) fn statement(
    result: &ExecuteResult,
    array_rows: bool,
    out: &mut [u8],
) -> Result<Option<JsValue>> {
    let kind = header(result, out)?;
    let response = match kind {
        NO_HEADER => return Ok(None),
        SELECT_HEADER => rows_text(result, array_rows)?.finish()?,
        _ => JsValue::TRUE,
    };
    // Marked only now, so that a result whose rows could not be written leaves no header.
    if let Some(first) = out.first_mut() {
        *first = kind;
    }
    Ok(Some(response))
}

/// Writes a result's header, all but its first byte, which is left as [`NO_HEADER`] and
/// returned as it is to be: [`NO_HEADER`] for a result that has not the shape of a header. A
/// number a response cannot carry is refused as the JSON writer refuses it.
fn header(result: &ExecuteResult, out: &mut [u8]) -> Result<u8> {
    match write_header(result, out) {
        Ok(kind) => Ok(kind),
        Err(Unwritten::Shape) => Ok(NO_HEADER),
        Err(Unwritten::Number) => Err(serialization()),
    }
}

/// Why a result's header is left unwritten.
enum Unwritten {
    /// It has not the shape of one, or one would not fit, so the JSON writer answers it.
    Shape,
    /// One of its keys is a number no response carries, which the JSON writer refuses too.
    Number,
}

fn write_header(result: &ExecuteResult, out: &mut [u8]) -> std::result::Result<u8, Unwritten> {
    let kind = match <[u8; 6]>::try_from(result.command.as_bytes()).map(|name| command_word(&name))
    {
        Ok(INSERT) => 1,
        Ok(UPDATE) => 2,
        Ok(DELETE) => 3,
        Ok(SELECT) => SELECT_HEADER,
        _ => return Err(Unwritten::Shape),
    };
    // What a result holds decides its shape before anything is written, the cheapest tests
    // first, so that a result which is answered in JSON costs little more than them.
    let select = kind == SELECT_HEADER;
    if !select
        && !(result.fields.is_empty()
            && result.rows.is_empty()
            && result
                .values
                .as_ref()
                .is_none_or(|values| values.rows.is_empty()))
    {
        return Err(Unwritten::Shape);
    }
    let changed = match (result.tables.as_slice(), result.keys.tables()) {
        ([], []) => None,
        ([table], []) if !select => Some((table, None)),
        ([table], [keys]) if !select && keys.schema.name == *table => Some((table, Some(keys))),
        _ => return Err(Unwritten::Shape),
    };
    // A revision or a row count past a u32, which no database reaches, is left to the JSON
    // writer, to write or to refuse.
    let (Ok(revision), Ok(row_count)) = (
        u32::try_from(result.revision),
        u32::try_from(result.row_count),
    ) else {
        return Err(Unwritten::Shape);
    };
    let room = out.len();
    let (fixed, rest) = out
        .split_first_chunk_mut::<HEADER_FIXED_BYTES>()
        .ok_or(Unwritten::Shape)?;
    let mut writer = HeaderWriter(rest);
    let (mut width, mut count) = (0, 0);
    if let Some((table, keys)) = changed {
        writer.text(table).ok_or(Unwritten::Shape)?;
        if let Some(keys) = keys {
            (width, count) = writer.keys(keys)?;
        }
    }
    // A header longer than its length counts has not the shape of one, however much room
    // there is.
    let length = u16::try_from(room - writer.0.len()).map_err(|_| Unwritten::Shape)?;
    // The fixed bytes are two words: the first byte and the two unused ones are zero.
    let ([shape, counts], []) = fixed.as_chunks_mut::<8>() else {
        return Err(Unwritten::Shape);
    };
    *shape =
        (u64::from(width) << 8 | u64::from(count) << 16 | u64::from(length) << 32).to_le_bytes();
    *counts = (u64::from(revision) | u64::from(row_count) << 32).to_le_bytes();
    Ok(kind)
}

/// The bytes of a header that are yet to be written, after its fixed part. Each write takes its
/// bytes from their front, or reports `None` when the header has no room for it, which leaves
/// room for nothing more.
struct HeaderWriter<'a>(&'a mut [u8]);

impl<'a> HeaderWriter<'a> {
    /// The next `N` bytes, to be written.
    fn take<const N: usize>(&mut self) -> Option<&'a mut [u8; N]> {
        let (bytes, rest) = std::mem::take(&mut self.0).split_first_chunk_mut()?;
        self.0 = rest;
        Some(bytes)
    }

    fn text(&mut self, text: &str) -> Option<()> {
        let length = u16::try_from(text.len())
            .ok()
            .filter(|length| *length < DECODED_TEXT)?;
        // So short a text is looked at a byte at a time, in place.
        let plain = text.len() <= PLAIN_TEXT_BYTES && text.bytes().all(|byte| byte < 0x80);
        *self.take()? = (length | if plain { 0 } else { DECODED_TEXT }).to_le_bytes();
        let (bytes, rest) = std::mem::take(&mut self.0).split_at_mut_checked(text.len())?;
        bytes.copy_from_slice(text.as_bytes());
        self.0 = rest;
        Some(())
    }

    /// Writes a table's changed keys: its key's column names, and then every key's values. It
    /// returns the number of columns and of keys. Keys a header cannot hold have not its
    /// shape: more columns or keys than a table declares or reports, values that do not make
    /// whole keys, a value that is not a scalar, or more bytes than there is room for.
    fn keys(&mut self, keys: &TableKeys) -> std::result::Result<(u8, u16), Unwritten> {
        let columns = &keys.schema.primary_key;
        let (Ok(width), Some(0)) = (
            u8::try_from(columns.len()),
            keys.values.len().checked_rem(columns.len()),
        ) else {
            return Err(Unwritten::Shape);
        };
        let count = keys.values.len() / columns.len();
        if count > MAX_CHANGED_KEYS_PER_TABLE {
            return Err(Unwritten::Shape);
        }
        for column in columns {
            self.text(column).ok_or(Unwritten::Shape)?;
        }
        for value in &keys.values {
            match value {
                Value::Null => self.tag(TAG_NULL),
                Value::Bool(false) => self.tag(TAG_FALSE),
                Value::Bool(true) => self.tag(TAG_TRUE),
                Value::Number(number) => {
                    // Written as the JS number a response carries, refusing what
                    // `Json::number` refuses: an integer beyond JavaScript's safe range,
                    // which one that an i64 cannot hold is far beyond, and a float that is
                    // not finite.
                    let (tag, value) = if let Some(value) = number.as_i64() {
                        if value.unsigned_abs() > MAX_SAFE_INTEGER {
                            return Err(Unwritten::Number);
                        }
                        (TAG_INTEGER, value as f64)
                    } else {
                        match number.as_f64() {
                            Some(value) if number.is_f64() && value.is_finite() => {
                                (TAG_FLOAT, value)
                            }
                            _ => return Err(Unwritten::Number),
                        }
                    };
                    self.take::<9>().map(|[first, rest @ ..]| {
                        *first = tag;
                        *rest = value.to_le_bytes();
                    })
                }
                Value::String(text) => self.tag(TAG_STRING).and_then(|()| self.text(text)),
                Value::Array(_) | Value::Object(_) => None,
            }
            .ok_or(Unwritten::Shape)?;
        }
        Ok((width, count as u16))
    }

    fn tag(&mut self, tag: u8) -> Option<()> {
        *self.take()? = [tag];
        Some(())
    }
}

/// A `SELECT`'s fields and rows alone, as the line of text that follows the header in its JSON
/// response. The text is bounded as that response was: the envelope and the header it no longer
/// holds still count toward the bound, so that exactly the same results are refused as too
/// large.
fn rows_text(result: &ExecuteResult, array_rows: bool) -> Result<Json> {
    let mut json = Json(spare());
    json.rows(result, array_rows)?;
    let unwritten =
        SELECT_PREAMBLE_BYTES + digits(result.revision) + digits(result.row_count as u64);
    if json.0.len() + unwritten > MAX_BYTES {
        return Err(limit());
    }
    Ok(json)
}

/// How many digits `value` is written with.
fn digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
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

thread_local! {
    /// The text of the last response, emptied, for the next to be written into, so that a
    /// statement's response neither allocates nor frees its text. One far longer than a statement's
    /// header is freed rather than kept.
    static SPARE: Cell<String> = const { Cell::new(String::new()) };
}

const SPARE_BYTES: usize = 64 * 1024;

/// The text of the last response, emptied, for the next to be written into.
fn spare() -> String {
    let mut text = SPARE.take();
    text.clear();
    // Room for a statement's header and a few rows, so that most responses are written without
    // growing the text.
    text.reserve(512);
    text
}

impl Json {
    fn envelope(status: u32, disposition: u32) -> Self {
        let mut json = Self(spare());
        json.raw("[");
        for value in [VERSION, status, disposition] {
            // Written as a u64, which numbers are written as anyway.
            json.raw(itoa::Buffer::new().format(u64::from(value)));
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
        let value = JsValue::from_str(&self.0);
        if self.0.capacity() <= SPARE_BYTES {
            SPARE.set(self.0);
        }
        Ok(value)
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
            self.string(table.table());
            self.raw(":[");
            for (index, values) in table.keys().enumerate() {
                self.raw(if index > 0 { ",{" } else { "{" });
                for (index, (column, value)) in table.columns().iter().zip(values).enumerate() {
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
            self.raw("],\"foreignKeys\":[");
            for (position, key) in table.foreign_keys.iter().enumerate() {
                if position > 0 {
                    self.raw(",");
                }
                self.raw("{\"name\":");
                self.string(&key.name);
                self.raw(",\"columns\":");
                self.strings(&key.columns);
                self.raw(",\"references\":");
                self.string(&key.references);
                self.raw(",\"referencedColumns\":");
                self.strings(&key.referenced_columns);
                self.raw(",\"onDelete\":");
                self.string(key.on_delete.name());
                self.raw(",\"onUpdate\":");
                self.string(key.on_update.name());
                self.raw("}");
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
            self.string(result.command);
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
        // Fields of one name, which only array rows are returned, leave object rows' values keyed
        // by field position, and so already in field order.
        let positional = (1..fields.len()).any(|index| {
            fields[..index]
                .iter()
                .any(|field| field.name == fields[index].name)
        });
        if positional && !array_rows {
            return Err(serialization());
        }
        // Where each field's escaped name sits in the fields list, from which object rows copy
        // their keys.
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
                keys.push(start..self.0.len());
            }
            self.raw(",\"dataTypeID\":");
            self.unsigned(field.data_type_id.into())?;
            self.raw("}");
        }
        self.raw("],\"rows\":[");
        if let Some(values) = &result.values {
            for (index, row) in values.rows.iter().enumerate() {
                if row.len() != fields.len() {
                    return Err(serialization());
                }
                self.row(index, &mut row.iter(), &keys, array_rows)?;
            }
            self.raw("]}");
            return Ok(());
        }
        // An object's values come in key order. Each field's position in that order is where its
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
        if positional {
            for (index, position) in positions.iter_mut().enumerate() {
                *position = index;
            }
        }
        let mut names = vec![""; fields.len()];
        for (field, position) in fields.iter().zip(&positions) {
            names[*position] = &field.name;
        }
        let mut values = Vec::with_capacity(fields.len());
        for (index, row) in result.rows.iter().enumerate() {
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
            self.row(
                index,
                &mut positions.iter().map(|position| values[*position]),
                &keys,
                array_rows,
            )?;
        }
        self.raw("]}");
        Ok(())
    }

    /// Writes the row at `index` of a result, from its values in field order: as an array, or as
    /// an object whose keys are copied from the fields list at `keys`.
    fn row(
        &mut self,
        index: usize,
        values: &mut dyn Iterator<Item = &Value>,
        keys: &[Range<usize>],
        array: bool,
    ) -> Result<()> {
        if index > 0 {
            self.raw(",");
        }
        self.raw(if array { "[" } else { "{" });
        for (position, value) in values.enumerate() {
            if position > 0 {
                self.raw(",");
            }
            if let Some(key) = keys.get(position) {
                self.0.extend_from_within(key.clone());
                self.raw(":");
            }
            self.value(value, 1)?;
        }
        self.raw(if array { "]" } else { "}" });
        self.bounded()
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
    use tinyjoin_core::{
        ColumnDefinition, ForeignKeyAction, ForeignKeyDefinition, ResultField, TableKeys, ValueRows,
    };

    fn result(fields: &[(&str, u32)], rows: Vec<Value>) -> ExecuteResult {
        ExecuteResult {
            command: "SELECT",
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
            values: None,
            tables: vec!["items".into()],
            keys: changed_keys("items", &["id"], vec![json!(1)]),
        }
    }

    fn table_keys(table: &str, columns: &[&str], values: Vec<Value>) -> TableKeys {
        TableKeys {
            schema: Rc::new(TableDefinition {
                name: table.into(),
                primary_key: columns.iter().map(|column| (*column).into()).collect(),
                columns: vec![],
                foreign_keys: vec![],
            }),
            values,
        }
    }

    fn changed_keys(table: &str, columns: &[&str], values: Vec<Value>) -> ChangedKeys {
        let mut keys = ChangedKeys::default();
        keys.insert(table_keys(table, columns, values));
        keys
    }

    fn written(results: &[ExecuteResult], array_rows: bool, list: bool) -> Result<String> {
        let mut json = Json::success(true);
        json.results(results, array_rows, list)?;
        // As a response is bounded when it is finished.
        json.bounded()?;
        Ok(json.0)
    }

    /// `result` with its rows as value rows, in field order.
    fn as_value_rows(mut result: ExecuteResult) -> ExecuteResult {
        let rows = std::mem::take(&mut result.rows)
            .iter()
            .map(|row| {
                result
                    .fields
                    .iter()
                    .map(|field| row[&field.name].clone())
                    .collect()
            })
            .collect();
        result.values = Some(ValueRows {
            rows,
            ..ValueRows::default()
        });
        result
    }

    #[test]
    fn value_rows_are_written_as_object_rows_are() {
        let fields = [
            ("id", 20),
            ("say \"hi\"\n", 25),
            ("é", 114),
            ("b", 16),
            ("f", 701),
        ];
        let rows = vec![
            json!({"id": 1, "say \"hi\"\n": "tab\tquote\"", "é": {"a": [1, null, "x"]}, "b": true, "f": 1.5}),
            json!({"id": 2, "say \"hi\"\n": null, "é": [], "b": false, "f": -0.0}),
            json!({"id": 3, "say \"hi\"\n": "\u{1f600}", "é": "text", "b": null, "f": 1e300}),
        ];
        for array_rows in [false, true] {
            for list in [false, true] {
                let objects = [result(&fields, rows.clone()), result(&fields, vec![])];
                let values = objects.clone().map(as_value_rows);
                assert_eq!(
                    written(&values, array_rows, list).unwrap(),
                    written(&objects, array_rows, list).unwrap()
                );
            }
        }
        // Object rows cannot hold two fields of one name, which array rows can.
        let mut repeated = as_value_rows(result(&[("id", 20)], vec![json!({"id": 1})]));
        repeated.fields.push(repeated.fields[0].clone());
        repeated.values.as_mut().unwrap().rows[0].push(json!(1));
        assert_eq!(
            written(std::slice::from_ref(&repeated), false, false)
                .unwrap_err()
                .code,
            "BRIDGE_SERIALIZATION_ERROR"
        );
        assert!(
            written(&[repeated], true, false)
                .unwrap()
                .ends_with(r#""rows":[[1,1]]}"#)
        );
    }

    #[test]
    fn operation_numbers_are_dense_and_stable() {
        assert_eq!(VERSION, 6);
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
                "[6,0,1,{header}]\n{fields},\"rows\":[{{\"title\":\"one\",\"id\":1}},{{\"title\":null,\"id\":2}}]}}"
            )
        );
        assert_eq!(
            written(std::slice::from_ref(&rows), true, false).unwrap(),
            format!("[6,0,1,{header}]\n{fields},\"rows\":[[\"one\",1],[null,2]]}}")
        );

        let empty = ExecuteResult {
            command: "UPDATE",
            revision: 8,
            row_count: 3,
            fields: vec![],
            rows: vec![],
            values: None,
            tables: vec![],
            keys: ChangedKeys::default(),
        };
        assert_eq!(
            written(&[empty.clone(), empty], false, true).unwrap(),
            [
                r#"[6,0,1,[{"command":"UPDATE","revision":8,"rowCount":3,"tables":[],"keys":{}},{"command":"UPDATE","revision":8,"rowCount":3,"tables":[],"keys":{}}]]"#,
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
            foreign_keys: vec![ForeignKeyDefinition {
                name: "k_fkey".into(),
                columns: vec!["k".into()],
                references: "t\"1".into(),
                referenced_columns: vec!["k".into()],
                on_delete: ForeignKeyAction::Cascade,
                on_update: ForeignKeyAction::NoAction,
            }],
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
                r#""primaryKey":["id","k"],"indexes":[{"name":"by_k","columns":["k"],"unique":true}],"#,
                r#""foreignKeys":[{"name":"k_fkey","columns":["k"],"references":"t\"1","#,
                r#""referencedColumns":["k"],"onDelete":"cascade","onUpdate":"no action"}]}]}"#,
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
        keys.insert(table_keys("empty", &["id"], vec![]));
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
            r#"[6,1,0,{"code":"CONSTRAINT","message":"a \"quoted\" failure","retryable":true}]"#
        );
        assert_eq!(
            error_text(&EngineError::new("HUGE", "x".repeat(MAX_BYTES))),
            r#"[6,1,0,{"code":"BRIDGE_SERIALIZATION_ERROR","message":"TinyJoin could not encode a structured bridge error","retryable":false}]"#
        );
    }

    /// `name` after a byte order mark, which is a character of a name like any other.
    fn after_byte_order_mark(name: &str) -> String {
        format!("{}{name}", char::from_u32(0xfeff).unwrap())
    }

    /// A write's result: one without fields or rows.
    fn write(command: &'static str, tables: &[&str], keys: ChangedKeys) -> ExecuteResult {
        ExecuteResult {
            command,
            revision: 7,
            row_count: 1,
            fields: vec![],
            rows: vec![],
            values: None,
            tables: tables.iter().map(|table| (*table).into()).collect(),
            keys,
        }
    }

    /// A result's header, with its first byte set as [`statement`] sets it, or `None` for a
    /// result that is left to the JSON writer.
    fn headed(result: &ExecuteResult) -> Result<Option<Vec<u8>>> {
        let mut out = vec![0xaa; HEADER_BYTES];
        Ok(match header(result, &mut out)? {
            NO_HEADER => None,
            kind => {
                out[0] = kind;
                let length = usize::from(u16::from_le_bytes([out[4], out[5]]));
                Some(out[..length].to_vec())
            }
        })
    }

    /// The header line of the JSON response the JSON writer gives `result`, without its
    /// envelope, and the line of fields and rows that follows it.
    fn json_lines(result: &ExecuteResult, array_rows: bool) -> Result<(String, String)> {
        let text = written(std::slice::from_ref(result), array_rows, false)?;
        let (envelope, rows) = text.split_once('\n').unwrap();
        let header = envelope
            .strip_prefix("[6,0,1,")
            .and_then(|header| header.strip_suffix(']'))
            .unwrap();
        Ok((header.to_owned(), rows.to_owned()))
    }

    /// Reads a header back, as the Worker reads one: the write or read it stands for, without
    /// fields and rows, and the header of its JSON response as a value.
    fn read_header(bytes: &[u8]) -> (ExecuteResult, Value) {
        let command = ["INSERT", "UPDATE", "DELETE", "SELECT"][usize::from(bytes[0]) - 1];
        let width = usize::from(bytes[1]);
        let count = usize::from(u16::from_le_bytes([bytes[2], bytes[3]]));
        assert_eq!(
            usize::from(u16::from_le_bytes([bytes[4], bytes[5]])),
            bytes.len()
        );
        assert_eq!(bytes[6..8], [0, 0]);
        let number = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let counted = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let (revision, row_count) = (counted(8), counted(12));
        let mut at = HEADER_FIXED_BYTES;
        let text = |at: &mut usize| {
            let start = *at + 2;
            let length = u16::from_le_bytes([bytes[*at], bytes[*at + 1]]);
            *at = start + usize::from(length & !DECODED_TEXT);
            let text = std::str::from_utf8(&bytes[start..*at]).unwrap();
            // Marked for decoding exactly when it is not plain.
            assert_eq!(
                length & DECODED_TEXT == 0,
                text.len() <= PLAIN_TEXT_BYTES && text.is_ascii(),
                "{text}"
            );
            text.to_owned()
        };
        let mut tables = vec![];
        let mut keys = ChangedKeys::default();
        let mut keyed = Map::new();
        if bytes.len() > HEADER_FIXED_BYTES {
            let table = text(&mut at);
            if width > 0 {
                let columns: Vec<String> = (0..width).map(|_| text(&mut at)).collect();
                let mut values = vec![];
                for _ in 0..count * width {
                    at += 1;
                    values.push(match bytes[at - 1] {
                        TAG_NULL => Value::Null,
                        TAG_FALSE => Value::Bool(false),
                        TAG_TRUE => Value::Bool(true),
                        TAG_INTEGER => {
                            at += 8;
                            assert_eq!(number(at - 8).fract(), 0.0);
                            Value::from(number(at - 8) as i64)
                        }
                        TAG_FLOAT => {
                            at += 8;
                            Value::Number(Number::from_f64(number(at - 8)).unwrap())
                        }
                        TAG_STRING => Value::String(text(&mut at)),
                        tag => panic!("a header holds no value tagged {tag}"),
                    });
                }
                keyed.insert(
                    table.clone(),
                    values
                        .chunks(width)
                        .map(|key| {
                            Value::Object(
                                columns.iter().cloned().zip(key.iter().cloned()).collect(),
                            )
                        })
                        .collect(),
                );
                let columns: Vec<&str> = columns.iter().map(String::as_str).collect();
                keys.insert(table_keys(&table, &columns, values));
            } else {
                assert_eq!(count, 0);
            }
            tables.push(table);
        } else {
            assert_eq!((width, count), (0, 0));
        }
        assert_eq!(at, bytes.len());
        let header = json!({
            "command": command,
            "revision": revision,
            "rowCount": row_count,
            "tables": tables,
            "keys": keyed,
        });
        let result = ExecuteResult {
            command,
            revision: revision.into(),
            row_count: row_count as usize,
            fields: vec![],
            rows: vec![],
            values: None,
            tables,
            keys,
        };
        (result, header)
    }

    #[test]
    fn statement_headers_are_written_byte_for_byte() {
        // Every header begins with its command, the width of its key, the number of its keys,
        // its length, and its revision and row count.
        let fixed = |kind: u8, width: u8, count: u16, length: u16| {
            let mut bytes = vec![kind, width];
            bytes.extend_from_slice(&count.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&7_u32.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes
        };
        assert_eq!(
            fixed(2, 0, 0, 16),
            [2, 0, 0, 0, 16, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0]
        );

        // A write that changed nothing, and a read, are those bytes alone.
        for (kind, command) in [(1, "INSERT"), (2, "UPDATE"), (3, "DELETE"), (4, "SELECT")] {
            let unchanged = write(command, &[], ChangedKeys::default());
            assert_eq!(headed(&unchanged).unwrap().unwrap(), fixed(kind, 0, 0, 16));
        }
        let read = ExecuteResult {
            tables: vec![],
            keys: ChangedKeys::default(),
            ..result(&[("id", 20)], vec![json!({"id": 1})])
        };
        assert_eq!(headed(&read).unwrap().unwrap(), fixed(4, 0, 0, 16));
        assert_eq!(
            headed(&as_value_rows(read)).unwrap().unwrap(),
            fixed(4, 0, 0, 16)
        );

        // A table whose keys are not reported follows them with its name alone.
        let unkeyed = write("DELETE", &["items"], ChangedKeys::default());
        assert_eq!(
            headed(&unkeyed).unwrap().unwrap(),
            [fixed(3, 0, 0, 23).as_slice(), &[5, 0], b"items"].concat()
        );
        // One whose keys are reported as none follows its name with its key's columns.
        let none = write("UPDATE", &["items"], changed_keys("items", &["id"], vec![]));
        assert_eq!(
            headed(&none).unwrap().unwrap(),
            [
                fixed(2, 1, 0, 27).as_slice(),
                &[5, 0],
                b"items",
                &[2, 0],
                b"id"
            ]
            .concat()
        );

        // A key of one integer, as a statement by key reports.
        let one = write(
            "INSERT",
            &["t"],
            changed_keys("t", &["id"], vec![json!(42)]),
        );
        assert_eq!(
            headed(&one).unwrap().unwrap(),
            [
                fixed(1, 1, 1, 32).as_slice(),
                &[1, 0],
                b"t",
                &[2, 0],
                b"id",
                &[TAG_INTEGER],
                &42_f64.to_le_bytes(),
            ]
            .concat()
        );

        // Keys of several columns, holding every kind of scalar, one key after another.
        let pairs = write(
            "UPDATE",
            &["naïve café"],
            changed_keys(
                "naïve café",
                &["a", "é", ""],
                vec![
                    json!(-3),
                    json!("x\"y"),
                    Value::Null,
                    json!(-0.0),
                    json!("é😀"),
                    json!(true),
                    json!(1.5),
                    json!(""),
                    json!(false),
                ],
            ),
        );
        // A text that is not all ASCII has the top bit of its length set.
        let mut expected = fixed(2, 3, 3, 0);
        expected.extend_from_slice(&[12, 0x80]);
        expected.extend_from_slice("naïve café".as_bytes());
        expected.extend_from_slice(&[1, 0, b'a', 2, 0x80]);
        expected.extend_from_slice("é".as_bytes());
        expected.extend_from_slice(&[0, 0]);
        let number = |tag: u8, value: f64| [[tag].as_slice(), &value.to_le_bytes()].concat();
        for value in [
            number(TAG_INTEGER, -3.0),
            [&[TAG_STRING, 3, 0][..], b"x\"y"].concat(),
            vec![TAG_NULL],
            number(TAG_FLOAT, -0.0),
            [&[TAG_STRING, 6, 0x80][..], "é😀".as_bytes()].concat(),
            vec![TAG_TRUE],
            number(TAG_FLOAT, 1.5),
            vec![TAG_STRING, 0, 0],
            vec![TAG_FALSE],
        ] {
            expected.extend_from_slice(&value);
        }
        let length = (expected.len() as u16).to_le_bytes();
        expected[4..6].copy_from_slice(&length);
        assert_eq!(headed(&pairs).unwrap().unwrap(), expected);
        // The sign of a zero is in its bytes.
        assert!(
            expected
                .windows(9)
                .any(|bytes| bytes == [TAG_FLOAT, 0, 0, 0, 0, 0, 0, 0, 0x80])
        );
    }

    #[test]
    fn results_of_any_other_shape_are_left_to_the_json_writer() {
        let keys = || changed_keys("items", &["id"], vec![json!(1)]);
        let field = ResultField {
            name: "id".into(),
            data_type_id: 20,
        };
        let row = || json!({"id": 1}).as_object().unwrap().clone();
        let mut two_tables = keys();
        two_tables.insert(table_keys("other", &["id"], vec![json!(2)]));
        for other in [
            // Any other command, whatever it holds.
            write("CREATE TABLE", &[], ChangedKeys::default()),
            write("ALTER TABLE", &["items"], ChangedKeys::default()),
            write("SELECTS", &[], ChangedKeys::default()),
            write("", &[], ChangedKeys::default()),
            // A write that returns fields, with or without rows under them.
            ExecuteResult {
                fields: vec![field.clone()],
                ..write("UPDATE", &["items"], keys())
            },
            ExecuteResult {
                fields: vec![field.clone()],
                rows: vec![row()],
                ..write("DELETE", &["items"], keys())
            },
            ExecuteResult {
                fields: vec![field.clone()],
                values: Some(ValueRows {
                    rows: vec![vec![json!(1)]],
                    estimated_bytes: 0,
                }),
                ..write("INSERT", &["items"], keys())
            },
            // More than one table, or keys of a table other than the one that changed.
            write("DELETE", &["items", "other"], ChangedKeys::default()),
            write("DELETE", &["items", "other"], keys()),
            write("DELETE", &["items"], two_tables),
            write("UPDATE", &["other"], keys()),
            write("UPDATE", &[], keys()),
            // A read that names a table or a key.
            write("SELECT", &["items"], ChangedKeys::default()),
            write("SELECT", &["items"], keys()),
            // A key that is not a scalar, wherever it stands.
            write(
                "INSERT",
                &["items"],
                changed_keys("items", &["id"], vec![json!(1), json!([1])]),
            ),
            write(
                "INSERT",
                &["items"],
                changed_keys("items", &["id"], vec![json!({"a": 1})]),
            ),
            // Values that do not make whole keys, and a key of no columns.
            write(
                "INSERT",
                &["items"],
                changed_keys("items", &["a", "b"], vec![json!(1), json!(2), json!(3)]),
            ),
            write("INSERT", &["items"], changed_keys("items", &[], vec![])),
            write(
                "INSERT",
                &["items"],
                changed_keys("items", &[], vec![json!(1)]),
            ),
            // More keys than a table reports.
            write(
                "INSERT",
                &["items"],
                changed_keys(
                    "items",
                    &["id"],
                    (0..=MAX_CHANGED_KEYS_PER_TABLE)
                        .map(|id| json!(id))
                        .collect(),
                ),
            ),
        ] {
            assert_eq!(headed(&other).unwrap(), None, "{other:?}");
            // Which the JSON writer answers.
            assert!(written(&[other], false, false).is_ok());
        }
        // Rows without the fields they belong under, which no statement returns, are left to
        // it too, to refuse.
        for stray in [
            ExecuteResult {
                rows: vec![row()],
                ..write("DELETE", &["items"], keys())
            },
            ExecuteResult {
                values: Some(ValueRows {
                    rows: vec![vec![json!(1)]],
                    estimated_bytes: 0,
                }),
                ..write("INSERT", &["items"], keys())
            },
        ] {
            assert_eq!(headed(&stray).unwrap(), None);
            assert!(written(&[stray], false, false).is_err());
        }
        // A write whose value rows are present but empty has no rows.
        let empty = ExecuteResult {
            values: Some(ValueRows::default()),
            ..write("INSERT", &["items"], keys())
        };
        assert!(headed(&empty).unwrap().is_some());
        // More columns than a byte counts.
        let names: Vec<String> = (0..256).map(|column| format!("c{column}")).collect();
        let columns: Vec<&str> = names.iter().map(String::as_str).collect();
        let wide = |width: usize| {
            write(
                "INSERT",
                &["items"],
                changed_keys("items", &columns[..width], vec![Value::Null; width]),
            )
        };
        assert!(headed(&wide(255)).unwrap().is_some());
        assert_eq!(headed(&wide(256)).unwrap(), None);
    }

    #[test]
    fn a_header_holds_what_fits_and_leaves_the_rest_to_the_json_writer() {
        // The most keys a table reports, of one integer column, fit beside long names.
        let name = "n".repeat(1024);
        let most = |columns: &[&str]| {
            write(
                "INSERT",
                &[&name],
                changed_keys(
                    &name,
                    columns,
                    (0..MAX_CHANGED_KEYS_PER_TABLE * columns.len())
                        .map(|id| json!(id))
                        .collect(),
                ),
            )
        };
        let bytes = headed(&most(&[&name])).unwrap().unwrap();
        assert_eq!(
            bytes.len(),
            HEADER_FIXED_BYTES + 2 * (2 + name.len()) + 9 * MAX_CHANGED_KEYS_PER_TABLE
        );
        assert!(bytes.len() <= HEADER_BYTES);
        assert_eq!(read_header(&bytes).0.keys, most(&[&name]).keys);
        // As many keys of two columns do not, and are written as JSON.
        assert_eq!(headed(&most(&["a", "b"])).unwrap(), None);
        assert!(written(&[most(&["a", "b"])], false, false).is_ok());

        // A header fits exactly, or not at all: with a byte less of room there is none.
        let pair = write(
            "UPDATE",
            &["items"],
            changed_keys("items", &["id", "name"], vec![json!(1), json!("one")]),
        );
        let whole = headed(&pair).unwrap().unwrap();
        for room in 0..=whole.len() + 1 {
            let mut out = vec![0; room];
            let kind = header(&pair, &mut out).unwrap();
            assert_eq!(kind != NO_HEADER, room >= whole.len(), "{room}");
            if kind != NO_HEADER {
                out[0] = kind;
                assert_eq!(out[..whole.len()], whole);
            }
        }
        // A name or a value longer than a text's length counts has no header either, however
        // much room there is, and nor has a header longer than its own length counts.
        let long = "x".repeat(usize::from(DECODED_TEXT));
        let mut out = vec![0; 8 * long.len()];
        for too_long in [
            write("DELETE", &[&long], ChangedKeys::default()),
            write(
                "DELETE",
                &["items"],
                changed_keys("items", &[&long], vec![]),
            ),
            write(
                "DELETE",
                &["items"],
                changed_keys("items", &["id"], vec![json!(long)]),
            ),
            write(
                "DELETE",
                &["items"],
                changed_keys("items", &["id"], vec![json!(long[1..]); 2]),
            ),
        ] {
            assert_eq!(header(&too_long, &mut out).unwrap(), NO_HEADER);
        }
        let longest = write(
            "DELETE",
            &["items"],
            changed_keys("items", &["id"], vec![json!(long[1..])]),
        );
        assert_eq!(header(&longest, &mut out).unwrap(), 3);
        assert_eq!(out[16 + 7 + 4..][..3], [TAG_STRING, 0xff, 0xff]);
        assert_eq!(headed(&longest).unwrap(), None);
    }

    #[test]
    fn a_header_s_text_is_marked_for_decoding_unless_it_is_short_and_ascii() {
        // The mark is the top bit of the text's length, here the table's, which follows the
        // header's fixed bytes.
        let marked = |name: &str| {
            let bytes = headed(&write("DELETE", &[name], ChangedKeys::default()))
                .unwrap()
                .unwrap();
            let length = u16::from_le_bytes([bytes[16], bytes[17]]);
            assert_eq!(usize::from(length & !DECODED_TEXT), name.len());
            assert_eq!(bytes[18..], *name.as_bytes());
            length & DECODED_TEXT != 0
        };
        for plain in ["", "t", "items", "\u{0}\u{7f}", &"x".repeat(64)] {
            assert!(!marked(plain), "{plain}");
        }
        for decoded in [
            "é",
            "\u{80}",
            "it\u{e9}ms",
            &after_byte_order_mark("items"),
            "😀",
            &"x".repeat(65),
            &format!("{}é", "x".repeat(62)),
            &"x".repeat(1_000),
        ] {
            assert!(marked(decoded), "{decoded}");
        }
    }

    #[test]
    fn a_header_refuses_the_numbers_the_json_writer_refuses() {
        let keyed = |value: Value| {
            write(
                "UPDATE",
                &["items"],
                changed_keys("items", &["a", "b"], vec![json!(1), value]),
            )
        };
        // A key that is a number no response carries.
        for result in [
            keyed(json!(9_007_199_254_740_992_u64)),
            keyed(json!(-9_007_199_254_740_992_i64)),
            keyed(json!(u64::MAX)),
            keyed(json!(i64::MIN)),
        ] {
            let error = headed(&result).unwrap_err();
            assert_eq!(
                (error.code.as_str(), error.message.as_str()),
                (
                    "BRIDGE_SERIALIZATION_ERROR",
                    "TinyJoin could not encode the structured bridge result"
                )
            );
            assert_eq!(written(&[result], false, false).unwrap_err(), error);
        }
        // The greatest numbers a response carries are written.
        let greatest = ExecuteResult {
            revision: u32::MAX.into(),
            row_count: u32::MAX as usize,
            ..keyed(json!(-9_007_199_254_740_991_i64))
        };
        let bytes = headed(&greatest).unwrap().unwrap();
        assert_eq!(bytes[8..16], [0xff; 8]);
        assert_eq!(read_header(&bytes).0, greatest);
        assert_eq!(
            bytes[bytes.len() - 9..],
            [
                &[TAG_INTEGER][..],
                &(-9_007_199_254_740_991_f64).to_le_bytes()
            ]
            .concat()
        );
        // A number that is refused where the JSON writer would also find a value it writes as
        // no header is refused all the same, since that writer refuses it too.
        let mixed = write(
            "UPDATE",
            &["items"],
            changed_keys("items", &["id"], vec![json!(u64::MAX), json!([1])]),
        );
        assert_eq!(
            headed(&mixed).unwrap_err().code,
            "BRIDGE_SERIALIZATION_ERROR"
        );
        assert!(written(&[mixed], false, false).is_err());

        // A revision or a row count past what a header's count holds has no header, and is
        // the JSON writer's to write, or to refuse where no response carries it.
        let counted = |revision: u64, row_count: u64| {
            usize::try_from(row_count)
                .ok()
                .map(|row_count| ExecuteResult {
                    revision,
                    row_count,
                    ..write("DELETE", &["items"], ChangedKeys::default())
                })
        };
        let past = u64::from(u32::MAX) + 1;
        for (revision, row_count, written_as_json) in [
            (past, 1, true),
            (1, past, true),
            (MAX_SAFE_INTEGER, MAX_SAFE_INTEGER, true),
            (MAX_SAFE_INTEGER + 1, 1, false),
            (1, MAX_SAFE_INTEGER + 1, false),
            (u64::MAX, u64::MAX, false),
        ] {
            // A row count past a usize, where a usize is as short as a module's, cannot be.
            let Some(result) = counted(revision, row_count) else {
                continue;
            };
            assert_eq!(headed(&result).unwrap(), None);
            let json = written(&[result], false, false);
            assert_eq!(json.is_ok(), written_as_json, "{revision} {row_count}");
            if let Err(error) = json {
                assert_eq!(error.code, "BRIDGE_SERIALIZATION_ERROR");
            }
        }
        // A read is counted alike.
        let read = |revision: u64| ExecuteResult {
            revision,
            tables: vec![],
            keys: ChangedKeys::default(),
            ..result(&[], vec![])
        };
        assert!(headed(&read(u32::MAX.into())).unwrap().is_some());
        assert_eq!(headed(&read(past)).unwrap(), None);
    }

    #[test]
    fn a_statement_marks_its_header_last_or_not_at_all() {
        let mut out = vec![NO_HEADER; HEADER_BYTES];
        // A write is answered `true`, with its header marked.
        let keyed = write("DELETE", &["t"], changed_keys("t", &["id"], vec![json!(9)]));
        assert!(statement(&keyed, false, &mut out).unwrap().is_some());
        assert_eq!(out[..32], headed(&keyed).unwrap().unwrap());
        // A result of another shape leaves the mark as it was: unset, as each call starts.
        out[0] = NO_HEADER;
        let other = write("CREATE INDEX", &[], ChangedKeys::default());
        assert!(statement(&other, false, &mut out).unwrap().is_none());
        assert_eq!(out[0], NO_HEADER);
        // So does one that is refused, and a read whose rows cannot be written, which fails
        // only after the rest of its header was.
        let refused = write(
            "DELETE",
            &["t"],
            changed_keys("t", &["id"], vec![json!(u64::MAX)]),
        );
        assert!(statement(&refused, false, &mut out).is_err());
        assert_eq!(out[0], NO_HEADER);
        let unwritable = ExecuteResult {
            tables: vec![],
            keys: ChangedKeys::default(),
            ..result(&[("id", 20), ("title", 25)], vec![json!({"id": 1})])
        };
        assert_eq!(header(&unwritable, &mut out).unwrap(), SELECT_HEADER);
        assert_eq!(
            statement(&unwritable, false, &mut out).unwrap_err().code,
            "BRIDGE_SERIALIZATION_ERROR"
        );
        assert_eq!(out[0], NO_HEADER);

        // An engine that was never asked where its header is has no room for one, so every
        // result of it, whatever its shape, is the JSON writer's: a write, a read, and one
        // that writer refuses.
        let read = ExecuteResult {
            tables: vec![],
            keys: ChangedKeys::default(),
            ..result(&[("id", 20)], vec![json!({"id": 1})])
        };
        for unasked in [
            write("UPDATE", &[], ChangedKeys::default()),
            keyed,
            read,
            refused,
            unwritable,
        ] {
            assert!(statement(&unasked, false, &mut []).unwrap().is_none());
        }
    }

    #[test]
    fn a_read_s_rows_are_the_second_line_of_its_json_response_and_bounded_as_it_is() {
        let read = |revision: u64, rows: Vec<Value>| ExecuteResult {
            revision,
            tables: vec![],
            keys: ChangedKeys::default(),
            ..result(&[("title", 25), ("id", 20)], rows)
        };
        let rows = vec![
            json!({"id": 1, "title": "one"}),
            json!({"id": 2, "title": null}),
        ];
        for array_rows in [false, true] {
            for result in [
                read(7, rows.clone()),
                read(7, vec![]),
                as_value_rows(read(7, rows.clone())),
            ] {
                assert_eq!(
                    rows_text(&result, array_rows).unwrap().0,
                    json_lines(&result, array_rows).unwrap().1
                );
            }
        }
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(MAX_SAFE_INTEGER), 16);
        assert_eq!(digits(u64::MAX), 20);

        // The rows of a response that is exactly as long as a response may be are written, and
        // those of one a byte longer are refused, whatever the digits of its header: those of
        // its revision, and those of its row count, which is the statement's to say.
        for (revision, row_count) in [
            (0, 1),
            (12_345, 1),
            (MAX_SAFE_INTEGER, 1),
            (7, 10),
            (7, 99_999),
            (12_345, u32::MAX as usize),
        ] {
            let room = |title: usize| ExecuteResult {
                row_count,
                ..read(revision, vec![json!({"id": 1, "title": "x".repeat(title)})])
            };
            let empty = written(&[room(0)], false, false).unwrap().len();
            let fits = room(MAX_BYTES - empty);
            assert_eq!(
                written(std::slice::from_ref(&fits), false, false)
                    .unwrap()
                    .len(),
                MAX_BYTES
            );
            assert!(rows_text(&fits, false).is_ok());
            let over = room(MAX_BYTES - empty + 1);
            for refused in [
                written(std::slice::from_ref(&over), false, false).map(drop),
                rows_text(&over, false).map(drop),
            ] {
                assert_eq!(refused.unwrap_err().code, "RESOURCE_LIMIT");
            }
        }
    }

    /// Whether a result has the shape of a header, as the protocol defines it: a `SELECT` that
    /// changed no table and reports no keys, or an `INSERT`, `UPDATE` or `DELETE` without fields
    /// and rows that changed at most one table, whose keys are none, or that table's alone with
    /// every value a scalar.
    fn has_the_shape_of_a_header(result: &ExecuteResult) -> bool {
        let keys = result.keys.tables();
        match result.command {
            "SELECT" => result.tables.is_empty() && keys.is_empty(),
            "INSERT" | "UPDATE" | "DELETE" => {
                result.fields.is_empty()
                    && result.rows.is_empty()
                    && result
                        .values
                        .as_ref()
                        .is_none_or(|values| values.rows.is_empty())
                    && result.tables.len() <= 1
                    && match keys {
                        [] => true,
                        [keys] => {
                            result.tables.first().map(String::as_str) == Some(keys.table())
                                && keys.values.iter().all(|value| {
                                    !matches!(value, Value::Array(_) | Value::Object(_))
                                })
                        }
                        _ => false,
                    }
            }
            _ => false,
        }
    }

    /// How many bytes the header of a result of that shape takes, or `None` for one that a
    /// header cannot count: its keys, their columns, or the bytes of a text.
    fn header_bytes(result: &ExecuteResult) -> Option<usize> {
        let text = |text: &str| {
            u16::try_from(text.len())
                .ok()
                .filter(|length| *length < DECODED_TEXT)
                .map(|length| 2 + usize::from(length))
        };
        u32::try_from(result.revision).ok()?;
        u32::try_from(result.row_count).ok()?;
        let mut bytes = HEADER_FIXED_BYTES;
        if let Some(table) = result.tables.first() {
            bytes += text(table)?;
        }
        if let Some(keys) = result.keys.tables().first() {
            let width = keys.columns().len();
            if width == 0
                || width > 255
                || keys.values.len() % width != 0
                || keys.values.len() / width > MAX_CHANGED_KEYS_PER_TABLE
            {
                return None;
            }
            for column in keys.columns() {
                bytes += text(column)?;
            }
            for value in &keys.values {
                bytes += match value {
                    Value::Number(_) => 9,
                    Value::String(value) => 1 + text(value)?,
                    _ => 1,
                };
            }
        }
        Some(bytes)
    }

    /// Generated results of every shape are headed exactly when they have the shape of a header
    /// that fits, their headers read back as what the JSON writer writes for them, and they are
    /// refused exactly when that writer refuses them.
    #[test]
    fn generated_results_are_headed_as_their_json_says() {
        struct Random(u64);
        impl Random {
            fn below(&mut self, bound: usize) -> usize {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((self.0 >> 33) as usize) % bound
            }

            fn name(&mut self) -> String {
                let marked = after_byte_order_mark("marked");
                let names = [
                    "t",
                    "id",
                    "items",
                    "__proto__",
                    "",
                    "naïve café",
                    "é😀",
                    "say \"hi\"\n\t\\",
                    marked.as_str(),
                    "\u{7}\u{0}",
                ];
                match self.below(12) {
                    10 => "long name ".repeat(1 + self.below(40)),
                    11 => "é".repeat(1 + self.below(300)),
                    name => names[name].to_owned(),
                }
            }

            fn key(&mut self) -> Value {
                match self.below(40) {
                    0..=13 => json!(self.below(100_000)),
                    14..=16 => json!(-(self.below(1_000) as i64)),
                    17 => json!(MAX_SAFE_INTEGER),
                    18 => json!(-(MAX_SAFE_INTEGER as i64)),
                    19..=21 => json!([0.5, -0.0, 0.1, 1e300, -2.5e-9, 1.0, 5e-324][self.below(7)]),
                    22..=27 => json!(self.name()),
                    28..=30 => Value::Null,
                    31..=33 => json!(self.below(2) == 0),
                    34 => json!("x".repeat(self.below(3_000))),
                    // What a header never holds: a number no response carries, and a value that
                    // is not a scalar.
                    35 if self.below(4) == 0 => json!(MAX_SAFE_INTEGER + 1 + self.below(3) as u64),
                    36 if self.below(4) == 0 => json!(-(MAX_SAFE_INTEGER as i64) - 1),
                    37 if self.below(4) == 0 => json!(u64::MAX),
                    38 if self.below(3) == 0 => json!([1, "two"]),
                    39 if self.below(3) == 0 => json!({"a": null}),
                    _ => json!(self.below(10)),
                }
            }

            fn keys(&mut self, table: &str) -> TableKeys {
                let width = [1, 1, 1, 1, 2, 2, 3, 5][self.below(8)];
                let names: Vec<String> = (0..width).map(|_| self.name()).collect();
                let columns: Vec<&str> = names.iter().map(String::as_str).collect();
                let count = match self.below(20) {
                    0 => 0,
                    1..=9 => 1,
                    10..=15 => 2 + self.below(8),
                    16 => 200,
                    17 => 990 + self.below(12),
                    18 => MAX_CHANGED_KEYS_PER_TABLE,
                    _ => 1 + self.below(600),
                };
                // Now and then, values that do not make whole keys.
                let ragged = usize::from(width > 1 && self.below(40) == 0);
                table_keys(
                    table,
                    &columns,
                    (0..count * width + ragged).map(|_| self.key()).collect(),
                )
            }
        }

        let commands = [
            "INSERT",
            "UPDATE",
            "DELETE",
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "SELECT",
            "CREATE TABLE",
            "DROP INDEX",
            "SET SCHEMA VERSION",
        ];
        let field = ResultField {
            name: "id".into(),
            data_type_id: 20,
        };
        let row = || json!({"id": 1}).as_object().unwrap().clone();
        let (mut headed, mut shapeless, mut refused, mut short) = (0, 0, 0, 0);
        for seed in [1, 0x5eed, 0xface_feed] {
            let mut random = Random(seed);
            for case in 0..1_500 {
                let context = format!("seed={seed:#x}, case={case}");
                let command = commands[random.below(commands.len())];
                let tables: Vec<String> = match random.below(10) {
                    0..=2 => vec![],
                    3..=8 => vec![random.name()],
                    _ => vec![random.name(), random.name()],
                };
                let mut keys = ChangedKeys::default();
                match random.below(10) {
                    0..=2 => {}
                    3..=7 => {
                        if let Some(table) = tables.first() {
                            keys.insert(random.keys(table));
                        }
                    }
                    8 => {
                        let other = random.name();
                        keys.insert(random.keys(&other));
                    }
                    _ => {
                        for table in tables.iter().chain(&[random.name()]) {
                            keys.insert(random.keys(table));
                        }
                    }
                }
                // A read has its fields and rows, and most writes have neither.
                let returning = command == "SELECT" || random.below(8) == 0;
                let value_rows = random.below(3) == 0;
                let result = ExecuteResult {
                    command,
                    revision: match random.below(40) {
                        0 => MAX_SAFE_INTEGER,
                        1 if random.below(3) == 0 => MAX_SAFE_INTEGER + 1,
                        2 => u32::MAX.into(),
                        3 => u64::from(u32::MAX) + 1,
                        _ => random.below(1_000_000) as u64,
                    },
                    row_count: random.below(10_000),
                    fields: if returning {
                        vec![field.clone()]
                    } else {
                        vec![]
                    },
                    rows: if returning && !value_rows {
                        vec![row(); random.below(3)]
                    } else {
                        vec![]
                    },
                    values: value_rows.then(|| ValueRows {
                        rows: if returning {
                            vec![vec![json!(1)]; random.below(3)]
                        } else {
                            vec![]
                        },
                        estimated_bytes: 0,
                    }),
                    tables: if command == "SELECT" && random.below(8) > 0 {
                        vec![]
                    } else {
                        tables
                    },
                    keys: if command == "SELECT" && random.below(8) > 0 {
                        ChangedKeys::default()
                    } else {
                        keys
                    },
                };
                let array_rows = random.below(2) == 0;
                // Mostly the room an engine keeps, and now and then too little.
                let room = match random.below(12) {
                    0 => random.below(64),
                    1 => 24 + random.below(2_000),
                    _ => HEADER_BYTES,
                };
                let mut out = vec![0xaa; room];
                let json = json_lines(&result, array_rows);
                let kind = match header(&result, &mut out) {
                    Ok(kind) => kind,
                    Err(error) => {
                        // Refused only as the JSON writer refuses it, which reads every key,
                        // and so finds the number a header was refused for.
                        assert_eq!(json.unwrap_err(), error, "{context}");
                        refused += 1;
                        continue;
                    }
                };
                let fits =
                    header_bytes(&result).filter(|bytes| *bytes <= room.min(usize::from(u16::MAX)));
                if kind == NO_HEADER {
                    // Without a header only for another shape, or for want of room.
                    match (has_the_shape_of_a_header(&result), fits) {
                        (false, _) => shapeless += 1,
                        (true, None) => short += 1,
                        (true, Some(bytes)) => panic!("{context}: {bytes} bytes fit {room}"),
                    }
                    continue;
                }
                assert!(has_the_shape_of_a_header(&result), "{context}");
                let length = fits.unwrap_or_else(|| panic!("{context}: headed without room"));
                out[0] = kind;
                assert_eq!(
                    usize::from(u16::from_le_bytes([out[4], out[5]])),
                    length,
                    "{context}"
                );
                // Nothing is written past a header's end.
                assert!(out[length..].iter().all(|byte| *byte == 0xaa), "{context}");
                let (read, value) = read_header(&out[..length]);
                // What it reads back as is the header the JSON writer gives the result, parsed,
                let (json_header, json_rows) =
                    json.unwrap_or_else(|error| panic!("{context}: {error:?}"));
                assert_eq!(
                    value,
                    serde_json::from_str::<Value>(&json_header).unwrap(),
                    "{context}"
                );
                // and written again it is that header to the letter, so every column is in its
                // place and every number as it was.
                assert_eq!(
                    json_lines(&read, array_rows).unwrap().0,
                    json_header,
                    "{context}"
                );
                if command == "SELECT" {
                    assert_eq!(
                        rows_text(&result, array_rows).unwrap().0,
                        json_rows,
                        "{context}"
                    );
                } else {
                    assert_eq!(json_rows, r#"{"fields":[],"rows":[]}"#, "{context}");
                }
                headed += 1;
            }
        }
        // Every outcome is generated often enough to mean something.
        assert!(headed > 1_000 && shapeless > 1_000, "{headed} {shapeless}");
        assert!(refused > 20 && short > 100, "{refused} {short}");
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

        // The greatest and the least integers a request holds are read as integers, and a
        // whole number tagged as a float is read as a float.
        let number = |tag: u8, value: f64| {
            let bytes = [&[0, 1, 0, 0, 0, tag][..], &value.to_le_bytes()].concat();
            let mut request = Request::new(&bytes);
            request.flag().unwrap();
            let values = request.values().unwrap();
            request.finish().unwrap();
            values[0].clone()
        };
        assert_eq!(
            number(TAG_INTEGER, MAX_SAFE_INTEGER as f64),
            json!(MAX_SAFE_INTEGER)
        );
        assert_eq!(
            number(TAG_INTEGER, -(MAX_SAFE_INTEGER as f64)),
            json!(-(MAX_SAFE_INTEGER as i64))
        );
        assert!(number(TAG_INTEGER, -7.0).is_i64());
        assert!(number(TAG_FLOAT, 7.0).is_f64());
        assert_eq!(number(TAG_FLOAT, 1e300), json!(1e300));

        // Truncated, trailing, unsafe, and unknown content is refused.
        for malformed in [
            &bytes[..bytes.len() - 1],
            &[bytes.as_slice(), &[0]].concat()[..],
            &[0, 1, 0, 0, 0, TAG_INTEGER, 0, 0, 0, 0, 0, 0, 0x40, 0x43][..],
            &[0, 1, 0, 0, 0, TAG_INTEGER, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f][..],
            // A number that is not finite, tagged as either kind.
            &[0, 1, 0, 0, 0, TAG_FLOAT, 0, 0, 0, 0, 0, 0, 0xf8, 0x7f][..],
            &[0, 1, 0, 0, 0, TAG_FLOAT, 0, 0, 0, 0, 0, 0, 0xf0, 0x7f][..],
            &[0, 1, 0, 0, 0, TAG_FLOAT, 0, 0, 0, 0, 0, 0, 0xf0, 0xff][..],
            &[0, 1, 0, 0, 0, TAG_INTEGER, 0, 0, 0, 0, 0, 0, 0xf8, 0x7f][..],
            &[0, 1, 0, 0, 0, TAG_INTEGER, 0, 0, 0, 0, 0, 0, 0xf0, 0x7f][..],
            // A number cut short, and a string longer than what is left.
            &[0, 1, 0, 0, 0, TAG_FLOAT, 0, 0, 0, 0, 0, 0, 0xf0][..],
            &[0, 1, 0, 0, 0, TAG_STRING, 2, 0, 0, 0, b'x'][..],
            &[0, 1, 0, 0, 0, TAG_STRING, 0xff, 0xff, 0xff, 0xff, b'x'][..],
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
