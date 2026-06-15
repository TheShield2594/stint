//! Shell hook logic for automatic time tracking.
//!
//! The hook fires on every shell prompt render. It detects the current project
//! from the working directory, manages sessions, and starts/stops timers.

use std::path::Path;

use time::OffsetDateTime;

use crate::config::StintConfig;
use crate::discover;
use crate::error::StintError;
use crate::models::entry::{EntrySource, TimeEntry};
use crate::models::project::{Project, ProjectStatus};
use crate::models::session::ShellSession;
use crate::models::types::{EntryId, ProjectId, SessionId};
use crate::storage::Storage;

/// Minimum stale session threshold in seconds (10 minutes).
/// The actual threshold is computed as max(this, idle_threshold * 2) to ensure
/// stale reaping never fires before idle detection.
const MIN_STALE_THRESHOLD_SECS: i64 = 600;

/// What happened as a result of the hook firing.
#[derive(Debug, PartialEq, Eq)]
pub enum HookAction {
    /// Session heartbeat updated, no project change.
    Heartbeat,
    /// Started tracking a new project.
    Started { project_name: String },
    /// Switched from one project to another.
    Switched { from: String, to: String },
    /// Stopped tracking (left project directory).
    Stopped { project_name: String },
    /// New session created, no project detected.
    SessionCreated { session_id: SessionId },
    /// New session created and started tracking.
    SessionStarted {
        project_name: String,
        session_id: SessionId,
    },
    /// Idle gap detected; previous entry stopped at last heartbeat, new one started.
    IdleResume { project_name: String },
}

/// Handles a shell hook invocation.
///
/// Called on every prompt render. Detects the current project from `cwd`,
/// manages the session lifecycle, and starts/stops/switches timers.
pub fn handle_hook(
    storage: &impl Storage,
    pid: u32,
    cwd: &Path,
    shell: Option<&str>,
    config: &StintConfig,
) -> Result<HookAction, StintError> {
    let now = OffsetDateTime::now_utc();

    match storage.get_session_by_pid(pid)? {
        None => handle_cold_start(storage, pid, cwd, shell, now, config),
        Some(session) => handle_warm_path(storage, session, cwd, now, config),
    }
}

/// Handles the first hook call in a new shell session.
fn handle_cold_start(
    storage: &impl Storage,
    pid: u32,
    cwd: &Path,
    shell: Option<&str>,
    now: OffsetDateTime,
    config: &StintConfig,
) -> Result<HookAction, StintError> {
    // Reap stale sessions opportunistically
    let _ = reap_stale_sessions(storage, now, config);

    // Detect project from cwd (registered paths first, then .git auto-discovery)
    let active_project = detect_or_discover(storage, cwd, now, config)?;

    let project_id = active_project.as_ref().map(|p| p.id.clone());

    let session = ShellSession {
        id: SessionId::new(),
        pid,
        shell: shell.map(|s| s.to_string()),
        cwd: cwd.to_path_buf(),
        current_project_id: project_id,
        started_at: now,
        last_heartbeat: now,
        ended_at: None,
    };
    storage.upsert_session(&session)?;

    match active_project {
        Some(project) => {
            // Merge mode: only create entry if none is running for this project
            try_create_hook_entry(storage, &project.id, &session.id, now)?;
            Ok(HookAction::SessionStarted {
                project_name: project.name,
                session_id: session.id,
            })
        }
        None => Ok(HookAction::SessionCreated {
            session_id: session.id,
        }),
    }
}

