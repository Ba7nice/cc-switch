//! Antigravity IDE 接管：包装 language_server，把模型请求指向 CC Switch 的供应商卡片。
//!
//! 只改 `resources/bin/language_server` 这一个入口：原 ELF 改名为
//! `language_server.real`，包装脚本仅在 `ANTIGRAVITY_CCSWITCH_MODEL` 非空时
//! 追加 `--model_api_client_type=gemini --override_model_name`，否则原样执行。
//! 这样直接启动 IDE 仍然走 Google 登录，从 CC Switch 启动时才注入 Key/中转地址。

use crate::provider::Provider;
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const MARKER: &str = "CC Switch Antigravity IDE";
const REAL_NAME: &str = "language_server.real";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgyIdeStatus {
    /// Linux/macOS 才支持；Windows 没有 Antigravity 桌面端。
    pub supported: bool,
    /// 找到 Antigravity IDE 的 language_server。
    pub installed: bool,
    pub version: Option<String>,
    pub root: Option<String>,
    pub binary: Option<String>,
    pub launcher: Option<String>,
    /// 包装脚本已就位（且原 ELF 备份还在）。
    pub wrapper_installed: bool,
    pub running: bool,
}

/// 探测本机 Antigravity IDE。
pub fn status() -> Result<AgyIdeStatus, String> {
    let install = discover();
    Ok(match install {
        Some(install) => install.status(),
        None => AgyIdeStatus {
            supported: supported(),
            installed: false,
            version: None,
            root: None,
            binary: None,
            launcher: None,
            wrapper_installed: false,
            running: false,
        },
    })
}

/// 安装/刷新包装脚本。已经装过则只重写脚本，不动 `.real` 备份。
pub fn install_wrapper() -> Result<AgyIdeStatus, String> {
    let install = discover().ok_or_else(|| "未检测到 Antigravity IDE".to_string())?;
    install.install_wrapper()?;
    status()
}

/// 还原原始 language_server。
pub fn uninstall_wrapper() -> Result<AgyIdeStatus, String> {
    let install = discover().ok_or_else(|| "未检测到 Antigravity IDE".to_string())?;
    install.uninstall_wrapper()?;
    status()
}

/// 默认供应商记录：在 CC Switch 里成功启动过一次的卡片会成为普通启动的默认值。
const DEFAULT_PROVIDER_FILE: &str = "agy-default-provider";

pub fn default_provider_path() -> PathBuf {
    crate::config::get_app_config_dir().join(DEFAULT_PROVIDER_FILE)
}

pub fn read_default_provider() -> Option<String> {
    fs::read_to_string(default_provider_path())
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn remember_default_provider(id: &str) {
    let path = default_provider_path();
    let tmp = path.with_extension("tmp");
    let result = fs::write(&tmp, id.trim()).and_then(|_| fs::rename(&tmp, &path));
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        log::warn!("记录默认 Antigravity 供应商失败: {error}");
    }
}

/// 从 CC Switch 启动 IDE。Key 卡注入 GEMINI_API_KEY / GOOGLE_GEMINI_BASE_URL /
/// ANTIGRAVITY_CCSWITCH_MODEL；官方卡清空这三项，走原生 Google 登录。
///
/// 返回启动出的 IDE 进程。GUI 会另起线程等待它退出；`agy-launch` CLI 则自己
/// 等待，以便进程内的推理深度代理一直存活到 IDE 关闭。
pub async fn launch(provider: &Provider) -> Result<std::process::Child, String> {
    launch_with_args(provider, &[]).await
}

