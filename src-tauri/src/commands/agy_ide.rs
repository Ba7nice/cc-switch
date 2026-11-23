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
    agy_ide::launch(&provider)
}
