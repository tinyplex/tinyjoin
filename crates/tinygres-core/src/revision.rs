use crate::{EngineError, Result};

/// Highest database revision represented exactly by JavaScript's `number` type.
pub(crate) const MAX_DATABASE_REVISION: u64 = 9_007_199_254_740_991;

pub(crate) fn validate_database_revision(revision: u64) -> Result<u64> {
    if revision <= MAX_DATABASE_REVISION {
        Ok(revision)
    } else {
        Err(revision_overflow())
    }
}

pub(crate) fn next_database_revision(revision: u64) -> Result<u64> {
    let next = revision.checked_add(1).ok_or_else(revision_overflow)?;
    validate_database_revision(next)
}

pub(crate) fn revision_overflow() -> EngineError {
    EngineError::new(
        "REVISION_OVERFLOW",
        format!(
            "Database revision cannot exceed the JavaScript-safe integer limit {MAX_DATABASE_REVISION}"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_revision_bound_is_exact_and_checked() {
        assert_eq!(
            validate_database_revision(MAX_DATABASE_REVISION).unwrap(),
            MAX_DATABASE_REVISION
        );
        assert_eq!(
            validate_database_revision(MAX_DATABASE_REVISION + 1)
                .unwrap_err()
                .code,
            "REVISION_OVERFLOW"
        );
        assert_eq!(
            next_database_revision(MAX_DATABASE_REVISION - 1).unwrap(),
            MAX_DATABASE_REVISION
        );
        assert_eq!(
            next_database_revision(MAX_DATABASE_REVISION)
                .unwrap_err()
                .code,
            "REVISION_OVERFLOW"
        );
        assert_eq!(
            next_database_revision(u64::MAX).unwrap_err().code,
            "REVISION_OVERFLOW"
        );
    }
}
