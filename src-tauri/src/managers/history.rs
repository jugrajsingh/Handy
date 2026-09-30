use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tauri_specta::Event;

/// Database migrations for transcription history.
/// Each migration is applied in order. The library tracks which migrations
/// have been applied using SQLite's user_version pragma.
///
/// Note: For users upgrading from tauri-plugin-sql, migrate_from_tauri_plugin_sql()
/// converts the old _sqlx_migrations table tracking to the user_version pragma,
/// ensuring migrations don't re-run on existing databases.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS transcription_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            saved BOOLEAN NOT NULL DEFAULT 0,
            title TEXT NOT NULL,
            transcription_text TEXT NOT NULL
        );",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL DEFAULT 0;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_provider TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_model TEXT;"),
];

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PaginatedHistory {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Type)]
pub struct HistoryClearSummary {
    pub entries: usize,
    pub recordings: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "action")]
pub enum HistoryUpdatePayload {
    #[serde(rename = "added")]
    Added { entry: HistoryEntry },
    #[serde(rename = "updated")]
    Updated { entry: HistoryEntry },
    #[serde(rename = "deleted")]
    Deleted { id: i64 },
    #[serde(rename = "toggled")]
    Toggled { id: i64 },
    #[serde(rename = "cleared")]
    Cleared {},
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_name: String,
    pub timestamp: i64,
    pub saved: bool,
    pub title: String,
    pub transcription_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub post_process_requested: bool,
    pub post_process_provider: Option<String>,
    pub post_process_model: Option<String>,
}

/// Provider and model from the settings snapshot used for successful cleanup.
#[derive(Clone, Debug)]
pub struct PostProcessProvenance {
    pub provider: String,
    pub model: Option<String>,
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
}

