//! SQLite-backed storage implementation.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::models::entry::{EntryFilter, EntrySource, TimeEntry};
use crate::models::project::{Project, ProjectSource, ProjectStatus};
use crate::models::session::ShellSession;
use crate::models::types::{EntryId, ProjectId, SessionId};

use super::error::StorageError;
use super::Storage;

/// Current schema version. Increment when adding migrations.
#[cfg(test)]
const SCHEMA_VERSION: i64 = 4;

/// SQLite-backed storage for Stint.
pub struct SqliteStorage {
    conn: Connection,
}

impl SqliteStorage {
    /// Opens or creates the database at the given path and runs migrations.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                StorageError::Migration(format!("failed to create database directory: {e}"))
            })?;
        }
        let conn = Connection::open(path)?;
        let storage = Self { conn };
        storage.migrate()?;
        Ok(storage)
    }

    /// Opens an existing database without creating directories or running migrations.
    ///
    /// Returns `Err` if the database file does not exist or cannot be opened.
    /// Intended for the shell hook fast path where the DB should already exist.
    pub fn open_existing(path: &Path) -> Result<Self, StorageError> {
        if !path.exists() {
            return Err(StorageError::Migration(
                "database does not exist".to_string(),
            ));
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self { conn })
    }

    /// Opens an in-memory database for testing.
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        let storage = Self { conn };
        storage.migrate()?;
        Ok(storage)
    }

    /// Returns the XDG-compliant default database path.
    ///
    /// Checks `STINT_DB_PATH` env var first, then falls back to
    /// `~/.local/share/stint/stint.db` (or platform equivalent).
    pub fn default_path() -> PathBuf {
        if let Ok(path) = std::env::var("STINT_DB_PATH") {
            return PathBuf::from(path);
        }
        let data_dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        data_dir.join("stint").join("stint.db")
    }

    /// Runs schema migrations up to the current version.
    fn migrate(&self) -> Result<(), StorageError> {
        self.conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        self.conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        // Create the meta table if it doesn't exist
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _stint_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        let current_version: i64 = self
            .conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM _stint_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if current_version < 1 {
            self.migrate_v1()?;
        }
        if current_version < 2 {
            self.migrate_v2()?;
        }
        if current_version < 3 {
            self.migrate_v3()?;
        }
        if current_version < 4 {
            self.migrate_v4()?;
        }

        Ok(())
    }

    /// Initial schema: all tables for Phase 0.
    fn migrate_v1(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects (
                id               TEXT PRIMARY KEY,
                name             TEXT NOT NULL UNIQUE COLLATE NOCASE,
                hourly_rate_cents INTEGER,
                status           TEXT NOT NULL DEFAULT 'active'
                                     CHECK(status IN ('active', 'archived')),
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS project_paths (
                project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                path       TEXT NOT NULL,
                PRIMARY KEY (project_id, path)
            );

            CREATE INDEX IF NOT EXISTS idx_project_paths_path
                ON project_paths(path);

            CREATE TABLE IF NOT EXISTS project_tags (
                project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                tag        TEXT NOT NULL,
                PRIMARY KEY (project_id, tag)
            );

            CREATE TABLE IF NOT EXISTS entries (
                id            TEXT PRIMARY KEY,
                project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                session_id    TEXT,
                start         TEXT NOT NULL,
                end_time      TEXT,
                duration_secs INTEGER,
                source        TEXT NOT NULL CHECK(source IN ('manual', 'hook', 'added')),
                notes         TEXT,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_entries_project_running
                ON entries(project_id) WHERE end_time IS NULL;

            CREATE INDEX IF NOT EXISTS idx_entries_running
                ON entries(end_time) WHERE end_time IS NULL;

            CREATE INDEX IF NOT EXISTS idx_entries_start
                ON entries(start);

            CREATE INDEX IF NOT EXISTS idx_entries_project_start
                ON entries(project_id, start);

            CREATE TABLE IF NOT EXISTS entry_tags (
                entry_id TEXT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
                tag      TEXT NOT NULL,
                PRIMARY KEY (entry_id, tag)
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id                  TEXT PRIMARY KEY,
                pid                 INTEGER NOT NULL,
                shell               TEXT,
                cwd                 TEXT NOT NULL,
                current_project_id  TEXT REFERENCES projects(id) ON DELETE SET NULL,
                started_at          TEXT NOT NULL,
                last_heartbeat      TEXT NOT NULL,
                ended_at            TEXT
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_pid
                ON sessions(pid) WHERE ended_at IS NULL;

            INSERT OR REPLACE INTO _stint_meta (key, value) VALUES ('schema_version', '1');",
        )?;

        Ok(())
    }

    /// Migration v2: indexes for session and hook queries.
    fn migrate_v2(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_active_project
                ON sessions(current_project_id) WHERE ended_at IS NULL;

            CREATE INDEX IF NOT EXISTS idx_sessions_active_heartbeat
                ON sessions(last_heartbeat) WHERE ended_at IS NULL;

            CREATE UNIQUE INDEX IF NOT EXISTS idx_entries_one_running_per_project
                ON entries(project_id) WHERE end_time IS NULL;

            INSERT OR REPLACE INTO _stint_meta (key, value) VALUES ('schema_version', '2');",
        )?;

        Ok(())
    }

    /// Migration v3: ignored paths table and project source column for auto-discovery.
    fn migrate_v3(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ignored_paths (
                path TEXT PRIMARY KEY
            );",
        )?;

        // Only add the source column if it doesn't already exist
        let has_source: bool = self
            .conn
            .prepare("PRAGMA table_info('projects')")?
            .query_map([], |row| row.get::<_, String>(1))?
            .any(|col| col.as_deref() == Ok("source"));

        if !has_source {
            self.conn.execute_batch(
                "ALTER TABLE projects ADD COLUMN source TEXT NOT NULL DEFAULT 'manual';",
            )?;
        }

        self.conn.execute_batch(
            "INSERT OR REPLACE INTO _stint_meta (key, value) VALUES ('schema_version', '3');",
        )?;

        Ok(())
    }

    /// Migration v4: add index on ignored_paths.path for fast hook lookups.
    fn migrate_v4(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_ignored_paths_path
                ON ignored_paths(path);

            INSERT OR REPLACE INTO _stint_meta (key, value) VALUES ('schema_version', '4');",
        )?;

        Ok(())
    }

    // --- Helper methods ---

    /// Loads paths for a project from the project_paths table.
    fn load_project_paths(&self, project_id: &ProjectId) -> Result<Vec<PathBuf>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM project_paths WHERE project_id = ?1")?;
        let paths = stmt
            .query_map(params![project_id.as_str()], |row| {
                let p: String = row.get(0)?;
                Ok(PathBuf::from(p))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(paths)
    }

    /// Loads tags for a project from the project_tags table.
    fn load_project_tags(&self, project_id: &ProjectId) -> Result<Vec<String>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag FROM project_tags WHERE project_id = ?1 ORDER BY tag")?;
        let tags = stmt
            .query_map(params![project_id.as_str()], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tags)
    }

    /// Loads tags for an entry from the entry_tags table.
    fn load_entry_tags(&self, entry_id: &EntryId) -> Result<Vec<String>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag FROM entry_tags WHERE entry_id = ?1 ORDER BY tag")?;
        let tags = stmt
            .query_map(params![entry_id.as_str()], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tags)
    }

    /// Saves paths for a project within a transaction, replacing any existing paths.
    fn save_project_paths_tx(
        conn: &Connection,
        project_id: &ProjectId,
        paths: &[PathBuf],
    ) -> Result<(), StorageError> {
        conn.execute(
            "DELETE FROM project_paths WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        let mut stmt =
            conn.prepare("INSERT INTO project_paths (project_id, path) VALUES (?1, ?2)")?;
        for path in paths {
            stmt.execute(params![project_id.as_str(), path.to_string_lossy()])?;
        }
        Ok(())
    }

    /// Saves tags for a project within a transaction, replacing any existing tags.
    fn save_project_tags_tx(
        conn: &Connection,
        project_id: &ProjectId,
        tags: &[String],
    ) -> Result<(), StorageError> {
        conn.execute(
            "DELETE FROM project_tags WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        let mut stmt =
            conn.prepare("INSERT INTO project_tags (project_id, tag) VALUES (?1, ?2)")?;
        for tag in tags {
            stmt.execute(params![project_id.as_str(), tag])?;
        }
        Ok(())
    }

    /// Saves tags for an entry within a transaction, replacing any existing tags.
    fn save_entry_tags_tx(
        conn: &Connection,
        entry_id: &EntryId,
        tags: &[String],
    ) -> Result<(), StorageError> {
        conn.execute(
            "DELETE FROM entry_tags WHERE entry_id = ?1",
            params![entry_id.as_str()],
        )?;
        let mut stmt = conn.prepare("INSERT INTO entry_tags (entry_id, tag) VALUES (?1, ?2)")?;
        for tag in tags {
            stmt.execute(params![entry_id.as_str(), tag])?;
        }
        Ok(())
    }

    /// Builds a Project from a row and loads its paths and tags.
    fn project_from_row(&self, row: &rusqlite::Row) -> Result<Project, rusqlite::Error> {
        let id: String = row.get("id")?;
        let name: String = row.get("name")?;
        let hourly_rate_cents: Option<i64> = row.get("hourly_rate_cents")?;
        let status_str: String = row.get("status")?;
        let source_str: String = row.get("source").unwrap_or_else(|_| "manual".to_string());
        let created_at_str: String = row.get("created_at")?;
        let updated_at_str: String = row.get("updated_at")?;

        let status = ProjectStatus::from_str_value(&status_str).unwrap_or(ProjectStatus::Active);
        let source = ProjectSource::from_str_value(&source_str);
        let created_at = Self::parse_ts(&created_at_str)?;
        let updated_at = Self::parse_ts(&updated_at_str)?;

        Ok(Project {
            id: ProjectId::from_storage(id),
            name,
            paths: vec![], // loaded separately
            tags: vec![],  // loaded separately
            hourly_rate_cents,
            status,
            source,
            created_at,
            updated_at,
        })
    }

    /// Hydrates a project's paths and tags after loading the core row.
    fn hydrate_project(&self, mut project: Project) -> Result<Project, StorageError> {
        project.paths = self.load_project_paths(&project.id)?;
        project.tags = self.load_project_tags(&project.id)?;
        Ok(project)
    }

    /// Builds a TimeEntry from a row.
    fn entry_from_row(&self, row: &rusqlite::Row) -> Result<TimeEntry, rusqlite::Error> {
        let id: String = row.get("id")?;
        let project_id: String = row.get("project_id")?;
        let session_id: Option<String> = row.get("session_id")?;
        let start_str: String = row.get("start")?;
        let end_str: Option<String> = row.get("end_time")?;
        let duration_secs: Option<i64> = row.get("duration_secs")?;
        let source_str: String = row.get("source")?;
        let notes: Option<String> = row.get("notes")?;
        let created_at_str: String = row.get("created_at")?;
        let updated_at_str: String = row.get("updated_at")?;

        let start = Self::parse_ts(&start_str)?;
        let end = end_str.map(|s| Self::parse_ts(&s)).transpose()?;
        let source = EntrySource::from_str_value(&source_str).unwrap_or(EntrySource::Manual);
        let created_at = Self::parse_ts(&created_at_str)?;
        let updated_at = Self::parse_ts(&updated_at_str)?;

        Ok(TimeEntry {
            id: EntryId::from_storage(id),
            project_id: ProjectId::from_storage(project_id),
            session_id: session_id.map(SessionId::from_storage),
            start,
            end,
            duration_secs,
            source,
            notes,
            tags: vec![], // loaded separately
            created_at,
            updated_at,
        })
    }

    /// Hydrates an entry's tags after loading the core row.
    fn hydrate_entry(&self, mut entry: TimeEntry) -> Result<TimeEntry, StorageError> {
        entry.tags = self.load_entry_tags(&entry.id)?;
        Ok(entry)
    }

    /// Builds a ShellSession from a row.
    fn session_from_row(&self, row: &rusqlite::Row) -> Result<ShellSession, rusqlite::Error> {
        let id: String = row.get("id")?;
        let pid: u32 = row.get("pid")?;
        let shell: Option<String> = row.get("shell")?;
        let cwd: String = row.get("cwd")?;
        let current_project_id: Option<String> = row.get("current_project_id")?;
        let started_at_str: String = row.get("started_at")?;
        let heartbeat_str: String = row.get("last_heartbeat")?;
        let ended_at_str: Option<String> = row.get("ended_at")?;

        let started_at = Self::parse_ts(&started_at_str)?;
        let last_heartbeat = Self::parse_ts(&heartbeat_str)?;
        let ended_at = ended_at_str.map(|s| Self::parse_ts(&s)).transpose()?;

        Ok(ShellSession {
            id: SessionId::from_storage(id),
            pid,
            shell,
            cwd: PathBuf::from(cwd),
            current_project_id: current_project_id.map(ProjectId::from_storage),
            started_at,
            last_heartbeat,
            ended_at,
        })
    }

    /// Formats an OffsetDateTime as RFC 3339 for storage.
    fn fmt_ts(ts: &OffsetDateTime) -> String {
        ts.format(&Rfc3339).unwrap_or_default()
    }

    /// Parses an RFC 3339 timestamp string, returning a rusqlite error on failure.
    ///
    /// Propagates the error rather than silently falling back to UNIX_EPOCH,
    /// which would cause idle detection to calculate a ~55-year gap.
    fn parse_ts(raw: &str) -> Result<OffsetDateTime, rusqlite::Error> {
        OffsetDateTime::parse(raw, &Rfc3339).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    }
}

impl Storage for SqliteStorage {
    // --- Projects ---

    fn create_project(&self, project: &Project) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;

        // Check for duplicate name
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM projects WHERE name = ?1 COLLATE NOCASE",
                params![&project.name],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Err(StorageError::DuplicateProjectName(project.name.clone()));
        }

        tx.execute(
            "INSERT INTO projects (id, name, hourly_rate_cents, status, source, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                project.id.as_str(),
                &project.name,
                project.hourly_rate_cents,
                project.status.as_str(),
                project.source.as_str(),
                Self::fmt_ts(&project.created_at),
                Self::fmt_ts(&project.updated_at),
            ],
        )?;

        Self::save_project_paths_tx(&tx, &project.id, &project.paths)?;
        Self::save_project_tags_tx(&tx, &project.id, &project.tags)?;

        tx.commit()?;
        Ok(())
    }

    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StorageError> {
        let project = self
            .conn
            .query_row(
                "SELECT * FROM projects WHERE id = ?1",
                params![id.as_str()],
                |row| self.project_from_row(row),
            )
            .optional()?;

        match project {
            Some(p) => Ok(Some(self.hydrate_project(p)?)),
            None => Ok(None),
        }
    }

    fn get_project_by_name(&self, name: &str) -> Result<Option<Project>, StorageError> {
        let project = self
            .conn
            .query_row(
                "SELECT * FROM projects WHERE name = ?1 COLLATE NOCASE",
                params![name],
                |row| self.project_from_row(row),
            )
            .optional()?;

        match project {
            Some(p) => Ok(Some(self.hydrate_project(p)?)),
            None => Ok(None),
        }
    }

    fn get_project_by_path(&self, path: &Path) -> Result<Option<Project>, StorageError> {
        let path_str = path.to_string_lossy();

        // Longest-prefix match: find the project whose registered path is the
        // most specific ancestor of (or exact match for) the given path.
        let project = self
            .conn
            .query_row(
                "SELECT p.* FROM projects p
                 JOIN project_paths pp ON p.id = pp.project_id
                 WHERE ?1 = pp.path
                    OR (LENGTH(?1) > LENGTH(pp.path)
                        AND SUBSTR(?1, 1, LENGTH(pp.path) + 1) = pp.path || '/')
                 ORDER BY LENGTH(pp.path) DESC
                 LIMIT 1",
                params![path_str],
                |row| self.project_from_row(row),
            )
            .optional()?;

        match project {
            Some(p) => Ok(Some(self.hydrate_project(p)?)),
            None => Ok(None),
        }
    }

    fn list_projects(&self, status: Option<ProjectStatus>) -> Result<Vec<Project>, StorageError> {
        let mut projects = if let Some(ref s) = status {
            let mut stmt = self
                .conn
                .prepare("SELECT * FROM projects WHERE status = ?1 ORDER BY name")?;
            let result = stmt
                .query_map(params![s.as_str()], |row| self.project_from_row(row))?
                .collect::<Result<Vec<_>, _>>()?;
            result
        } else {
            let mut stmt = self.conn.prepare("SELECT * FROM projects ORDER BY name")?;
            let result = stmt
                .query_map([], |row| self.project_from_row(row))?
                .collect::<Result<Vec<_>, _>>()?;
            result
        };

        for project in &mut projects {
            project.paths = self.load_project_paths(&project.id)?;
            project.tags = self.load_project_tags(&project.id)?;
        }

        Ok(projects)
    }

    fn update_project(&self, project: &Project) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;

        let changed = tx.execute(
            "UPDATE projects SET name = ?1, hourly_rate_cents = ?2, status = ?3, updated_at = ?4
             WHERE id = ?5",
            params![
                &project.name,
                project.hourly_rate_cents,
                project.status.as_str(),
                Self::fmt_ts(&project.updated_at),
                project.id.as_str(),
            ],
        )?;

        if changed == 0 {
            return Err(StorageError::ProjectNotFound(project.id.to_string()));
        }

        Self::save_project_paths_tx(&tx, &project.id, &project.paths)?;
        Self::save_project_tags_tx(&tx, &project.id, &project.tags)?;

        tx.commit()?;
        Ok(())
    }

    fn delete_project(&self, id: &ProjectId) -> Result<(), StorageError> {
        let changed = self
            .conn
            .execute("DELETE FROM projects WHERE id = ?1", params![id.as_str()])?;

        if changed == 0 {
            return Err(StorageError::ProjectNotFound(id.to_string()));
        }

        Ok(())
    }

    // --- Time Entries ---

    fn create_entry(&self, entry: &TimeEntry) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        let end_str = entry.end.as_ref().map(Self::fmt_ts);

        tx.execute(
            "INSERT INTO entries
                (id, project_id, session_id, start, end_time, duration_secs, source, notes,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.id.as_str(),
                entry.project_id.as_str(),
                entry.session_id.as_ref().map(|s| s.as_str().to_owned()),
                Self::fmt_ts(&entry.start),
                end_str,
                entry.duration_secs,
                entry.source.as_str(),
                &entry.notes,
                Self::fmt_ts(&entry.created_at),
                Self::fmt_ts(&entry.updated_at),
            ],
        )?;

        Self::save_entry_tags_tx(&tx, &entry.id, &entry.tags)?;

        tx.commit()?;
        Ok(())
    }

    fn get_entry(&self, id: &EntryId) -> Result<Option<TimeEntry>, StorageError> {
        let entry = self
            .conn
            .query_row(
                "SELECT * FROM entries WHERE id = ?1",
                params![id.as_str()],
                |row| self.entry_from_row(row),
            )
            .optional()?;

        match entry {
            Some(e) => Ok(Some(self.hydrate_entry(e)?)),
            None => Ok(None),
        }
    }

    fn get_running_entry(&self, project_id: &ProjectId) -> Result<Option<TimeEntry>, StorageError> {
        let entry = self
            .conn
            .query_row(
                "SELECT * FROM entries WHERE project_id = ?1 AND end_time IS NULL LIMIT 1",
                params![project_id.as_str()],
                |row| self.entry_from_row(row),
            )
            .optional()?;

        match entry {
            Some(e) => Ok(Some(self.hydrate_entry(e)?)),
            None => Ok(None),
        }
    }

    fn get_running_hook_entry(
        &self,
        project_id: &ProjectId,
    ) -> Result<Option<TimeEntry>, StorageError> {
        let entry = self
            .conn
            .query_row(
                "SELECT * FROM entries WHERE project_id = ?1 AND end_time IS NULL AND source = 'hook' LIMIT 1",
                params![project_id.as_str()],
                |row| self.entry_from_row(row),
            )
            .optional()?;

        match entry {
            Some(e) => Ok(Some(self.hydrate_entry(e)?)),
            None => Ok(None),
        }
    }

    fn get_any_running_entry(&self) -> Result<Option<TimeEntry>, StorageError> {
        let entry = self
            .conn
            .query_row(
                "SELECT * FROM entries WHERE end_time IS NULL LIMIT 1",
                [],
                |row| self.entry_from_row(row),
            )
            .optional()?;

        match entry {
            Some(e) => Ok(Some(self.hydrate_entry(e)?)),
            None => Ok(None),
        }
    }

    fn list_entries(&self, filter: &EntryFilter) -> Result<Vec<TimeEntry>, StorageError> {
        let mut sql = String::from("SELECT * FROM entries WHERE 1=1");
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];

        if let Some(ref pid) = filter.project_id {
            param_values.push(Box::new(pid.as_str().to_owned()));
            sql.push_str(&format!(" AND project_id = ?{}", param_values.len()));
        }
        if let Some(ref from) = filter.from {
            param_values.push(Box::new(Self::fmt_ts(from)));
            sql.push_str(&format!(" AND start >= ?{}", param_values.len()));
        }
        if let Some(ref to) = filter.to {
            param_values.push(Box::new(Self::fmt_ts(to)));
            sql.push_str(&format!(" AND start < ?{}", param_values.len()));
        }
        if let Some(ref source) = filter.source {
            param_values.push(Box::new(source.as_str().to_owned()));
            sql.push_str(&format!(" AND source = ?{}", param_values.len()));
        }

        // Tag filtering: entries must have ALL specified tags
        for tag in &filter.tags {
            param_values.push(Box::new(tag.clone()));
            sql.push_str(&format!(
                " AND id IN (SELECT entry_id FROM entry_tags WHERE tag = ?{})",
                param_values.len()
            ));
        }

        sql.push_str(" ORDER BY start DESC");

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let entries = stmt
            .query_map(params_refs.as_slice(), |row| self.entry_from_row(row))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut hydrated = Vec::with_capacity(entries.len());
        for entry in entries {
            hydrated.push(self.hydrate_entry(entry)?);
        }

        Ok(hydrated)
    }

    fn get_last_entry(&self) -> Result<Option<TimeEntry>, StorageError> {
        let entry = self
            .conn
            .query_row(
                "SELECT * FROM entries ORDER BY start DESC LIMIT 1",
                [],
                |row| self.entry_from_row(row),
            )
            .optional()?;

        match entry {
            Some(e) => Ok(Some(self.hydrate_entry(e)?)),
            None => Ok(None),
        }
    }

    fn update_entry(&self, entry: &TimeEntry) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        let end_str = entry.end.as_ref().map(Self::fmt_ts);

        let changed = tx.execute(
            "UPDATE entries SET project_id = ?1, session_id = ?2, start = ?3, end_time = ?4,
                duration_secs = ?5, source = ?6, notes = ?7, updated_at = ?8
             WHERE id = ?9",
            params![
                entry.project_id.as_str(),
                entry.session_id.as_ref().map(|s| s.as_str().to_owned()),
                Self::fmt_ts(&entry.start),
                end_str,
                entry.duration_secs,
                entry.source.as_str(),
                &entry.notes,
                Self::fmt_ts(&entry.updated_at),
                entry.id.as_str(),
            ],
        )?;

        if changed == 0 {
            return Err(StorageError::EntryNotFound(entry.id.to_string()));
        }

        Self::save_entry_tags_tx(&tx, &entry.id, &entry.tags)?;

        tx.commit()?;
        Ok(())
    }

    fn delete_entry(&self, id: &EntryId) -> Result<(), StorageError> {
        let changed = self
            .conn
            .execute("DELETE FROM entries WHERE id = ?1", params![id.as_str()])?;

        if changed == 0 {
            return Err(StorageError::EntryNotFound(id.to_string()));
        }

        Ok(())
    }

    // --- Sessions ---

    fn upsert_session(&self, session: &ShellSession) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;

        // Close any existing active session with the same PID (handles PID reuse)
        tx.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE pid = ?2 AND ended_at IS NULL AND id != ?3",
            params![
                Self::fmt_ts(&session.started_at),
                session.pid,
                session.id.as_str(),
            ],
        )?;

        tx.execute(
            "INSERT INTO sessions
                (id, pid, shell, cwd, current_project_id, started_at, last_heartbeat, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                cwd = excluded.cwd,
                current_project_id = excluded.current_project_id,
                last_heartbeat = excluded.last_heartbeat,
                ended_at = excluded.ended_at",
            params![
                session.id.as_str(),
                session.pid,
                &session.shell,
                session.cwd.to_string_lossy(),
                session
                    .current_project_id
                    .as_ref()
                    .map(|p| p.as_str().to_owned()),
                Self::fmt_ts(&session.started_at),
                Self::fmt_ts(&session.last_heartbeat),
                session.ended_at.as_ref().map(Self::fmt_ts),
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    fn get_session(&self, id: &SessionId) -> Result<Option<ShellSession>, StorageError> {
        let session = self
            .conn
            .query_row(
                "SELECT * FROM sessions WHERE id = ?1",
                params![id.as_str()],
                |row| self.session_from_row(row),
            )
            .optional()?;

        Ok(session)
    }

    fn get_session_by_pid(&self, pid: u32) -> Result<Option<ShellSession>, StorageError> {
        let session = self
            .conn
            .query_row(
                "SELECT * FROM sessions WHERE pid = ?1 AND ended_at IS NULL LIMIT 1",
                params![pid],
                |row| self.session_from_row(row),
            )
            .optional()?;

        Ok(session)
    }

    fn end_session(&self, id: &SessionId, ended_at: OffsetDateTime) -> Result<(), StorageError> {
        let changed = self.conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![Self::fmt_ts(&ended_at), id.as_str()],
        )?;

        if changed == 0 {
            return Err(StorageError::SessionNotFound(id.to_string()));
        }

        Ok(())
    }

    fn count_active_sessions_for_project(
        &self,
        project_id: &ProjectId,
        exclude_session_id: &SessionId,
    ) -> Result<usize, StorageError> {
        let count: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions
             WHERE current_project_id = ?1 AND ended_at IS NULL AND id != ?2",
            params![project_id.as_str(), exclude_session_id.as_str()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    fn get_stale_sessions(
        &self,
        older_than: OffsetDateTime,
    ) -> Result<Vec<ShellSession>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM sessions WHERE ended_at IS NULL AND last_heartbeat < ?1")?;
        let sessions = stmt
            .query_map(params![Self::fmt_ts(&older_than)], |row| {
                self.session_from_row(row)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    // --- Ignored Paths ---

    fn add_ignored_path(&self, path: &Path) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO ignored_paths (path) VALUES (?1)",
            params![path.to_string_lossy()],
        )?;
        Ok(())
    }

    fn remove_ignored_path(&self, path: &Path) -> Result<bool, StorageError> {
        let changed = self.conn.execute(
            "DELETE FROM ignored_paths WHERE path = ?1",
            params![path.to_string_lossy()],
        )?;
        Ok(changed > 0)
    }

    fn is_path_ignored(&self, path: &Path) -> Result<bool, StorageError> {
        let path_str = path.to_string_lossy();
        // Check if the path itself or any of its ancestors is ignored
        let ignored: bool = self.conn.query_row(
            "SELECT EXISTS(
                    SELECT 1 FROM ignored_paths
                    WHERE ?1 = path
                       OR (LENGTH(?1) > LENGTH(path)
                           AND SUBSTR(?1, 1, LENGTH(path) + 1) = path || '/')
                )",
            params![path_str],
            |row| row.get(0),
        )?;
        Ok(ignored)
    }

    fn list_ignored_paths(&self) -> Result<Vec<PathBuf>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM ignored_paths ORDER BY path")?;
        let paths = stmt
            .query_map([], |row| {
                let p: String = row.get(0)?;
                Ok(PathBuf::from(p))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Creates a test project with sensible defaults.
    fn test_project(name: &str, paths: Vec<&str>) -> Project {
        let now = OffsetDateTime::now_utc();
        Project {
            id: ProjectId::new(),
            name: name.to_string(),
            paths: paths.into_iter().map(PathBuf::from).collect(),
            tags: vec![],
            hourly_rate_cents: None,
            status: ProjectStatus::Active,
            source: ProjectSource::Manual,
            created_at: now,
            updated_at: now,
        }
    }

    /// Creates a test entry with sensible defaults.
    fn test_entry(project_id: &ProjectId, start: OffsetDateTime) -> TimeEntry {
        let now = OffsetDateTime::now_utc();
        let end = start + time::Duration::hours(1);
        TimeEntry {
            id: EntryId::new(),
            project_id: project_id.clone(),
            session_id: None,
            start,
            end: Some(end),
            duration_secs: Some(3600),
            source: EntrySource::Manual,
            notes: None,
            tags: vec![],
            created_at: now,
            updated_at: now,
        }
    }

    // --- Migration tests ---

    #[test]
    fn migration_creates_all_tables() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let tables: Vec<String> = {
            let mut stmt = storage
                .conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };

        assert!(tables.contains(&"projects".to_string()));
        assert!(tables.contains(&"project_paths".to_string()));
        assert!(tables.contains(&"project_tags".to_string()));
        assert!(tables.contains(&"entries".to_string()));
        assert!(tables.contains(&"entry_tags".to_string()));
        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"_stint_meta".to_string()));
    }

    #[test]
    fn migration_is_idempotent() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        // Run migrate again — should not error
        storage.migrate().unwrap();
    }

    #[test]
    fn schema_version_is_set() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let version: i64 = storage
            .conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM _stint_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    // --- Project tests ---

    #[test]
    fn create_and_get_project() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec!["/home/user/projects/my-app"]);

        storage.create_project(&project).unwrap();

        let loaded = storage.get_project(&project.id).unwrap().unwrap();
        assert_eq!(loaded.name, "my-app");
        assert_eq!(loaded.paths, project.paths);
    }

    #[test]
    fn create_project_with_tags_and_rate() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let mut project = test_project("client-work", vec!["/home/user/client"]);
        project.tags = vec!["client".to_string(), "frontend".to_string()];
        project.hourly_rate_cents = Some(15000);

        storage.create_project(&project).unwrap();

        let loaded = storage.get_project(&project.id).unwrap().unwrap();
        assert_eq!(loaded.tags, vec!["client", "frontend"]);
        assert_eq!(loaded.hourly_rate_cents, Some(15000));
    }

    #[test]
    fn create_project_duplicate_name_errors() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let p1 = test_project("my-app", vec!["/path/a"]);
        let p2 = test_project("my-app", vec!["/path/b"]);

        storage.create_project(&p1).unwrap();
        let result = storage.create_project(&p2);

        assert!(matches!(result, Err(StorageError::DuplicateProjectName(_))));
    }

    #[test]
    fn get_project_by_name_case_insensitive() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("My-App", vec![]);
        storage.create_project(&project).unwrap();

        let loaded = storage.get_project_by_name("my-app").unwrap().unwrap();
        assert_eq!(loaded.id, project.id);
    }

    #[test]
    fn get_project_by_path_exact_match() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec!["/home/user/projects/my-app"]);
        storage.create_project(&project).unwrap();

        let loaded = storage
            .get_project_by_path(Path::new("/home/user/projects/my-app"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id, project.id);
    }

    #[test]
    fn get_project_by_path_subdirectory_match() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec!["/home/user/projects/my-app"]);
        storage.create_project(&project).unwrap();

        let loaded = storage
            .get_project_by_path(Path::new("/home/user/projects/my-app/src/components"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id, project.id);
    }

    #[test]
    fn get_project_by_path_longest_prefix_wins() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let monorepo = test_project("monorepo", vec!["/home/user/monorepo"]);
        let frontend = test_project("frontend", vec!["/home/user/monorepo/packages/frontend"]);

        storage.create_project(&monorepo).unwrap();
        storage.create_project(&frontend).unwrap();

        // Deep inside frontend — frontend should win
        let loaded = storage
            .get_project_by_path(Path::new(
                "/home/user/monorepo/packages/frontend/src/index.ts",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.name, "frontend");

        // Inside monorepo but not frontend — monorepo should win
        let loaded = storage
            .get_project_by_path(Path::new("/home/user/monorepo/packages/backend"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.name, "monorepo");
    }

    #[test]
    fn get_project_by_path_no_partial_name_match() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec!["/home/user/project"]);
        storage.create_project(&project).unwrap();

        // "/home/user/project-foo" should NOT match "/home/user/project"
        let loaded = storage
            .get_project_by_path(Path::new("/home/user/project-foo"))
            .unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn get_project_by_path_no_match() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec!["/home/user/projects/my-app"]);
        storage.create_project(&project).unwrap();

        let loaded = storage
            .get_project_by_path(Path::new("/home/user/other"))
            .unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn list_projects_filters_by_status() {
        let storage = SqliteStorage::open_in_memory().unwrap();

        let active = test_project("active-proj", vec![]);
        let mut archived = test_project("archived-proj", vec![]);
        archived.status = ProjectStatus::Archived;

        storage.create_project(&active).unwrap();
        storage.create_project(&archived).unwrap();

        let active_list = storage.list_projects(Some(ProjectStatus::Active)).unwrap();
        assert_eq!(active_list.len(), 1);
        assert_eq!(active_list[0].name, "active-proj");

        let archived_list = storage
            .list_projects(Some(ProjectStatus::Archived))
            .unwrap();
        assert_eq!(archived_list.len(), 1);
        assert_eq!(archived_list[0].name, "archived-proj");

        let all = storage.list_projects(None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn update_project() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let mut project = test_project("my-app", vec!["/home/user/my-app"]);
        storage.create_project(&project).unwrap();

        project.name = "renamed-app".to_string();
        project.hourly_rate_cents = Some(20000);
        project.paths = vec![
            PathBuf::from("/home/user/my-app"),
            PathBuf::from("/home/user/my-app-v2"),
        ];
        storage.update_project(&project).unwrap();

        let loaded = storage.get_project(&project.id).unwrap().unwrap();
        assert_eq!(loaded.name, "renamed-app");
        assert_eq!(loaded.hourly_rate_cents, Some(20000));
        assert_eq!(loaded.paths.len(), 2);
    }

    #[test]
    fn delete_project_cascades() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec!["/home/user/my-app"]);
        storage.create_project(&project).unwrap();

        let entry = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        storage.create_entry(&entry).unwrap();

        storage.delete_project(&project.id).unwrap();

        assert!(storage.get_project(&project.id).unwrap().is_none());
        assert!(storage.get_entry(&entry.id).unwrap().is_none());
    }

    // --- Entry tests ---

    #[test]
    fn create_and_get_entry() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec![]);
        storage.create_project(&project).unwrap();

        let mut entry = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        entry.tags = vec!["bugfix".to_string()];
        entry.notes = Some("Fixed the login bug".to_string());
        storage.create_entry(&entry).unwrap();

        let loaded = storage.get_entry(&entry.id).unwrap().unwrap();
        assert_eq!(loaded.project_id, project.id);
        assert_eq!(loaded.tags, vec!["bugfix"]);
        assert_eq!(loaded.notes.as_deref(), Some("Fixed the login bug"));
    }

    #[test]
    fn get_running_entry_for_project() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec![]);
        storage.create_project(&project).unwrap();

        let mut entry = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        entry.end = None;
        entry.duration_secs = None;
        storage.create_entry(&entry).unwrap();

        let running = storage.get_running_entry(&project.id).unwrap().unwrap();
        assert_eq!(running.id, entry.id);
    }

    #[test]
    fn get_running_entry_none_when_stopped() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec![]);
        storage.create_project(&project).unwrap();

        let mut entry = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        entry.end = Some(datetime!(2026-01-01 10:00 UTC));
        entry.duration_secs = Some(3600);
        storage.create_entry(&entry).unwrap();

        let running = storage.get_running_entry(&project.id).unwrap();
        assert!(running.is_none());
    }

    #[test]
    fn get_any_running_entry() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let p1 = test_project("app-1", vec![]);
        let p2 = test_project("app-2", vec![]);
        storage.create_project(&p1).unwrap();
        storage.create_project(&p2).unwrap();

        let mut entry = test_entry(&p2.id, datetime!(2026-01-01 9:00 UTC));
        entry.end = None;
        entry.duration_secs = None;
        storage.create_entry(&entry).unwrap();

        let running = storage.get_any_running_entry().unwrap().unwrap();
        assert_eq!(running.project_id, p2.id);
    }

    #[test]
    fn list_entries_by_project() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let p1 = test_project("app-1", vec![]);
        let p2 = test_project("app-2", vec![]);
        storage.create_project(&p1).unwrap();
        storage.create_project(&p2).unwrap();

        let e1 = test_entry(&p1.id, datetime!(2026-01-01 9:00 UTC));
        let e2 = test_entry(&p2.id, datetime!(2026-01-01 10:00 UTC));
        storage.create_entry(&e1).unwrap();
        storage.create_entry(&e2).unwrap();

        let filter = EntryFilter {
            project_id: Some(p1.id.clone()),
            ..Default::default()
        };
        let entries = storage.list_entries(&filter).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].project_id, p1.id);
    }

    #[test]
    fn list_entries_by_date_range() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec![]);
        storage.create_project(&project).unwrap();

        let e1 = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        let e2 = test_entry(&project.id, datetime!(2026-01-02 9:00 UTC));
        let e3 = test_entry(&project.id, datetime!(2026-01-03 9:00 UTC));
        storage.create_entry(&e1).unwrap();
        storage.create_entry(&e2).unwrap();
        storage.create_entry(&e3).unwrap();

        let filter = EntryFilter {
            from: Some(datetime!(2026-01-02 0:00 UTC)),
            to: Some(datetime!(2026-01-03 0:00 UTC)),
            ..Default::default()
        };
        let entries = storage.list_entries(&filter).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, e2.id);
    }

    #[test]
    fn list_entries_by_tag() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec![]);
        storage.create_project(&project).unwrap();

        let mut e1 = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        e1.tags = vec!["bugfix".to_string(), "urgent".to_string()];
        let mut e2 = test_entry(&project.id, datetime!(2026-01-02 9:00 UTC));
        e2.tags = vec!["feature".to_string()];
        storage.create_entry(&e1).unwrap();
        storage.create_entry(&e2).unwrap();

        // Filter by single tag
        let filter = EntryFilter {
            tags: vec!["bugfix".to_string()],
            ..Default::default()
        };
        let entries = storage.list_entries(&filter).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, e1.id);

        // Filter by multiple tags (AND)
        let filter = EntryFilter {
            tags: vec!["bugfix".to_string(), "urgent".to_string()],
            ..Default::default()
        };
        let entries = storage.list_entries(&filter).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn update_entry_stop() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec![]);
        storage.create_project(&project).unwrap();

        let mut entry = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        storage.create_entry(&entry).unwrap();

        entry.end = Some(datetime!(2026-01-01 10:30 UTC));
        entry.duration_secs = Some(5400);
        storage.update_entry(&entry).unwrap();

        let loaded = storage.get_entry(&entry.id).unwrap().unwrap();
        assert!(!loaded.is_running());
        assert_eq!(loaded.duration_secs, Some(5400));
    }

    #[test]
    fn delete_entry() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let project = test_project("my-app", vec![]);
        storage.create_project(&project).unwrap();

        let mut entry = test_entry(&project.id, datetime!(2026-01-01 9:00 UTC));
        entry.tags = vec!["test".to_string()];
        storage.create_entry(&entry).unwrap();

        storage.delete_entry(&entry.id).unwrap();

        assert!(storage.get_entry(&entry.id).unwrap().is_none());
    }

    // --- Session tests ---

    #[test]
    fn upsert_session_creates() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let now = OffsetDateTime::now_utc();
        let session = ShellSession {
            id: SessionId::new(),
            pid: 12345,
            shell: Some("zsh".to_string()),
            cwd: PathBuf::from("/home/user/projects"),
            current_project_id: None,
            started_at: now,
            last_heartbeat: now,
            ended_at: None,
        };

        storage.upsert_session(&session).unwrap();

        let loaded = storage.get_session(&session.id).unwrap().unwrap();
        assert_eq!(loaded.pid, 12345);
        assert_eq!(loaded.shell.as_deref(), Some("zsh"));
    }

    #[test]
    fn upsert_session_updates_heartbeat() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let now = OffsetDateTime::now_utc();
        let mut session = ShellSession {
            id: SessionId::new(),
            pid: 12345,
            shell: Some("bash".to_string()),
            cwd: PathBuf::from("/home/user"),
            current_project_id: None,
            started_at: now,
            last_heartbeat: now,
            ended_at: None,
        };

        storage.upsert_session(&session).unwrap();

        // Simulate heartbeat update
        session.cwd = PathBuf::from("/home/user/projects/my-app");
        session.last_heartbeat = now + time::Duration::seconds(30);
        storage.upsert_session(&session).unwrap();

        let loaded = storage.get_session(&session.id).unwrap().unwrap();
        assert_eq!(loaded.cwd, PathBuf::from("/home/user/projects/my-app"));
    }

    #[test]
    fn get_session_by_pid() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let now = OffsetDateTime::now_utc();
        let session = ShellSession {
            id: SessionId::new(),
            pid: 99999,
            shell: Some("fish".to_string()),
            cwd: PathBuf::from("/tmp"),
            current_project_id: None,
            started_at: now,
            last_heartbeat: now,
            ended_at: None,
        };

        storage.upsert_session(&session).unwrap();

        let loaded = storage.get_session_by_pid(99999).unwrap().unwrap();
        assert_eq!(loaded.id, session.id);

        // Ended sessions should not be found by PID
        storage.end_session(&session.id, now).unwrap();
        let loaded = storage.get_session_by_pid(99999).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn end_session() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let now = OffsetDateTime::now_utc();
        let session = ShellSession {
            id: SessionId::new(),
            pid: 11111,
            shell: None,
            cwd: PathBuf::from("/home/user"),
            current_project_id: None,
            started_at: now,
            last_heartbeat: now,
            ended_at: None,
        };

        storage.upsert_session(&session).unwrap();
        storage.end_session(&session.id, now).unwrap();

        let loaded = storage.get_session(&session.id).unwrap().unwrap();
        assert!(loaded.ended_at.is_some());
    }
}