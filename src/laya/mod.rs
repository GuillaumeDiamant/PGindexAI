pub mod types;
pub use types::*;

use reqwest::Client;
use std::collections::HashMap;
use tracing::debug;

/// HTTP client to the Laya server
#[derive(Clone)]
pub struct LayaClient {
    client: Client,
    base_url: String,
}

impl LayaClient {
    pub fn new(base_url: &str, timeout_secs: u64) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .build()
                .expect("failed to build reqwest client"),
            base_url: base_url.to_string(),
        }
    }

    /// DEEP health check: sends a real test decision payload
    /// to the /v1/systemone endpoint and verifies that the response is structured
    /// and coherent (choice present, confidence in [0,1]).
    ///
    /// This test detects cases where /health responds but the decision
    /// pipeline (model, variant, response parsing) is broken.
    pub async fn deep_health(&self) -> Result<String, LayaError> {
        let payload = LayaClient::build_payload(
            "deep-health-check",
            HashMap::from([
                ("0".to_string(), "Option A: no action needed".to_string()),
                (
                    "1".to_string(),
                    "Option B: perform a maintenance action".to_string(),
                ),
            ]),
        );

        let response = self.decision(&payload).await?;

        // Structural validation of the response
        let choice = response.answers.action.choice.as_str();
        let confidence = response.answers.action.confidence;

        if confidence < 0.0 || confidence > 1.0 {
            return Err(LayaError::InvalidResponse(format!(
                "confidence out of range [0,1]: {}",
                confidence
            )));
        }
        if choice != "0" && choice != "1" {
            return Err(LayaError::InvalidResponse(format!(
                "unexpected choice '{}': expected '0' or '1'",
                choice
            )));
        }

        debug!(
            "Deep health OK: choice={}, confidence={}",
            choice, confidence
        );
        Ok(format!("choice={}, confidence={:.2}", choice, confidence))
    }

    /// Send a decision question to Laya
    pub async fn decision(&self, payload: &LayaPayload) -> Result<LayaResponse, LayaError> {
        let url = format!("{}/v1/systemone", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    LayaError::Unreachable(format!("{}: {}", self.base_url, e))
                } else {
                    LayaError::Http(format!("reqwest error: {}", e))
                }
            })?;

        if resp.status().is_success() {
            let body = resp
                .text()
                .await
                .map_err(|e| LayaError::Http(format!("read error: {}", e)))?;
            let response: LayaResponse =
                serde_json::from_str(&body).map_err(|e| LayaError::Deserialize(body.clone(), e))?;
            Ok(response)
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(LayaError::Http(format!(
                "Laya returned HTTP {}: {}",
                status, body
            )))
        }
    }

    /// Build the payload for a decision proposal
    pub fn build_payload(state: &str, criteria: HashMap<String, String>) -> LayaPayload {
        let mut creds = criteria;
        // Make sure the keys are in the order 0,1,2,...
        let mut ordered = HashMap::new();
        for i in 0..creds.len() {
            let key = i.to_string();
            if let Some(val) = creds.remove(&key) {
                ordered.insert(key, val);
            }
        }
        LayaPayload {
            state: state.to_string(),
            questions: types::Questions {
                action: types::ActionQuestion {
                    type_: "choice".to_string(),
                    instructions:
                        "Choose the single best action for this PostgreSQL index situation."
                            .to_string(),
                    criteria: ordered,
                },
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LayaError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Deserialize error (body: {0}): {1}")]
    Deserialize(String, serde_json::Error),
    #[error("Laya not reachable at {0}")]
    Unreachable(String),
    #[error("Invalid Laya response: {0}")]
    InvalidResponse(String),
}
