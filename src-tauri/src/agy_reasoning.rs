//! Antigravity IDE 推理深度代理。
//!
//! IDE 的 language_server 自己会给主模型请求带上 `generationConfig.thinkingConfig`，
//! 但没有可用的命令行开关。这里在 CC Switch 内部起一个只监听 127.0.0.1 的小代理：
//! IDE 的 `GOOGLE_GEMINI_BASE_URL` 指向它，它只改写与所选模型完全匹配的
//! `thinkingBudget`，再补上 Bearer 与 x-goog-api-key 转发给真实中转站。
//! 辅助请求（如 gemini-3.1-flash-lite）原样透传。

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};

/// 供应商卡片 `settings_config.env` 里保存推理深度的键。
pub const THINKING_ENV: &str = "ANTIGRAVITY_CCSWITCH_THINKING";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDepth {
    Low,
    Medium,
    High,
}

impl ThinkingDepth {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Gemini 的 thinkingBudget：数值越大思考预算越多。实测 0 仍会产生少量思考，
    /// 所以“关闭”靠不启用代理来实现，而不是传 0。
    pub fn thinking_budget(self) -> u32 {
        match self {
            Self::Low => 2048,
            Self::Medium => 8192,
            Self::High => 24576,
        }
    }
}

/// 从供应商 settings 读取推理深度；未设置或值非法都当作 auto（不启用代理）。
pub fn depth_of(settings: &Value) -> Option<ThinkingDepth> {
    settings
        .pointer("/env")
        .and_then(|env| env.get(THINKING_ENV))
        .and_then(Value::as_str)
        .and_then(ThinkingDepth::parse)
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// 真实上游地址，可带或不带 /v1beta。
    pub upstream: String,
    pub api_key: String,
    /// 需要改写的模型名（与 language_server 的 --override_model_name 一致）。
    pub model: String,
    pub depth: ThinkingDepth,
}

#[derive(Clone)]
struct Shared {
    settings: Arc<RwLock<Settings>>,
    client: reqwest::Client,
}

struct ProxyHandle {
    base_url: String,
    settings: Arc<RwLock<Settings>>,
}

static PROXY: OnceLock<Mutex<Option<ProxyHandle>>> = OnceLock::new();

/// 启动（或复用）进程内代理，返回应写入 GOOGLE_GEMINI_BASE_URL 的地址。
pub async fn ensure(settings: Settings) -> Result<String, String> {
    let slot = PROXY.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().await;
    if let Some(handle) = guard.as_ref() {
        *handle.settings.write().await = settings;
        return Ok(handle.base_url.clone());
    }

    let depth = settings.depth;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("启动推理深度代理失败: {error}"))?;
    let addr: SocketAddr = listener
        .local_addr()
        .map_err(|error| format!("读取推理深度代理地址失败: {error}"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("创建推理深度代理客户端失败: {error}"))?;
    let shared = Shared {
        settings: Arc::new(RwLock::new(settings)),
        client,
    };
    let app = Router::new()
        .fallback(any(proxy))
        .with_state(shared.clone());
    tauri::async_runtime::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            log::warn!("推理深度代理退出: {error}");
        }
    });

    let base_url = format!("http://{addr}");
    log::info!(
        "Antigravity 推理深度代理已启动: {base_url}（默认深度 {}）",
        depth.as_str()
    );
    *guard = Some(ProxyHandle {
        base_url: base_url.clone(),
        settings: shared.settings,
    });
    Ok(base_url)
}

async fn proxy(State(shared): State<Shared>, request: Request) -> Response {
    match forward(shared, request).await {
        Ok(response) => response,
        Err(error) => {
            log::warn!("推理深度代理转发失败: {error}");
            (
                StatusCode::BAD_GATEWAY,
                format!("推理深度代理转发失败: {error}"),
            )
                .into_response()
        }
    }
}

async fn forward(shared: Shared, request: Request) -> Result<Response, String> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .map_err(|error| format!("读取请求体失败: {error}"))?;
    let settings = shared.settings.read().await.clone();

    let path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let target = join_upstream(&settings.upstream, path);

    let mut outbound_body = body.to_vec();
    if let Some(model) = model_from_path(parts.uri.path()) {
        if model == settings.model {
            if let Ok(mut value) = serde_json::from_slice::<Value>(&body) {
                if rewrite_thinking(&mut value, settings.depth) {
                    outbound_body = serde_json::to_vec(&value)
                        .map_err(|error| format!("序列化请求体失败: {error}"))?;
                }
            }
        }
    }

    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .map_err(|error| format!("不支持的请求方法: {error}"))?;
    let headers = outbound_headers(&parts.headers, &settings.api_key)?;

    let response = shared
        .client
        .request(method, &target)
        .headers(headers)
        .body(outbound_body)
        .send()
        .await
        .map_err(|error| format!("请求上游失败: {error}"))?;

    let status = response.status();
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in response.headers() {
        if is_hop_by_hop(name.as_str())
            || name == header::CONTENT_LENGTH
            || name == header::CONTENT_ENCODING
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .map_err(|error| format!("构造响应失败: {error}"))
}