/// 与 [`launch`] 相同，但把 `extra_args` 原样传给 IDE 启动器。
pub async fn launch_with_args(
    provider: &Provider,
    extra_args: &[String],
) -> Result<std::process::Child, String> {
    let started_at = SystemTime::now();
    let install = discover().ok_or_else(|| "未检测到 Antigravity IDE".to_string())?;
    if !install.is_wrapper_installed() {
        return Err("接管尚未启用，请先在 Antigravity 页启用接管".to_string());
    }
    if ide_running() {
        return Err("Antigravity IDE 正在运行，请先退出后再启动".to_string());
    }

    let official = crate::services::provider::agy_direct::is_official(provider);
    let settings = crate::services::provider::agy_direct::live_settings(provider)
        .map_err(|error| error.to_string())?;
    let mut plan = plan_launch(&settings, official)?;
    let mut bridge = None;
    if let Some(reasoning) = plan.reasoning.take() {
        let upstream = reasoning.upstream.clone();
        let api_key = reasoning.api_key.clone();
        let model = reasoning.model.clone();
        let proxy = crate::agy_reasoning::ensure(reasoning).await?;
        for (key, value) in &mut plan.env {
            if *key == "GOOGLE_GEMINI_BASE_URL" {
                *value = Some(proxy.clone());
            }
        }
        bridge = Some(crate::agy_plugins_boot::BridgeConfig {
            proxy_url: proxy,
            upstream,
            api_key,
            model,
        });
    }

    let launcher = install
        .launcher
        .clone()
        .ok_or_else(|| "找不到 Antigravity 启动器".to_string())?;

    log::info!("Antigravity IDE: 从供应商卡片启动，模型 {:?}", plan.model);
    let child = spawn_ide(&launcher, &plan.env, extra_args)?;
    crate::agy_plugins_boot::watch(started_at, bridge);
    Ok(child)
}

/// 不注入任何 CC Switch 配置，按原生方式启动 IDE。
///
/// 这是 `agy-launch` 的回退路径：没有可用卡片、接管未启用、数据库读不了，
/// 或者 IDE 已经开着（交给 Electron 单实例去聚焦现有窗口）时使用。
pub fn launch_native(extra_args: &[String]) -> Result<std::process::Child, String> {
    let install = discover().ok_or_else(|| "未检测到 Antigravity IDE".to_string())?;
    let launcher = install
        .launcher
        .clone()
        .ok_or_else(|| "找不到 Antigravity 启动器".to_string())?;
    log::info!("Antigravity IDE: 原生方式启动");
    spawn_ide(&launcher, &clear_env(), extra_args)
}

