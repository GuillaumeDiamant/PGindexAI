use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ─────────────────────────────────────────────
// Payload sent to Laya /v1/systemone
// ─────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct LayaPayload {
    pub state: String,
    pub questions: Questions,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Questions {
    pub action: ActionQuestion,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActionQuestion {
    #[serde(rename = "type")]
    pub type_: String,
    pub instructions: String,
    /// `BTreeMap` (not `HashMap`): the serialized JSON keys appear in
    /// deterministic sorted order ("0" < "1" < "2" < …). A `HashMap` iterates
    /// in arbitrary (hash-seeded) order — Laya must not depend on key order,
    /// but we make it stable regardless.
    pub criteria: BTreeMap<String, String>,
}

// ─────────────────────────────────────────────
// Response received from Laya
// ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LayaResponse {
    pub answers: Answers,
    pub usage: Usage,
}

#[derive(Debug, Deserialize)]
pub struct Answers {
    pub action: ActionAnswer,
}

#[derive(Debug, Deserialize)]
pub struct ActionAnswer {
    pub choice: String,
    pub confidence: f64,
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
}

// ─────────────────────────────────────────────
// Internal daemon types
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecisionAction {
    Ignore,
    CreateIndex,
    CreateFilteredIndex,
    DropIndex,
    RebuildIndex,
}

impl std::fmt::Display for DecisionAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecisionAction::Ignore => write!(f, "IGNORE"),
            DecisionAction::CreateIndex => write!(f, "CREATE INDEX"),
            DecisionAction::CreateFilteredIndex => write!(f, "CREATE INDEX (filtered)"),
            DecisionAction::DropIndex => write!(f, "DROP INDEX"),
            DecisionAction::RebuildIndex => write!(f, "REINDEX"),
        }
    }
}

/// An action proposal resulting from the analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(
        serialize_with = "serialize_action",
        deserialize_with = "deserialize_action"
    )]
    pub action: DecisionAction,
    pub ddl: String,
    pub reason: String,
    pub confidence: f64,
    pub table: String,
    /// Schema of the table concerned (filled in for CREATE INDEX).
    #[serde(default)]
    pub schema: String,
    /// Name of the index (filled in for CREATE INDEX; for DROP/REINDEX it is
    /// already carried by `ddl`).
    #[serde(default)]
    pub index_name: Option<String>,
    /// Indexed column (filled in for CREATE INDEX).
    #[serde(default)]
    pub column: Option<String>,
    /// Index access method for CREATE INDEX: "btree" | "gin" | "gist".
    /// Additive field — `#[serde(default)]` keeps backward compatibility with
    /// Laya payloads / older proposals that predate GIN/GIST detection.
    #[serde(default)]
    pub index_method: Option<String>,
}

fn serialize_action<S>(action: &DecisionAction, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match action {
        DecisionAction::Ignore => serializer.serialize_str("IGNORE"),
        DecisionAction::CreateIndex => serializer.serialize_str("CREATE INDEX"),
        DecisionAction::CreateFilteredIndex => serializer.serialize_str("CREATE INDEX (filtered)"),
        DecisionAction::DropIndex => serializer.serialize_str("DROP INDEX"),
        DecisionAction::RebuildIndex => serializer.serialize_str("REINDEX"),
    }
}

fn deserialize_action<'de, D>(deserializer: D) -> Result<DecisionAction, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(match s.as_str() {
        "IGNORE" => DecisionAction::Ignore,
        "CREATE INDEX" => DecisionAction::CreateIndex,
        "CREATE INDEX (filtered)" => DecisionAction::CreateFilteredIndex,
        "DROP INDEX" => DecisionAction::DropIndex,
        "REINDEX" => DecisionAction::RebuildIndex,
        _ => DecisionAction::Ignore,
    })
}
