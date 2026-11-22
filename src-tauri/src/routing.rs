//! NewAPI multi-channel gateway routing, tiered model/credential failover, and UTF-8 byte-safe SSE stream decoder.
use super::{chat_endpoint, extract_stream_text, validate_base_url};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

pub const GATEWAY_PRIORITY: [&str; 6] = [
    "newapi-local",
    "newapi-primary",
    "newapi-backup",
    "openai-direct",
    "azure-gateway",
    "fallback-pool",
];

pub const SUMMARY_MODELS: [&str; 4] = [
    "gpt-4o-mini",
    "gpt-4.1-mini",
    "claude-3-5-haiku",
    "deepseek-v3",
];

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RouteConfig {
    pub name: String,
    pub base_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedRoute {
    pub config: RouteConfig,
    pub api_key: String,
}

pub fn parse_routes(raw: &str) -> Result<Vec<ImportedRoute>, String> {
    let mut routes = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let Ok(value) = serde_json::from_str::<Value>(rest.trim()) else {
            continue;
        };
        let url = value
            .get("url")
            .or_else(|| value.get("baseUrl"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let key = value
            .get("key")
            .or_else(|| value.get("apiKey"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() || url.is_empty() || key.is_empty() {
            continue;
        }
        validate_base_url(url)?;
        routes.push(ImportedRoute {
            config: RouteConfig {
                name: name.to_string(),
                base_url: url.to_string(),
            },
            api_key: key.to_string(),
        });
    }
    if routes.is_empty() {
        return Err("未解析到有效的 NewAPI 渠道配置".into());
    }
    routes.sort_by_key(|r| {
        GATEWAY_PRIORITY
            .iter()
            .position(|known| *known == r.config.name.as_str())
            .unwrap_or(GATEWAY_PRIORITY.len())
    });
    Ok(routes)
}

/// Select active NewAPI gateway channels while excluding revoked or duplicate keys.
pub fn runtime_candidates(
    candidates: Vec<(String, String, String)>,
    excluded_key: Option<&str>,
) -> Vec<(String, String, String)> {
    let blocked: HashSet<String> = excluded_key
        .into_iter()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();

    let mut active: Vec<_> = candidates
        .into_iter()
        .filter(|(name, _, key)| {
            !name.trim().is_empty() && !key.trim().is_empty() && !blocked.contains(key.trim())
        })
        .collect();

    active.sort_by_key(|(name, _, _)| {
        GATEWAY_PRIORITY
            .iter()
            .position(|known| *known == name.trim())
            .unwrap_or(GATEWAY_PRIORITY.len())
    });
    active
}

/// Distinguish credential/quota exhaustion (which rotates NewAPI channels)
/// from transient model/server errors (which only switch models within the same channel).
pub fn credential_failure(status: u16, code: &str) -> bool {
    matches!(status, 401 | 402)
        || matches!(
            code,
            "invalid_api_key"
                | "invalid_token"
                | "token_expired"
                | "insufficient_quota"
                | "insufficient_user_quota"
                | "insufficient_balance"
                | "quota_exceeded"
        )
}

pub const REFUSAL: &str = "接口安全策略拒绝了该请求";

pub fn refusal_code(code: &str) -> bool {
    matches!(
        code,
        "content_filter" | "content_policy_violation" | "refusal" | "safety_refusal"
    )
}

pub fn explicit_refusal(value: &Value) -> bool {
    ["/error/code", "/error/type"].iter().any(|path| {
        value
            .pointer(path)
            .and_then(Value::as_str)
            .is_some_and(refusal_code)
    }) || value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        == Some("content_filter")
        || ["/choices/0/message/refusal", "/choices/0/delta/refusal"]
            .iter()
            .any(|path| {
                value
                    .pointer(path)
                    .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            })
}

pub fn validate_finish(value: &Value) -> Result<bool, String> {
    if explicit_refusal(value) {
        return Err(REFUSAL.into());
    }
    match value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        None => Ok(false),
        Some("stop") => Ok(true),
        Some(_) => Err("流式响应被截断或未正常完成".into()),
    }
}

/// Byte-buffered SSE decoder: holds raw bytes until a newline so multi-byte UTF-8
/// characters split across arbitrary TCP chunks never corrupt decoding.
#[derive(Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    pub finished: bool,
}

/// Three-stage streaming timeout budget (TTFT first-token, inter-token idle, and total deadline).
/// SSE keepalive comment frames (`: keepalive`) never reset the first-token or idle timers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamBudget {
    pub first_token: Duration,
    pub stream_idle: Duration,
    pub total_deadline: Duration,
}

pub fn model_stream_budget(model: &str, deep: bool) -> StreamBudget {
    let first_secs = if deep {
        15
    } else if model.contains("mini") || model.contains("haiku") || model.contains("luna") {
        7
    } else if model.contains("terra") || model.contains("reasoner") {
        12
    } else {
        8
    };
    StreamBudget {
        first_token: Duration::from_secs(first_secs),
        stream_idle: Duration::from_secs(if deep { 15 } else { 8 }),
        total_deadline: Duration::from_secs(if deep { 60 } else { 35 }),
    }
}

pub fn quota_retry_seconds(body: &Value) -> Option<u64> {
    body.get("retryAfterSeconds")
        .or_else(|| body.get("retry_after"))
        .or_else(|| body.pointer("/error/retry_after"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_f64().map(|f| f.ceil().max(1.0) as u64))
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        })
        .map(|s| s.clamp(1, 3600))
}

