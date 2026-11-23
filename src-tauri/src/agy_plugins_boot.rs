//! Antigravity 冷启动时的插件 UI 自检。
//!
//! 语言服务器启动后 Mendel flags 往往要几十秒才同步完成，而首页在这之前
//! 就已经完成首次渲染；前端不会因为后续 flags 到达而重新挂载 extensibility，
//! 结果就是侧边栏整组插件入口（Plugins / 自动化 / 自定义扩展）消失。
//!
//! 这里在 IDE 启动后立刻读一次 flags：只有确认首帧为空时才继续等待，等 flags
//! 就绪后通过 Electron 的 DevTools 协议刷新页面一次。不会重启 IDE 或语言服务器。

use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const FLAGS_PATH: &str = "/exa.language_server_pb.LanguageServerService/GetMendelFlags";
const CSRF_HEADER: &str = "x-codeium-csrf-token";
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// 启动注入所需的供应商信息。模型列表在启动线程里实时拉取。
#[derive(Clone, Debug)]
pub struct BridgeConfig {
    pub proxy_url: String,
    pub upstream: String,
    pub api_key: String,
    pub model: String,
}

/// 启动一个后台自检线程。`started_at` 必须是拉起 IDE 之前的时间。
pub fn watch(started_at: SystemTime, bridge: Option<BridgeConfig>) {
    let _ = std::thread::Builder::new()
        .name("agy-plugin-boot".into())
        .spawn(move || {
            log("插件 UI 自检线程启动");
            match std::panic::catch_unwind(|| run(started_at, bridge)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log(&format!("插件 UI 自检未完成: {error}")),
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|message| message.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "未知原因".to_string());
                    log(&format!("插件 UI 自检线程 panic: {message}"));
                }
            }
        });
}

fn run(started_at: SystemTime, bridge: Option<BridgeConfig>) -> Result<(), String> {
    let hub = wait_for_hub(started_at, Duration::from_secs(25))?;
    let (http_port, csrf) = read_hub(&hub)?;

    let first = flags_count(http_port, &csrf)?;
    if first > 0 {
        log(&format!("插件 flags 首帧已就绪（{first} 项）"));
    } else {
        log("插件 flags 首帧为空，等待语言服务器同步");
        let mut ready = 0;
        for _ in 0..90 {
            std::thread::sleep(Duration::from_secs(2));
            match flags_count(http_port, &csrf) {
                Ok(count) if count > 0 => {
                    ready = count;
                    break;
                }
                _ => {}
            }
        }
        if ready == 0 {
            return Err("等待插件 flags 超时（180 秒）".to_string());
        }
        log(&format!("插件 flags 已就绪（{ready} 项）"));
    }

    if bridge.is_none() && first > 0 {
        log("页面已有扩展入口，无需刷新");
        return Ok(());
    }

    let devtools_port = wait_for_devtools(started_at, Duration::from_secs(20))?;
    log(&format!("DevTools 端口 {devtools_port}，查找页面 target"));
    let ws_url = page_ws_url(devtools_port)?;
    log("已找到页面 target，检查扩展入口");
    let has_extensions = cdp_has_extensions(&ws_url)?;
    log(&format!("页面扩展入口: {has_extensions}"));

    if let Some(bridge) = bridge {
        match fetch_provider_models(&bridge) {
            Ok(models) => {
                let script = build_bridge_script(&bridge, &models);
                log(&format!(
                    "注入应用内模型选择器：{} 个供应商模型，override {}，控制端点 {}",
                    models.len(),
                    bridge.model,
                    bridge.proxy_url
                ));
                // 注入脚本 + reload 同时解决 flags 首帧问题和模型列表桥接；随后保持
                // DevTools 会话存活，页面再次 reload 时脚本仍然有效。
                match cdp_inject_and_hold(&ws_url, &script) {
                    Ok(()) => return Ok(()),
                    Err(error) => log(&format!("模型桥接注入失败，退回插件自检: {error}")),
                }
            }
            Err(error) => log(&format!("获取供应商模型失败，退回插件自检: {error}")),
        }
    }

    if has_extensions {
        log("页面已有扩展入口，跳过刷新");
        return Ok(());
    }
    cdp_reload(&ws_url)?;
    log("已刷新 Antigravity 页面，插件入口随 flags 恢复");
    Ok(())
}

