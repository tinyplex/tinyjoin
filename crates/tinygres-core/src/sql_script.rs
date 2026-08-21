use std::cell::Cell;

use crate::{EngineError, Result};

pub(crate) const MAX_SQL_SCRIPT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SQL_SCRIPT_STATEMENTS: usize = 256;
pub(crate) const MAX_SQL_SCRIPT_OPERATIONS: usize = 1_000_000;

const MAX_COMMENT_DEPTH: usize = 32;
const MAX_PARENTHESIS_DEPTH: usize = 256;

pub(crate) fn charge_operations(operations: &Cell<usize>, count: usize) -> Result<()> {
    let next = operations
        .get()
        .checked_add(count)
        .ok_or_else(script_too_large)?;
    if next > MAX_SQL_SCRIPT_OPERATIONS {
        return Err(EngineError::new(
            "TRANSACTION_TOO_LARGE",
            format!(
                "A SQL script cannot require more than {MAX_SQL_SCRIPT_OPERATIONS} row, index, and join operations"
            ),
        ));
    }
    operations.set(next);
    Ok(())
}

fn script_too_large() -> EngineError {
    EngineError::new("TRANSACTION_TOO_LARGE", "SQL script work limit exceeded")
}

/// Splits a SQL script at top-level statement terminators.
///
/// This scanner deliberately retains the original statement text so the normal SQL lexer remains
/// the sole authority for tokens and diagnostics. It recognizes every lexical region which can
/// legally hide a semicolon (strings, quoted identifiers, dollar-quoted strings, and nested block
/// or line comments) and does not terminate a statement while parentheses remain open.
pub(crate) fn split(sql: &str) -> Result<Vec<&str>> {
    if sql.len() > MAX_SQL_SCRIPT_BYTES {
        return Err(EngineError::invalid_query(format!(
            "SQL script exceeds the {MAX_SQL_SCRIPT_BYTES}-byte limit"
        )));
    }

    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut statement_start = 0usize;
    let mut position = 0usize;
    let mut parentheses = 0usize;
    let mut has_sql = false;

    while position < bytes.len() {
        match bytes[position] {
            b'\'' => {
                has_sql = true;
                position = scan_quoted(bytes, position, b'\'', "string literal")?;
            }
            b'"' => {
                has_sql = true;
                position = scan_quoted(bytes, position, b'"', "quoted identifier")?;
            }
            b'-' if bytes.get(position + 1) == Some(&b'-') => {
                position = scan_line_comment(bytes, position + 2);
            }
            b'/' if bytes.get(position + 1) == Some(&b'*') => {
                position = scan_block_comment(bytes, position + 2)?;
            }
            b'$' => {
                if let Some((delimiter_end, delimiter)) = dollar_delimiter(sql, position) {
                    has_sql = true;
                    position = scan_dollar_quote(sql, delimiter_end, delimiter)?;
                } else {
                    has_sql = true;
                    position += 1;
                }
            }
            b'(' => {
                has_sql = true;
                parentheses = parentheses.checked_add(1).ok_or_else(|| {
                    EngineError::invalid_query("SQL parenthesis nesting overflowed")
                })?;
                if parentheses > MAX_PARENTHESIS_DEPTH {
                    return Err(EngineError::invalid_query(format!(
                        "SQL parentheses cannot nest more than {MAX_PARENTHESIS_DEPTH} levels"
                    )));
                }
                position += 1;
            }
            b')' => {
                has_sql = true;
                parentheses = parentheses.saturating_sub(1);
                position += 1;
            }
            b';' if parentheses == 0 => {
                if has_sql {
                    push_statement(&mut statements, &sql[statement_start..=position])?;
                }
                position += 1;
                statement_start = position;
                has_sql = false;
            }
            byte if byte.is_ascii_whitespace() || byte == b';' => position += 1,
            _ => {
                has_sql = true;
                position += 1;
            }
        }
    }

    if has_sql {
        push_statement(&mut statements, &sql[statement_start..])?;
    }
    Ok(statements)
}

fn push_statement<'a>(statements: &mut Vec<&'a str>, statement: &'a str) -> Result<()> {
    if statements.len() == MAX_SQL_SCRIPT_STATEMENTS {
        return Err(EngineError::invalid_query(format!(
            "A SQL script cannot contain more than {MAX_SQL_SCRIPT_STATEMENTS} statements"
        )));
    }
    statements.push(statement);
    Ok(())
}

fn scan_quoted(bytes: &[u8], mut position: usize, quote: u8, description: &str) -> Result<usize> {
    position += 1;
    while position < bytes.len() {
        if bytes[position] != quote {
            position += 1;
            continue;
        }
        if bytes.get(position + 1) == Some(&quote) {
            position += 2;
            continue;
        }
        return Ok(position + 1);
    }
    Err(EngineError::parse_error(format!(
        "Unterminated {description} in SQL text"
    )))
}

fn scan_line_comment(bytes: &[u8], mut position: usize) -> usize {
    while position < bytes.len() && !matches!(bytes[position], b'\n' | b'\r') {
        position += 1;
    }
    position
}