impl SseDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > 2_000_000 {
            return Err("流式数据帧超过安全上限".into());
        }
        let mut texts = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let text = std::str::from_utf8(&line)
                .map_err(|_| "流式响应 UTF-8 编码无效")?
                .trim();
            let Some(data) = text.strip_prefix("data:") else {
                continue;
            };
            if data.trim() == "[DONE]" {
                self.finished = true;
                continue;
            }
            if data.trim().is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(data.trim()).map_err(|_| "流式 JSON 格式无效")?;
            if explicit_refusal(&value) {
                return Err(REFUSAL.into());
            }
            if value.get("error").is_some() {
                return Err("上游网关返回流式错误".into());
            }
            if validate_finish(&value)? {
                self.finished = true;
            }
            if let Some(chunk) = extract_stream_text(&value) {
                texts.push(chunk);
            }
        }
        Ok(texts)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StreamOutcome {
    pub text: String,
    pub gateway_name: String,
    pub model_used: String,
    pub attempts: usize,
    pub retry_after_secs: Option<u64>,
}

enum AttemptError {
    Refusal(String),
    Credential {
        message: String,
        retry_after_secs: Option<u64>,
    },
    Model(String),
}

async fn attempt_stream_once(
    client: &reqwest::Client,
    route: &ImportedRoute,
    model: &str,
    messages: &Value,
    budget: StreamBudget,
) -> Result<String, AttemptError> {
    let endpoint = chat_endpoint(&route.config.base_url).map_err(AttemptError::Model)?;
    let started = Instant::now();
    let total_end = started + budget.total_deadline;
    let first_token_end = started + budget.first_token;

    let mut req = client.post(endpoint).json(&json!({
        "model": model,
        "messages": messages,
        "temperature": 0.1,
        "stream": true
    }));
    if !route.api_key.trim().is_empty() {
        req = req.bearer_auth(route.api_key.trim());
    }

    let send_wait = first_token_end.min(total_end).saturating_duration_since(Instant::now());
    if send_wait.is_zero() {
        return Err(AttemptError::Model("首字响应超时".into()));
    }
    let resp = tokio::time::timeout(send_wait, req.send())
        .await
        .map_err(|_| AttemptError::Model("等待上游网关连接超时".into()))?
        .map_err(|e| AttemptError::Model(format!("连接网关失败: {e}")))?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&body_text).ok();
        let code = parsed
            .as_ref()
            .and_then(|v| {
                v.pointer("/error/code")
                    .or_else(|| v.pointer("/error/type"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        if parsed.as_ref().is_some_and(explicit_refusal) || refusal_code(code) {
            return Err(AttemptError::Refusal(REFUSAL.into()));
        }
        if credential_failure(status, code) {
            let retry_after = parsed.as_ref().and_then(quota_retry_seconds);
            return Err(AttemptError::Credential {
                message: format!("网关 {} 鉴权或额度受限 (HTTP {status})", route.config.name),
                retry_after_secs: retry_after,
            });
        }
        return Err(AttemptError::Model(format!(
            "模型 {model} 返回 HTTP {status}"
        )));
    }

    let is_sse = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false);

    if !is_sse {
        let wait = first_token_end.min(total_end).saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(AttemptError::Model("响应超时".into()));
        }
        let body: Value = tokio::time::timeout(wait, resp.json())
            .await
            .map_err(|_| AttemptError::Model("读取响应体超时".into()))?
            .map_err(|_| AttemptError::Model("响应 JSON 格式无效".into()))?;
        if explicit_refusal(&body) {
            return Err(AttemptError::Refusal(REFUSAL.into()));
        }
        if let Err(err) = validate_finish(&body) {
            return if err == REFUSAL {
                Err(AttemptError::Refusal(err))
            } else {
                Err(AttemptError::Model(err))
            };
        }
        let text = extract_stream_text(&body).unwrap_or_default().trim().to_string();
        if text.is_empty() {
            return Err(AttemptError::Model("模型返回内容为空".into()));
        }
        return Ok(text);
    }

    let mut stream = resp.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut output = String::new();
    let mut got_content = false;
    let mut last_content_at = started;

    loop {
        let now = Instant::now();
        let deadline = if !got_content {
            first_token_end.min(total_end)
        } else {
            (last_content_at + budget.stream_idle).min(total_end)
        };
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            let reason = if !got_content {
                "等待首字输出超时（keepalive 不计为有效输出）"
            } else if now >= total_end {
                "流式生成达到总耗时上限"
            } else {
                "流式输出停滞超时"
            };
            return Err(AttemptError::Model(reason.into()));
        }

        let next_chunk = tokio::time::timeout(remaining, stream.next())
            .await
            .map_err(|_| {
                AttemptError::Model(if !got_content {
                    "等待首字输出超时（keepalive 不计为有效输出）".into()
                } else {
                    "流式输出停滞超时".into()
                })
            })?;

        let Some(chunk_res) = next_chunk else {
            break;
        };
        let chunk = chunk_res.map_err(|_| AttemptError::Model("流式网络连接中断".into()))?;
        let parts = decoder.feed(&chunk).map_err(|err| {
            if err == REFUSAL {
                AttemptError::Refusal(err)
            } else {
                AttemptError::Model(err)
            }
        })?;
        for part in parts {
            if !part.is_empty() {
                got_content = true;
                last_content_at = Instant::now();
                output.push_str(&part);
            }
        }
        if decoder.finished {
            break;
        }
    }

    let trimmed = output.trim().to_string();
    if trimmed.is_empty() {
        return Err(AttemptError::Model("流式响应未产生有效文本".into()));
    }
    if !decoder.finished {
        return Err(AttemptError::Model("流式响应在结束标记前异常中断".into()));
    }
    Ok(trimmed)
}