struct ModelChoice {
    name: String,
    display: String,
}

fn models_url(upstream: &str) -> String {
    let mut base = upstream.trim_end_matches('/').to_string();
    for suffix in ["/v1beta", "/v1"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.to_string();
            break;
        }
    }
    format!("{base}/v1beta/models?pageSize=1000")
}

fn clean_label(display: &str, fallback: &str) -> String {
    let mut label = display.trim();
    for suffix in [" (Low)", " (Medium)", " (High)", " (XHigh)"] {
        if let Some(stripped) = label.strip_suffix(suffix) {
            label = stripped;
            break;
        }
    }
    if label.is_empty() {
        fallback.to_string()
    } else {
        label.to_string()
    }
}

fn fetch_provider_models(bridge: &BridgeConfig) -> Result<Vec<ModelChoice>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("创建模型列表客户端失败: {error}"))?;
    let url = models_url(&bridge.upstream);
    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", bridge.api_key))
        .header("x-goog-api-key", &bridge.api_key)
        .send()
        .map_err(|error| format!("请求供应商模型列表失败: {error}"))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|error| format!("读取供应商模型列表失败: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "供应商模型列表 HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("解析供应商模型列表失败: {error}"))?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| "供应商模型列表缺少 models 数组".to_string())?;
    let mut seen = BTreeSet::new();
    let mut choices = Vec::new();
    for model in models {
        let name = model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .trim_start_matches("models/")
            .to_string();
        if name.is_empty() || !seen.insert(name.clone()) {
            continue;
        }
        let display = model
            .get("displayName")
            .and_then(Value::as_str)
            .map(|display| clean_label(display, &name))
            .unwrap_or_else(|| name.clone());
        choices.push(ModelChoice { name, display });
    }
    if choices.is_empty() {
        return Err("供应商没有返回任何模型".to_string());
    }
    Ok(prefer_configured_model(choices, &bridge.model))
}

fn prefer_configured_model(mut choices: Vec<ModelChoice>, preferred: &str) -> Vec<ModelChoice> {
    let preferred = preferred.trim_start_matches("models/");
    if let Some(index) = choices.iter().position(|choice| choice.name == preferred) {
        let choice = choices.remove(index);
        choices.insert(0, choice);
    }
    choices
}

fn stable_model_id(name: &str) -> i64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let base = 100_000 + (hash % 500_000_000) as i64;
    base * 3
}