fn spawn_ide(
    launcher: &Path,
    env: &[(&'static str, Option<String>)],
    extra_args: &[String],
) -> Result<std::process::Child, String> {
    let mut command = Command::new(launcher);
    for (key, value) in env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    // 启动器脚本用这个变量避免再次转进 agy-launch，形成递归。
    command.env("ANTIGRAVITY_CCSWITCH_BYPASS", "1");
    for arg in extra_args {
        command.arg(arg);
    }
    command
        .current_dir(crate::config::get_home_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    command
        .spawn()
        .map_err(|error| format!("启动 Antigravity IDE 失败: {error}"))
}

struct LaunchPlan {
    env: Vec<(&'static str, Option<String>)>,
    model: Option<String>,
    reasoning: Option<crate::agy_reasoning::Settings>,
}

fn plan_launch(settings: &Value, official: bool) -> Result<LaunchPlan, String> {
    if official {
        return Ok(LaunchPlan {
            env: clear_env(),
            model: None,
            reasoning: None,
        });
    }

    let projection = crate::agy_config::AgyProjection::of(settings);
    let api_key = projection
        .api_key
        .ok_or_else(|| "这张卡片没有 GEMINI_API_KEY，无法在 IDE 中使用".to_string())?;
    let base_url = projection
        .base_url
        .ok_or_else(|| "这张卡片没有 GOOGLE_GEMINI_BASE_URL，无法在 IDE 中使用".to_string())?;
    let model = projection
        .model
        .ok_or_else(|| "这张卡片没有模型名（config.model.name 或 GEMINI_MODEL）".to_string())?;
    // 代理同时负责应用内模型/推理切换与深度改写，所以 Key 卡总是经过代理；
    // 未显式配置深度时用 medium，应用内选择会覆盖它。
    let depth = crate::agy_reasoning::depth_of(settings)
        .unwrap_or(crate::agy_reasoning::ThinkingDepth::Medium);
    let reasoning = Some(crate::agy_reasoning::Settings {
        upstream: base_url.clone(),
        api_key: api_key.clone(),
        model: model.clone(),
        depth,
    });
    Ok(LaunchPlan {
        env: vec![
            ("GEMINI_API_KEY", Some(api_key)),
            ("GOOGLE_GEMINI_BASE_URL", Some(base_url)),
            ("ANTIGRAVITY_CCSWITCH_MODEL", Some(model.clone())),
            ("GOOGLE_API_KEY", None),
        ],
        model: Some(model),
        reasoning,
    })
}

fn clear_env() -> Vec<(&'static str, Option<String>)> {
    vec![
        ("GEMINI_API_KEY", None),
        ("GOOGLE_GEMINI_BASE_URL", None),
        ("ANTIGRAVITY_CCSWITCH_MODEL", None),
        ("GOOGLE_API_KEY", None),
    ]
}

#[derive(Clone)]
struct IdeInstall {
    root: PathBuf,
    binary: PathBuf,
    launcher: Option<PathBuf>,
    version: Option<String>,
}

impl IdeInstall {
    fn backup(&self) -> PathBuf {
        self.binary.with_file_name(REAL_NAME)
    }

    fn is_wrapper_installed(&self) -> bool {
        is_wrapper(&self.binary) && self.backup().is_file()
    }

    fn status(&self) -> AgyIdeStatus {
        AgyIdeStatus {
            supported: supported(),
            installed: self.binary.is_file(),
            version: self.version.clone(),
            root: Some(self.root.display().to_string()),
            binary: Some(self.binary.display().to_string()),
            launcher: self
                .launcher
                .as_ref()
                .map(|path| path.display().to_string()),
            wrapper_installed: self.is_wrapper_installed(),
            running: ide_running(),
        }
    }

    fn install_wrapper(&self) -> Result<(), String> {
        if !self.binary.is_file() {
            return Err(format!("找不到 {}", self.binary.display()));
        }
        let backup = self.backup();
        if !is_wrapper(&self.binary) {
            if backup.exists() {
                return Err(format!(
                    "{} 已存在，拒绝覆盖；请先移除接管",
                    backup.display()
                ));
            }
            fs::rename(&self.binary, &backup)
                .map_err(|error| format!("备份原始 language_server 失败: {error}"))?;
        } else if !backup.is_file() {
            return Err(format!("包装脚本缺少原文件备份 {}", backup.display()));
        }

        if let Err(error) = write_wrapper(&self.binary) {
            if !is_wrapper(&self.binary) && backup.exists() {
                let _ = fs::rename(&backup, &self.binary);
            }
            return Err(error);
        }
        Ok(())
    }

    fn uninstall_wrapper(&self) -> Result<(), String> {
        if !is_wrapper(&self.binary) {
            return Ok(());
        }
        let backup = self.backup();
        if !backup.is_file() {
            return Err(format!(
                "找不到原始 language_server 备份 {}",
                backup.display()
            ));
        }
        let pending = self
            .binary
            .with_file_name("language_server.ccswitch-remove");
        let _ = fs::remove_file(&pending);
        fs::rename(&self.binary, &pending).map_err(|error| format!("移开包装脚本失败: {error}"))?;
        if let Err(error) = fs::rename(&backup, &self.binary) {
            let _ = fs::rename(&pending, &self.binary);
            return Err(format!("还原原始 language_server 失败: {error}"));
        }
        let _ = fs::remove_file(&pending);
        Ok(())
    }
}

fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

fn discover() -> Option<IdeInstall> {
    discover_in(&crate::config::get_home_dir())
}

fn discover_in(home: &Path) -> Option<IdeInstall> {
    if !supported() {
        return None;
    }
    for candidate in candidate_binaries(home) {
        if !candidate.is_file() {
            continue;
        }
        let binary = fs::canonicalize(&candidate).unwrap_or(candidate);
        let Some(root) = app_root(&binary) else {
            continue;
        };
        let version = fs::read_to_string(root.join("version"))
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let launcher = launcher_for(&root, home);
        return Some(IdeInstall {
            root,
            binary,
            launcher,
            version,
        });
    }
    None
}

fn candidate_binaries(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let mut binaries = vec![
            home.join(".local/opt/antigravity/resources/bin/language_server"),
            PathBuf::from("/opt/antigravity/resources/bin/language_server"),
            PathBuf::from("/usr/lib/antigravity/resources/bin/language_server"),
        ];
        let opt = home.join(".local/opt");
        if let Ok(entries) = fs::read_dir(&opt) {
            let mut versioned: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("antigravity-"))
                })
                .collect();
            versioned.sort();
            versioned.reverse();
            for root in versioned {
                binaries.push(root.join("resources/bin/language_server"));
            }
        }
        binaries
    }
    #[cfg(target_os = "macos")]
    {
        let _ = home;
        vec![PathBuf::from(
            "/Applications/Antigravity.app/Contents/Resources/bin/language_server",
        )]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = home;
        Vec::new()
    }
}

