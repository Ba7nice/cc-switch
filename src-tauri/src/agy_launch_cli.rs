//! `cc-switch agy-launch`：不打开 CC Switch 界面，直接按供应商卡片拉起 IDE。
//!
//! 由普通启动器 `~/.local/bin/antigravity` 的集成块调用。这里只读打开
//! `cc-switch.db`，复用 GUI 的启动逻辑（语言服务器包装器、Key/Base URL、推理深度
//! 代理），并一直等到 IDE 退出，让进程内代理保持存活。

use crate::app_config::AppType;
use crate::database::Database;
use crate::provider::Provider;
use indexmap::IndexMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const PROVIDER_ENV: &str = "ANTIGRAVITY_CCSWITCH_PROVIDER";

pub fn run(args: &[String]) -> i32 {
    // 从终端启动时，用户关掉终端不应把代理和 IDE 一起带走。
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    let (requested, ide_args) = split_args(args);
    match launch(requested.as_deref(), &ide_args) {
        Ok(()) => 0,
        Err(error) => {
            write_log(&format!("启动失败: {error}"));
            notify_error(&error);
            1
        }
    }
}

fn launch(requested: Option<&str>, ide_args: &[String]) -> Result<(), String> {
    let status = match crate::agy_ide::status() {
        Ok(status) => status,
        Err(error) => {
            write_log(&format!("读取 IDE 状态失败，按原生方式启动: {error}"));
            return launch_native(ide_args);
        }
    };
    if !status.installed {
        write_log("未检测到 Antigravity IDE，按原生方式启动");
        return launch_native(ide_args);
    }
    if !status.wrapper_installed {
        write_log("CC Switch 接管未启用，按原生方式启动");
        return launch_native(ide_args);
    }
    if status.running {
        write_log("Antigravity IDE 已在运行，交给 Electron 单实例聚焦现有窗口");
        return launch_native(ide_args);
    }

    let db = match open_db() {
        Ok(db) => db,
        Err(error) => {
            write_log(&format!("读取供应商数据库失败，按原生方式启动: {error}"));
            return launch_native(ide_args);
        }
    };
    let providers = match db.get_all_providers(&AppType::Agy) {
        Ok(providers) => providers,
        Err(error) => {
            write_log(&format!("读取供应商卡片失败，按原生方式启动: {error}"));
            return launch_native(ide_args);
        }
    };
    let Some(provider) = pick_provider(&db, &providers, requested)? else {
        write_log("还没有 Antigravity 供应商卡片，按原生方式启动");
        return launch_native(ide_args);
    };

    write_log(&format!(
        "使用供应商 {}（{}）启动 Antigravity IDE",
        provider.name, provider.id
    ));
    let mut child =
        tauri::async_runtime::block_on(crate::agy_ide::launch_with_args(&provider, ide_args))?;
    crate::agy_ide::remember_default_provider(&provider.id);
    let status = child
        .wait()
        .map_err(|error| format!("等待 Antigravity IDE 退出失败: {error}"))?;
    write_log(&format!("Antigravity IDE 已退出（{status}）"));
    Ok(())
}

fn launch_native(ide_args: &[String]) -> Result<(), String> {
    let mut child = crate::agy_ide::launch_native(ide_args)?;
    let status = child
        .wait()
        .map_err(|error| format!("等待 Antigravity IDE 退出失败: {error}"))?;
    write_log(&format!("Antigravity IDE 已退出（{status}）"));
    Ok(())
}

/// 选择这次要用的卡片。显式 `--provider` 找不到时直接报错，避免静默换成别的卡；
/// 文件里的旧 id 失效则记日志并继续回退。
fn pick_provider(
    db: &Database,
    providers: &IndexMap<String, Provider>,
    requested: Option<&str>,
) -> Result<Option<Provider>, String> {
    if providers.is_empty() {
        return Ok(None);
    }
    if let Some(id) = requested {
        return providers
            .get(id)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("指定的供应商不存在: {id}"));
    }

    let env_id = std::env::var(PROVIDER_ENV)
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let current_id = db.get_current_provider(&AppType::Agy).ok().flatten();
    let candidate = env_id
        .or_else(crate::agy_ide::read_default_provider)
        .or(current_id);

    if let Some(id) = candidate {
        if let Some(provider) = providers.get(id.trim()) {
            return Ok(Some(provider.clone()));
        }
        write_log(&format!("供应商 {id} 已不存在，回退到第一张卡片"));
    }
    Ok(providers.values().next().cloned())
}

fn open_db() -> Result<Database, String> {
    let path = crate::config::get_app_config_dir().join("cc-switch.db");
    if !path.is_file() {
        return Err(format!("找不到 {}", path.display()));
    }
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("打开 {} 失败: {error}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("设置数据库等待超时失败: {error}"))?;
    Ok(Database {
        conn: Mutex::new(conn),
        log_count_cache: Mutex::new(None),
    })
}

fn split_args(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut requested = None;
    let mut ide_args = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--provider" {
            requested = iter.next().cloned();
        } else if let Some(id) = arg.strip_prefix("--provider=") {
            requested = Some(id.to_string());
        } else {
            ide_args.push(arg.clone());
        }
    }
    (requested, ide_args)
}

fn log_path() -> PathBuf {
    crate::config::get_app_config_dir().join("agy-launch.log")
}

fn write_log(message: &str) {
    let line = format!(
        "[{}] {message}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
    eprint!("{line}");
}

fn notify_error(message: &str) {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
        // 桌面双击没有终端，能弹通知就弹一条；失败不影响原生回退。
        let _ = std::process::Command::new("notify-send")
            .arg("Antigravity 启动失败")
            .arg(message)
            .spawn();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = message;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_provider_and_forwards_ide_args() {
        let args = vec![
            "--provider".to_string(),
            "runapi".to_string(),
            "/tmp/a.txt".to_string(),
        ];
        let (provider, ide_args) = split_args(&args);
        assert_eq!(provider.as_deref(), Some("runapi"));
        assert_eq!(ide_args, vec!["/tmp/a.txt"]);
    }

    #[test]
    fn supports_provider_equals_form() {
        let args = vec!["--provider=runapi".to_string()];
        let (provider, ide_args) = split_args(&args);
        assert_eq!(provider.as_deref(), Some("runapi"));
        assert!(ide_args.is_empty());
    }
}
