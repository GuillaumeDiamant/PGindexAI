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
    /// Bearer token for the Laya channel (CWE-306 fix, v0.3.3). `None` =
    /// unauthenticated channel (pre-v0.3.3 deployments; the daemon warns at
    /// startup). A blank key is normalized to `None` — an empty bearer would
    /// only produce 401s.
    api_key: Option<String>,
}

impl LayaClient {
    pub fn new(base_url: &str, timeout_secs: u64, api_key: Option<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .build()
                .expect("failed to build reqwest client"),
            base_url: base_url.to_string(),
            api_key: api_key.filter(|k| !k.trim().is_empty()),
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
        let mut request = self.client.post(&url).json(payload);
        // CWE-306 (v0.3.3): authenticate the channel. When a key is
        // configured it is sent on EVERY decision call — Laya (LAYA_API_KEY)
        // answers 401 without it, which we map to a hard fail-closed error
        // below (no decision may ever be taken on an unauthenticated channel
        // we cannot prove is the real engine).
        if let Some(key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let resp = request.send().await.map_err(|e| {
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
            if status == 401 {
                // Fail-closed: the engine rejected the bearer token (or
                // requires one that was not sent). Continuing would mean
                // trusting a channel we cannot authenticate — block every
                // decision until the keys match.
                tracing::warn!(
                    "Laya rejected the bearer token (401) at {url} — check that \
                     `laya.api_key` in the daemon config equals `LAYA_API_KEY` of the \
                     Laya service. ALL decisions are blocked until this is fixed."
                );
                return Err(LayaError::Unauthorized);
            }
            Err(LayaError::Http(format!(
                "Laya returned HTTP {}: {}",
                status, body
            )))
        }
    }

    /// Build the payload for a decision proposal.
    ///
    /// Criteria are stored in a `BTreeMap` (see [`types::ActionQuestion`]) so
    /// the serialized JSON keys are emitted in deterministic sorted order.
    /// Callers may pass a plain `HashMap`; the conversion is done here.
    pub fn build_payload(state: &str, criteria: HashMap<String, String>) -> LayaPayload {
        LayaPayload {
            state: state.to_string(),
            questions: types::Questions {
                action: types::ActionQuestion {
                    type_: "choice".to_string(),
                    instructions:
                        "Choose the single best action for this PostgreSQL index situation."
                            .to_string(),
                    criteria: criteria.into_iter().collect(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-shot local HTTP responder for transport-level tests (CI has no
    /// Laya binary): reads one request, records the `Authorization` header,
    /// and replies either 200 (a structurally valid LayaResponse) or 401.
    /// Returns `(base_url, handle)` — joining the handle yields the captured
    /// Authorization header value (`None` when absent).
    fn mock_laya(reject_401: bool) -> (String, std::thread::JoinHandle<Option<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|e| panic!("mock_laya: accept failed: {e}"));
            let mut data: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            // Read the request head (ends at \r\n\r\n)
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        data.extend_from_slice(&buf[..n]);
                        if data.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(e) => panic!("mock_laya: head read failed: {e}"),
                }
            }
            let head = String::from_utf8_lossy(&data).to_string();
            let head_end = head.find("\r\n\r\n").unwrap() + 4;
            let head_part = &head[..head_end];
            let content_length = head_part
                .lines()
                .find_map(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower.strip_prefix("content-length:").map(str::to_string)
                })
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            // Drain the body (part of it may already be buffered)
            let mut have = data.len() - head_end;
            while have < content_length {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => have += n,
                    Err(_) => break,
                }
            }
            let auth = head_part
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, value)| value.trim().to_string());
            let (status, body) = if reject_401 {
                (
                    "401 Unauthorized",
                    r#"{"error":"Invalid or missing bearer token"}"#,
                )
            } else {
                (
                    "200 OK",
                    r#"{"answers":{"action":{"choice":"0","confidence":0.42}}}"#,
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .and_then(|_| stream.flush())
                .unwrap_or_else(|e| panic!("mock_laya: response write failed: {e}"));
            auth
        });
        (addr, handle)
    }

    /// CWE-306 (v0.3.3): when a key is configured, EVERY decision call must
    /// carry `Authorization: Bearer <key>` — the header the Laya engine
    /// (LAYA_API_KEY set) validates with a 401 otherwise.
    #[test]
    fn test_decision_sends_bearer_header() {
        let (addr, handle) = mock_laya(false);
        let client = LayaClient::new(&addr, 5, Some("test-key-123".to_string()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt
            .block_on(async {
                client
                    .decision(&LayaClient::build_payload("state", HashMap::new()))
                    .await
            })
            .expect("a 200 responder must yield a parsed decision");
        assert_eq!(resp.answers.action.choice, "0");

        let auth = handle
            .join()
            .unwrap()
            .expect("the Authorization header must be sent");
        assert_eq!(
            auth, "Bearer test-key-123",
            "the header must be exactly `Bearer <key>`"
        );
    }

    /// Without a configured key the daemon must not send a garbage header
    /// (pre-v0.3.3 deployments keep working — unauthenticated channel, the
    /// daemon warns at startup).
    #[test]
    fn test_decision_without_key_sends_no_auth_header() {
        let (addr, handle) = mock_laya(false);
        let client = LayaClient::new(&addr, 5, None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt
            .block_on(async {
                client
                    .decision(&LayaClient::build_payload("state", HashMap::new()))
                    .await
            })
            .expect("an unauthenticated channel still parses the decision");
        assert_eq!(resp.answers.action.choice, "0");

        assert!(
            handle.join().unwrap().is_none(),
            "without a key, NO Authorization header must be sent"
        );
    }

    /// CWE-306 (v0.3.3): a 401 from Laya must surface as
    /// `LayaError::Unauthorized` — FAIL-CLOSED: no decision may be taken on a
    /// channel the daemon cannot authenticate.
    #[test]
    fn test_401_maps_to_unauthorized_fail_closed() {
        let (addr, handle) = mock_laya(true);
        let client = LayaClient::new(&addr, 5, Some("wrong-key".to_string()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(async {
                client
                    .decision(&LayaClient::build_payload("state", HashMap::new()))
                    .await
            })
            .expect_err("a 401 must be an error, never a decision");
        assert!(
            matches!(err, LayaError::Unauthorized),
            "401 must map to Unauthorized (fail-closed), got: {err:?}"
        );
        let _ = handle.join();
    }

    #[test]
    fn test_build_payload_criteria_are_ordered() {
        // Regression (v0.1.1): criteria must serialize in deterministic
        // "0", "1", "2" order. With a `HashMap` the JSON key order was
        // arbitrary (hash-seeded); now a `BTreeMap` guarantees sorted order.
        // Insert the keys in a deliberately scrambled order to prove the
        // output order does NOT depend on insertion order.
        let criteria = [
            ("2".to_string(), "Option C: rebuild".to_string()),
            ("0".to_string(), "Option A: no action".to_string()),
            ("1".to_string(), "Option B: drop index".to_string()),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let payload = LayaClient::build_payload("state", criteria);
        let json = serde_json::to_string(&payload).unwrap();
        // Keys are serialized as "0":, "1":, "2": — assert sorted order.
        let i0 = json.find("\"0\":").expect("key 0 missing");
        let i1 = json.find("\"1\":").expect("key 1 missing");
        let i2 = json.find("\"2\":").expect("key 2 missing");
        assert!(i0 < i1 && i1 < i2, "criteria keys out of order: {json}");
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
    /// CWE-306 (v0.3.3): Laya answered 401 — the bearer token is missing or
    /// wrong. FAIL-CLOSED: the daemon must not take any decision on a channel
    /// it cannot authenticate (a responder on the port could otherwise
    /// approve every proposal).
    #[error(
        "Laya rejected the bearer token (HTTP 401) — `laya.api_key` (daemon) must \
         equal `LAYA_API_KEY` (Laya service); all decisions are BLOCKED"
    )]
    Unauthorized,
}