/// Handles subsequent hook calls in an existing session.
fn handle_warm_path(
    storage: &impl Storage,
    mut session: ShellSession,
    cwd: &Path,
    now: OffsetDateTime,
    config: &StintConfig,
) -> Result<HookAction, StintError> {
    let idle_gap = (now - session.last_heartbeat).whole_seconds();
    let is_idle = idle_gap > config.idle_threshold_secs;
    let cwd_changed = session.cwd != cwd;

    // Fast path: no idle, no cwd change — just heartbeat
    if !is_idle && !cwd_changed {
        session.last_heartbeat = now;
        storage.upsert_session(&session)?;
        return Ok(HookAction::Heartbeat);
    }

    // Need to detect project (cwd changed or idle gap)
    let new_active = detect_or_discover(storage, cwd, now, config)?;
    let new_project_id = new_active.as_ref().map(|p| p.id.clone());

    let old_project_id = session.current_project_id.clone();
    let project_changed = new_project_id != old_project_id;

    // Handle idle gap: stop old entry at last_heartbeat time
    if is_idle {
        if let Some(ref old_pid) = old_project_id {
            // Only stop if no other active sessions share this project
            let others = storage.count_active_sessions_for_project(old_pid, &session.id)?;
            if others == 0 {
                stop_hook_entry_for_project(storage, old_pid, session.last_heartbeat)?;
            }
        }

        // Update session
        session.cwd = cwd.to_path_buf();
        session.current_project_id = new_project_id;
        session.last_heartbeat = now;
        storage.upsert_session(&session)?;

        // Start new entry if we're in a project
        if let Some(project) = new_active {
            try_create_hook_entry(storage, &project.id, &session.id, now)?;
            return Ok(HookAction::IdleResume {
                project_name: project.name,
            });
        }
        return Ok(HookAction::Heartbeat);
    }

    // cwd changed but no idle — check if project changed
    if !project_changed {
        session.cwd = cwd.to_path_buf();
        session.last_heartbeat = now;
        storage.upsert_session(&session)?;
        return Ok(HookAction::Heartbeat);
    }

    // Project changed — stop old, start new
    let old_name = if let Some(ref old_pid) = old_project_id {
        let old_project = storage.get_project(old_pid)?;
        // Only stop if no other active sessions share this project
        let others = storage.count_active_sessions_for_project(old_pid, &session.id)?;
        if others == 0 {
            stop_hook_entry_for_project(storage, old_pid, now)?;
        }
        old_project.map(|p| p.name)
    } else {
        None
    };

    session.cwd = cwd.to_path_buf();
    session.current_project_id = new_project_id;
    session.last_heartbeat = now;
    storage.upsert_session(&session)?;

    match (old_name, new_active) {
        (Some(from), Some(to_project)) => {
            try_create_hook_entry(storage, &to_project.id, &session.id, now)?;
            Ok(HookAction::Switched {
                from,
                to: to_project.name,
            })
        }
        (Some(from), None) => Ok(HookAction::Stopped { project_name: from }),
        (None, Some(to_project)) => {
            try_create_hook_entry(storage, &to_project.id, &session.id, now)?;
            Ok(HookAction::Started {
                project_name: to_project.name,
            })
        }
        (None, None) => Ok(HookAction::Heartbeat),
    }
}

/// Handles shell exit: ends the session and conditionally stops the timer.
///
/// `session_id` should be the ID emitted by this shell's cold-start hook call.
/// When provided it is used directly, preventing PID-reuse from accidentally
/// closing a new session that inherited this PID. Falls back to a PID lookup
/// when `session_id` is `None` (e.g. older shell script integrations).
///
/// In merge mode, the entry is only stopped if no other active sessions
/// share the same project. If the session was idle at exit, clamps the
/// stop time to last_heartbeat to avoid counting idle time.
pub fn handle_hook_exit(
    storage: &impl Storage,
    pid: u32,
    session_id: Option<&SessionId>,
    config: &StintConfig,
) -> Result<(), StintError> {
    let session = match session_id {
        Some(id) => match storage.get_session(id)? {
            // Only act if the session is still active; if it's already ended
            // (e.g. reaped as stale), there's nothing to do.
            Some(s) if s.ended_at.is_none() => s,
            _ => return Ok(()),
        },
        None => match storage.get_session_by_pid(pid)? {
            Some(s) => s,
            None => return Ok(()), // No active session for this PID
        },
    };

    let now = OffsetDateTime::now_utc();

    // Clamp stop time to last_heartbeat if idle gap exceeds threshold
    let idle_gap = (now - session.last_heartbeat).whole_seconds();
    let stop_time = if idle_gap > config.idle_threshold_secs {
        session.last_heartbeat
    } else {
        now
    };

    // End the session
    storage.end_session(&session.id, stop_time)?;

    // In merge mode, only stop the entry if no other sessions share this project
    if let Some(ref project_id) = session.current_project_id {
        let other_sessions = storage.count_active_sessions_for_project(project_id, &session.id)?;
        if other_sessions == 0 {
            stop_hook_entry_for_project(storage, project_id, stop_time)?;
        }
    }

    Ok(())
}

