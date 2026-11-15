use crate::database::{Database, PaginatedUpstreamErrors, UpstreamErrorFilters, UpstreamErrorLog};
use crate::error::AppError;
use crate::store::AppState;
use tauri::State;

async fn query<T, F>(state: &State<'_, AppState>, operation: F) -> Result<T, AppError>
where T: Send + 'static, F: FnOnce(&Database) -> Result<T, AppError> + Send + 'static {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || operation(&db)).await
        .map_err(|error| AppError::Message(format!("Upstream error query failed: {error}")))?
}

#[tauri::command]
pub async fn get_upstream_error_logs(
    state: State<'_, AppState>, filters: UpstreamErrorFilters, page: u32, page_size: u32,
) -> Result<PaginatedUpstreamErrors, AppError> {
    query(&state, move |db| db.get_upstream_error_logs(&filters, page, page_size)).await
}

#[tauri::command]
pub async fn get_upstream_error_detail(state: State<'_, AppState>, id: i64) -> Result<Option<UpstreamErrorLog>, AppError> {
    query(&state, move |db| db.get_upstream_error_detail(id)).await
}

#[tauri::command]
pub async fn clear_upstream_error_logs(state: State<'_, AppState>) -> Result<usize, AppError> {
    query(&state, Database::clear_upstream_error_logs).await
}