fn build_bridge_script(bridge: &BridgeConfig, models: &[ModelChoice]) -> String {
    let efforts = ["Low", "Medium", "High"];
    let mut configs = Vec::new();
    let mut labels = Vec::new();
    let mut selection = Map::new();
    for model in models {
        let base = stable_model_id(&model.name);
        for (level, effort) in efforts.iter().enumerate() {
            let id = base + level as i64;
            let label = format!("{} ({effort})", model.display);
            configs.push(json!({
                "label": label,
                "modelOrAlias": { "model": id },
                "supportedMimeTypes": {},
                "contextWindowOptions": [],
                "disabled": false
            }));
            labels.push(label);
            selection.insert(
                id.to_string(),
                json!({ "model": model.name, "effort": effort.to_lowercase() }),
            );
        }
    }
    let config = json!({
        "configs": configs,
        "sorts": [{
            "name": "recommended",
            "groups": [{ "groupName": "Gemini", "modelLabels": labels }]
        }]
    });
    let config_json = config.to_string();
    let selection_json = Value::Object(selection).to_string();
    let select_url = Value::String(format!(
        "{}/__ccswitch_select",
        bridge.proxy_url.trim_end_matches('/')
    ))
    .to_string();
    let metrics_url = Value::String(format!(
        "{}/__ccswitch_metrics",
        bridge.proxy_url.trim_end_matches('/')
    ))
    .to_string();

    let mut script = String::new();
    script.push_str("(() => {\n  if (window.__ccModelBridge) return;\n  const CFG = ");
    script.push_str(&config_json);
    script.push_str(";\n  const MAP = ");
    script.push_str(&selection_json);
    script.push_str(";\n  const SELECT_URL = ");
    script.push_str(&select_url);
    script.push_str(";\n  const METRICS_URL = ");
    script.push_str(&metrics_url);
    script.push_str(
        r#";
  window.__ccModelBridge = true;
  window.__ccBridge = { req: 0, sel: 0, err: null, last: null, patched: true };
  window.__ccGrpcHit = { req: 0, rew: 0, err: null };
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const orig = window.fetch.bind(window);
  function installMetrics() {
    if (window.__ccMetricsPoller) return;
    window.__ccMetricsPoller = true;
    async function tick() {
      let data;
      try {
        const response = await fetch(METRICS_URL, { cache: 'no-store' });
        if (!response.ok) return;
        data = await response.json();
      } catch (error) {
        return;
      }
      if (!data || !data.model) return;
      if (!data.active && !(data.finished_ago_ms >= 0 && data.finished_ago_ms < 300000)) return;
      const bar = document.getElementById('ag-prism-pulse-bar');
      const shadow = bar && bar.shadowRoot;
      if (!shadow) return;
      const tps = shadow.getElementById('tps');
      if (tps) {
        if (data.tok_s > 0) tps.textContent = String(Math.max(1, Math.round(data.tok_s)));
        tps.title = 'CC Switch 本地估算（供应商未返回流式明细）';
      }
      const latest = shadow.getElementById('latest-rate');
      if (latest && data.tok_s > 0) latest.textContent = data.tok_s.toFixed(1) + ' tok/s';
      const ttft = shadow.getElementById('ttft');
      if (ttft && data.ttft_ms > 0) ttft.textContent = data.ttft_ms + ' ms';
      const cache = shadow.getElementById('cache-rate');
      if (cache) cache.title = '中转供应商未返回缓存命中数据';
    }
    setInterval(tick, 600);
    tick();
  }
  function decodeFrame(bytes) {
    if (bytes.length < 5) throw new Error('short frame');
    const len = ((bytes[1] << 24) | (bytes[2] << 16) | (bytes[3] << 8) | bytes[4]) >>> 0;
    if (5 + len > bytes.length) throw new Error('bad frame length');
    return { json: JSON.parse(dec.decode(bytes.slice(5, 5 + len))), tail: bytes.slice(5 + len) };
  }
  function encodeFrame(jsonValue, tail) {
    const body = enc.encode(JSON.stringify(jsonValue));
    const out = new Uint8Array(5 + body.length + (tail ? tail.length : 0));
    out[0] = 0;
    out[1] = (body.length >>> 24) & 255;
    out[2] = (body.length >>> 16) & 255;
    out[3] = (body.length >>> 8) & 255;
    out[4] = body.length & 255;
    out.set(body, 5);
    if (tail && tail.length) out.set(tail, 5 + body.length);
    return out;
  }
  async function publish(sel) {
    try {
      await fetch(SELECT_URL, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(sel)
      });
    } catch (error) {
      window.__ccBridge.err = String(error);
    }
  }
  window.fetch = async function (input, init) {
    let req;
    try { req = new Request(input, init); } catch (error) { return orig(input, init); }
    const url = req.url || '';
    if (/SendUserCascadeMessage/.test(url)) {
      window.__ccBridge.req++;
      try {
        const frame = decodeFrame(new Uint8Array(await req.clone().arrayBuffer()));
        const planner = frame.json.cascadeConfig && frame.json.cascadeConfig.plannerConfig;
        const key = planner && planner.planModel;
        const sel = MAP[String(key)];
        if (sel && planner) {
          window.__ccBridge.sel++;
          window.__ccBridge.last = { key: key, sel: sel };
          await publish(sel);
          planner.planModel = 'MODEL_GOOGLE_GEMINI_INTERNAL_BYOM';
        }
        return orig(new Request(req, { body: encodeFrame(frame.json, frame.tail) }));
      } catch (error) {
        window.__ccBridge.err = String(error);
        return orig(req);
      }
    }
    if (!/GetUserStatus/.test(url)) return orig(req);
    const res = await orig(req);
    window.__ccGrpcHit.req++;
    try {
      const frame = decodeFrame(new Uint8Array(await res.clone().arrayBuffer()));
      frame.json.userStatus = frame.json.userStatus || {};
      frame.json.userStatus.cascadeModelConfigData = Object.assign(
        {}, frame.json.userStatus.cascadeModelConfigData, {
          clientModelConfigs: CFG.configs,
          clientModelSorts: CFG.sorts
        });
      window.__ccGrpcHit.rew++;
      return new Response(encodeFrame(frame.json, frame.tail), {
        status: res.status,
        statusText: res.statusText,
        headers: res.headers
      });
    } catch (error) {
      window.__ccGrpcHit.err = String(error);
      return res;
    }
  };
  installMetrics();
})();
"#,
    );
    script
}