/// Reaps stale sessions whose last heartbeat is older than the threshold.
///
/// Ends all stale sessions first, then stops hook entries only for projects
/// with no remaining active sessions (preserving merge mode invariant).
/// Returns the number of sessions reaped.
pub fn reap_stale_sessions(
    storage: &impl Storage,
    now: OffsetDateTime,
    config: &StintConfig,
) -> Result<usize, StintError> {
    let stale_secs = config
        .idle_threshold_secs
        .saturating_mul(2)
        .max(MIN_STALE_THRESHOLD_SECS);
    let threshold = now - time::Duration::seconds(stale_secs);
    let stale = storage.get_stale_sessions(threshold)?;
    let count = stale.len();

    if count == 0 {
        return Ok(0);
    }

    // Group by project_id, tracking the max last_heartbeat per project
    let mut project_max_heartbeat: std::collections::HashMap<String, (ProjectId, OffsetDateTime)> =
        std::collections::HashMap::new();

    // End all stale sessions first
    for session in &stale {
        if let Some(ref project_id) = session.current_project_id {
            let key = project_id.as_str().to_owned();
            project_max_heartbeat
                .entry(key)
                .and_modify(|(_, max_hb)| {
                    if session.last_heartbeat > *max_hb {
                        *max_hb = session.last_heartbeat;
                    }
                })
                .or_insert((project_id.clone(), session.last_heartbeat));
        }
        storage.end_session(&session.id, session.last_heartbeat)?;
    }

    // Stop entries only for projects with no remaining active sessions
    for (project_id, max_heartbeat) in project_max_heartbeat.values() {
        let dummy_id = SessionId::new();
        let active_count = storage.count_active_sessions_for_project(project_id, &dummy_id)?;
        if active_count == 0 {
            stop_hook_entry_for_project(storage, project_id, *max_heartbeat)?;
        }
    }

    Ok(count)
}

/// Stops the running hook-sourced entry for a project.
///
/// Only stops entries with `source: Hook`. Manual entries are left untouched
/// so that `stint start`/`stint stop` are not interfered with by the hook.
fn stop_hook_entry_for_project(
    storage: &impl Storage,
    project_id: &ProjectId,
    end_time: OffsetDateTime,
) -> Result<(), StintError> {
    if let Some(mut entry) = storage.get_running_hook_entry(project_id)? {
        entry.end = Some(end_time);
        entry.duration_secs = Some((end_time - entry.start).whole_seconds());
        entry.updated_at = end_time;
        storage.update_entry(&entry)?;
    }
    Ok(())
}

/// Tries to create a hook entry, skipping if one is already running (merge mode).
fn try_create_hook_entry(
    storage: &impl Storage,
    project_id: &ProjectId,
    session_id: &SessionId,
    now: OffsetDateTime,
) -> Result<(), StintError> {
    if storage.get_running_entry(project_id)?.is_none() {
        let entry = new_hook_entry(project_id, session_id, now);
        storage.create_entry(&entry)?;
    }
    Ok(())
}

