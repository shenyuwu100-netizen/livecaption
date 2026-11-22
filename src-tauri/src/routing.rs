//! NewAPI multi-channel gateway routing, tiered model/credential failover, and UTF-8 byte-safe SSE stream decoder.
use super::{extract_stream_text, validate_base_url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

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
}