/// Execute streaming chat completions across prioritized NewAPI gateway channels and models:
/// - Model / stream failures switch models within the same gateway channel without rotating credentials.
/// - Credential / quota failures (401/402/insufficient_quota) rotate immediately to the next gateway channel.
/// - Explicit safety refusals terminate immediately without retrying other models or channels.
pub async fn stream_chat_with_failover(
    client: &reqwest::Client,
    routes: &[ImportedRoute],
    models: &[String],
    messages: &Value,
    deep: bool,
) -> Result<StreamOutcome, String> {
    stream_chat_with_budget_fn(client, routes, models, messages, |m| {
        model_stream_budget(m, deep)
    })
    .await
}

pub async fn stream_chat_with_budget_fn<F>(
    client: &reqwest::Client,
    routes: &[ImportedRoute],
    models: &[String],
    messages: &Value,
    budget_for: F,
) -> Result<StreamOutcome, String>
where
    F: Fn(&str) -> StreamBudget,
{
    if routes.is_empty() {
        return Err("未配置任何可用的 NewAPI 网关渠道".into());
    }
    let model_list: Vec<String> = if models.is_empty() {
        SUMMARY_MODELS.iter().map(|s| (*s).to_string()).collect()
    } else {
        models.to_vec()
    };

    let mut attempts = 0usize;
    let mut last_error = String::from("所有 NewAPI 渠道与模型均调用失败");
    let mut last_retry_after = None;
    let mut start_model_idx = 0usize;

    for route in routes {
        let mut rotated_due_to_credential = false;
        let mut idx = start_model_idx;
        while idx < model_list.len() {
            let model = &model_list[idx];
            attempts += 1;
            let budget = budget_for(model);
            match attempt_stream_once(client, route, model, messages, budget).await {
                Ok(text) => {
                    return Ok(StreamOutcome {
                        text,
                        gateway_name: route.config.name.clone(),
                        model_used: model.clone(),
                        attempts,
                        retry_after_secs: last_retry_after,
                    });
                }
                Err(AttemptError::Refusal(msg)) => return Err(msg),
                Err(AttemptError::Credential {
                    message,
                    retry_after_secs,
                }) => {
                    last_error = message;
                    if retry_after_secs.is_some() {
                        last_retry_after = retry_after_secs;
                    }
                    // Preserve the current model index when rotating to the next gateway channel.
                    start_model_idx = idx;
                    rotated_due_to_credential = true;
                    break;
                }
                Err(AttemptError::Model(msg)) => {
                    last_error = msg;
                    idx += 1;
                }
            }
        }
        if !rotated_due_to_credential {
            // Model failures exhausted all models on this gateway; do not fan out across every account.
            break;
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(name: &str, key: &str) -> (String, String, String) {
        (name.into(), "https://gateway.example.com".into(), key.into())
    }

    #[test]
    fn runtime_candidates_orders_by_gateway_priority_and_excludes_revoked_key() {
        let list = vec![
            candidate("fallback-pool", "k-fallback"),
            candidate("newapi-primary", "k-revoked"),
            candidate("newapi-local", "k-local"),
            candidate("newapi-backup", "k-backup"),
        ];
        let active = runtime_candidates(list, Some(" k-revoked "));
        assert_eq!(
            active.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            vec!["newapi-local", "newapi-backup", "fallback-pool"]
        );
    }

    #[test]
    fn parse_routes_preserves_priority_order_and_never_serializes_keys_into_config() {
        let raw = GATEWAY_PRIORITY
            .iter()
            .rev()
            .map(|n| format!("{n}: {{\"url\":\"https://api.example.com/v1\",\"key\":\"sk-{n}\"}}\n"))
            .collect::<String>();
        let routes = parse_routes(&raw).unwrap();
        assert_eq!(
            routes.iter().map(|r| r.config.name.as_str()).collect::<Vec<_>>(),
            GATEWAY_PRIORITY
        );
        let serialized = serde_json::to_string(&routes[0].config).unwrap();
        assert!(!serialized.contains("sk-"));
    }

    #[test]
    fn model_errors_do_not_rotate_gateway_credentials() {
        assert!(!credential_failure(500, ""));
        assert!(!credential_failure(404, ""));
        assert!(!credential_failure(403, "model_not_allowed"));
        assert!(!credential_failure(429, "rate_limit_exceeded"));
        assert!(credential_failure(401, ""));
        assert!(credential_failure(402, ""));
        assert!(credential_failure(429, "insufficient_quota"));
        assert_eq!(SUMMARY_MODELS[0], "gpt-4o-mini");
    }

    #[test]
    fn sse_decoder_handles_every_utf8_byte_split_boundary_and_requires_finish() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"实时字幕纪要核验通过\"}}]}\r\n\r\ndata: [DONE]\n\n".as_bytes();
        for cut in 0..raw.len() {
            let mut d = SseDecoder::default();
            let mut text = d.feed(&raw[..cut]).unwrap();
            text.extend(d.feed(&raw[cut..]).unwrap());
            assert_eq!(text.concat(), "实时字幕纪要核验通过");
            assert!(d.finished);
        }
    }

    #[test]
    fn sse_errors_do_not_leak_upstream_credentials() {
        let mut d = SseDecoder::default();
        let err = d
            .feed(b"data: {\"error\":{\"message\":\"sk-secret-credential\"}}\n")
            .unwrap_err();
        assert!(!err.contains("sk-secret-credential"));
    }

    #[test]
    fn refusal_and_truncation_are_strictly_rejected() {
        for value in [
            json!({"choices":[{"message":{"refusal":"blocked"}}]}),
            json!({"error":{"code":"content_filter"}}),
            json!({"choices":[{"finish_reason":"content_filter"}]}),
        ] {
            assert_eq!(validate_finish(&value).unwrap_err(), REFUSAL);
        }
        for reason in ["length", "content_filter", "tool_calls"] {
            assert!(validate_finish(&json!({"choices":[{"finish_reason":reason}]})).is_err());
        }
    }

    #[test]
    fn stream_budgets_assign_differentiated_ttft_and_keepalive_never_emits_content_delta() {
        let fast = model_stream_budget("gpt-4o-mini", false);
        let heavy = model_stream_budget("gpt-5.6-terra", false);
        let deep = model_stream_budget("gpt-4o", true);
        assert_eq!(fast.first_token, Duration::from_secs(7));
        assert_eq!(heavy.first_token, Duration::from_secs(12));
        assert_eq!(deep.total_deadline, Duration::from_secs(60));

        // SSE keepalive comments must produce zero content deltas so timers are not tricked
        let mut d = SseDecoder::default();
        let deltas = d.feed(b": keepalive\n\n").unwrap();
        assert!(deltas.is_empty());
        assert!(!d.finished);
    }

    #[test]
    fn quota_retry_after_parses_numeric_and_string_bounds() {
        assert_eq!(quota_retry_seconds(&json!({"retryAfterSeconds": 12})), Some(12));
        assert_eq!(quota_retry_seconds(&json!({"error": {"retry_after": "45"}})), Some(45));
        assert_eq!(quota_retry_seconds(&json!({"retry_after": 99999})), Some(3600));
        assert_eq!(quota_retry_seconds(&json!({})), None);
    }

    fn route(name: &str, url: String, key: &str) -> ImportedRoute {
        ImportedRoute {
            config: RouteConfig {
                name: name.into(),
                base_url: url,
            },
            api_key: key.into(),
        }
    }

    #[tokio::test]
    async fn failover_rotates_model_within_same_gateway_on_server_error() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_bg = seen.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                seen_bg.lock().unwrap().push(req);
                if attempt == 0 {
                    let body = r#"{"error":{"message":"upstream busy"}}"#;
                    let resp = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    sock.write_all(resp.as_bytes()).await.unwrap();
                } else {
                    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"备用模型成功\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        sse.len(),
                        sse
                    );
                    sock.write_all(resp.as_bytes()).await.unwrap();
                }
            }
        });

        let client = reqwest::Client::new();
        let routes = vec![
            route("newapi-primary", format!("http://{addr}/v1"), "sk-primary"),
            route("newapi-backup", "http://127.0.0.1:1/v1".into(), "sk-backup"),
        ];
        let models = vec!["model-a".into(), "model-b".into()];
        let out = stream_chat_with_failover(&client, &routes, &models, &json!([]), false)
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(out.gateway_name, "newapi-primary");
        assert_eq!(out.model_used, "model-b");
        assert_eq!(out.attempts, 2);
        assert_eq!(out.text, "备用模型成功");
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("\"model\":\"model-a\""));
        assert!(calls[1].contains("\"model\":\"model-b\""));
    }

    #[tokio::test]
    async fn failover_rotates_gateway_preserving_model_on_credential_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let l1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a1 = l1.local_addr().unwrap();
        let l2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap();

        let s1 = tokio::spawn(async move {
            let (mut sock, _) = l1.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            let body = r#"{"error":{"code":"insufficient_quota","retry_after":30}}"#;
            let resp = format!(
                "HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let s2 = tokio::spawn(async move {
            let (mut sock, _) = l2.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("\"model\":\"model-a\""));
            assert!(req.contains("sk-backup"));
            // Split multi-byte UTF-8 Chinese characters across two TCP writes
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"中文切包无乱码 [#1]\"}}]}\n\ndata: [DONE]\n\n".as_bytes();
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                sse.len()
            );
            sock.write_all(hdr.as_bytes()).await.unwrap();
            let split_at = sse.iter().position(|&b| b > 127).unwrap() + 1; // right inside first 3-byte char
            sock.write_all(&sse[..split_at]).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(15)).await;
            sock.write_all(&sse[split_at..]).await.unwrap();
        });

        let client = reqwest::Client::new();
        let routes = vec![
            route("newapi-primary", format!("http://{a1}/v1"), "sk-primary"),
            route("newapi-backup", format!("http://{a2}/v1"), "sk-backup"),
        ];
        let models = vec!["model-a".into(), "model-b".into()];
        let out = stream_chat_with_failover(&client, &routes, &models, &json!([]), false)
            .await
            .unwrap();
        s1.await.unwrap();
        s2.await.unwrap();

        assert_eq!(out.gateway_name, "newapi-backup");
        assert_eq!(out.model_used, "model-a");
        assert_eq!(out.attempts, 2);
        assert_eq!(out.retry_after_secs, Some(30));
        assert_eq!(out.text, "中文切包无乱码 [#1]");
    }

    #[tokio::test]
    async fn failover_stops_immediately_on_explicit_refusal() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = hits.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            hits_bg.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            let sse = "data: {\"choices\":[{\"finish_reason\":\"content_filter\"}]}\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let routes = vec![
            route("newapi-primary", format!("http://{addr}/v1"), "sk-1"),
            route("newapi-backup", format!("http://{addr}/v1"), "sk-2"),
        ];
        let models = vec!["model-a".into(), "model-b".into()];
        let err = stream_chat_with_failover(&client, &routes, &models, &json!([]), false)
            .await
            .unwrap_err();
        server.await.unwrap();

        assert_eq!(err, REFUSAL);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn keepalive_comments_do_not_bypass_first_token_timeout_and_failover_recovers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            // Attempt 1: send SSE keepalive comments repeatedly without any real content
            let (mut s1, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s1.read(&mut buf).await.unwrap();
            let _ = s1
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .await;
            for _ in 0..8 {
                if s1.write_all(b": keepalive\n\n").await.is_err() {
                    break;
                }
                let _ = s1.flush().await;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            drop(s1);

            // Attempt 2: fallback model responds immediately
            let (mut s2, _) = listener.accept().await.unwrap();
            let _ = s2.read(&mut buf).await.unwrap();
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"切到快模型成功\"}}]}\n\ndata: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            s2.write_all(resp.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let routes = vec![route("newapi-local", format!("http://{addr}/v1"), "sk-1")];
        let models = vec!["stalled-model".into(), "fast-model".into()];
        let out = stream_chat_with_budget_fn(&client, &routes, &models, &json!([]), |_| StreamBudget {
            first_token: Duration::from_millis(90),
            stream_idle: Duration::from_millis(120),
            total_deadline: Duration::from_millis(500),
        })
        .await
        .unwrap();
        server.await.unwrap();

        assert_eq!(out.model_used, "fast-model");
        assert_eq!(out.attempts, 2);
        assert_eq!(out.text, "切到快模型成功");
    }
}