fn wait_for_hub(started_at: SystemTime, timeout: Duration) -> Result<PathBuf, String> {
    let dir = crate::config::get_home_dir().join(".gemini/antigravity/daemon");
    let deadline = started_at + timeout;
    loop {
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut candidates: Vec<(SystemTime, PathBuf)> = entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    let name = path.file_name()?.to_string_lossy().to_string();
                    if !name.starts_with("hub_") || !name.ends_with(".json") {
                        return None;
                    }
                    let modified = entry.metadata().ok()?.modified().ok()?;
                    (modified >= started_at).then_some((modified, path))
                })
                .collect();
            candidates.sort_by_key(|(modified, _)| *modified);
            if let Some((_, path)) = candidates.pop() {
                return Ok(path);
            }
        }
        if SystemTime::now() >= deadline {
            return Err("等待语言服务器 hub 文件超时".to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_hub(path: &Path) -> Result<(u16, String), String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| format!("解析 hub 文件失败: {error}"))?;
    let port = value
        .get("httpPort")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| "hub 文件缺少 httpPort".to_string())?;
    let csrf = value
        .get("csrfToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "hub 文件缺少 csrfToken".to_string())?;
    Ok((port, csrf.to_string()))
}

fn flags_count(port: u16, csrf: &str) -> Result<usize, String> {
    let body = http_request(
        port,
        "POST",
        FLAGS_PATH,
        &[("Content-Type", "application/json"), (CSRF_HEADER, csrf)],
        b"{}",
    )?;
    flags_count_from_body(&body)
}

fn flags_count_from_body(body: &str) -> Result<usize, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| format!("解析 flags 响应失败: {error}"))?;
    Ok(value
        .get("experimentConfig")
        .and_then(|config| config.get("experiments"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0))
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<String, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|error| format!("连接 127.0.0.1:{port} 失败: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .map_err(|error| format!("设置读取超时失败: {error}"))?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (key, value) in headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|error| format!("发送 HTTP 请求失败: {error}"))?;

    let raw = read_http_response(&mut stream)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "HTTP 响应缺少头部".to_string())?;
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        return Err(format!("HTTP 请求失败: {status}"));
    }
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked(body)
    } else {
        Ok(body.to_string())
    }
}

fn read_http_response(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                raw.extend_from_slice(&buffer[..count]);
                if response_complete(&raw) {
                    break;
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(error) => return Err(format!("读取 HTTP 响应失败: {error}")),
        }
    }
    if raw.is_empty() {
        return Err("HTTP 响应为空".to_string());
    }
    Ok(raw)
}

fn response_complete(raw: &[u8]) -> bool {
    let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
    let body = &raw[end + 4..];
    if let Some(length) = head.lines().find_map(|line| {
        line.strip_prefix("content-length:")
            .and_then(|value| value.trim().parse::<usize>().ok())
    }) {
        return body.len() >= length;
    }
    if head.contains("transfer-encoding: chunked") {
        return body == b"0\r\n\r\n" || body.ends_with(b"\r\n0\r\n\r\n");
    }
    false
}

