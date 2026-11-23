//! Antigravity IDE 接管命令。

use crate::agy_ide::{self, AgyIdeStatus};
use crate::app_config::AppType;
use crate::store::AppState;
use tauri::State;

#[tauri::command]
pub async fn agy_ide_status() -> Result<AgyIdeStatus, String> {
    agy_ide::status()
}

#[tauri::command]
pub async fn agy_ide_install_wrapper() -> Result<AgyIdeStatus, String> {
    agy_ide::install_wrapper()
}

#[tauri::command]
pub async fn agy_ide_uninstall_wrapper() -> Result<AgyIdeStatus, String> {
    agy_ide::uninstall_wrapper()
}

#[tauri::command]
pub async fn agy_ide_launch(state: State<'_, AppState>, provider_id: String) -> Result<(), String> {
    let provider = state
        .db
        .get_provider_by_id(&provider_id, &AppType::Agy)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("供应商不存在: {provider_id}"))?;
    let mut child = agy_ide::launch(&provider).await?;
    // 下一次从桌面图标/终端普通启动时，默认就用这张卡。
    agy_ide::remember_default_provider(&provider.id);
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            log::warn!("等待 Antigravity IDE 退出失败: {error}");
        }
    });
    Ok(())
}