fn outbound_headers(inbound: &HeaderMap, api_key: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (name, value) in inbound {
        let lower = name.as_str();
        // Host/长度/连接类头由 reqwest 重新计算；认证头统一替换。
        // 去掉 accept-encoding，避免上游返回压缩流导致 SSE 无法逐块转发。
        if is_hop_by_hop(lower)
            || matches!(
                lower,
                "host" | "content-length" | "authorization" | "x-goog-api-key" | "accept-encoding"
            )
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    let bearer = header::HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|error| format!("API Key 不合法: {error}"))?;
    let google_key = header::HeaderValue::from_str(api_key)
        .map_err(|error| format!("API Key 不合法: {error}"))?;
    headers.insert(header::AUTHORIZATION, bearer);
    headers.insert("x-goog-api-key", google_key);
    Ok(headers)
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// 把上游地址与 language_server 请求路径拼起来，允许配置里带 /v1 或 /v1beta。
pub fn join_upstream(upstream: &str, path: &str) -> String {
    let base = upstream.trim().trim_end_matches('/');
    let base = base
        .strip_suffix("/v1beta")
        .or_else(|| base.strip_suffix("/v1"))
        .unwrap_or(base);
    format!("{base}{path}")
}

fn model_from_path(path: &str) -> Option<String> {
    let rest = path.split_once("/models/")?.1;
    let model = rest.split(':').next()?.trim();
    (!model.is_empty()).then(|| model.to_string())
}

/// 只改 thinkingConfig，保留请求里的其它字段与 includeThoughts。
pub fn rewrite_thinking(body: &mut Value, depth: ThinkingDepth) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    let generation = object
        .entry("generationConfig")
        .or_insert_with(|| json!({}));
    let Some(generation) = generation.as_object_mut() else {
        return false;
    };
    let thinking = generation
        .entry("thinkingConfig")
        .or_insert_with(|| json!({}));
    let Some(thinking) = thinking.as_object_mut() else {
        return false;
    };
    thinking
        .entry("includeThoughts")
        .or_insert_with(|| Value::Bool(true));
    thinking.remove("thinkingLevel");
    thinking.insert("thinkingBudget".into(), json!(depth.thinking_budget()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_base_with_or_without_version() {
        assert_eq!(
            join_upstream("https://runapi.host", "/v1beta/models/x:generateContent"),
            "https://runapi.host/v1beta/models/x:generateContent"
        );
        assert_eq!(
            join_upstream(
                "https://runapi.host/v1beta/",
                "/v1beta/models/x:streamGenerateContent"
            ),
            "https://runapi.host/v1beta/models/x:streamGenerateContent"
        );
        assert_eq!(
            join_upstream("https://runapi.host/v1", "/v1beta/models/x:generateContent"),
            "https://runapi.host/v1beta/models/x:generateContent"
        );
    }

    #[test]
    fn extracts_model_from_generate_paths() {
        assert_eq!(
            model_from_path("/v1beta/models/gemini-4-neon:streamGenerateContent").as_deref(),
            Some("gemini-4-neon")
        );
        assert_eq!(model_from_path("/v1beta/models"), None);
    }

    #[test]
    fn depth_parsing_is_lenient() {
        assert_eq!(ThinkingDepth::parse("HIGH"), Some(ThinkingDepth::High));
        assert_eq!(
            ThinkingDepth::parse(" medium "),
            Some(ThinkingDepth::Medium)
        );
        assert_eq!(ThinkingDepth::parse("max"), None);
        let settings = json!({"env": {THINKING_ENV: "low"}});
        assert_eq!(depth_of(&settings), Some(ThinkingDepth::Low));
        assert_eq!(depth_of(&json!({"env": {}})), None);
        assert_eq!(depth_of(&json!({})), None);
    }

    #[test]
    fn rewrite_sets_budget_and_keeps_other_fields() {
        let mut body = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {
                "temperature": 0.2,
                "thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}
            }
        });
        assert!(rewrite_thinking(&mut body, ThinkingDepth::High));
        assert_eq!(
            body.pointer("/generationConfig/thinkingConfig/thinkingBudget"),
            Some(&json!(24576))
        );
        assert_eq!(
            body.pointer("/generationConfig/thinkingConfig/includeThoughts"),
            Some(&json!(true))
        );
        assert_eq!(
            body.pointer("/generationConfig/temperature"),
            Some(&json!(0.2))
        );
        assert!(body
            .pointer("/generationConfig/thinkingConfig/thinkingLevel")
            .is_none());
    }

    #[test]
    fn rewrite_creates_missing_thinking_config() {
        let mut body = json!({"contents": []});
        assert!(rewrite_thinking(&mut body, ThinkingDepth::Low));
        assert_eq!(
            body.pointer("/generationConfig/thinkingConfig/thinkingBudget"),
            Some(&json!(2048))
        );
        assert!(rewrite_thinking(&mut json!([]), ThinkingDepth::Low) == false);
    }
}
