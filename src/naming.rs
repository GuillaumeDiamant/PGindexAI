//! Convention de nommage explicite pour les index.
//!
//! PostgreSQL limite les identifiants à 63 octets (`NAMEDATALEN - 1`).
//! Toute génération de nom d'index doit respecter cette limite et être
//! validée avant d'être injectée dans un DDL.

/// Limite PostgreSQL pour les identifiants (63 octets, non extensible)
pub const MAX_IDENTIFIER_LEN: usize = 63;

/// Résultat d'une validation de nom d'identifiant PostgreSQL
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedName {
    /// Le nom est tel quel
    AsIs(String),
    /// Le nom a été tronqué pour respecter la limite
    Truncated(String),
}

/// Erreur de validation de nom d'index
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NamingError {
    #[error("Index name is empty after cleaning")]
    Empty,
}

/// Valider un identifiant PostgreSQL.
///
/// Règles :
/// - 1 à 63 octets
/// - première lettre : `[a-z_]`
/// - suite : `[a-z0-9_]`
/// - si le nom est valide mais trop long → on propose une version tronquée
///   (retourne `Ok(Truncated)`), sinon on renvoie une erreur.
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
        // Numéro en premier : on préfixe par '_'
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

/// Tronque une chaîne à `max` caractères (les noms validés sont ASCII,
/// donc caractères = octets ici) sans couper de caractère multi-octet.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        result.push(c);
    }
    // Retirer un '_' de fin éventuel pour garder un identifiant "propre"
    while result.ends_with('_') {
        result.pop();
    }
    result
}

/// Générer le nom d'un index pour une table + colonne, en respectant la convention
/// `idx_<table>_<col>` et la limite de 63 octets.
///
/// En cas de dépassement, la colonne est raccourcie (hash court en suffixe
/// pour préserver l'unicité entre colonnes longues).
pub fn generate_index_name(schema: &str, table: &str, column: &str) -> Result<String, NamingError> {
    let _ = schema; // le schéma n'entre pas dans le nom (l'index est qualifié au DDL)
    let base = format!("idx_{}_{}", table, column);
    match validate_identifier(&base)? {
        ValidatedName::AsIs(n) => Ok(n),
        ValidatedName::Truncated(n) => {
            // Garantir l'unicité : si deux colonnes longues se tronquent pareil,
            // on ajoute un hash court de la colonne complète.
            let hash = short_hash(&base);
            let budget = MAX_IDENTIFIER_LEN - hash.len() - 1; // +1 pour '_'
            let head = truncate_chars(&n, budget);
            Ok(format!("{}_{}", head, hash))
        }
    }
}

/// Hash déterministe (FNV-1a) court en base 36 ([0-9a-z]), stable entre exécutions
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

/// Vérifie qu'un identifiant (issu du catalogue PostgreSQL, ex. noms de
/// schéma / table / index) est un identifiant « simple » non-quoté :
/// `[a-z_][a-z0-9_]*`, <= 63 octets.
///
/// Seuls ces identifiants peuvent être injectés dans du DDL généré ou dans
/// un payload Laya (les identifiants exotiques/quotés sont exclus du périmètre
/// de gestion — loggé en warning, jamais exécuté).
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
        assert!(!is_simple_identifier("MyIndex")); // majuscules = identifiant quoté
        assert!(!is_simple_identifier("a-b"));
        assert!(!is_simple_identifier(&"a".repeat(64)));
        assert!(is_simple_identifier(&"a".repeat(63)));
    }
}