impl HistoryManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create recordings directory in app data dir
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        let recordings_dir = app_data_dir.join("recordings");
        let db_path = app_data_dir.join("history.db");

        // Ensure recordings directory exists
        if !recordings_dir.exists() {
            fs::create_dir_all(&recordings_dir)?;
            debug!("Created recordings directory: {:?}", recordings_dir);
        }

        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
        };

        // Initialize database and run migrations synchronously
        manager.init_database()?;

        Ok(manager)
    }

    fn init_database(&self) -> Result<()> {
        info!("Initializing database at {:?}", self.db_path);

        let mut conn = Connection::open(&self.db_path)?;

        // Handle migration from tauri-plugin-sql to rusqlite_migration
        // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
        self.migrate_from_tauri_plugin_sql(&conn)?;

        // Create migrations object and run to latest version
        let migrations = Migrations::new(MIGRATIONS.to_vec());

        // Validate migrations in debug builds
        #[cfg(debug_assertions)]
        migrations.validate().expect("Invalid migrations");

        // Get current version before migration
        let version_before: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        debug!("Database version before migration: {}", version_before);

        // Apply any pending migrations
        migrations.to_latest(&mut conn)?;

        // Get version after migration
        let version_after: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version_after > version_before {
            info!(
                "Database migrated from version {} to {}",
                version_before, version_after
            );
        } else {
            debug!("Database already at latest version {}", version_after);
        }

        Ok(())
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used a _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(&self, conn: &Connection) -> Result<()> {
        // Check if the old _sqlx_migrations table exists
        let has_sqlx_migrations: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_sqlx_migrations {
            return Ok(());
        }

        // Check current user_version
        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > 0 {
            // Already migrated to rusqlite_migration system
            return Ok(());
        }

        // Get the highest version from the old migrations table
        let old_version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if old_version > 0 {
            info!(
                "Migrating from tauri-plugin-sql (version {}) to rusqlite_migration",
                old_version
            );

            // Set user_version to match the old migration state
            conn.pragma_update(None, "user_version", old_version)?;

            // Optionally drop the old migrations table (keeping it doesn't hurt)
            // conn.execute("DROP TABLE IF EXISTS _sqlx_migrations", [])?;

            info!(
                "Migration tracking converted: user_version set to {}",
                old_version
            );
        }

        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        Ok(conn)
    }

    fn map_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
        Ok(HistoryEntry {
            id: row.get("id")?,
            file_name: row.get("file_name")?,
            timestamp: row.get("timestamp")?,
            saved: row.get("saved")?,
            title: row.get("title")?,
            transcription_text: row.get("transcription_text")?,
            post_processed_text: row.get("post_processed_text")?,
            post_process_prompt: row.get("post_process_prompt")?,
            post_process_requested: row.get("post_process_requested")?,
            post_process_provider: row.get("post_process_provider")?,
            post_process_model: row.get("post_process_model")?,
        })
    }

    pub fn recordings_dir(&self) -> &std::path::Path {
        &self.recordings_dir
    }

    /// Save a new history entry to the database.
    /// The WAV file should already have been written to the recordings directory.
    pub fn save_entry(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        provenance: Option<PostProcessProvenance>,
    ) -> Result<HistoryEntry> {
        let timestamp = Utc::now().timestamp();
        let title = self.format_timestamp_title(timestamp);

        let mut entry = HistoryEntry {
            id: 0,
            file_name,
            timestamp,
            saved: false,
            title,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            post_process_requested,
            post_process_provider: provenance.as_ref().map(|value| value.provider.clone()),
            post_process_model: provenance.and_then(|value| value.model),
        };

        let conn = self.get_connection()?;
        entry.id = Self::insert_history_entry_with_conn(&conn, &entry)?;

        debug!("Saved history entry with id {}", entry.id);

        self.cleanup_old_entries()?;

        // Emit typed event for real-time frontend updates
        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    fn insert_history_entry_with_conn(conn: &Connection, entry: &HistoryEntry) -> Result<i64> {
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title,
             transcription_text, post_processed_text, post_process_prompt,
             post_process_requested, post_process_provider, post_process_model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.file_name,
                entry.timestamp,
                entry.saved,
                entry.title,
                entry.transcription_text,
                entry.post_processed_text,
                entry.post_process_prompt,
                entry.post_process_requested,
                entry.post_process_provider,
                entry.post_process_model,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Update an existing history entry with new transcription results (used by retry).
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        provenance: Option<PostProcessProvenance>,
    ) -> Result<HistoryEntry> {
        let conn = self.get_connection()?;
        let entry = Self::update_transcription_with_conn(
            &conn,
            id,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            provenance,
        )?;

        debug!("Updated transcription for history entry {}", id);

        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    /// Write retry text and provenance together using the supplied connection.
    pub fn update_transcription_with_conn(
        conn: &Connection,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        provenance: Option<PostProcessProvenance>,
    ) -> Result<HistoryEntry> {
        let updated = conn.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                 post_processed_text = ?2,
                 post_process_prompt = ?3,
                 post_process_provider = ?4,
                 post_process_model = ?5
             WHERE id = ?6",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                provenance.as_ref().map(|value| value.provider.as_str()),
                provenance.as_ref().and_then(|value| value.model.as_deref()),
                id
            ],
        )?;

        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = conn
            .query_row(
                "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, post_process_provider, post_process_model
                 FROM transcription_history WHERE id = ?1",
                params![id],
                Self::map_history_entry,
            )?;

        Ok(entry)
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        let retention_period = crate::settings::get_recording_retention_period(&self.app_handle);

        match retention_period {
            crate::settings::RecordingRetentionPeriod::Never => {
                // Don't delete anything
                Ok(())
            }
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                // Use the old count-based logic with history_limit
                let limit = crate::settings::get_history_limit(&self.app_handle);
                self.cleanup_by_count(limit)
            }
            _ => {
                // Use time-based logic
                self.cleanup_by_time(retention_period)
            }
        }
    }

    fn delete_entries_and_files(&self, entries: &[(i64, String)]) -> Result<usize> {
        let conn = self.get_connection()?;
        let removed =
            Self::delete_entries_and_files_with_conn(&conn, &self.recordings_dir, entries)?;
        Ok(removed.recordings)
    }

    fn clear_candidates(conn: &Connection, keep_saved: bool) -> Result<Vec<(i64, String)>> {
        let mut statement = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE (?1 = 0 OR saved = 0) ORDER BY id",
        )?;
        let rows =
            statement.query_map(params![keep_saved], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn delete_entries_and_files_with_conn(
        conn: &Connection,
        recordings_dir: &Path,
        entries: &[(i64, String)],
    ) -> Result<HistoryClearSummary> {
        let mut removed = HistoryClearSummary::default();
        for (id, file_name) in entries {
            let path = recordings_dir.join(file_name);
            match fs::remove_file(&path) {
                Ok(()) => removed.recordings += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => error!("Failed to delete WAV file {}: {}", file_name, error),
            }
            removed.entries += conn.execute(
                "DELETE FROM transcription_history WHERE id = ?1",
                params![id],
            )?;
        }
        Ok(removed)
    }

    fn clear_history_with_connection(
        conn: &mut Connection,
        recordings_dir: &Path,
        keep_saved: bool,
    ) -> Result<HistoryClearSummary> {
        Self::with_clear_transaction(conn, |transaction| {
            let entries = Self::clear_candidates(transaction, keep_saved)?;
            Self::delete_entries_and_files_with_conn(transaction, recordings_dir, &entries)
        })
    }

    fn with_clear_transaction<T>(
        conn: &mut Connection,
        operation: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Count all eligible history rows and their existing recordings for confirmation.
    pub fn history_clear_summary(&self, keep_saved: bool) -> Result<HistoryClearSummary> {
        let conn = self.get_connection()?;
        Self::history_clear_summary_with_conn(&conn, &self.recordings_dir, keep_saved)
    }

    fn history_clear_summary_with_conn(
        conn: &Connection,
        recordings_dir: &Path,
        keep_saved: bool,
    ) -> Result<HistoryClearSummary> {
        let entries = Self::clear_candidates(conn, keep_saved)?;
        let recordings = entries
            .iter()
            .filter(|(_, name)| recordings_dir.join(name).is_file())
            .count();
        Ok(HistoryClearSummary {
            entries: entries.len(),
            recordings,
        })
    }

    /// Clear a serialized snapshot of history and emit one update after commit.
    pub fn clear_history(&self, keep_saved: bool) -> Result<HistoryClearSummary> {
        let mut conn = self.get_connection()?;
        Self::clear_history_and_notify_with_connection(
            &mut conn,
            &self.recordings_dir,
            keep_saved,
            |event| {
                if let Err(error) = event.emit(&self.app_handle) {
                    error!("Failed to emit cleared history event: {error}");
                }
            },
        )
    }

    fn clear_history_and_notify_with_connection(
        conn: &mut Connection,
        recordings_dir: &Path,
        keep_saved: bool,
        mut notify: impl FnMut(HistoryUpdatePayload),
    ) -> Result<HistoryClearSummary> {
        let removed = Self::clear_history_with_connection(conn, recordings_dir, keep_saved)?;
        notify(HistoryUpdatePayload::Cleared {});
        Ok(removed)
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;

        // Get all entries that are not saved, ordered by timestamp desc
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 ORDER BY timestamp DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries.push(row?);
        }

        if entries.len() > limit {
            let entries_to_delete = &entries[limit..];
            let deleted_count = self.delete_entries_and_files(entries_to_delete)?;

            if deleted_count > 0 {
                debug!("Cleaned up {} old history entries by count", deleted_count);
            }
        }

        Ok(())
    }

    fn cleanup_by_time(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
    ) -> Result<()> {
        let conn = self.get_connection()?;

        // Calculate cutoff timestamp (current time minus retention period)
        let now = Utc::now().timestamp();
        let cutoff_timestamp = match retention_period {
            crate::settings::RecordingRetentionPeriod::Days3 => now - (3 * 24 * 60 * 60), // 3 days in seconds
            crate::settings::RecordingRetentionPeriod::Weeks2 => now - (2 * 7 * 24 * 60 * 60), // 2 weeks in seconds
            crate::settings::RecordingRetentionPeriod::Months3 => now - (3 * 30 * 24 * 60 * 60), // 3 months in seconds (approximate)
            _ => unreachable!("Should not reach here"),
        };

        // Get all unsaved entries older than the cutoff timestamp
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;

        let rows = stmt.query_map(params![cutoff_timestamp], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries_to_delete: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries_to_delete.push(row?);
        }

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    pub async fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        let limit = limit.map(|l| l.min(100));

        let mut entries: Vec<HistoryEntry> = match (cursor, limit) {
            (Some(cursor_id), Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, post_process_provider, post_process_model
                     FROM transcription_history
                     WHERE id < ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let result = stmt
                    .query_map(params![cursor_id, fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (None, Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, post_process_provider, post_process_model
                     FROM transcription_history
                     ORDER BY id DESC
                     LIMIT ?1",
                )?;
                let result = stmt
                    .query_map(params![fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (_, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, post_process_provider, post_process_model
                     FROM transcription_history
                     ORDER BY id DESC",
                )?;
                let result = stmt
                    .query_map([], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
        };

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }

        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                post_process_provider,
                post_process_model
             FROM transcription_history
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest entry with non-empty transcription text.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                post_process_provider,
                post_process_model
             FROM transcription_history
             WHERE transcription_text != ''
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Toggle a star in an immediate transaction and emit its update after commit.
    pub fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let mut conn = self.get_connection()?;
        let new_saved = Self::toggle_saved_status_with_conn(&mut conn, id)?;

        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    fn toggle_saved_status_with_conn(conn: &mut Connection, id: i64) -> Result<bool> {
        Self::toggle_saved_status_with_conn_after_read(conn, id, || Ok(()))
    }

    fn toggle_saved_status_with_conn_after_read(
        conn: &mut Connection,
        id: i64,
        after_read: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current_saved: bool = transaction.query_row(
            "SELECT saved FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get("saved"),
        )?;

        after_read()?;
        let new_saved = !current_saved;

        transaction.execute(
            "UPDATE transcription_history SET saved = ?1 WHERE id = ?2",
            params![new_saved, id],
        )?;

        transaction.commit()?;
        Ok(new_saved)
    }

    pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf {
        self.recordings_dir.join(file_name)
    }

    pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                post_process_provider,
                post_process_model
             FROM transcription_history
             WHERE id = ?1",
        )?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name
        if let Some(entry) = self.get_entry_by_id(id).await? {
            // Delete the audio file first
            let file_path = self.get_audio_file_path(&entry.file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete audio file {}: {}", entry.file_name, e);
                    // Continue with database deletion even if file deletion fails
                }
            }
        }

        // Delete from database
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![id],
        )?;

        debug!("Deleted history entry with id: {}", id);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Deleted { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    fn format_timestamp_title(&self, timestamp: i64) -> String {
        if let Some(utc_datetime) = DateTime::from_timestamp(timestamp, 0) {
            // Convert UTC to local timezone
            let local_datetime = utc_datetime.with_timezone(&Local);
            local_datetime.format("%B %e, %Y - %l:%M%p").to_string()
        } else {
            format!("Recording {}", timestamp)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use std::sync::mpsc;
    use std::time::Duration;

    fn setup_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .unwrap();
        conn
    }

    #[test]
    fn clear_summary_counts_full_history_and_existing_eligible_recordings_without_changes() {
        let conn = setup_conn();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            HistoryManager::history_clear_summary_with_conn(&conn, dir.path(), true).unwrap(),
            HistoryClearSummary::default()
        );
        assert_eq!(
            HistoryManager::history_clear_summary_with_conn(&conn, dir.path(), false).unwrap(),
            HistoryClearSummary::default()
        );
        for timestamp in 1..=150 {
            insert_entry(&conn, timestamp, "raw", None);
            if timestamp % 3 == 0 {
                conn.execute(
                    "UPDATE transcription_history SET saved = 1 WHERE timestamp = ?1",
                    [timestamp],
                )
                .unwrap();
            }
            if timestamp % 2 == 0 {
                fs::write(dir.path().join(format!("handy-{timestamp}.wav")), b"audio").unwrap();
            }
        }
        assert_eq!(
            HistoryManager::history_clear_summary_with_conn(&conn, dir.path(), true).unwrap(),
            HistoryClearSummary {
                entries: 100,
                recordings: 50
            }
        );
        assert_eq!(
            HistoryManager::history_clear_summary_with_conn(&conn, dir.path(), false).unwrap(),
            HistoryClearSummary {
                entries: 150,
                recordings: 75
            }
        );
        let (rows, saved): (usize, usize) = conn
            .query_row(
                "SELECT COUNT(*), SUM(saved) FROM transcription_history",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, saved), (150, 50));
        for timestamp in 1..=150 {
            assert_eq!(
                dir.path().join(format!("handy-{timestamp}.wav")).is_file(),
                timestamp % 2 == 0
            );
        }
    }

    #[test]
    fn clear_notifies_once_after_commit_and_never_on_delete_or_commit_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let mut conn = Connection::open(&path).unwrap();
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .unwrap();
        let observer = Connection::open(path).unwrap();
        observer.busy_timeout(Duration::ZERO).unwrap();
        insert_entry(&conn, 100, "raw", None);
        fs::write(dir.path().join("handy-100.wav"), b"audio").unwrap();
        let mut events = Vec::new();
        let removed = HistoryManager::clear_history_and_notify_with_connection(
            &mut conn,
            dir.path(),
            true,
            |event| {
                assert!(matches!(event, HistoryUpdatePayload::Cleared {}));
                let committed_rows: usize = observer
                    .query_row("SELECT COUNT(*) FROM transcription_history", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(committed_rows, 0, "Clear must commit before notifying");
                assert!(!dir.path().join("handy-100.wav").exists());
                events.push(event);
            },
        )
        .unwrap();
        assert_eq!(
            removed,
            HistoryClearSummary {
                entries: 1,
                recordings: 1
            }
        );
        assert_eq!(events.len(), 1, "successful Clear must notify exactly once");

        insert_entry(&conn, 200, "delete failure", None);
        fs::write(dir.path().join("handy-200.wav"), b"audio").unwrap();
        conn.execute_batch("CREATE TRIGGER stop_delete BEFORE DELETE ON transcription_history BEGIN SELECT RAISE(ABORT, 'blocked'); END;").unwrap();
        let failed = HistoryManager::clear_history_and_notify_with_connection(
            &mut conn,
            dir.path(),
            true,
            |event| events.push(event),
        );
        assert_eq!(failed.unwrap_err().to_string(), "blocked");
        assert_eq!(events.len(), 1, "failed deletion must not notify");
        assert!(!dir.path().join("handy-200.wav").exists());
        conn.execute_batch("DROP TRIGGER stop_delete;").unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch("CREATE TABLE history_reference (history_id INTEGER REFERENCES transcription_history(id) DEFERRABLE INITIALLY DEFERRED); INSERT INTO history_reference SELECT id FROM transcription_history;").unwrap();
        let failed = HistoryManager::clear_history_and_notify_with_connection(
            &mut conn,
            dir.path(),
            true,
            |event| events.push(event),
        );
        assert_eq!(
            failed.unwrap_err().to_string(),
            "FOREIGN KEY constraint failed"
        );
        assert_eq!(events.len(), 1, "failed commit must not notify");
        let remaining: usize = observer
            .query_row("SELECT COUNT(*) FROM transcription_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn toggle_saved_blocks_a_competing_writer_after_read_and_clear_preserves_the_star() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let mut conn = Connection::open(&path).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .unwrap();
        insert_entry(&conn, 100, "raw", None);
        let id = conn.last_insert_rowid();
        fs::write(dir.path().join("handy-100.wav"), b"audio").unwrap();
        let (read_tx, read_rx) = mpsc::channel();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (committed_tx, committed_rx) = mpsc::channel();
        let recordings = dir.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            let mut conn = Connection::open(path).unwrap();
            conn.busy_timeout(Duration::ZERO).unwrap();
            read_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut entered = false;
            let attempt = HistoryManager::with_clear_transaction(&mut conn, |_| {
                entered = true;
                Ok(())
            });
            let busy = matches!(attempt.as_ref().err().and_then(|error| error.downcast_ref::<rusqlite::Error>()),
                Some(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy);
            attempted_tx.send(!entered && busy).unwrap();
            committed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            conn.busy_timeout(Duration::from_secs(30)).unwrap();
            HistoryManager::clear_history_with_connection(&mut conn, &recordings, true).unwrap()
        });
        let mut blocked_after_read = false;
        let saved = HistoryManager::toggle_saved_status_with_conn_after_read(&mut conn, id, || {
            read_tx.send(()).unwrap();
            blocked_after_read = attempted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        });
        committed_tx.send(()).unwrap();
        let removed = writer.join().unwrap();
        assert!(saved.unwrap());
        assert!(
            blocked_after_read,
            "competing writer must reach SQLite and be blocked after the saved-status SELECT"
        );
        assert_eq!(removed, HistoryClearSummary::default());
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .unwrap();
        assert!(entry.saved);
        assert!(dir.path().join(&entry.file_name).is_file());
    }

    #[test]
    fn clear_keeps_starred_rows_and_their_files_and_handles_missing_files() {
        let mut conn = setup_conn();
        let dir = tempfile::tempdir().unwrap();
        for timestamp in [100, 200, 300] {
            insert_entry(&conn, timestamp, "raw", None);
        }
        conn.execute(
            "UPDATE transcription_history SET saved = 1 WHERE timestamp = 200",
            [],
        )
        .unwrap();
        fs::write(dir.path().join("handy-100.wav"), b"audio").unwrap();
        fs::write(dir.path().join("handy-200.wav"), b"starred").unwrap();
        let removed =
            HistoryManager::clear_history_with_connection(&mut conn, dir.path(), true).unwrap();
        assert_eq!(
            removed,
            HistoryClearSummary {
                entries: 2,
                recordings: 1
            }
        );
        assert!(!dir.path().join("handy-100.wav").exists());
        assert!(dir.path().join("handy-200.wav").exists());
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM transcription_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 1);
        let all =
            HistoryManager::clear_history_with_connection(&mut conn, dir.path(), false).unwrap();
        assert_eq!(
            all,
            HistoryClearSummary {
                entries: 1,
                recordings: 1
            }
        );
        assert_eq!(
            HistoryManager::clear_history_with_connection(&mut conn, dir.path(), true).unwrap(),
            HistoryClearSummary::default()
        );
    }

    #[test]
    fn clear_and_save_are_serialized_without_orphaning_a_recording() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let recordings = dir.path().join("recordings");
        fs::create_dir(&recordings).unwrap();
        let mut conn = Connection::open(&path).unwrap();
        conn.busy_timeout(Duration::from_secs(30)).unwrap();
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .unwrap();
        insert_entry(&conn, 100, "before clear", None);
        fs::write(recordings.join("handy-100.wav"), b"before").unwrap();
        let mut pending = HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .unwrap();
        pending.file_name = "handy-200.wav".into();
        pending.timestamp = 200;
        pending.transcription_text = "after clear".into();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let writer_dir = recordings.clone();
        let (removed, writer) = HistoryManager::with_clear_transaction(&mut conn, |transaction| {
            let writer = std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(Duration::ZERO).unwrap();
                fs::write(writer_dir.join(&pending.file_name), b"in flight").unwrap();
                let attempt = HistoryManager::insert_history_entry_with_conn(&conn, &pending);
                let blocked = matches!(attempt.as_ref().err().and_then(|error| error.downcast_ref::<rusqlite::Error>()),
                    Some(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy);
                attempted_tx.send(blocked).unwrap();
                if attempt.is_err() {
                    conn.busy_timeout(Duration::from_secs(30)).unwrap();
                    HistoryManager::insert_history_entry_with_conn(&conn, &pending).unwrap();
                }
                done_tx.send(()).unwrap();
            });
            assert!(attempted_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                "writer must reach SQLite and encounter Clear's write lock");
            assert!(matches!(done_rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)));
            let candidates = HistoryManager::clear_candidates(transaction, true).unwrap();
            let removed = HistoryManager::delete_entries_and_files_with_conn(transaction, &recordings, &candidates).unwrap();
            assert_eq!(removed.entries, 1);
            Ok((removed, writer))
        }).unwrap();
        assert_eq!(removed.recordings, 1);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        writer.join().unwrap();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .unwrap();
        assert_eq!(entry.transcription_text, "after clear");
        assert!(recordings.join(&entry.file_name).exists());
        assert!(!recordings.join("handy-100.wav").exists());
    }

    #[test]
    fn clear_removes_file_before_attempting_database_delete() {
        let conn = setup_conn();
        let dir = tempfile::tempdir().unwrap();
        insert_entry(&conn, 100, "raw", None);
        fs::write(dir.path().join("handy-100.wav"), b"audio").unwrap();
        conn.execute_batch("CREATE TRIGGER stop_delete BEFORE DELETE ON transcription_history BEGIN SELECT RAISE(ABORT, 'blocked'); END;").unwrap();
        let candidates = HistoryManager::clear_candidates(&conn, true).unwrap();
        assert!(
            HistoryManager::delete_entries_and_files_with_conn(&conn, dir.path(), &candidates)
                .is_err()
        );
        assert!(!dir.path().join("handy-100.wav").exists());
    }

    #[test]
    fn toggle_saved_commits_before_clear_and_deleted_entries_stay_deleted() {
        let mut conn = setup_conn();
        let dir = tempfile::tempdir().unwrap();
        insert_entry(&conn, 100, "raw", None);
        let id = conn.last_insert_rowid();
        fs::write(dir.path().join("handy-100.wav"), b"audio").unwrap();
        assert!(HistoryManager::toggle_saved_status_with_conn(&mut conn, id).unwrap());
        assert_eq!(
            HistoryManager::clear_history_with_connection(&mut conn, dir.path(), true).unwrap(),
            HistoryClearSummary::default()
        );
        assert!(!HistoryManager::toggle_saved_status_with_conn(&mut conn, id).unwrap());
        assert_eq!(
            HistoryManager::clear_history_with_connection(&mut conn, dir.path(), true)
                .unwrap()
                .entries,
            1
        );
        assert!(HistoryManager::toggle_saved_status_with_conn(&mut conn, id).is_err());
        let error = HistoryManager::update_transcription_with_conn(
            &conn,
            id,
            "retry".into(),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), format!("History entry {id} not found"));
        assert!(HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .is_none());
    }

    #[test]
    fn cleared_event_uses_existing_action_protocol() {
        assert_eq!(
            serde_json::to_value(HistoryUpdatePayload::Cleared {}).unwrap(),
            serde_json::json!({"action": "cleared"})
        );
    }

    #[test]
    fn pre_provenance_rows_migrate_and_read_with_null_provenance() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::new(MIGRATIONS[..4].to_vec())
            .to_latest(&mut conn)
            .unwrap();
        insert_entry(&conn, 100, "before migration", Some("Before migration."));
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .unwrap();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .unwrap();
        assert_eq!(entry.transcription_text, "before migration");
        assert_eq!(entry.post_process_provider, None);
        assert_eq!(entry.post_process_model, None);
    }

    #[test]
    fn retry_writes_and_clears_provenance_with_the_processed_text() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "raw", None);
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .unwrap();
        let updated = HistoryManager::update_transcription_with_conn(
            &conn,
            entry.id,
            "raw".into(),
            Some("Clean.".into()),
            None,
            Some(PostProcessProvenance {
                provider: "local_llm".into(),
                model: Some("s1-mini-q4km".into()),
            }),
        )
        .unwrap();
        assert_eq!(updated.post_processed_text.as_deref(), Some("Clean."));
        assert_eq!(updated.post_process_provider.as_deref(), Some("local_llm"));
        assert_eq!(updated.post_process_model.as_deref(), Some("s1-mini-q4km"));
        let failed = HistoryManager::update_transcription_with_conn(
            &conn,
            entry.id,
            "retry raw".into(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(failed.post_processed_text, None);
        assert_eq!(failed.post_process_provider, None);
        assert_eq!(failed.post_process_model, None);
    }

    #[test]
    fn initial_save_roundtrips_processed_text_and_provenance_together() {
        let conn = setup_conn();
        let mut entry = HistoryEntry {
            id: 0,
            file_name: "handy-100.wav".into(),
            timestamp: 100,
            saved: false,
            title: "Recording".into(),
            transcription_text: "raw".into(),
            post_processed_text: Some("Clean.".into()),
            post_process_prompt: None,
            post_process_requested: true,
            post_process_provider: Some("local_llm".into()),
            post_process_model: Some("s1-mini-q4km".into()),
        };
        entry.id = HistoryManager::insert_history_entry_with_conn(&conn, &entry).unwrap();
        let stored = HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .unwrap();
        assert_eq!(stored.id, entry.id);
        assert_eq!(stored.post_processed_text, entry.post_processed_text);
        assert_eq!(stored.post_process_provider, entry.post_process_provider);
        assert_eq!(stored.post_process_model, entry.post_process_model);
    }

    #[test]
    fn pending_cleanup_stores_the_request_snapshot_after_settings_change() {
        use crate::actions::process_transcription_output_with_dependencies;
        use std::cell::{Cell, RefCell};
        use std::future::{poll_fn, ready, Future};
        use std::task::Poll;

        let conn = setup_conn();
        insert_entry(&conn, 100, "raw", None);
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .unwrap()
            .unwrap();
        let mut settings = crate::settings::get_default_settings();
        settings.post_process_provider_id = "local_llm".into();
        settings.local_llm_model_id = Some("s1-mini-q4km".into());
        let source = RefCell::new(settings);
        let reads = Cell::new(0);
        let cleanup_started = Cell::new(false);
        let (complete, response) = tokio::sync::oneshot::channel();

        let processed = tauri::async_runtime::block_on(async {
            let request = process_transcription_output_with_dependencies(
                "raw",
                true,
                || {
                    reads.set(reads.get() + 1);
                    source.borrow().clone()
                },
                |snapshot, _| {
                    assert_eq!(snapshot.post_process_provider_id, "local_llm");
                    ready(None)
                },
                |snapshot, text| {
                    cleanup_started.set(true);
                    assert_eq!(text, "raw");
                    assert_eq!(snapshot.post_process_provider_id, "local_llm");
                    assert_eq!(snapshot.local_llm_model_id.as_deref(), Some("s1-mini-q4km"));
                    async move { response.await.unwrap() }
                },
            );
            let mut request = std::pin::pin!(request);
            poll_fn(|cx| {
                assert!(request.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(cleanup_started.get());
            source.borrow_mut().post_process_provider_id = "custom".into();
            source.borrow_mut().local_llm_model_id = Some("later-selection".into());
            source
                .borrow_mut()
                .post_process_models
                .insert("custom".into(), "later-api-model".into());
            complete.send(Some("Clean.".into())).unwrap();
            request.await
        });

        assert_eq!(reads.get(), 1);
        assert_eq!(source.borrow().post_process_provider_id, "custom");
        assert_eq!(processed.final_text, "Clean.");
        assert_eq!(processed.post_processed_text.as_deref(), Some("Clean."));
        let stored = HistoryManager::update_transcription_with_conn(
            &conn,
            entry.id,
            "raw".into(),
            processed.post_processed_text,
            processed.post_process_prompt,
            processed.provenance,
        )
        .unwrap();
        assert_eq!(stored.post_processed_text.as_deref(), Some("Clean."));
        assert_eq!(stored.post_process_provider.as_deref(), Some("local_llm"));
        assert_eq!(stored.post_process_model.as_deref(), Some("s1-mini-q4km"));
    }

    fn insert_entry(conn: &Connection, timestamp: i64, text: &str, post_processed: Option<&str>) {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("handy-{}.wav", timestamp),
                timestamp,
                false,
                format!("Recording {}", timestamp),
                text,
                post_processed,
                Option::<String>::None,
                false,
            ],
        )
        .expect("insert history entry");
    }

    #[test]
    fn get_latest_entry_returns_none_when_empty() {
        let conn = setup_conn();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn).expect("fetch latest entry");
        assert!(entry.is_none());
    }

    #[test]
    fn get_latest_entry_returns_newest_entry() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 200, "second", Some("processed"));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");

        assert_eq!(entry.timestamp, 200);
        assert_eq!(entry.transcription_text, "second");
        assert_eq!(entry.post_processed_text.as_deref(), Some("processed"));
    }

    #[test]
    fn get_latest_completed_entry_skips_empty_entries() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "completed", None);
        insert_entry(&conn, 200, "", None);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("completed entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "completed");
    }
}