/// Detects a project from registered paths, or auto-discovers from `.git`.
///
/// 1. Checks registered project paths via `get_project_by_path`
/// 2. If not found, checks if the path is ignored
/// 3. If not ignored, looks for a `.git` directory up the tree
/// 4. If found, auto-creates a project with `source: discovered`
fn detect_or_discover(
    storage: &impl Storage,
    cwd: &Path,
    now: OffsetDateTime,
    config: &StintConfig,
) -> Result<Option<Project>, StintError> {
    // First: check registered projects
    let registered = storage.get_project_by_path(cwd)?;
    if let Some(project) = registered {
        if project.status == ProjectStatus::Active {
            return Ok(Some(project));
        }
        return Ok(None);
    }

    // Second: check if auto-discovery is enabled
    if !config.auto_discover {
        return Ok(None);
    }

    // Third: check if this path is ignored
    if storage.is_path_ignored(cwd)? {
        return Ok(None);
    }

    // Fourth: try .git auto-discovery
    let discovered = match discover::discover_project(cwd) {
        Some(d) => d,
        None => return Ok(None),
    };

    // Check if the discovered root is ignored
    if storage.is_path_ignored(&discovered.root)? {
        return Ok(None);
    }

    // Try to create the project — if it already exists (race or archived), use the existing one
    use crate::models::project::ProjectSource;
    let project = Project {
        id: ProjectId::new(),
        name: discovered.name.clone(),
        paths: vec![discovered.root],
        tags: config.default_tags.clone(),
        hourly_rate_cents: config.default_rate_cents,
        status: ProjectStatus::Active,
        source: ProjectSource::Discovered,
        created_at: now,
        updated_at: now,
    };

    match storage.create_project(&project) {
        Ok(()) => Ok(Some(project)),
        Err(crate::storage::error::StorageError::DuplicateProjectName(_)) => {
            // Project already exists — use it only if active AND path matches
            match storage.get_project_by_name(&discovered.name)? {
                Some(p)
                    if p.status == ProjectStatus::Active && p.paths.contains(&project.paths[0]) =>
                {
                    Ok(Some(p))
                }
                _ => Ok(None),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Creates a new hook-sourced time entry.
fn new_hook_entry(
    project_id: &ProjectId,
    session_id: &SessionId,
    now: OffsetDateTime,
) -> TimeEntry {
    TimeEntry {
        id: EntryId::new(),
        project_id: project_id.clone(),
        session_id: Some(session_id.clone()),
        start: now,
        end: None,
        duration_secs: None,
        source: EntrySource::Hook,
        notes: None,
        tags: vec![],
        created_at: now,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StintConfig;
    use crate::models::project::{Project, ProjectStatus};
    use crate::storage::sqlite::SqliteStorage;
    use crate::storage::Storage;

    fn test_config() -> StintConfig {
        StintConfig::default()
    }
    use std::path::PathBuf;

    fn setup() -> SqliteStorage {
        SqliteStorage::open_in_memory().unwrap()
    }

    fn create_project(storage: &SqliteStorage, name: &str, path: &str) {
        let now = OffsetDateTime::now_utc();
        let project = Project {
            id: ProjectId::new(),
            name: name.to_string(),
            paths: vec![PathBuf::from(path)],
            tags: vec![],
            hourly_rate_cents: None,
            status: ProjectStatus::Active,
            source: crate::models::project::ProjectSource::Manual,
            created_at: now,
            updated_at: now,
        };
        storage.create_project(&project).unwrap();
    }

    #[test]
    fn cold_start_in_project_creates_session_and_entry() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app/src"),
            None,
            &test_config(),
        )
        .unwrap();

        assert!(matches!(action, HookAction::SessionStarted { .. }));

        // Session exists
        let session = storage.get_session_by_pid(1234).unwrap().unwrap();
        assert!(session.current_project_id.is_some());

        // Entry exists and is running
        let entry = storage.get_any_running_entry().unwrap().unwrap();
        assert_eq!(entry.source, EntrySource::Hook);
    }

    #[test]
    fn cold_start_outside_project_creates_session_only() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/other"),
            None,
            &test_config(),
        )
        .unwrap();

        assert!(matches!(action, HookAction::SessionCreated { .. }));

        let session = storage.get_session_by_pid(1234).unwrap().unwrap();
        assert!(session.current_project_id.is_none());
        assert!(storage.get_any_running_entry().unwrap().is_none());
    }

    #[test]
    fn warm_path_same_cwd_is_heartbeat() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();
        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();

        assert_eq!(action, HookAction::Heartbeat);
    }

    #[test]
    fn cwd_change_to_different_project_switches() {
        let storage = setup();
        create_project(&storage, "app-1", "/home/user/app-1");
        create_project(&storage, "app-2", "/home/user/app-2");

        handle_hook(
            &storage,
            1234,
            Path::new("/home/user/app-1"),
            None,
            &test_config(),
        )
        .unwrap();
        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/app-2"),
            None,
            &test_config(),
        )
        .unwrap();

        assert!(
            matches!(action, HookAction::Switched { from, to } if from == "app-1" && to == "app-2")
        );

        // Old entry should be stopped
        let app1 = storage.get_project_by_name("app-1").unwrap().unwrap();
        assert!(storage.get_running_entry(&app1.id).unwrap().is_none());

        // New entry should be running
        let app2 = storage.get_project_by_name("app-2").unwrap().unwrap();
        assert!(storage.get_running_entry(&app2.id).unwrap().is_some());
    }

    #[test]
    fn cwd_change_to_non_project_stops() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();
        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/other"),
            None,
            &test_config(),
        )
        .unwrap();

        assert!(matches!(action, HookAction::Stopped { .. }));
        assert!(storage.get_any_running_entry().unwrap().is_none());
    }

    #[test]
    fn cwd_change_from_non_project_to_project_starts() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        handle_hook(
            &storage,
            1234,
            Path::new("/home/user/other"),
            None,
            &test_config(),
        )
        .unwrap();
        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();

        assert!(matches!(action, HookAction::Started { .. }));
        assert!(storage.get_any_running_entry().unwrap().is_some());
    }

    #[test]
    fn manual_start_is_not_duplicated_by_hook() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        // Manually start a timer
        let project = storage.get_project_by_name("my-app").unwrap().unwrap();
        let now = OffsetDateTime::now_utc();
        let manual_entry = TimeEntry {
            id: EntryId::new(),
            project_id: project.id.clone(),
            session_id: None,
            start: now,
            end: None,
            duration_secs: None,
            source: EntrySource::Manual,
            notes: None,
            tags: vec![],
            created_at: now,
            updated_at: now,
        };
        storage.create_entry(&manual_entry).unwrap();

        // Hook fires in the same project directory
        handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();

        // Should still be only one running entry (the manual one)
        let filter = crate::models::entry::EntryFilter::default();
        let entries = storage.list_entries(&filter).unwrap();
        let running: Vec<_> = entries.iter().filter(|e| e.is_running()).collect();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].source, EntrySource::Manual);
    }

    #[test]
    fn hook_does_not_stop_manual_entry_on_exit() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        // Manually start a timer
        let project = storage.get_project_by_name("my-app").unwrap().unwrap();
        let now = OffsetDateTime::now_utc();
        let manual_entry = TimeEntry {
            id: EntryId::new(),
            project_id: project.id.clone(),
            session_id: None,
            start: now,
            end: None,
            duration_secs: None,
            source: EntrySource::Manual,
            notes: None,
            tags: vec![],
            created_at: now,
            updated_at: now,
        };
        storage.create_entry(&manual_entry).unwrap();

        // Hook creates a session in the same project
        handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();

        // Shell exits
        handle_hook_exit(&storage, 1234, None, &test_config()).unwrap();

        // Manual entry should still be running
        let loaded = storage.get_entry(&manual_entry.id).unwrap().unwrap();
        assert!(loaded.is_running());
    }

    #[test]
    fn archived_project_is_not_tracked() {
        let storage = setup();
        create_project(&storage, "old-app", "/home/user/old-app");

        // Archive the project
        let mut project = storage.get_project_by_name("old-app").unwrap().unwrap();
        project.status = ProjectStatus::Archived;
        project.updated_at = OffsetDateTime::now_utc();
        storage.update_project(&project).unwrap();

        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/old-app"),
            None,
            &test_config(),
        )
        .unwrap();

        assert!(matches!(action, HookAction::SessionCreated { .. }));
        assert!(storage.get_any_running_entry().unwrap().is_none());
    }

    #[test]
    fn exit_ends_session_and_stops_entry() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();
        assert!(storage.get_any_running_entry().unwrap().is_some());

        handle_hook_exit(&storage, 1234, None, &test_config()).unwrap();

        // Session should be ended
        assert!(storage.get_session_by_pid(1234).unwrap().is_none());

        // Entry should be stopped
        assert!(storage.get_any_running_entry().unwrap().is_none());
    }

    #[test]
    fn exit_with_session_id_ends_session_and_stops_entry() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        let action = handle_hook(
            &storage,
            1234,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();

        let session_id = match action {
            HookAction::SessionStarted { session_id, .. } => session_id,
            _ => panic!("expected SessionStarted"),
        };

        handle_hook_exit(&storage, 1234, Some(&session_id), &test_config()).unwrap();

        // Session should be ended
        assert!(storage.get_session_by_pid(1234).unwrap().is_none());

        // Entry should be stopped
        assert!(storage.get_any_running_entry().unwrap().is_none());
    }

    #[test]
    fn exit_in_merge_mode_keeps_entry_if_other_sessions() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        // Two shells in the same project
        handle_hook(
            &storage,
            1111,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();
        handle_hook(
            &storage,
            2222,
            Path::new("/home/user/my-app"),
            None,
            &test_config(),
        )
        .unwrap();

        // Only one running entry (merge mode)
        let filter = crate::models::entry::EntryFilter::default();
        let entries = storage.list_entries(&filter).unwrap();
        let running: Vec<_> = entries.iter().filter(|e| e.is_running()).collect();
        assert_eq!(running.len(), 1);

        // First shell exits
        handle_hook_exit(&storage, 1111, None, &test_config()).unwrap();

        // Entry should still be running (shell 2222 still active)
        assert!(storage.get_any_running_entry().unwrap().is_some());

        // Second shell exits
        handle_hook_exit(&storage, 2222, None, &test_config()).unwrap();

        // Now the entry should be stopped
        assert!(storage.get_any_running_entry().unwrap().is_none());
    }

    #[test]
    fn switch_in_merge_mode_keeps_entry_if_other_sessions() {
        let storage = setup();
        create_project(&storage, "app-1", "/home/user/app-1");
        create_project(&storage, "app-2", "/home/user/app-2");

        // Two shells in the same project
        handle_hook(
            &storage,
            1111,
            Path::new("/home/user/app-1"),
            None,
            &test_config(),
        )
        .unwrap();
        handle_hook(
            &storage,
            2222,
            Path::new("/home/user/app-1"),
            None,
            &test_config(),
        )
        .unwrap();

        // Shell 1 switches to app-2 — app-1's entry should NOT stop
        handle_hook(
            &storage,
            1111,
            Path::new("/home/user/app-2"),
            None,
            &test_config(),
        )
        .unwrap();

        let app1 = storage.get_project_by_name("app-1").unwrap().unwrap();
        assert!(
            storage.get_running_entry(&app1.id).unwrap().is_some(),
            "app-1 entry should still be running because shell 2222 is still there"
        );
    }

    #[test]
    fn exit_with_no_session_is_noop() {
        let storage = setup();
        // Should not error
        handle_hook_exit(&storage, 9999, None, &test_config()).unwrap();
    }

    #[test]
    fn stale_session_reaping() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        // Create a session with an old heartbeat
        let old_time = OffsetDateTime::now_utc() - time::Duration::hours(2);
        let project = storage.get_project_by_name("my-app").unwrap().unwrap();

        let session = ShellSession {
            id: SessionId::new(),
            pid: 5555,
            shell: Some("bash".to_string()),
            cwd: PathBuf::from("/home/user/my-app"),
            current_project_id: Some(project.id.clone()),
            started_at: old_time,
            last_heartbeat: old_time,
            ended_at: None,
        };
        storage.upsert_session(&session).unwrap();

        // Create a running entry for that session
        let entry = new_hook_entry(&project.id, &session.id, old_time);
        storage.create_entry(&entry).unwrap();

        // Reap stale sessions
        let now = OffsetDateTime::now_utc();
        let reaped = reap_stale_sessions(&storage, now, &test_config()).unwrap();
        assert_eq!(reaped, 1);

        // Session should be ended
        assert!(storage.get_session_by_pid(5555).unwrap().is_none());

        // Entry should be stopped at last_heartbeat time
        let stopped = storage.get_entry(&entry.id).unwrap().unwrap();
        assert!(!stopped.is_running());
    }

    #[test]
    fn stale_reaping_at_minimum_threshold() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        // Session 11 minutes old (just over the 10-minute minimum)
        let old_time = OffsetDateTime::now_utc() - time::Duration::minutes(11);
        let project = storage.get_project_by_name("my-app").unwrap().unwrap();

        let session = ShellSession {
            id: SessionId::new(),
            pid: 6666,
            shell: Some("bash".to_string()),
            cwd: PathBuf::from("/home/user/my-app"),
            current_project_id: Some(project.id.clone()),
            started_at: old_time,
            last_heartbeat: old_time,
            ended_at: None,
        };
        storage.upsert_session(&session).unwrap();
        let entry = new_hook_entry(&project.id, &session.id, old_time);
        storage.create_entry(&entry).unwrap();

        let now = OffsetDateTime::now_utc();
        let reaped = reap_stale_sessions(&storage, now, &test_config()).unwrap();
        assert_eq!(reaped, 1);
        assert!(storage.get_session_by_pid(6666).unwrap().is_none());
    }

    #[test]
    fn stale_reaping_uses_idle_threshold_times_two() {
        let storage = setup();
        create_project(&storage, "my-app", "/home/user/my-app");

        // Custom config: idle = 8 minutes, so stale = 16 minutes
        let mut config = test_config();
        config.idle_threshold_secs = 480; // 8 minutes

        // Session 17 minutes old (just over 16-minute computed threshold)
        let old_time = OffsetDateTime::now_utc() - time::Duration::minutes(17);
        let project = storage.get_project_by_name("my-app").unwrap().unwrap();

        let session = ShellSession {
            id: SessionId::new(),
            pid: 7777,
            shell: Some("bash".to_string()),
            cwd: PathBuf::from("/home/user/my-app"),
            current_project_id: Some(project.id.clone()),
            started_at: old_time,
            last_heartbeat: old_time,
            ended_at: None,
        };
        storage.upsert_session(&session).unwrap();
        let entry = new_hook_entry(&project.id, &session.id, old_time);
        storage.create_entry(&entry).unwrap();

        let now = OffsetDateTime::now_utc();
        let reaped = reap_stale_sessions(&storage, now, &config).unwrap();
        assert_eq!(reaped, 1);

        // Verify a 15-minute-old session is NOT reaped with this config
        let recent_time = OffsetDateTime::now_utc() - time::Duration::minutes(15);
        let session2 = ShellSession {
            id: SessionId::new(),
            pid: 8888,
            shell: None,
            cwd: PathBuf::from("/home/user/my-app"),
            current_project_id: None,
            started_at: recent_time,
            last_heartbeat: recent_time,
            ended_at: None,
        };
        storage.upsert_session(&session2).unwrap();

        let now = OffsetDateTime::now_utc();
        let reaped = reap_stale_sessions(&storage, now, &config).unwrap();
        assert_eq!(
            reaped, 0,
            "15-min-old session should not be reaped with 16-min threshold"
        );
    }

    #[test]
    fn auto_discovers_git_repo() {
        let storage = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path().join("my-project");
        std::fs::create_dir_all(project_dir.join(".git")).unwrap();

        let action = handle_hook(&storage, 1234, &project_dir, None, &test_config()).unwrap();

        assert!(matches!(action, HookAction::SessionStarted { .. }));

        // Project should have been auto-created
        let project = storage.get_project_by_name("my-project").unwrap().unwrap();
        assert_eq!(project.paths[0], project_dir);

        // Entry should be running
        assert!(storage.get_any_running_entry().unwrap().is_some());
    }

    #[test]
    fn auto_discovers_from_subdirectory() {
        let storage = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path().join("my-project");
        std::fs::create_dir_all(project_dir.join(".git")).unwrap();
        let sub = project_dir.join("src").join("lib");
        std::fs::create_dir_all(&sub).unwrap();

        handle_hook(&storage, 1234, &sub, None, &test_config()).unwrap();

        // Should discover the project root, not the subdirectory
        let project = storage.get_project_by_name("my-project").unwrap().unwrap();
        assert_eq!(project.paths[0], project_dir);
    }

    #[test]
    fn ignored_path_prevents_discovery() {
        let storage = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path().join("dotfiles");
        std::fs::create_dir_all(project_dir.join(".git")).unwrap();

        // Ignore this path
        storage.add_ignored_path(&project_dir).unwrap();

        let action = handle_hook(&storage, 1234, &project_dir, None, &test_config()).unwrap();

        assert!(matches!(action, HookAction::SessionCreated { .. }));
        assert!(storage.get_project_by_name("dotfiles").unwrap().is_none());
    }

    #[test]
    fn registered_project_takes_precedence_over_discovery() {
        let storage = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path().join("my-project");
        std::fs::create_dir_all(project_dir.join(".git")).unwrap();

        // Register with a custom name
        create_project(&storage, "custom-name", &project_dir.to_string_lossy());

        handle_hook(&storage, 1234, &project_dir, None, &test_config()).unwrap();

        // Should use the registered name, not the directory name
        assert!(storage
            .get_project_by_name("custom-name")
            .unwrap()
            .is_some());
        // Should NOT have created a "my-project" entry
        assert!(storage.get_project_by_name("my-project").unwrap().is_none());
    }
}
