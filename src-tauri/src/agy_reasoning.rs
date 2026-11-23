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
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
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

/// 页面内模型选择器通过本地控制端点写入的当前选择。
#[derive(Clone, Debug)]
pub struct Selection {
    pub model: String,
    pub depth: ThinkingDepth,
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

#[derive(Debug, Default)]
struct MetricsState {
    model: String,
    active: bool,
    done: bool,
    started: Option<Instant>,
    first: Option<Instant>,
    ended: Option<Instant>,
    prompt_tokens: u64,
    output_tokens: u64,
    cached_tokens: Option<u64>,
    output_chars: u64,
}

impl MetricsState {
    fn start(&mut self, model: &str) {
        *self = Self {
            model: model.to_string(),
            active: true,
            started: Some(Instant::now()),
            ..Self::default()
        };
    }

    fn fail(&mut self) {
        self.active = false;
        self.done = true;
        self.ended = Some(Instant::now());
    }

    fn finish(&mut self) {
        if self.output_tokens == 0 {
            self.output_tokens = self.output_chars / 4;
        }
        self.active = false;
        self.done = true;
        self.ended = Some(Instant::now());
    }

    fn apply_sse_line(&mut self, line: &str) {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(usage) = value.get("usageMetadata") {
            if let Some(tokens) = usage.get("promptTokenCount").and_then(Value::as_u64) {
                self.prompt_tokens = tokens;
            }
            if let Some(tokens) = usage.get("candidatesTokenCount").and_then(Value::as_u64) {
                self.output_tokens = tokens;
            }
            if let Some(tokens) = usage.get("cachedContentTokenCount").and_then(Value::as_u64) {
                self.cached_tokens = Some(tokens);
            }
        }
        if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
            for candidate in candidates {
                if let Some(parts) = candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            self.output_chars += text.chars().count() as u64;
                            self.first.get_or_insert_with(Instant::now);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct Shared {
    settings: Arc<RwLock<Settings>>,
    selection: Arc<RwLock<Option<Selection>>>,
    metrics: Arc<StdMutex<MetricsState>>,
    client: reqwest::Client,
}

struct ProxyHandle {
    base_url: String,
    settings: Arc<RwLock<Settings>>,
    selection: Arc<RwLock<Option<Selection>>>,
}

static PROXY: OnceLock<Mutex<Option<ProxyHandle>>> = OnceLock::new();

/// 启动（或复用）进程内代理，返回应写入 GOOGLE_GEMINI_BASE_URL 的地址。
pub async fn ensure(settings: Settings) -> Result<String, String> {
    let slot = PROXY.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().await;
    if let Some(handle) = guard.as_ref() {
        let previous = handle.settings.read().await.clone();
        let reset = previous.model != settings.model || previous.upstream != settings.upstream;
        *handle.settings.write().await = settings;
        if reset {
            *handle.selection.write().await = None;
        }
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
    let selection = Arc::new(RwLock::new(None));
    let shared = Shared {
        settings: Arc::new(RwLock::new(settings)),
        selection: selection.clone(),
        metrics: Arc::new(StdMutex::new(MetricsState::default())),
        client,
    };
    let app = Router::new()
        .route("/__ccswitch_select", any(select))
        .route("/__ccswitch_metrics", get(metrics))
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
        selection: shared.selection,
    });
    Ok(base_url)
}

/// 页面注入的桥接脚本在每次发送前调用这里，原子地更新当前模型与推理等级。
/// 代理本身只监听 127.0.0.1，并允许应用页面（https://127.0.0.1:LS 端口）跨域调用。
async fn select(State(shared): State<Shared>, request: Request) -> Response {
    let cors = request.method() == Method::OPTIONS;
    let result: Result<serde_json::Value, String> = if cors {
        Ok(serde_json::json!({}))
    } else {
        let body = match axum::body::to_bytes(request.into_body(), 64 * 1024).await {
            Ok(body) => body,
            Err(error) => {
                return with_cors(error_response(StatusCode::BAD_REQUEST, &error.to_string()))
            }
        };
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                return with_cors(error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("选择内容不是 JSON: {error}"),
                ))
            }
        };
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        let depth = value
            .get("effort")
            .and_then(Value::as_str)
            .and_then(ThinkingDepth::parse);
        match (model, depth) {
            (Some(model), Some(depth)) => {
                *shared.selection.write().await = Some(Selection {
                    model: model.clone(),
                    depth,
                });
                log::info!("Antigravity 模型选择已更新: {model} / {}", depth.as_str());
                Ok(serde_json::json!({"ok": true, "model": model, "effort": depth.as_str()}))
            }
            _ => Err("选择缺少合法的 model 或 effort".to_string()),
        }
    };