fn decode_chunked(body: &str) -> Result<String, String> {
    let mut rest = body;
    let mut output = String::new();
    loop {
        let (line, tail) = rest
            .split_once("\r\n")
            .ok_or_else(|| "chunk 头缺失".to_string())?;
        let size = usize::from_str_radix(line.trim(), 16)
            .map_err(|error| format!("chunk 长度非法: {error}"))?;
        if size == 0 {
            break;
        }
        if tail.len() < size {
            return Err("chunk 数据不完整".to_string());
        }
        output.push_str(&tail[..size]);
        rest = &tail[size..];
        rest = rest.strip_prefix("\r\n").unwrap_or(rest);
    }
    Ok(output)
}

fn wait_for_devtools(started_at: SystemTime, timeout: Duration) -> Result<u16, String> {
    let path = crate::config::get_home_dir().join(".config/Antigravity/DevToolsActivePort");
    let deadline = started_at + timeout;
    loop {
        if let Ok(metadata) = fs::metadata(&path) {
            if let Ok(modified) = metadata.modified() {
                if modified >= started_at {
                    if let Ok(raw) = fs::read_to_string(&path) {
                        if let Some(port) =
                            raw.lines().next().and_then(|line| line.trim().parse().ok())
                        {
                            return Ok(port);
                        }
                    }
                }
            }
        }
        if SystemTime::now() >= deadline {
            return Err("等待 Electron DevTools 端口超时".to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn page_ws_url(devtools_port: u16) -> Result<String, String> {
    let body = http_request(devtools_port, "GET", "/json/list", &[], b"")?;
    let targets: Value =
        serde_json::from_str(&body).map_err(|error| format!("解析 DevTools 目标失败: {error}"))?;
    let target = targets
        .as_array()
        .and_then(|targets| {
            targets.iter().find(|target| {
                target.get("type").and_then(Value::as_str) == Some("page")
                    && target
                        .get("url")
                        .and_then(Value::as_str)
                        .map(|url| url.starts_with("https://127.0.0.1"))
                        .unwrap_or(false)
            })
        })
        .ok_or_else(|| "没有找到 Antigravity 页面 target".to_string())?;
    target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "页面 target 缺少 webSocketDebuggerUrl".to_string())
}

fn cdp_has_extensions(ws_url: &str) -> Result<bool, String> {
    let response = cdp_request(
        ws_url,
        r#"{"id":1,"method":"Runtime.evaluate","params":{"expression":"Boolean(document.querySelector('[data-testid=\"new-tab-card-extensions\"]')) || document.body.innerText.includes('自定义扩展')","returnByValue":true}}"#,
    )?;
    Ok(response
        .pointer("/result/result/value")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

fn cdp_reload(ws_url: &str) -> Result<(), String> {
    let _ = cdp_request(
        ws_url,
        r#"{"id":2,"method":"Page.reload","params":{"ignoreCache":false}}"#,
    )?;
    Ok(())
}

fn cdp_open(ws_url: &str) -> Result<TcpStream, String> {
    let (host, port, path) = split_ws_url(ws_url)?;
    let mut stream = TcpStream::connect((host.as_str(), port))
        .map_err(|error| format!("连接 DevTools 失败: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .map_err(|error| format!("设置 DevTools 读取超时失败: {error}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("发送 DevTools 握手失败: {error}"))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream
            .read_exact(&mut byte)
            .map_err(|error| format!("读取 DevTools 握手失败: {error}"))?;
        head.push(byte[0]);
        if head.len() > 8192 {
            return Err("DevTools 握手响应过长".to_string());
        }
    }
    let handshake = String::from_utf8_lossy(&head);
    if !handshake.starts_with("HTTP/1.1 101")
        && !handshake.starts_with("HTTP/1.1 101 Switching Protocols")
    {
        return Err(format!(
            "DevTools 握手失败: {}",
            handshake.lines().next().unwrap_or_default()
        ));
    }
    Ok(stream)
}

fn cdp_call(stream: &mut TcpStream, id: u64, method: &str, params: Value) -> Result<Value, String> {
    let message = json!({ "id": id, "method": method, "params": params }).to_string();
    write_ws_text(stream, &message)?;
    loop {
        let text = read_ws_text(stream)?;
        let value: Value =
            serde_json::from_str(&text).map_err(|error| format!("解析 CDP 响应失败: {error}"))?;
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
}

fn cdp_inject_and_hold(ws_url: &str, source: &str) -> Result<(), String> {
    let mut stream = cdp_open(ws_url)?;
    for (id, method, params) in [
        (1, "Page.enable", json!({})),
        (
            2,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": source }),
        ),
        (3, "Page.reload", json!({ "ignoreCache": false })),
    ] {
        let response = cdp_call(&mut stream, id, method, params)?;
        if let Some(error) = response.get("error") {
            return Err(format!("CDP {method} 失败: {error}"));
        }
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(3600)))
        .map_err(|error| format!("设置 DevTools 保活超时失败: {error}"))?;
    loop {
        match read_ws_text(&mut stream) {
            Ok(_) => {}
            Err(error) => return Err(format!("DevTools 注入会话结束: {error}")),
        }
    }
}

fn cdp_request(ws_url: &str, message: &str) -> Result<Value, String> {
    let mut stream = cdp_open(ws_url)?;
    write_ws_text(&mut stream, message)?;
    let response = read_ws_text(&mut stream)?;
    serde_json::from_str(&response).map_err(|error| format!("解析 CDP 响应失败: {error}"))
}

fn split_ws_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("不是 ws:// URL: {url}"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| format!("ws URL 缺少端口: {url}"))?;
    let port = port
        .parse()
        .map_err(|error| format!("ws URL 端口非法: {error}"))?;
    Ok((host.to_string(), port, format!("/{path}")))
}

fn write_ws_text(stream: &mut TcpStream, text: &str) -> Result<(), String> {
    let payload = text.as_bytes();
    if payload.len() > 65535 {
        return Err("CDP 消息过长".to_string());
    }
    let mut frame = Vec::with_capacity(payload.len() + 8);
    frame.push(0x81);
    if payload.len() <= 125 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    let mask = [0x37u8, 0x9a, 0x21, 0x5c];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream
        .write_all(&frame)
        .map_err(|error| format!("发送 CDP 消息失败: {error}"))
}

fn read_ws_text(stream: &mut TcpStream) -> Result<String, String> {
    let mut message = Vec::new();
    loop {
        let mut header = [0u8; 2];
        stream
            .read_exact(&mut header)
            .map_err(|error| format!("读取 CDP 帧失败: {error}"))?;
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut length = u64::from(header[1] & 0x7f);
        if length == 126 {
            let mut bytes = [0u8; 2];
            stream
                .read_exact(&mut bytes)
                .map_err(|error| format!("读取 CDP 帧长度失败: {error}"))?;
            length = u64::from(u16::from_be_bytes(bytes));
        } else if length == 127 {
            let mut bytes = [0u8; 8];
            stream
                .read_exact(&mut bytes)
                .map_err(|error| format!("读取 CDP 帧长度失败: {error}"))?;
            length = u64::from_be_bytes(bytes);
        }
        if length > 4 * 1024 * 1024 {
            return Err("CDP 帧过大".to_string());
        }
        let mask = if masked {
            let mut bytes = [0u8; 4];
            stream
                .read_exact(&mut bytes)
                .map_err(|error| format!("读取 CDP 掩码失败: {error}"))?;
            Some(bytes)
        } else {
            None
        };
        let mut payload = vec![0u8; length as usize];
        stream
            .read_exact(&mut payload)
            .map_err(|error| format!("读取 CDP 载荷失败: {error}"))?;
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        if opcode == 1 || opcode == 0 {
            message.extend_from_slice(&payload);
            if fin {
                return String::from_utf8(message)
                    .map_err(|error| format!("CDP 文本不是 UTF-8: {error}"));
            }
        } else if opcode == 8 {
            return Err("DevTools 主动关闭连接".to_string());
        }
    }
}

fn log(message: &str) {
    let line = format!(
        "[{}] {message}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    let path = crate::config::get_app_config_dir().join("agy-launch.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
    let _ = write!(std::io::stderr(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunked_body() {
        assert_eq!(
            decode_chunked("5\r\n{\"a\"}\r\n0\r\n\r\n").unwrap(),
            "{\"a\"}"
        );
    }

    #[test]
    fn detects_complete_response() {
        assert!(!response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}"
        ));
        assert!(response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"
        ));
        assert!(response_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"
        ));
    }

    #[test]
    fn parses_ws_url() {
        assert_eq!(
            split_ws_url("ws://127.0.0.1:4321/devtools/page/ABC").unwrap(),
            (
                "127.0.0.1".to_string(),
                4321,
                "/devtools/page/ABC".to_string()
            )
        );
    }

    #[test]
    fn counts_flags() {
        let body = r#"{"experimentConfig":{"experiments":[{"keyString":"a"},{"keyString":"b"}]}}"#;
        assert_eq!(flags_count_from_body(body).unwrap(), 2);
    }

    #[test]
    fn empty_flags_is_zero() {
        assert_eq!(
            flags_count_from_body("{\"experimentConfig\":{}}").unwrap(),
            0
        );
    }

    #[test]
    fn builds_models_url_without_duplicate_version() {
        assert_eq!(
            models_url("https://runapi.host/v1beta"),
            "https://runapi.host/v1beta/models?pageSize=1000"
        );
        assert_eq!(
            models_url("https://runapi.host/"),
            "https://runapi.host/v1beta/models?pageSize=1000"
        );
    }

    #[test]
    fn cleans_effort_suffix_from_display_name() {
        assert_eq!(clean_label("Gemini 4 Neon (High)", "x"), "Gemini 4 Neon");
        assert_eq!(clean_label("  ", "gemini-4-neon"), "gemini-4-neon");
    }

    #[test]
    fn prefers_configured_model() {
        let choices = vec![
            ModelChoice {
                name: "gemini-2.5-flash".to_string(),
                display: "Gemini 2.5 Flash".to_string(),
            },
            ModelChoice {
                name: "gemini-4-neon".to_string(),
                display: "Gemini 4 Neon".to_string(),
            },
        ];
        let ordered = prefer_configured_model(choices, "models/gemini-4-neon");
        assert_eq!(ordered[0].name, "gemini-4-neon");
    }

    #[test]
    fn model_ids_are_stable_across_order() {
        let first = stable_model_id("gemini-4-neon");
        let second = stable_model_id("gemini-4-neon");
        assert_eq!(first, second);
        assert_ne!(first, stable_model_id("gemini-4-carbon"));
        assert!(first < i64::from(i32::MAX));
    }

    #[test]
    fn bridge_script_contains_models_and_select_endpoint() {
        let bridge = BridgeConfig {
            proxy_url: "http://127.0.0.1:5000".to_string(),
            upstream: "https://runapi.host".to_string(),
            api_key: "secret".to_string(),
            model: "gemini-4-neon".to_string(),
        };
        let models = vec![
            ModelChoice {
                name: "gemini-4-neon".to_string(),
                display: "Gemini 4 Neon".to_string(),
            },
            ModelChoice {
                name: "gemini-4-carbon".to_string(),
                display: "Gemini 4 Carbon".to_string(),
            },
        ];
        let script = build_bridge_script(&bridge, &models);
        assert!(script.contains("Gemini 4 Neon (Low)"));
        assert!(script.contains("Gemini 4 Carbon (High)"));
        assert!(script.contains("http://127.0.0.1:5000/__ccswitch_select"));
        assert!(script.contains("MODEL_GOOGLE_GEMINI_INTERNAL_BYOM"));
        assert!(script.contains("__ccModelBridge"));
        assert!(!script.contains("secret"));
    }
}
