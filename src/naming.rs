//! Explicit naming convention for indexes.
//!
//! PostgreSQL limits identifiers to 63 bytes (`NAMEDATALEN - 1`).
//! Any index name generation must respect this limit and be
//! validated before being injected into DDL.

/// PostgreSQL limit for identifiers (63 bytes, not extensible)
pub const MAX_IDENTIFIER_LEN: usize = 63;

/// Result of validating a PostgreSQL identifier name
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedName {
    /// The name as-is
    AsIs(String),
    /// The name was truncated to respect the limit
    Truncated(String),
}

/// Error validating an index name
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NamingError {
    #[error("Index name is empty after cleaning")]
    Empty,
}

/// Validate a PostgreSQL identifier.
///
/// Rules:
/// - 1 to 63 bytes
/// - first character: `[a-z_]`
/// - rest: `[a-z0-9_]`
/// - if the name is valid but too long → propose a truncated version
///   (returns `Ok(Truncated)`), otherwise return an error.
pub fn validate_identifier(raw: &str) -> Result<ValidatedName, NamingError> {
    let cleaned: String = raw
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();

    if cleaned.is_empty() {
        return Err(NamingError::Empty);
    }

    let first = cleaned.chars().next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        // Leading digit: prefix with '_'
        let cleaned = format!("_{}", cleaned);
        let bytes = cleaned.len();
        if bytes > MAX_IDENTIFIER_LEN {
            let truncated = truncate_chars(&cleaned, MAX_IDENTIFIER_LEN);
            return Ok(ValidatedName::Truncated(truncated));
        }
        return Ok(ValidatedName::AsIs(cleaned));
    }

    let bytes = cleaned.len();
    if bytes > MAX_IDENTIFIER_LEN {
        let truncated = truncate_chars(&cleaned, MAX_IDENTIFIER_LEN);
        return Ok(ValidatedName::Truncated(truncated));
    }
    Ok(ValidatedName::AsIs(cleaned))
}

/// Truncates a string to `max` characters (validated names are ASCII,
/// so characters = bytes here) without splitting a multi-byte character.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        result.push(c);
    }
    // Strip a trailing '_' if any to keep a "clean" identifier
    while result.ends_with('_') {
        result.pop();
    }
    result
}

/// Generate the name of an index for a table + column, respecting the
/// `idx_<table>_<col>` convention and the 63-byte limit.
///
/// If the limit is exceeded, the column is shortened (short hash as suffix
/// to preserve uniqueness between long columns).
pub fn generate_index_name(schema: &str, table: &str, column: &str) -> Result<String, NamingError> {
    let _ = schema; // the schema does not enter the name (the index is qualified in the DDL)
    let base = format!("idx_{}_{}", table, column);
    match validate_identifier(&base)? {
        ValidatedName::AsIs(n) => Ok(n),
        ValidatedName::Truncated(n) => {
            // Guarantee uniqueness: if two long columns truncate the same way,
            // add a short hash of the full column.
            let hash = short_hash(&base);
            let budget = MAX_IDENTIFIER_LEN - hash.len() - 1; // +1 for '_'
            let head = truncate_chars(&n, budget);
            Ok(format!("{}_{}", head, hash))
        }
    }
}

/// Deterministic short (FNV-1a) hash in base 36 ([0-9a-z]), stable across runs
fn short_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let digit = |d: u64| -> char {
        if d < 10 {
            (b'0' + d as u8) as char
        } else {
            (b'a' + (d - 10) as u8) as char
        }
    };
    let mut s36 = String::new();
    let mut v = hash;
    while v > 0 {
        s36.push(digit(v % 36));
        v /= 36;
    }
    s36.chars().take(6).collect()
}

/// Checks that an identifier (from the PostgreSQL catalog, e.g. names of
/// schema / table / index) is a "simple" unquoted identifier:
/// `[a-z_][a-z0-9_]*`, <= 63 bytes.
///
/// Only these identifiers may be injected into generated DDL or into
/// a Laya payload (exotic/quoted identifiers are excluded from the
/// management scope — logged as a warning, never executed).
pub fn is_simple_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_IDENTIFIER_LEN {
        return false;
    }
    let first = bytes[0] as char;
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_simple() {
        assert_eq!(
            validate_identifier("idx_orders_status").unwrap(),
            ValidatedName::AsIs("idx_orders_status".to_string())
        );
    }

    #[test]
    fn test_validate_uppercase_normalized() {
        assert_eq!(
            validate_identifier("IDX_ORDERS_STATUS").unwrap(),
            ValidatedName::AsIs("idx_orders_status".to_string())
        );
    }

    #[test]
    fn test_validate_leading_digit() {
        assert_eq!(
            validate_identifier("9lives").unwrap(),
            ValidatedName::AsIs("_9lives".to_string())
        );
    }

    #[test]
    fn test_validate_empty() {
        assert_eq!(validate_identifier("   ").unwrap_err(), NamingError::Empty);
        assert_eq!(validate_identifier("///").unwrap_err(), NamingError::Empty);
    }

    #[test]
    fn test_generate_truncates_and_stays_unique() {
        let long_table = "a_very_long_table_name_that_exceeds_the_identifier_limit_for_sure";
        let col_a = "some_really_long_column_name_a";
        let col_b = "some_really_long_column_name_b";

        let n1 = generate_index_name("public", long_table, col_a).unwrap();
        let n2 = generate_index_name("public", long_table, col_b).unwrap();

        assert!(n1.len() <= MAX_IDENTIFIER_LEN, "n1 len = {}", n1.len());
        assert!(n2.len() <= MAX_IDENTIFIER_LEN, "n2 len = {}", n2.len());
        assert_ne!(n1, n2, "names must differ for different columns");
        assert!(is_simple_identifier(&n1));
        assert!(is_simple_identifier(&n2));
    }

    #[test]
    fn test_is_simple_identifier() {
        assert!(is_simple_identifier("idx_ok"));
        assert!(is_simple_identifier("_private"));
        assert!(!is_simple_identifier("9bad"));
        assert!(!is_simple_identifier(""));
        assert!(!is_simple_identifier("MyIndex")); // uppercase = quoted identifier
        assert!(!is_simple_identifier("a-b"));
        assert!(!is_simple_identifier(&"a".repeat(64)));
        assert!(is_simple_identifier(&"a".repeat(63)));
    }
}