    match result {
        Ok(value) => with_cors(axum::Json(value).into_response()),
        Err(error) => with_cors(error_response(StatusCode::BAD_REQUEST, &error)),
    }
}

/// 页面里的 Prism 指标条通过这里读取代理侧的真实请求计时与输出 token，估算 tok/s。
async fn metrics(State(shared): State<Shared>) -> Response {
    let state = match shared.metrics.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    let elapsed_ms = state
        .started
        .map(|started| now.saturating_duration_since(started).as_millis() as u64)
        .unwrap_or(0);
    let ttft_ms = match (state.started, state.first) {
        (Some(started), Some(first)) => first.saturating_duration_since(started).as_millis() as u64,
        _ => 0,
    };
    let effective_ms = match state.started {
        Some(started) => state
            .ended
            .unwrap_or(now)
            .saturating_duration_since(started)
            .as_millis() as u64,
        None => 0,
    };
    let tok_s = if state.output_tokens > 0 && effective_ms > 0 {
        state.output_tokens as f64 / (effective_ms as f64 / 1000.0)
    } else {
        0.0
    };
    let value = json!({
        "active": state.active,
        "done": state.done,
        "model": state.model,
        "tok_s": tok_s,
        "ttft_ms": ttft_ms,
        "output_tokens": state.output_tokens,
        "prompt_tokens": state.prompt_tokens,
        "cached_tokens": state.cached_tokens,
        "elapsed_ms": elapsed_ms,
        "finished_ago_ms": state
            .ended
            .map(|ended| now.saturating_duration_since(ended).as_millis() as u64)
            .unwrap_or(0),
    });
    with_cors(axum::Json(value).into_response())
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, message.to_string()).into_response()
}