fn app_root(binary: &Path) -> Option<PathBuf> {
    let bin_dir = binary.parent()?;
    let resources = bin_dir.parent()?;
    #[cfg(target_os = "linux")]
    {
        return resources.parent().map(Path::to_path_buf);
    }
    #[cfg(target_os = "macos")]
    {
        return resources.parent()?.parent().map(Path::to_path_buf);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = resources;
        None
    }
}

fn launcher_for(root: &Path, home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let local = home.join(".local/bin/antigravity");
        if local.is_file() {
            return Some(local);
        }
        let bundled = root.join("antigravity");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let bundled = root.join("Contents/MacOS/Antigravity");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    None
}

fn wrapper_script() -> String {
    format!(
        concat!(
            "#!/bin/sh\n",
            "# {marker} wrapper\n",
            "# 本文件由 CC Switch 生成；卸载接管时会还原 language_server.real。\n",
            "if [ -n \"${{ANTIGRAVITY_CCSWITCH_MODEL:-}}\" ]; then\n",
            "  exec \"$(dirname \"$0\")/language_server.real\" \"$@\" \\\n",
            "    --model_api_client_type=gemini \\\n",
            "    --override_model_name=\"$ANTIGRAVITY_CCSWITCH_MODEL\"\n",
            "fi\n",
            "exec \"$(dirname \"$0\")/language_server.real\" \"$@\"\n",
        ),
        marker = MARKER,
    )
}