fn scan_block_comment(bytes: &[u8], mut position: usize) -> Result<usize> {
    let mut depth = 1usize;
    while position < bytes.len() {
        if bytes.get(position..position + 2) == Some(b"/*") {
            depth += 1;
            if depth > MAX_COMMENT_DEPTH {
                return Err(EngineError::invalid_query(format!(
                    "SQL comments cannot nest more than {MAX_COMMENT_DEPTH} levels"
                )));
            }
            position += 2;
        } else if bytes.get(position..position + 2) == Some(b"*/") {
            depth -= 1;
            position += 2;
            if depth == 0 {
                return Ok(position);
            }
        } else {
            position += 1;
        }
    }
    Err(EngineError::parse_error(
        "Unterminated block comment in SQL text",
    ))
}

fn dollar_delimiter(sql: &str, start: usize) -> Option<(usize, &str)> {
    let bytes = sql.as_bytes();
    let mut position = start + 1;
    while let Some(byte) = bytes.get(position).copied() {
        if byte == b'$' {
            let tag = &bytes[start + 1..position];
            let valid = tag.is_empty()
                || (matches!(tag.first(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
                    && tag
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_'));
            return valid.then(|| (position + 1, &sql[start..=position]));
        }
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return None;
        }
        position += 1;
    }
    None
}

fn scan_dollar_quote(sql: &str, body_start: usize, delimiter: &str) -> Result<usize> {
    sql[body_start..]
        .find(delimiter)
        .map(|offset| body_start + offset + delimiter.len())
        .ok_or_else(|| EngineError::parse_error("Unterminated dollar-quoted string in SQL text"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_only_top_level_semicolons() {
        let sql = r#"
            INSERT INTO notes (id, body) VALUES (1, 'one;''two');
            SELECT "semi;colon" FROM notes WHERE id IN (1; 2);
            SELECT $tag$dollar;quoted$tag$ FROM notes;
        "#;
        let statements = split(sql).unwrap();
        assert_eq!(statements.len(), 3);
        assert!(statements[0].contains("'one;''two'"));
        assert!(statements[1].contains("\"semi;colon\""));
        assert!(statements[1].contains("(1; 2)"));
        assert!(statements[2].contains("$tag$dollar;quoted$tag$"));
    }

    #[test]
    fn ignores_semicolons_and_statement_like_text_in_comments() {
        let sql = r#"
            -- ; DROP TABLE hidden;
            CREATE TABLE notes (id INTEGER PRIMARY KEY);
            /* outer ; SELECT ignored;
               /* nested ; INSERT ignored; */
            */
            SELECT * FROM notes; -- trailing ; UPDATE ignored;
        "#;
        let statements = split(sql).unwrap();
        assert_eq!(statements.len(), 2);
        assert!(statements[0].contains("CREATE TABLE notes"));
        assert!(statements[1].contains("SELECT * FROM notes"));
    }

    #[test]
    fn accepts_empty_comments_only_consecutive_and_trailing_terminators() {
        assert!(split("").unwrap().is_empty());
        assert!(
            split("  -- only ;\n /* comments ; */  ")
                .unwrap()
                .is_empty()
        );
        assert!(split(";;; /* gap */ ;").unwrap().is_empty());
        assert_eq!(split("SELECT * FROM notes;;;;").unwrap().len(), 1);
        assert_eq!(split("SELECT * FROM notes").unwrap().len(), 1);
    }

    #[test]
    fn rejects_unterminated_lexical_regions() {
        for sql in ["SELECT 'missing", "SELECT \"missing", "SELECT /* missing"] {
            assert_eq!(split(sql).unwrap_err().code, "SQL_PARSE_ERROR");
        }
        assert_eq!(
            split("SELECT $tag$missing").unwrap_err().code,
            "SQL_PARSE_ERROR"
        );
    }

    #[test]
    fn enforces_script_statement_and_nesting_limits() {
        let too_long = "x".repeat(MAX_SQL_SCRIPT_BYTES + 1);
        assert_eq!(split(&too_long).unwrap_err().code, "INVALID_QUERY");

        let at_limit = "SELECT * FROM notes;".repeat(MAX_SQL_SCRIPT_STATEMENTS);
        assert_eq!(split(&at_limit).unwrap().len(), MAX_SQL_SCRIPT_STATEMENTS);
        let too_many = format!("{at_limit}SELECT * FROM notes;");
        assert_eq!(split(&too_many).unwrap_err().code, "INVALID_QUERY");

        let too_deep = format!(
            "SELECT * FROM notes WHERE {}id = 1{};",
            "(".repeat(MAX_PARENTHESIS_DEPTH + 1),
            ")".repeat(MAX_PARENTHESIS_DEPTH + 1)
        );
        assert_eq!(split(&too_deep).unwrap_err().code, "INVALID_QUERY");

        let nested_comment = format!(
            "{}SELECT * FROM notes{}",
            "/*".repeat(MAX_COMMENT_DEPTH + 1),
            "*/".repeat(MAX_COMMENT_DEPTH + 1)
        );
        assert_eq!(split(&nested_comment).unwrap_err().code, "INVALID_QUERY");
    }
}