fn with_cors(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "content-type".parse().unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, POST, OPTIONS".parse().unwrap(),
    );
    response
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
    let selection = shared.selection.read().await.clone();

    let mut path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());

    // LS 的主模型请求路径始终是 --override_model_name；页面选择只改代理转发
    // 的目标模型。辅助请求（如 gemini-3.1-flash-lite）保持原样。
    let mut main_request = false;
    if let Some(model) = model_from_path(parts.uri.path()) {
        if model == settings.model {
            main_request = true;
            if let Some(selection) = &selection {
                path = replace_model(&path, &settings.model, &selection.model);
            }
        }
    }

    let metrics_request = main_request
        && !parts.uri.path().contains(":countTokens")
        && (parts.uri.path().contains(":generateContent")
            || parts.uri.path().contains(":streamGenerateContent"));

    let target = join_upstream(&settings.upstream, &path);

    let mut outbound_body = body.to_vec();
    if main_request {
        let depth = selection
            .as_ref()
            .map(|selection| selection.depth)
            .unwrap_or(settings.depth);
        if let Ok(mut value) = serde_json::from_slice::<Value>(&body) {
            if rewrite_thinking(&mut value, depth) {
                outbound_body = serde_json::to_vec(&value)
                    .map_err(|error| format!("序列化请求体失败: {error}"))?;
            }
        }
        if let Some(selection) = &selection {
            log::info!(
                "Antigravity 代理改写主模型请求: {} -> {} / {}",
                settings.model,
                selection.model,
                selection.depth.as_str()
            );
        }
    }

    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .map_err(|error| format!("不支持的请求方法: {error}"))?;
    let headers = outbound_headers(&parts.headers, &settings.api_key)?;

    if metrics_request {
        let model = selection
            .as_ref()
            .map(|selection| selection.model.clone())
            .unwrap_or_else(|| settings.model.clone());
        if let Ok(mut state) = shared.metrics.lock() {
            state.start(&model);
        }
    }

    let response = match shared
        .client
        .request(method, &target)
        .headers(headers)
        .body(outbound_body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if metrics_request {
                if let Ok(mut state) = shared.metrics.lock() {
                    state.fail();
                }
            }
            return Err(format!("请求上游失败: {error}"));
        }
    };

    let status = response.status();
    if metrics_request && !status.is_success() {
        if let Ok(mut state) = shared.metrics.lock() {
            state.fail();
        }
    }
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
    if !metrics_request {
        return builder
            .body(Body::from_stream(response.bytes_stream()))
            .map_err(|error| format!("构造响应失败: {error}"));
    }

    let metrics = shared.metrics.clone();
    let upstream = response.bytes_stream();
    let stream = async_stream::stream! {
        let mut upstream = upstream;
        let mut line_buffer: Vec<u8> = Vec::new();
        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    if let Ok(mut state) = metrics.lock() {
                        state.first.get_or_insert_with(Instant::now);
                        line_buffer.extend_from_slice(&bytes);
                        while let Some(position) = line_buffer.iter().position(|byte| *byte == b'\n') {
                            let line: Vec<u8> = line_buffer.drain(..=position).collect();
                            state.apply_sse_line(&String::from_utf8_lossy(&line));
                        }
                        if line_buffer.len() > 1024 * 1024 {
                            line_buffer.clear();
                        }
                    }
                    yield Ok::<Bytes, reqwest::Error>(bytes);
                }
                Err(error) => {
                    if let Ok(mut state) = metrics.lock() {
                        state.fail();
                    }
                    yield Err(error);
                    return;
                }
            }
        }
        if let Ok(mut state) = metrics.lock() {
            state.finish();
        }
    };
    builder
        .body(Body::from_stream(stream))
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

fn replace_model(path: &str, from: &str, to: &str) -> String {
    let needle = format!("/models/{from}");
    let Some(position) = path.find(&needle) else {
        return path.to_string();
    };
    let rest = &path[position + needle.len()..];
    if !(rest.is_empty() || rest.starts_with(':') || rest.starts_with('/')) {
        return path.to_string();
    }
    let mut replaced = String::with_capacity(path.len() + to.len());
    replaced.push_str(&path[..position]);
    replaced.push_str(&format!("/models/{to}"));
    replaced.push_str(rest);
    replaced
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
    fn replaces_only_exact_model_segment() {
        assert_eq!(
            replace_model(
                "/v1beta/models/gemini-4-neon:streamGenerateContent?alt=sse",
                "gemini-4-neon",
                "gemini-4-carbon"
            ),
            "/v1beta/models/gemini-4-carbon:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            replace_model(
                "/v1beta/models/gemini-4-neon-lite:generateContent",
                "gemini-4-neon",
                "x"
            ),
            "/v1beta/models/gemini-4-neon-lite:generateContent"
        );
    }

    #[test]
    fn metrics_reads_sse_usage_and_text() {
        let mut state = MetricsState::default();
        state.apply_sse_line(r#"data: {"candidates":[{"content":{"parts":[{"text":"你好"}]}}]}"#);
        state.apply_sse_line(
            r#"data: {"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":8,"cachedContentTokenCount":64}}"#,
        );
        state.finish();
        assert_eq!(state.output_chars, 2);
        assert_eq!(state.prompt_tokens, 120);
        assert_eq!(state.output_tokens, 8);
        assert_eq!(state.cached_tokens, Some(64));
        assert!(state.done && !state.active);
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
