//! Local diagnostic history, deliberately separate from billable request logs.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::{params, params_from_iter, types::Value, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const UPSTREAM_ERROR_RETENTION_LIMIT: i64 = 10_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamErrorSource {
    KeepAlive,
    AggregationRetry,
}

impl UpstreamErrorSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeepAlive => "keep_alive",
            Self::AggregationRetry => "aggregation_retry",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamErrorLog {
    pub id: i64,
    pub created_at: i64,
    pub source: UpstreamErrorSource,
    pub app_type: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub request_id: String,
    pub upstream_request_id: Option<String>,
    pub attempt: u64,
    pub status_code: Option<u16>,
    pub error_code: String,
    pub message: String,
    pub response_body: Option<String>,
    pub body_truncated: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamErrorFilters {
    pub source: Option<UpstreamErrorSource>,
    pub app_type: Option<String>,
    pub status_code: Option<u16>,
    pub query: Option<String>,
    pub since: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginatedUpstreamErrors {
    pub data: Vec<UpstreamErrorLog>,
    pub total: u64,
    pub page: u32,
    pub page_size: u32,
    pub retention_limit: i64,
}

const COLUMNS: &str = "id, created_at, source, app_type, provider_id, provider_name,
    model, request_id, upstream_request_id, attempt, status_code, error_code,
    message, response_body, body_truncated, elapsed_ms";

fn db_error(error: rusqlite::Error) -> AppError {
    AppError::Database(format!("Upstream error history: {error}"))
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UpstreamErrorLog> {
    let source: String = row.get(2)?;
    let source = match source.as_str() {
        "keep_alive" => UpstreamErrorSource::KeepAlive,
        "aggregation_retry" => UpstreamErrorSource::AggregationRetry,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(UpstreamErrorLog {
        id: row.get(0)?,
        created_at: row.get(1)?,
        source,
        app_type: row.get(3)?,
        provider_id: row.get(4)?,
        provider_name: row.get(5)?,
        model: row.get(6)?,
        request_id: row.get(7)?,
        upstream_request_id: row.get(8)?,
        attempt: row.get(9)?,
        status_code: row.get(10)?,
        error_code: row.get(11)?,
        message: row.get(12)?,
        response_body: row.get(13)?,
        body_truncated: row.get(14)?,
        elapsed_ms: row.get(15)?,
    })
}

impl Database {
    pub(crate) fn create_upstream_error_table(conn: &rusqlite::Connection) -> Result<(), AppError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS upstream_error_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at INTEGER NOT NULL,
                source TEXT NOT NULL CHECK (source IN ('keep_alive', 'aggregation_retry')),
                app_type TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                provider_name TEXT NOT NULL,
                model TEXT NOT NULL,
                request_id TEXT NOT NULL,
                upstream_request_id TEXT,
                attempt INTEGER NOT NULL,
                status_code INTEGER,
                error_code TEXT NOT NULL,
                message TEXT NOT NULL,
                response_body TEXT,
                body_truncated INTEGER NOT NULL DEFAULT 0,
                elapsed_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_upstream_errors_created ON upstream_error_logs(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_upstream_errors_source ON upstream_error_logs(source, id DESC);",
        )
        .map_err(db_error)
    }

    pub(crate) fn insert_upstream_error(&self, entry: &UpstreamErrorLog) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        Self::in_savepoint(&conn, "upstream_error_insert", || {
            conn.execute(
                "INSERT INTO upstream_error_logs (
                    created_at, source, app_type, provider_id, provider_name, model,
                    request_id, upstream_request_id, attempt, status_code, error_code,
                    message, response_body, body_truncated, elapsed_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![entry.created_at, entry.source.as_str(), entry.app_type,
                    entry.provider_id, entry.provider_name, entry.model, entry.request_id,
                    entry.upstream_request_id, entry.attempt.min(i64::MAX as u64), entry.status_code,
                    entry.error_code, entry.message, entry.response_body, entry.body_truncated,
                    entry.elapsed_ms.min(i64::MAX as u64)],
            ).map_err(db_error)?;
            // Monotonic IDs keep pruning cheap even for zero-delay retries.
            conn.execute(
                "DELETE FROM upstream_error_logs WHERE id <= ?1",
                [conn.last_insert_rowid() - UPSTREAM_ERROR_RETENTION_LIMIT],
            ).map_err(db_error)?;
            Ok(())
        })
    }

    pub fn get_upstream_error_logs(
        &self,
        filters: &UpstreamErrorFilters,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedUpstreamErrors, AppError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let mut clauses = vec!["1 = 1".to_string()];
        let mut values: Vec<Value> = Vec::new();
        if let Some(source) = filters.source {
            clauses.push("source = ?".into());
            values.push(source.as_str().to_string().into());
        }
        if let Some(app) = filters.app_type.as_ref().filter(|s| !s.trim().is_empty()) {
            clauses.push("app_type = ?".into());
            values.push(app.clone().into());
        }
        if let Some(status) = filters.status_code {
            clauses.push("status_code = ?".into());
            values.push(i64::from(status).into());
        }
        if let Some(since) = filters.since {
            clauses.push("created_at >= ?".into());
            values.push(since.into());
        }
        if let Some(query) = filters.query.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            // instr treats '%' and '_' literally; no SQL or wildcard interpolation.
            clauses.push("instr(lower(provider_name || ' ' || model || ' ' || error_code || ' ' || message || ' ' || request_id || ' ' || coalesce(upstream_request_id, '')), lower(?)) > 0".into());
            values.push(query.chars().take(256).collect::<String>().into());
        }
        let predicate = clauses.join(" AND ");
        let conn = lock_conn!(self.conn);
        let total = conn.query_row(
            &format!("SELECT COUNT(*) FROM upstream_error_logs WHERE {predicate}"),
            params_from_iter(values.iter()),
            |row| row.get::<_, u64>(0),
        ).map_err(db_error)?;
        values.push(i64::from(page_size).into());
        values.push((i64::from(page - 1) * i64::from(page_size)).into());
        // Large response details are fetched only when the user opens a record.
        let summary_columns = COLUMNS.replace("response_body", "NULL AS response_body");
        let mut statement = conn.prepare(&format!(
            "SELECT {summary_columns} FROM upstream_error_logs WHERE {predicate} ORDER BY id DESC LIMIT ? OFFSET ?"
        )).map_err(db_error)?;
        let data = statement.query_map(params_from_iter(values.iter()), read_row)
            .map_err(db_error)?.collect::<Result<Vec<_>, _>>().map_err(db_error)?;
        Ok(PaginatedUpstreamErrors { data, total, page, page_size, retention_limit: UPSTREAM_ERROR_RETENTION_LIMIT })
    }

    pub fn get_upstream_error_detail(&self, id: i64) -> Result<Option<UpstreamErrorLog>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(&format!("SELECT {COLUMNS} FROM upstream_error_logs WHERE id = ?1"), [id], read_row)
            .optional().map_err(db_error)
    }

    pub fn clear_upstream_error_logs(&self) -> Result<usize, AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute("DELETE FROM upstream_error_logs", []).map_err(db_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_errors_upgrade_v26_is_idempotent() {
        let db = Database::memory().unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("DROP TABLE upstream_error_logs; PRAGMA user_version = 26;").unwrap();
        }
        db.apply_schema_migrations().unwrap();
        db.insert_upstream_error(&entry(UpstreamErrorSource::KeepAlive, "model-a")).unwrap();
        db.apply_schema_migrations().unwrap();
        assert_eq!(db.get_upstream_error_logs(&Default::default(), 1, 50).unwrap().total, 1);
    }

    fn entry(source: UpstreamErrorSource, model: &str) -> UpstreamErrorLog {
        UpstreamErrorLog {
            id: 0, created_at: 1234567890000, source, app_type: "codex".into(),
            provider_id: "provider-a".into(), provider_name: "Gateway".into(), model: model.into(),
            request_id: "request-test".into(), upstream_request_id: Some("upstream-test".into()),
            attempt: 2, status_code: Some(500), error_code: "get_channel_failed".into(),
            message: "No available channel".into(), response_body: Some("{\"error\":\"busy\"}".into()),
            body_truncated: false, elapsed_ms: 42,
        }
    }

    #[test]
    fn upstream_errors_filter_paginate_and_keep_details_separate() {
        let db = Database::memory().unwrap();
        db.insert_upstream_error(&entry(UpstreamErrorSource::KeepAlive, "model-a")).unwrap();
        db.insert_upstream_error(&entry(UpstreamErrorSource::AggregationRetry, "model-b")).unwrap();
        let all = db.get_upstream_error_logs(&Default::default(), 1, 1).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.data[0].model, "model-b");
        assert!(all.data[0].response_body.is_none());
        let detail = db.get_upstream_error_detail(all.data[0].id).unwrap().unwrap();
        assert_eq!(detail.response_body.as_deref(), Some("{\"error\":\"busy\"}"));
        let filters = UpstreamErrorFilters { source: Some(UpstreamErrorSource::KeepAlive), app_type: Some("codex".into()),
            status_code: Some(500), query: Some("MODEL-A".into()), since: Some(1234567890000) };
        assert_eq!(db.get_upstream_error_logs(&filters, 1, 50).unwrap().total, 1);
        let filters = UpstreamErrorFilters { query: Some("%' OR 1=1 --".into()), ..Default::default() };
        assert_eq!(db.get_upstream_error_logs(&filters, 1, 50).unwrap().total, 0);
        assert_eq!(db.get_upstream_error_logs(&Default::default(), 2, 1).unwrap().data[0].model, "model-a");
    }

    #[test]
    fn upstream_errors_do_not_write_or_clear_request_usage() {
        let db = Database::memory().unwrap();
        db.insert_upstream_error(&entry(UpstreamErrorSource::KeepAlive, "model-a")).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let requests: i64 = conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| row.get(0)).unwrap();
            assert_eq!(requests, 0);
            conn.execute("INSERT INTO proxy_request_logs (request_id, provider_id, app_type, model, latency_ms, status_code, created_at) VALUES ('keep-me', 'provider-a', 'codex', 'model-a', 1, 200, 12345)", []).unwrap();
        }
        assert_eq!(db.clear_upstream_error_logs().unwrap(), 1);
        assert_eq!(db.get_upstream_error_logs(&Default::default(), 1, 50).unwrap().total, 0);
        let conn = db.conn.lock().unwrap();
        let requests: i64 = conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| row.get(0)).unwrap();
        assert_eq!(requests, 1);
    }

    #[test]
    fn upstream_errors_retain_newest_entries_and_clamp_pagination() {
        let db = Database::memory().unwrap();
        let entry = entry(UpstreamErrorSource::KeepAlive, "model-a");
        for _ in 0..UPSTREAM_ERROR_RETENTION_LIMIT + 2 { db.insert_upstream_error(&entry).unwrap(); }
        let page = db.get_upstream_error_logs(&Default::default(), 0, u32::MAX).unwrap();
        assert_eq!(page.total, UPSTREAM_ERROR_RETENTION_LIMIT as u64);
        assert_eq!(page.page, 1);
        assert_eq!(page.data.len(), 100);
        assert!(db.get_upstream_error_detail(1).unwrap().is_none());
        assert!(db.get_upstream_error_detail(3).unwrap().is_some());
    }
}
