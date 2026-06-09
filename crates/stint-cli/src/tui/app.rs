//! TUI application state and data fetching.

use stint_core::models::entry::{EntryFilter, TimeEntry};
use stint_core::models::project::Project;
use stint_core::service::StintService;
use stint_core::storage::sqlite::SqliteStorage;
use time::OffsetDateTime;

/// Which panel is currently focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// Today's entries list.
    Today,
    /// Timeline view.
    Timeline,
    /// Weekly project totals.
    Week,
}

impl Panel {
    /// Cycles to the next panel.
    pub fn next(self) -> Self {
        match self {
            Self::Today => Self::Timeline,
            Self::Timeline => Self::Week,
            Self::Week => Self::Today,
        }
    }
}

/// Which date range the timeline is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineView {
    /// Show today's timeline.
    Today,
    /// Show yesterday's timeline.
    Yesterday,
}

impl TimelineView {
    /// Toggle between today and yesterday.
    pub fn next(self) -> Self {
        match self {
            Self::Today => Self::Yesterday,
            Self::Yesterday => Self::Today,
        }
    }
}

/// Dashboard application state.
pub struct App {
    service: StintService<SqliteStorage>,
    /// Currently running timer and its project, if any.
    pub running_timer: Option<(TimeEntry, Project)>,
    /// Today's time entries with their projects.
    pub today_entries: Vec<(TimeEntry, Project)>,
    /// Yesterday's time entries with their projects (for timeline toggle).
    pub yesterday_entries: Vec<(TimeEntry, Project)>,
    /// This week's per-project totals: (project_name, total_secs).
    pub week_totals: Vec<(String, i64)>,
    /// Currently focused panel.
    pub selected_panel: Panel,
    /// Scroll offset for the today panel.
    pub today_scroll: usize,
    /// Scroll offset for the timeline panel.
    pub timeline_scroll: usize,
    /// Scroll offset for the week panel.
    pub week_scroll: usize,
    /// Whether the timeline is showing today or yesterday.
    pub timeline_view: TimelineView,
    /// Whether the app should quit.
    pub should_quit: bool,
}

impl App {
    /// Creates a new App with the given storage backend.
    pub fn new(storage: SqliteStorage) -> Self {
        let service = StintService::new(storage);
        let mut app = Self {
            service,
            running_timer: None,
            today_entries: vec![],
            yesterday_entries: vec![],
            week_totals: vec![],
            selected_panel: Panel::Today,
            today_scroll: 0,
            timeline_scroll: 0,
            week_scroll: 0,
            timeline_view: TimelineView::Today,
            should_quit: false,
        };
        app.refresh();
        app
    }

    /// Refreshes all dashboard data from the database.
    pub fn refresh(&mut self) {
        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());

        // Running timer
        self.running_timer = self.service.get_status().unwrap_or(None);

        // Today's entries — use replace_time to preserve the local offset
        let today_start = now.replace_time(time::Time::MIDNIGHT);
        let today_filter = EntryFilter {
            from: Some(today_start),
            ..Default::default()
        };
        self.today_entries = self.service.get_entries(&today_filter).unwrap_or_default();

        // Yesterday's entries
        let yesterday_start = today_start - time::Duration::days(1);
        let yesterday_filter = EntryFilter {
            from: Some(yesterday_start),
            to: Some(today_start),
            ..Default::default()
        };
        self.yesterday_entries = self
            .service
            .get_entries(&yesterday_filter)
            .unwrap_or_default();

        // This week's totals (Monday to now)
        let weekday = now.weekday().number_days_from_monday();
        let week_start = today_start - time::Duration::days(weekday as i64);
        let week_filter = EntryFilter {
            from: Some(week_start),
            ..Default::default()
        };
        let week_entries = self.service.get_entries(&week_filter).unwrap_or_default();

        // Aggregate by project
        let mut totals: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
        for (entry, project) in &week_entries {
            let duration = entry.computed_duration_secs().unwrap_or(0);
            *totals.entry(project.name.clone()).or_insert(0) += duration;
        }
        self.week_totals = totals.into_iter().collect();
        // Sort by total descending
        self.week_totals
            .sort_by_key(|(_, total)| std::cmp::Reverse(*total));
    }

    /// Scrolls the focused panel up.
    pub fn scroll_up(&mut self) {
        match self.selected_panel {
            Panel::Today => {
                self.today_scroll = self.today_scroll.saturating_sub(1);
            }
            Panel::Timeline => {
                self.timeline_scroll = self.timeline_scroll.saturating_sub(1);
            }
            Panel::Week => {
                self.week_scroll = self.week_scroll.saturating_sub(1);
            }
        }
    }

    /// Scrolls the focused panel down.
    pub fn scroll_down(&mut self) {
        match self.selected_panel {
            Panel::Today => {
                let max = self.today_entries.len().saturating_sub(1);
                if self.today_scroll < max {
                    self.today_scroll += 1;
                }
            }
            Panel::Timeline => {
                let max = self.timeline_scroll_items().saturating_sub(1);
                if self.timeline_scroll < max {
                    self.timeline_scroll += 1;
                }
            }
            Panel::Week => {
                let max = self.week_totals.len().saturating_sub(1);
                if self.week_scroll < max {
                    self.week_scroll += 1;
                }
            }
        }
    }

    /// Returns the current timeline entries based on the view.
    pub fn timeline_entries(&self) -> &[(TimeEntry, Project)] {
        match self.timeline_view {
            TimelineView::Today => &self.today_entries,
            TimelineView::Yesterday => &self.yesterday_entries,
        }
    }

    /// Returns the exact rendered line count of the timeline panel, so scrolling
    /// clamps precisely to the content (no stranded bottom, no blank overscroll).
    pub fn timeline_scroll_items(&self) -> usize {
        super::timeline::line_count(self.timeline_entries(), self.timeline_view)
    }

    /// Toggle the timeline view between today and yesterday.
    pub fn toggle_timeline_view(&mut self) {
        self.timeline_view = self.timeline_view.next();
        self.timeline_scroll = 0;
    }

    /// Check if timeline view is showing today.
    pub fn is_timeline_today(&self) -> bool {
        self.timeline_view == TimelineView::Today
    }
}
