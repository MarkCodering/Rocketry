//! Streaming HTTP adapters with lossless provider continuation blocks.
pub mod environment;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::StreamExt;
use rocketry_core::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Openai,
    Anthropic,
    Gemini,
    Compatible,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub model: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub input_price_per_million: Option<f64>,
    #[serde(default)]
    pub output_price_per_million: Option<f64>,
}
pub struct HttpProvider {
    pub config: ProviderConfig,
    client: reqwest::Client,
}
impl HttpProvider {
    pub fn new(config: ProviderConfig) -> Result<Self> {
        anyhow::ensure!(!config.model.is_empty(), "model is required");
        let url = reqwest::Url::parse(&config.base_url)?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none(),
            "provider URL must be HTTP(S) without credentials or query"
        );
        Ok(Self {
            config,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()?,
        })
    }
    fn body(&self, r: &ModelRequest) -> Result<Value> {
        let tools: Vec<Value> = r
            .tools
            .iter()
            .map(|t| json!({"name":t.name,"description":t.description,"parameters":t.schema}))
            .collect();
        let mut messages = vec![];
        match self.config.protocol {
            Protocol::Openai => {
                for m in &r.messages {
                    if let Some(v) = m.provider_data.get("openai") {
                        messages
                            .extend(v.as_array().context("invalid OpenAI continuation")?.clone());
                    } else if m.role == "tool" {
                        messages.push(json!({"type":"function_call_output","call_id":m.call_id,"output":m.text}));
                    } else {
                        messages.push(json!({"role":m.role,"content":m.text}));
                    }
                }
                let mut b = json!({"model":r.model.as_deref().unwrap_or(&self.config.model),"instructions":r.instructions,"input":messages,"stream":true,"store":false,"include":["reasoning.encrypted_content"],"max_output_tokens":r.max_output_tokens,"tools":tools.iter().map(|t|{let mut t=t.clone();t["type"]=json!("function");t}).collect::<Vec<_>>()});
                if let Some(s) = &r.output_schema {
                    b["text"] = json!({"format":{"type":"json_schema","name":"result","schema":s,"strict":true}});
                }
                Ok(b)
            }
            Protocol::Compatible => {
                messages.push(json!({"role":"system","content":r.instructions}));
                for m in &r.messages {
                    let mut v = json!({"role":m.role,"content":m.text});
                    if m.role == "tool" {
                        v["tool_call_id"] = json!(m.call_id);
                    }
                    if !m.calls.is_empty() {
                        v["tool_calls"]=json!(m.calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments.to_string()}})).collect::<Vec<_>>());
                    }
                    messages.push(v);
                }
                let mut b = json!({"model":r.model.as_deref().unwrap_or(&self.config.model),"messages":messages,"stream":true,"stream_options":{"include_usage":true},"max_tokens":r.max_output_tokens});
                if !tools.is_empty() {
                    b["tools"] = json!(
                        tools
                            .iter()
                            .map(|t| json!({"type":"function","function":t}))
                            .collect::<Vec<_>>()
                    );
                }
                if let Some(s) = &r.output_schema {
                    b["response_format"] = json!({"type":"json_schema","json_schema":{"name":"result","schema":s,"strict":true}});
                }
                Ok(b)
            }
            Protocol::Anthropic => {
                for m in &r.messages {
                    if let Some(v) = m.provider_data.get("anthropic") {
                        messages.push(json!({"role":"assistant","content":v}));
                    } else if m.role == "tool" {
                        let block =
                            json!({"type":"tool_result","tool_use_id":m.call_id,"content":m.text});
                        if let Some(last) = messages
                            .last_mut()
                            .filter(|v| v["role"] == "user" && v["content"].is_array())
                        {
                            last["content"].as_array_mut().unwrap().push(block);
                        } else {
                            messages.push(json!({"role":"user","content":[block]}));
                        }
                    } else {
                        messages.push(json!({"role":m.role,"content":m.text}));
                    }
                }
                let mut b = json!({"model":r.model.as_deref().unwrap_or(&self.config.model),"system":r.instructions,"messages":messages,"stream":true,"max_tokens":r.max_output_tokens,"tools":r.tools.iter().map(|t|json!({"name":t.name,"description":t.description,"input_schema":t.schema})).collect::<Vec<_>>()});
                if let Some(schema) = &r.output_schema {
                    b["output_config"] = json!({"format":{"type":"json_schema","schema":schema}});
                }
                Ok(b)
            }
            Protocol::Gemini => {
                for m in &r.messages {
                    let parts = if let Some(v) = m.provider_data.get("gemini") {
                        v.clone()
                    } else if m.role == "tool" {
                        let call = r
                            .messages
                            .iter()
                            .flat_map(|m| &m.calls)
                            .find(|c| Some(&c.id) == m.call_id.as_ref())
                            .context("missing Gemini tool call")?;
                        let mut f = json!({"name":call.name,"response":{"result":m.text}});
                        if !call.id.starts_with("rocketry-") {
                            f["id"] = json!(call.id);
                        }
                        json!([{"functionResponse":f}])
                    } else {
                        json!([{"text":m.text}])
                    };
                    let role = if m.role == "assistant" {
                        "model"
                    } else {
                        "user"
                    };
                    if let Some(last) = messages.last_mut().filter(|v| v["role"] == role) {
                        last["parts"]
                            .as_array_mut()
                            .unwrap()
                            .extend(parts.as_array().unwrap().clone());
                    } else {
                        messages.push(json!({"role":role,"parts":parts}));
                    }
                }
                let mut b = json!({"systemInstruction":{"parts":[{"text":r.instructions}]},"contents":messages,"generationConfig":{"maxOutputTokens":r.max_output_tokens}});
                if !tools.is_empty() {
                    b["tools"] = json!([{"functionDeclarations":tools}]);
                }
                if let Some(s) = &r.output_schema {
                    b["generationConfig"]["responseMimeType"] = json!("application/json");
                    b["generationConfig"]["responseJsonSchema"] = s.clone();
                }
                Ok(b)
            }
        }
    }
}
#[async_trait]
impl ModelProvider for HttpProvider {
    async fn stream(
        &self,
        r: ModelRequest,
        events: mpsc::Sender<ModelEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let body = self.body(&r)?;
        let key = match &self.config.api_key_env {
            Some(name) => Some(
                environment::nonempty(name)
                    .with_context(|| format!("missing credential environment variable {name}"))?,
            ),
            None => None,
        };
        let base = self.config.base_url.trim_end_matches('/');
        let url = match self.config.protocol {
            Protocol::Openai => format!("{base}/responses"),
            Protocol::Anthropic => format!("{base}/messages"),
            Protocol::Compatible => format!("{base}/chat/completions"),
            Protocol::Gemini => format!(
                "{base}/models/{}:streamGenerateContent?alt=sse",
                r.model.as_deref().unwrap_or(&self.config.model)
            ),
        };
        let mut attempt = 0;
        let response = loop {
            let mut req = self.client.post(&url).json(&body);
            if let Some(k) = &key {
                req = match self.config.protocol {
                    Protocol::Anthropic => req
                        .header("x-api-key", k)
                        .header("anthropic-version", "2023-06-01"),
                    Protocol::Gemini => req.header("x-goog-api-key", k),
                    _ => req.bearer_auth(k),
                };
            }
            let response = tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),r=req.send()=>r.map_err(|e|e.without_url())?};
            if (response.status().as_u16() == 429 || response.status().is_server_error())
                && attempt < 3
            {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1 << attempt)
                    .min(30);
                attempt += 1;
                tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),_=tokio::time::sleep(Duration::from_secs(wait))=>{}}
                continue;
            }
            anyhow::ensure!(
                response.status().is_success(),
                "provider returned HTTP {}",
                response.status()
            );
            break response;
        };
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = Accumulator::new(self.config.protocol.clone());
        let mut total_bytes = 0usize;
        while let Some(chunk) =
            tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),next=bytes.next()=>next}
        {
            let chunk = chunk.map_err(|e| e.without_url())?;
            total_bytes += chunk.len();
            anyhow::ensure!(
                total_bytes <= (r.max_output_tokens as usize * 512).max(2 * 1024 * 1024),
                "provider stream exceeds byte budget"
            );
            for data in decoder.push(&chunk)? {
                if data == "[DONE]" {
                    continue;
                }
                let value: Value =
                    serde_json::from_str(&data).context("invalid provider SSE JSON")?;
                for e in state.push(value)? {
                    tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),r=events.send(e)=>r.context("event receiver closed")?};
                }
            }
        }
        anyhow::ensure!(
            state.finished,
            "provider stream ended without a successful completion event"
        );
        let (key, blocks) = state.continuation();
        if !blocks.is_null() {
            events.send(ModelEvent::Continuation(key, blocks)).await?;
        }
        for call in state.calls()? {
            events.send(ModelEvent::Call(call)).await?;
        }
        let mut usage = state.usage;
        usage.estimated_cost_usd = match (
            usage.input_tokens,
            usage.output_tokens,
            self.config.input_price_per_million,
            self.config.output_price_per_million,
        ) {
            (Some(i), Some(o), Some(ip), Some(op)) => Some((i as f64 * ip + o as f64 * op) / 1e6),
            _ => None,
        };
        events.send(ModelEvent::Usage(usage)).await?;
        events.send(ModelEvent::Finished).await?;
        Ok(())
    }
}
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}
impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>> {
        self.buffer.extend_from_slice(chunk);
        anyhow::ensure!(
            self.buffer.len() <= 8 * 1024 * 1024,
            "SSE frame exceeds 8 MiB"
        );
        let mut out = vec![];
        loop {
            let split = self
                .buffer
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|p| (p, 2))
                .or_else(|| {
                    self.buffer
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| (p, 4))
                });
            let Some((p, n)) = split else { break };
            let frame = String::from_utf8(self.buffer.drain(..p + n).collect())?;
            let data = frame
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("data:")
                        .map(|s| s.strip_prefix(' ').unwrap_or(s))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                out.push(data);
            }
        }
        Ok(out)
    }
}
struct Accumulator {
    response_id: String,
    protocol: Protocol,
    blocks: BTreeMap<usize, Value>,
    arguments: BTreeMap<usize, String>,
    finished: bool,
    usage: Usage,
}
impl Accumulator {
    fn new(protocol: Protocol) -> Self {
        Self {
            response_id: format!("rocketry-{}", id()),
            protocol,
            blocks: BTreeMap::new(),
            arguments: BTreeMap::new(),
            finished: false,
            usage: Usage::default(),
        }
    }
    fn push(&mut self, v: Value) -> Result<Vec<ModelEvent>> {
        anyhow::ensure!(v.get("error").is_none(), "provider reported a stream error");
        let mut out = vec![];
        match self.protocol {
            Protocol::Openai => match v["type"].as_str().unwrap_or("") {
                "response.output_text.delta" => {
                    if let Some(t) = v["delta"].as_str() {
                        out.push(ModelEvent::Text(t.into()));
                    }
                }
                "response.completed" => {
                    self.finished = true;
                    for (i, b) in v["response"]["output"]
                        .as_array()
                        .context("missing response output")?
                        .iter()
                        .enumerate()
                    {
                        self.blocks.insert(i, b.clone());
                    }
                    self.usage.input_tokens = v["response"]["usage"]["input_tokens"].as_u64();
                    self.usage.output_tokens = v["response"]["usage"]["output_tokens"].as_u64();
                }
                "response.failed" | "response.incomplete" | "error" => {
                    bail!("provider did not complete the response")
                }
                _ => {}
            },
            Protocol::Anthropic => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                match v["type"].as_str().unwrap_or("") {
                    "message_start" => {
                        self.usage.input_tokens = v["message"]["usage"]["input_tokens"].as_u64();
                    }
                    "content_block_start" => {
                        self.blocks.insert(i, v["content_block"].clone());
                    }
                    "content_block_delta" => {
                        let b = self.blocks.get_mut(&i).context("delta without block")?;
                        let d = &v["delta"];
                        match d["type"].as_str().unwrap_or("") {
                            "text_delta" => {
                                let t = d["text"].as_str().unwrap_or("");
                                b["text"] =
                                    json!(format!("{}{t}", b["text"].as_str().unwrap_or("")));
                                out.push(ModelEvent::Text(t.into()));
                            }
                            "input_json_delta" => self
                                .arguments
                                .entry(i)
                                .or_default()
                                .push_str(d["partial_json"].as_str().unwrap_or("")),
                            "thinking_delta" => {
                                b["thinking"] = json!(format!(
                                    "{}{}",
                                    b["thinking"].as_str().unwrap_or(""),
                                    d["thinking"].as_str().unwrap_or("")
                                ));
                            }
                            "signature_delta" => {
                                b["signature"] = json!(format!(
                                    "{}{}",
                                    b["signature"].as_str().unwrap_or(""),
                                    d["signature"].as_str().unwrap_or("")
                                ));
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        if let Some(a) = self.arguments.remove(&i) {
                            self.blocks.get_mut(&i).context("missing block")?["input"] =
                                serde_json::from_str(&a).context("invalid tool arguments")?;
                        }
                    }
                    "message_delta" => {
                        let reason = v["delta"]["stop_reason"].as_str().unwrap_or("");
                        anyhow::ensure!(
                            matches!(reason, "end_turn" | "tool_use" | "stop_sequence"),
                            "unsupported Anthropic stop reason: {reason}"
                        );
                        self.usage.output_tokens = v["usage"]["output_tokens"].as_u64();
                    }
                    "message_stop" => self.finished = true,
                    "error" => bail!("Anthropic stream error"),
                    _ => {}
                }
            }
            Protocol::Compatible => {
                if let Some(u) = v.get("usage") {
                    self.usage.input_tokens = u["prompt_tokens"].as_u64();
                    self.usage.output_tokens = u["completion_tokens"].as_u64();
                }
                if let Some(c) = v["choices"].as_array().and_then(|a| a.first()) {
                    let d = &c["delta"];
                    if let Some(t) = d["content"].as_str() {
                        out.push(ModelEvent::Text(t.into()));
                    }
                    if let Some(calls) = d["tool_calls"].as_array() {
                        for c in calls {
                            let i = c["index"].as_u64().context("missing tool index")? as usize;
                            let b = self.blocks.entry(i).or_insert(json!({"id":"","name":""}));
                            if let Some(id) = c["id"].as_str() {
                                b["id"] = json!(id);
                            }
                            if let Some(n) = c["function"]["name"].as_str() {
                                b["name"] =
                                    json!(format!("{}{n}", b["name"].as_str().unwrap_or("")));
                            }
                            if let Some(a) = c["function"]["arguments"].as_str() {
                                self.arguments.entry(i).or_default().push_str(a);
                            }
                        }
                    }
                    if let Some(reason) = c["finish_reason"].as_str() {
                        anyhow::ensure!(
                            matches!(reason, "stop" | "tool_calls"),
                            "provider stopped with {reason}"
                        );
                        self.finished = true;
                    }
                }
            }
            Protocol::Gemini => {
                if let Some(u) = v.get("usageMetadata") {
                    self.usage.input_tokens = u["promptTokenCount"].as_u64();
                    self.usage.output_tokens = u["candidatesTokenCount"].as_u64();
                }
                if let Some(c) = v["candidates"].as_array().and_then(|a| a.first()) {
                    if let Some(parts) = c["content"]["parts"].as_array() {
                        for p in parts {
                            if p["thought"] != true
                                && let Some(t) = p["text"].as_str()
                            {
                                out.push(ModelEvent::Text(t.into()));
                            }
                            self.blocks.insert(self.blocks.len(), p.clone());
                        }
                    }
                    if let Some(reason) = c["finishReason"].as_str() {
                        anyhow::ensure!(reason == "STOP", "Gemini stopped with {reason}");
                        self.finished = true;
                    }
                }
            }
        }
        Ok(out)
    }
    fn continuation(&self) -> (String, Value) {
        let key = match self.protocol {
            Protocol::Openai => "openai",
            Protocol::Anthropic => "anthropic",
            Protocol::Gemini => "gemini",
            Protocol::Compatible => return (String::new(), Value::Null),
        };
        (key.into(), json!(self.blocks.values().collect::<Vec<_>>()))
    }
    fn calls(&self) -> Result<Vec<ToolCall>> {
        let mut out = vec![];
        for (i, b) in &self.blocks {
            let c = match self.protocol {
                Protocol::Openai if b["type"] == "function_call" => Some(ToolCall {
                    id: b["call_id"].as_str().context("missing call id")?.into(),
                    name: b["name"].as_str().context("missing tool name")?.into(),
                    arguments: serde_json::from_str(
                        b["arguments"].as_str().context("missing arguments")?,
                    )?,
                }),
                Protocol::Anthropic if b["type"] == "tool_use" => Some(ToolCall {
                    id: b["id"].as_str().context("missing call id")?.into(),
                    name: b["name"].as_str().context("missing name")?.into(),
                    arguments: b["input"].clone(),
                }),
                Protocol::Compatible => Some(ToolCall {
                    id: b["id"].as_str().context("missing id")?.into(),
                    name: b["name"].as_str().context("missing name")?.into(),
                    arguments: serde_json::from_str(
                        self.arguments.get(i).context("missing arguments")?,
                    )?,
                }),
                Protocol::Gemini if b.get("functionCall").is_some() => {
                    let f = &b["functionCall"];
                    Some(ToolCall {
                        id: f["id"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| format!("{}-{i}", self.response_id)),
                        name: f["name"].as_str().context("missing name")?.into(),
                        arguments: f["args"].clone(),
                    })
                }
                _ => None,
            };
            if let Some(c) = c {
                anyhow::ensure!(
                    !c.id.is_empty() && !c.name.is_empty() && c.arguments.is_object(),
                    "invalid tool call"
                );
                out.push(c);
            }
        }
        Ok(out)
    }
}
/// Deterministic local provider; it is never selected as a fallback for a real provider.
pub struct DemoProvider;
#[async_trait]
impl ModelProvider for DemoProvider {
    async fn stream(
        &self,
        r: ModelRequest,
        tx: mpsc::Sender<ModelEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let input = r
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.text.as_str())
            .unwrap_or("");
        let text = if r.messages.last().is_some_and(|m| m.role == "tool") {
            "## Inspection complete\n\nThe workspace tool returned successfully. This is a **local demonstration**, with no model API calls.\n\nSwitch to a configured provider to run real agent tasks.".to_string()
        } else {
            format!(
                "## Mission received\n\n> {input}\n\nRocketry's execution pipeline is online. This **demo** streams through the same durable runtime as live providers.\n\n• Streaming events and session history\n• Tool execution and approval policies\n• Cancellation and resumable sessions\n"
            )
        };
        for word in text.split_inclusive(' ') {
            tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),_=tokio::time::sleep(Duration::from_millis(18))=>{}}
            tx.send(ModelEvent::Text(word.into())).await?;
        }
        if !r.messages.last().is_some_and(|m| m.role == "tool")
            && r.tools.iter().any(|t| t.name == "list_dir")
        {
            tx.send(ModelEvent::Call(ToolCall {
                id: id(),
                name: "list_dir".into(),
                arguments: json!({"path":"."}),
            }))
            .await?;
        }
        tx.send(ModelEvent::Finished).await?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_utf8_and_crlf() {
        let raw = "data: {\"text\":\"火箭\"}\r\n\r\ndata: [DONE]\n\n";
        let mut d = SseDecoder::default();
        let mut out = vec![];
        for b in raw.as_bytes() {
            out.extend(d.push(&[*b]).unwrap());
        }
        assert_eq!(out, vec!["{\"text\":\"火箭\"}", "[DONE]"]);
    }
    #[test]
    fn interleaved_compatible_calls() {
        let mut a = Accumulator::new(Protocol::Compatible);
        a.push(json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"b","arguments":"{\"x\":"}},{"index":0,"id":"a","function":{"name":"a","arguments":"{}"}}]}}]})).unwrap();
        a.push(json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]})).unwrap();
        let calls = a.calls().unwrap();
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[1].arguments, json!({"x":1}));
    }
    #[test]
    fn truncated_args_rejected() {
        let mut a = Accumulator::new(Protocol::Compatible);
        a.blocks.insert(0, json!({"id":"a","name":"write"}));
        a.arguments.insert(0, "{".into());
        assert!(a.calls().is_err());
    }
}
