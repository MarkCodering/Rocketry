use anyhow::{Context, Result};
use rocketry_core::*;
use rocketry_runtime::Harness;
use serde_json::{Value, json};
use std::collections::BTreeMap;
#[derive(Clone)]
pub enum Client {
    Local(Box<Harness>),
    Remote {
        url: String,
        token: String,
        http: reqwest::Client,
    },
}
impl Client {
    pub fn remote(url: String, token: String) -> Result<Self> {
        Ok(Self::Remote {
            url: url.trim_end_matches('/').into(),
            token,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
        })
    }
    async fn get(&self, path: &str) -> Result<Value> {
        let Self::Remote { url, token, http } = self else {
            unreachable!()
        };
        let r = http
            .get(format!("{url}{path}"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| e.without_url())?;
        anyhow::ensure!(
            r.status().is_success(),
            "server returned HTTP {}",
            r.status()
        );
        Ok(r.json().await?)
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let Self::Remote { url, token, http } = self else {
            unreachable!()
        };
        let r = http
            .post(format!("{url}{path}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let status = r.status();
        let body: Value = r.json().await?;
        anyhow::ensure!(
            status.is_success(),
            "{}",
            body["error"].as_str().unwrap_or("server request failed")
        );
        Ok(body)
    }
    pub async fn agents(&self) -> Result<BTreeMap<String, Agent>> {
        match self {
            Self::Local(h) => Ok(h.agents.clone()),
            _ => Ok(serde_json::from_value(self.get("/v1/agents").await?)?),
        }
    }
    pub async fn sessions(&self) -> Result<Vec<Session>> {
        match self {
            Self::Local(h) => h.store.sessions().await,
            _ => Ok(serde_json::from_value(self.get("/v1/sessions").await?)?),
        }
    }
    pub async fn runs(&self) -> Result<Vec<Run>> {
        match self {
            Self::Local(h) => h.store.runs().await,
            _ => Ok(serde_json::from_value(self.get("/v1/runs").await?)?),
        }
    }
    pub async fn messages(&self, id: &str) -> Result<Vec<Message>> {
        match self {
            Self::Local(h) => h.store.messages(id).await,
            _ => Ok(serde_json::from_value(
                self.get(&format!("/v1/sessions/{id}/messages")).await?,
            )?),
        }
    }
    pub async fn events(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        match self {
            Self::Local(h) => h.store.events(id, after, 500).await,
            _ => Ok(serde_json::from_value(
                self.get(&format!("/v1/runs/{id}/events?after={after}"))
                    .await?,
            )?),
        }
    }
    pub async fn start(&self, agent: &str, input: &str, session: Option<String>) -> Result<Run> {
        match self {
            Self::Local(h) => {
                let handle = h.start(agent, input, session).await?;
                h.store.run(&handle.id).await
            }
            _ => Ok(serde_json::from_value(
                self.post(
                    "/v1/runs",
                    json!({"agent":agent,"input":input,"session_id":session}),
                )
                .await?,
            )?),
        }
    }
    pub async fn cancel(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(h) => h.cancel(id).await,
            _ => {
                self.post(&format!("/v1/runs/{id}/cancel"), json!({}))
                    .await?;
                Ok(())
            }
        }
    }
    pub async fn resume(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(h) => {
                h.resume(id).await?;
                Ok(())
            }
            _ => {
                self.post(&format!("/v1/runs/{id}/resume"), json!({}))
                    .await?;
                Ok(())
            }
        }
    }
    pub async fn approve(&self, id: &str, allow: bool) -> Result<()> {
        match self {
            Self::Local(h) => {
                h.approve(id, allow).await?;
                Ok(())
            }
            _ => {
                self.post(&format!("/v1/approvals/{id}"), json!({"allow":allow}))
                    .await?;
                Ok(())
            }
        }
    }
    pub async fn run(&self, id: &str) -> Result<Run> {
        self.runs()
            .await?
            .into_iter()
            .find(|r| r.id == id)
            .context("unknown run")
    }
    pub async fn shutdown(&self) {
        if let Self::Local(h) = self {
            h.shutdown().await;
        }
    }
}
