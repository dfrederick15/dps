use std::sync::Arc;
use std::time::Duration;

use reqwest::{
    header::{HeaderMap, HeaderValue, COOKIE},
    Client, Response,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::debug;

use crate::config::{AuthConfig, ProxmoxConfig};
use crate::error::{Error, Result};

// ── Session state for password/ticket auth ───────────────────────────────────

#[derive(Debug, Clone)]
struct Ticket {
    cookie: String,
    csrf:   String,
}

// ── Client ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ProxmoxClient {
    http:    Client,
    base:    String,
    auth:    AuthConfig,
    ticket:  Arc<Mutex<Option<Ticket>>>,
}

impl ProxmoxClient {
    pub fn new(cfg: &ProxmoxConfig) -> Result<Self> {
        let http = Client::builder()
            .danger_accept_invalid_certs(!cfg.verify_ssl)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Config(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            http,
            base:   cfg.base_url(),
            auth:   cfg.auth.clone(),
            ticket: Arc::new(Mutex::new(None)),
        })
    }

    // ── Auth ──────────────────────────────────────────────────────────────────

    /// Authenticate (no-op for token auth; fetches a ticket for password auth).
    pub async fn authenticate(&self) -> Result<()> {
        match &self.auth {
            AuthConfig::Token { .. } => Ok(()),
            AuthConfig::Password { user, realm, password } => {
                #[derive(Deserialize)]
                struct TicketResponse {
                    ticket:                String,
                    #[serde(rename = "CSRFPreventionToken")]
                    csrf_prevention_token: String,
                }

                let login = format!("{user}@{realm}");
                let params = [("username", login.as_str()), ("password", password.as_str())];

                let resp = self
                    .http
                    .post(format!("{}/access/ticket", self.base))
                    .form(&params)
                    .send()
                    .await?;

                let raw = extract_data(resp).await?;
                let data: TicketResponse = serde_json::from_value(raw)
                    .map_err(|e| Error::ProxmoxApi { status: 200, message: e.to_string() })?;
                *self.ticket.lock().await = Some(Ticket {
                    cookie: data.ticket,
                    csrf:   data.csrf_prevention_token,
                });
                Ok(())
            }
        }
    }

    /// Build request headers for auth.
    async fn auth_headers(&self, is_write: bool) -> HeaderMap {
        let mut map = HeaderMap::new();
        match &self.auth {
            AuthConfig::Token { user, token_name, token_value } => {
                let hv = HeaderValue::from_str(&format!(
                    "PVEAPIToken={user}!{token_name}={token_value}"
                ))
                .expect("invalid auth header");
                map.insert("Authorization", hv);
            }
            AuthConfig::Password { .. } => {
                if let Some(t) = self.ticket.lock().await.as_ref() {
                    let cookie = format!("PVEAuthCookie={}", t.cookie);
                    map.insert(COOKIE, HeaderValue::from_str(&cookie).unwrap());
                    if is_write {
                        map.insert(
                            "CSRFPreventionToken",
                            HeaderValue::from_str(&t.csrf).unwrap(),
                        );
                    }
                }
            }
        }
        map
    }

    // ── Low-level HTTP helpers ────────────────────────────────────────────────

    pub async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        debug!("GET {url}");
        let headers = self.auth_headers(false).await;
        let resp = self.http.get(&url).headers(headers).send().await?;
        extract_data(resp).await
    }

    #[allow(dead_code)]
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        debug!("POST {url}");
        let headers = self.auth_headers(true).await;
        let resp = self.http.post(&url).headers(headers).json(body).send().await?;
        extract_data(resp).await
    }

    pub async fn post_form(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<Value> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        debug!("POST (form) {url}");
        let headers = self.auth_headers(true).await;
        let resp = self.http.post(&url).headers(headers).form(params).send().await?;
        extract_data(resp).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        debug!("DELETE {url}");
        let headers = self.auth_headers(true).await;
        let resp = self.http.delete(&url).headers(headers).send().await?;
        extract_data(resp).await
    }

    // ── Task polling ──────────────────────────────────────────────────────────

    /// Poll a Proxmox task UPID until it completes (or times out).
    pub async fn wait_for_task(&self, node: &str, upid: &str) -> Result<()> {
        let path = format!("nodes/{node}/tasks/{upid}/status");
        let max_polls = 120; // 120 × 2 s = 4 minutes
        for _ in 0..max_polls {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let v = self.get(&path).await?;
            let status = v["status"].as_str().unwrap_or("");
            debug!("task {upid} status: {status}");
            match status {
                "stopped" => {
                    let exit = v["exitstatus"].as_str().unwrap_or("unknown");
                    return if exit == "OK" {
                        Ok(())
                    } else {
                        Err(Error::TaskFailed(format!("task {upid} exited with: {exit}")))
                    };
                }
                "running" => continue,
                other => {
                    return Err(Error::TaskFailed(format!(
                        "unexpected task state: {other}"
                    )))
                }
            }
        }
        Err(Error::Timeout(format!("task {upid}")))
    }
}

// ── Response extraction ───────────────────────────────────────────────────────

/// Unwrap the `{ "data": ... }` envelope that Proxmox wraps all responses in.
async fn extract_data(resp: Response) -> Result<Value> {
    let status = resp.status();

    // Read the body as text first so we can provide a useful error on parse
    // failure and handle the empty-body case (some endpoints return 200 + "").
    let text = resp.text().await?;

    if text.trim().is_empty() {
        return if status.is_success() {
            Ok(Value::Null)
        } else {
            Err(Error::ProxmoxApi { status: status.as_u16(), message: status.to_string() })
        };
    }

    let body: Value = serde_json::from_str(&text).map_err(|e| {
        let snippet = &text[..text.len().min(300)];
        Error::ProxmoxApi {
            status:  status.as_u16(),
            message: format!("could not parse Proxmox response as JSON: {e} — body: {snippet}"),
        }
    })?;

    if status.is_success() {
        Ok(body["data"].clone())
    } else {
        let message = body["errors"]
            .as_object()
            .and_then(|e| {
                Some(
                    e.iter()
                        .map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or("?")))
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            })
            .or_else(|| body["message"].as_str().map(String::from))
            .unwrap_or_else(|| status.to_string());

        Err(Error::ProxmoxApi { status: status.as_u16(), message })
    }
}
