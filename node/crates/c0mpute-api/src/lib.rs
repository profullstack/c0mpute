//! Typed client for the coordinator REST API. Mirrors the routes documented
//! in PRD §10, all mounted under `/video/api/v1`.

use anyhow::{Context, Result};
use c0mpute_proto::{Capabilities, TranscodeJob, TranscodeResult};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
pub struct CoordinatorClient {
    base: Url,
    http: Client,
    api_token: Option<String>,
}

impl CoordinatorClient {
    pub fn new(base: Url, api_token: Option<String>) -> Self {
        Self {
            base,
            http: Client::new(),
            api_token,
        }
    }

    fn url(&self, path: &str) -> Result<Url> {
        // base is e.g. https://depin.quest/video; we always join into
        // `/video/api/v1/...` so the caller passes a relative path like
        // "providers/heartbeat".
        let joined = self
            .base
            .join("api/v1/")
            .context("invalid base URL")?
            .join(path)
            .context("invalid path")?;
        Ok(joined)
    }

    fn req(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let url = self.url(path)?;
        let mut b = self.http.request(method, url);
        if let Some(token) = &self.api_token {
            b = b.bearer_auth(token);
        }
        Ok(b)
    }

    pub async fn heartbeat(&self, provider_id: Uuid, caps: &Capabilities) -> Result<()> {
        let path = format!("providers/{}/heartbeat", provider_id);
        let resp = self
            .req(reqwest::Method::POST, &path)?
            .json(&HeartbeatBody { capabilities: caps.clone() })
            .send()
            .await?;
        resp.error_for_status()?;
        Ok(())
    }

    pub async fn claim_job(&self) -> Result<Option<TranscodeJob>> {
        let resp = self
            .req(reqwest::Method::POST, "jobs/claim")?
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let resp = resp.error_for_status()?;
        let job = resp.json::<TranscodeJob>().await?;
        Ok(Some(job))
    }

    pub async fn complete_job(&self, result: &TranscodeResult) -> Result<()> {
        let path = format!("jobs/{}/complete", result.job_id);
        let resp = self
            .req(reqwest::Method::POST, &path)?
            .json(result)
            .send()
            .await?;
        resp.error_for_status()?;
        Ok(())
    }

    pub async fn fail_job(&self, job_id: Uuid, error: &str) -> Result<()> {
        let path = format!("jobs/{}/fail", job_id);
        let resp = self
            .req(reqwest::Method::POST, &path)?
            .json(&FailBody { error: error.into() })
            .send()
            .await?;
        resp.error_for_status()?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct HeartbeatBody {
    capabilities: Capabilities,
}

#[derive(Serialize, Deserialize)]
struct FailBody {
    error: String,
}