fn write_wrapper(binary: &Path) -> Result<(), String> {
    let tmp = binary.with_file_name("language_server.ccswitch-tmp");
    let result = (|| {
        let mut file =
            fs::File::create(&tmp).map_err(|error| format!("创建包装脚本失败: {error}"))?;
        file.write_all(wrapper_script().as_bytes())
            .map_err(|error| format!("写入包装脚本失败: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("同步包装脚本失败: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("设置包装脚本权限失败: {error}"))?;
        }
        fs::rename(&tmp, binary).map_err(|error| format!("替换 language_server 失败: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn is_wrapper(binary: &Path) -> bool {
    let mut buffer = [0u8; 512];
    match fs::File::open(binary) {
        Ok(mut file) => {
            let read = file.read(&mut buffer).unwrap_or(0);
            String::from_utf8_lossy(&buffer[..read]).contains(MARKER)
        }
        Err(_) => false,
    }
}

fn ide_running() -> bool {
    #[cfg(unix)]
    {
        // language_server 这个名字被多个 Electron/Codeium 系应用共用，
        // 只看进程名会把别的 IDE 也算成 Antigravity 在运行，所以再要求
        // 命令行里出现 antigravity（App 自己的可执行文件则按名字判断）。
        let output = Command::new("ps").args(["-axo", "comm=,args="]).output();
        return output
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| is_antigravity_process(line))
            });
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn is_antigravity_process(line: &str) -> bool {
    let line = line.trim_start();
    let (comm, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let name = Path::new(comm)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.eq_ignore_ascii_case("antigravity") {
        return true;
    }
    matches!(name, "language_server" | "language_server.real")
        && args.to_ascii_lowercase().contains("antigravity")
}

fn detach(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid 是 async-signal-safe；fork 出来的子进程不是会话组长。
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake_install() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join(".local/opt/antigravity-2.34.0");
        fs::create_dir_all(root.join("resources/bin")).expect("bin dir");
        let binary = root.join("resources/bin/language_server");
        fs::write(&binary, b"\x7fELF original language server").expect("binary");
        fs::write(root.join("version"), b"44.9.1\n").expect("version");
        (dir, root)
    }

    #[test]
    fn install_then_uninstall_restores_original() {
        let (dir, root) = fake_install();
        let install = discover_in(dir.path()).expect("discovered");
        assert_eq!(install.root, fs::canonicalize(&root).unwrap());
        assert!(!install.is_wrapper_installed());

        install.install_wrapper().expect("install");
        assert!(install.is_wrapper_installed());
        let wrapper = fs::read_to_string(&install.binary).expect("wrapper text");
        assert!(wrapper.contains(MARKER));
        assert!(wrapper.contains("ANTIGRAVITY_CCSWITCH_MODEL"));
        install.uninstall_wrapper().expect("uninstall");
        assert!(!is_wrapper(&install.binary));
        assert_eq!(
            fs::read(&install.binary).unwrap(),
            b"\x7fELF original language server"
        );
        assert!(!install.backup().exists());
    }

    #[test]
    fn install_keeps_original_backup() {
        let (dir, _) = fake_install();
        let install = discover_in(dir.path()).expect("discovered");
        install.install_wrapper().expect("install");
        assert_eq!(
            fs::read(install.backup()).unwrap(),
            b"\x7fELF original language server"
        );
    }

    #[test]
    fn key_card_sets_all_three_variables() {
        let plan = plan_launch(
            &json!({
                "env": {
                    "GEMINI_API_KEY": "sk-test",
                    "GOOGLE_GEMINI_BASE_URL": "https://runapi.host",
                    "GEMINI_MODEL": "gemini-3.5-flash"
                }
            }),
            false,
        )
        .expect("plan");
        assert_eq!(plan.model.as_deref(), Some("gemini-3.5-flash"));
        let env: std::collections::HashMap<_, _> = plan.env.into_iter().collect();
        assert_eq!(
            env.get("GEMINI_API_KEY").cloned().flatten().as_deref(),
            Some("sk-test")
        );
        assert_eq!(
            env.get("GOOGLE_GEMINI_BASE_URL")
                .cloned()
                .flatten()
                .as_deref(),
            Some("https://runapi.host")
        );
        assert_eq!(
            env.get("ANTIGRAVITY_CCSWITCH_MODEL")
                .cloned()
                .flatten()
                .as_deref(),
            Some("gemini-3.5-flash")
        );
        assert_eq!(env.get("GOOGLE_API_KEY").cloned().flatten(), None);
    }

    #[test]
    fn key_card_requires_endpoint_and_model() {
        let no_endpoint = json!({ "env": { "GEMINI_API_KEY": "sk-test", "GEMINI_MODEL": "m" } });
        assert!(plan_launch(&no_endpoint, false).is_err());
        let no_model = json!({ "env": { "GEMINI_API_KEY": "sk-test", "GOOGLE_GEMINI_BASE_URL": "https://runapi.host" } });
        assert!(plan_launch(&no_model, false).is_err());
    }

    #[test]
    fn process_detection_ignores_unrelated_language_servers() {
        assert!(is_antigravity_process(
            "/home/u/.local/opt/antigravity-2.34.0/antigravity --ozone-platform-hint=auto"
        ));
        assert!(is_antigravity_process(
            "/home/u/.local/opt/antigravity-2.34.0/resources/bin/language_server --app_data_dir antigravity"
        ));
        assert!(is_antigravity_process(
            "/home/u/.local/opt/antigravity-2.34.0/resources/bin/language_server.real --app_data_dir antigravity"
        ));
        assert!(!is_antigravity_process(
            "/usr/lib/windsurf/resources/bin/language_server --app_data_dir windsurf"
        ));
        assert!(!is_antigravity_process(
            "/usr/bin/clangd --background-index"
        ));
    }

    #[test]
    fn official_card_clears_credentials() {
        let plan = plan_launch(
            &json!({ "env": { "GEMINI_API_KEY": "sk-test", "GOOGLE_GEMINI_BASE_URL": "https://runapi.host" } }),
            true,
        )
        .expect("plan");
        assert_eq!(plan.model, None);
        assert!(plan.env.iter().all(|(_, value)| value.is_none()));
    }
}
