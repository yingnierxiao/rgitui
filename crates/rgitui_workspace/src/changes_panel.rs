use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    div, px, uniform_list, App, ClickEvent, Context, ElementId, Entity, FocusHandle,
    KeyDownEvent, ListSizingBehavior, Render, ScrollStrategy, SharedString,
    UniformListScrollHandle, WeakEntity, Window,
};
use rgitui_git::{FileChangeKind, FileStatus};
use rgitui_i18n::{key, t, tp};
use rgitui_settings::SettingsState;
use rgitui_theme::{ActiveTheme, Color, StyledExt};
use rgitui_ui::{
    Button, ButtonSize, CheckState, Checkbox, DiffStat, Label, LabelSize, TextInput,
    TextInputEvent, Tooltip,
};

use crate::{Sidebar, SidebarEvent};

/// One row per path, even when both the index and working tree have changes.
/// A partially staged file is represented by an indeterminate checkbox.
#[derive(Debug, Clone, PartialEq)]
struct ChangeRow {
    path: PathBuf,
    staged: Option<FileStatus>,
    unstaged: Option<FileStatus>,
}

impl ChangeRow {
    fn state(&self) -> CheckState {
        match (self.staged.is_some(), self.unstaged.is_some()) {
            (true, true) => CheckState::Indeterminate,
            (true, false) => CheckState::Checked,
            (false, true) => CheckState::Unchecked,
            (false, false) => CheckState::Unchecked,
        }
    }

    fn kind(&self) -> FileChangeKind {
        self.unstaged
            .as_ref()
            .or(self.staged.as_ref())
            .map_or(FileChangeKind::Modified, |status| status.kind)
    }

    fn selected_event(&self) -> SidebarEvent {
        let path = self.path.to_string_lossy().to_string();
        match self.kind() {
            FileChangeKind::UntrackedDirectory if self.unstaged.is_some() => {
                SidebarEvent::ExpandUntrackedDirectory(path)
            }
            FileChangeKind::Conflicted if self.unstaged.is_some() => {
                SidebarEvent::ConflictFileSelected(path)
            }
            _ => SidebarEvent::FileSelected {
                path,
                staged: self.unstaged.is_none(),
            },
        }
    }

    /// Partial staging preserves two independently viewable diffs: the row
    /// opens the working-tree delta, and a small index control opens the
    /// staged delta without changing the checkbox.
    fn staged_preview_event(&self) -> Option<SidebarEvent> {
        if self.staged.is_some() && self.unstaged.is_some() {
            Some(SidebarEvent::FileSelected {
                path: self.path.to_string_lossy().to_string(),
                staged: true,
            })
        } else {
            None
        }
    }

    fn toggle_event(&self) -> SidebarEvent {
        let path = self.path.to_string_lossy().to_string();
        if self.unstaged.is_some() {
            // Unchecked and partially staged files become fully staged.
            SidebarEvent::StageFile(path)
        } else {
            SidebarEvent::UnstageFile(path)
        }
    }

    fn has_conflict(&self) -> bool {
        self.staged
            .iter()
            .chain(self.unstaged.iter())
            .any(|status| status.kind == FileChangeKind::Conflicted)
    }

    fn display_status(&self) -> Option<&FileStatus> {
        self.unstaged.as_ref().or(self.staged.as_ref())
    }
}

/// Build a flat, deterministic list without losing partially staged files.
fn combine_changes(staged: Vec<FileStatus>, unstaged: Vec<FileStatus>) -> Vec<ChangeRow> {
    let mut rows: Vec<ChangeRow> = Vec::with_capacity(staged.len() + unstaged.len());
    let mut positions = HashMap::with_capacity(rows.capacity());

    for file in staged {
        let path = file.path.clone();
        positions.insert(path.clone(), rows.len());
        rows.push(ChangeRow {
            path,
            staged: Some(file),
            unstaged: None,
        });
    }

    for file in unstaged {
        if let Some(&index) = positions.get(&file.path) {
            rows[index].unstaged = Some(file);
        } else {
            let path = file.path.clone();
            positions.insert(path.clone(), rows.len());
            rows.push(ChangeRow {
                path,
                staged: None,
                unstaged: Some(file),
            });
        }
    }

    rows.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    rows
}

/// Determine header state and its action from *visible*, non-conflicted rows.
/// If a filter hides files, the action is restricted to visible paths.
fn bulk_toggle_for_visible(
    rows: &[ChangeRow],
    visible: &[usize],
) -> (CheckState, Option<SidebarEvent>) {
    let selectable = visible
        .iter()
        .filter_map(|&index| rows.get(index))
        .filter(|row| !row.has_conflict())
        .collect::<Vec<_>>();
    if selectable.is_empty() {
        return (CheckState::Unchecked, None);
    }
    let all_checked = selectable
        .iter()
        .all(|row| row.state() == CheckState::Checked);
    let any_checked = selectable.iter().any(|row| row.staged.is_some());
    let state = if all_checked {
        CheckState::Checked
    } else if any_checked {
        CheckState::Indeterminate
    } else {
        CheckState::Unchecked
    };

    // Conflict rows are never valid toggle targets. Even with no text
    // filter, a bulk operation must not implicitly reset those index entries.
    let restricted = visible.len() != rows.len() || selectable.len() != rows.len();
    let action = if !restricted {
        if all_checked {
            SidebarEvent::UnstageAll
        } else {
            SidebarEvent::StageAll
        }
    } else if all_checked {
        SidebarEvent::UnstageFiles(
            selectable
                .iter()
                .map(|row| row.path.to_string_lossy().to_string())
                .collect(),
        )
    } else {
        SidebarEvent::StageFiles(
            selectable
                .iter()
                .filter(|row| row.unstaged.is_some())
                .map(|row| row.path.to_string_lossy().to_string())
                .collect(),
        )
    };
    (state, Some(action))
}

fn change_color(kind: FileChangeKind) -> Color {
    match kind {
        FileChangeKind::Added
        | FileChangeKind::Untracked
        | FileChangeKind::UntrackedDirectory => Color::Added,
        FileChangeKind::Deleted => Color::Deleted,
        FileChangeKind::Conflicted => Color::Conflict,
        FileChangeKind::Modified => Color::Modified,
        _ => Color::Muted,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKindFilter {
    All,
    Modified,
    Added,
    Deleted,
    Other,
}

impl ChangeKindFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Modified,
            Self::Modified => Self::Added,
            Self::Added => Self::Deleted,
            Self::Deleted => Self::Other,
            Self::Other => Self::All,
        }
    }

    fn matches(self, kind: FileChangeKind) -> bool {
        match self {
            Self::All => true,
            Self::Modified => kind == FileChangeKind::Modified,
            Self::Added => matches!(
                kind,
                FileChangeKind::Added
                    | FileChangeKind::Untracked
                    | FileChangeKind::UntrackedDirectory
            ),
            Self::Deleted => kind == FileChangeKind::Deleted,
            Self::Other => !matches!(
                kind,
                FileChangeKind::Modified
                    | FileChangeKind::Added
                    | FileChangeKind::Untracked
                    | FileChangeKind::UntrackedDirectory
                    | FileChangeKind::Deleted
            ),
        }
    }
}

fn change_matches_filter(
    row: &ChangeRow,
    needle: &str,
    kind: ChangeKindFilter,
    only_staged: bool,
) -> bool {
    (!only_staged || row.staged.is_some())
        && kind.matches(row.kind())
        && (needle.is_empty()
            || row.path.to_string_lossy().to_lowercase().contains(needle))
}

/// The Git index remains authoritative; these are temporary visual states
/// while an asynchronous stage/unstage request is in flight.
fn visual_state(row: &ChangeRow, pending: &HashMap<PathBuf, CheckState>) -> CheckState {
    pending.get(&row.path).copied().unwrap_or_else(|| row.state())
}

/// Clear optimistic states only after the refreshed index agrees with them.
/// An earlier watcher snapshot must not undo a checkbox while Git is still working.
fn reconcile_pending(rows: &[ChangeRow], pending: &mut HashMap<PathBuf, CheckState>) -> bool {
    let previous_count = pending.len();
    pending.retain(|path, desired| {
        rows.binary_search_by(|row| row.path.cmp(path))
            .ok()
            .is_some_and(|index| rows[index].state() != *desired)
    });
    previous_count != pending.len()
}

fn visual_header_state(
    rows: &[ChangeRow],
    visible: &[usize],
    pending: &HashMap<PathBuf, CheckState>,
) -> CheckState {
    let mut any = false;
    let mut all = true;
    let mut found = false;
    for row in visible.iter().filter_map(|index| rows.get(*index)) {
        if row.has_conflict() {
            continue;
        }
        found = true;
        match visual_state(row, pending) {
            CheckState::Checked => any = true,
            CheckState::Indeterminate => {
                any = true;
                all = false;
            }
            CheckState::Unchecked => all = false,
        }
    }
    if found && all {
        CheckState::Checked
    } else if any {
        CheckState::Indeterminate
    } else {
        CheckState::Unchecked
    }
}

/// Select a range in the displayed order (not path order), so the keyboard
/// and mouse behave consistently even with sorting and filters enabled.
fn selection_after_click(
    rows: &[ChangeRow],
    visible: &[usize],
    current: &HashSet<PathBuf>,
    anchor: Option<&Path>,
    clicked: usize,
    extend: bool,
    additive: bool,
) -> (HashSet<PathBuf>, PathBuf) {
    let clicked_path = rows[visible[clicked]].path.clone();
    let mut selection = if additive {
        current.clone()
    } else {
        HashSet::new()
    };
    if extend {
        let first = anchor
            .and_then(|path| {
                visible
                    .iter()
                    .position(|index| rows[*index].path.as_path() == path)
            })
            .unwrap_or(clicked);
        for position in first.min(clicked)..=first.max(clicked) {
            selection.insert(rows[visible[position]].path.clone());
        }
    } else if additive {
        if !selection.insert(clicked_path.clone()) {
            selection.remove(&clicked_path);
        }
    } else {
        selection.insert(clicked_path.clone());
    }
    (selection, clicked_path)
}

/// Build one Git operation for the selected visible paths. Already staged,
/// conflicted and in-flight paths are excluded to prevent redundant writes.
fn selected_staging_action(
    rows: &[ChangeRow],
    visible: &[usize],
    selected: &HashSet<PathBuf>,
    pending: &HashMap<PathBuf, CheckState>,
    target: CheckState,
) -> Option<SidebarEvent> {
    let paths: Vec<String> = visible
        .iter()
        .filter_map(|index| rows.get(*index))
        .filter(|row| {
            selected.contains(&row.path)
                && !row.has_conflict()
                && !pending.contains_key(&row.path)
                && match target {
                    CheckState::Checked => row.unstaged.is_some(),
                    CheckState::Unchecked => row.staged.is_some(),
                    CheckState::Indeterminate => false,
                }
        })
        .map(|row| row.path.to_string_lossy().into_owned())
        .collect();
    if paths.is_empty() {
        None
    } else if target == CheckState::Checked {
        Some(SidebarEvent::StageFiles(paths))
    } else {
        Some(SidebarEvent::UnstageFiles(paths))
    }
}

/// GitHub Desktop-style working-tree list. The repository navigation Sidebar
/// retains sole ownership of Git actions; this panel forwards SidebarEvent
/// instead of implementing a second set of Stage/Unstage handlers.
pub struct ChangesPanel {
    sidebar: WeakEntity<Sidebar>,
    rows: Arc<Vec<ChangeRow>>,
    pending: Arc<HashMap<PathBuf, CheckState>>,
    visible: Arc<Vec<usize>>,
    selected_paths: Arc<HashSet<PathBuf>>,
    selection_anchor: Option<PathBuf>,
    /// Cached counts avoid scanning huge working trees on every 60 FPS render.
    staged_count: usize,
    header_state: CheckState,
    header_action: Option<SidebarEvent>,
    filter: String,
    kind_filter: ChangeKindFilter,
    only_staged: bool,
    sort_by_kind: bool,
    filter_editor: Entity<TextInput>,
    selected_path: Option<PathBuf>,
    selected_row: Option<usize>,
    worktree_path: Option<PathBuf>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl ChangesPanel {
    pub fn new(sidebar: WeakEntity<Sidebar>, cx: &mut Context<Self>) -> Self {
        let filter_editor = cx.new(|cx| {
            let mut editor = TextInput::new(cx);
            editor.set_placeholder_message(key!("changes.filter.placeholder"));
            editor.set_compact(true);
            editor
        });
        cx.subscribe(
            &filter_editor,
            |this: &mut Self, _, event: &TextInputEvent, cx| {
                if let TextInputEvent::Changed(text) = event {
                    this.filter = text.to_string();
                    this.rebuild_filter();
                    cx.notify();
                }
            },
        )
        .detach();
        Self {
            sidebar,
            rows: Arc::new(Vec::new()),
            pending: Arc::new(HashMap::new()),
            visible: Arc::new(Vec::new()),
            selected_paths: Arc::new(HashSet::new()),
            selection_anchor: None,
            staged_count: 0,
            header_state: CheckState::Unchecked,
            header_action: None,
            filter: String::new(),
            kind_filter: ChangeKindFilter::All,
            only_staged: false,
            sort_by_kind: false,
            filter_editor,
            selected_path: None,
            selected_row: None,
            worktree_path: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_worktree(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.worktree_path.as_ref() != Some(&path) {
            self.worktree_path = Some(path);
            self.pending = Arc::new(HashMap::new());
            self.selected_paths = Arc::new(HashSet::new());
            self.selection_anchor = None;
            self.selected_path = None;
            self.selected_row = None;
            self.scroll = UniformListScrollHandle::new();
            cx.notify();
        }
    }

    pub fn update_status(
        &mut self,
        staged: Vec<FileStatus>,
        unstaged: Vec<FileStatus>,
        cx: &mut Context<Self>,
    ) {
        let updated = combine_changes(staged, unstaged);
        let pending_changed = reconcile_pending(&updated, Arc::make_mut(&mut self.pending));
        if self.rows.as_ref() == &updated && !pending_changed {
            return;
        }
        if self
            .selected_path
            .as_ref()
            .is_some_and(|path| !updated.iter().any(|row| &row.path == path))
        {
            self.selected_path = None;
            self.selected_row = None;
        }
        self.update_counts(&updated);
        self.rows = Arc::new(updated);
        self.rebuild_filter();
        cx.notify();
    }

    /// Expand a placeholder without rescanning the whole Git repository.
    /// The navigation Sidebar receives the same children through its own event
    /// handler so that existing operations and keyboard paths stay consistent.
    pub fn expand_untracked_directory(
        &mut self,
        worktree: &Path,
        path: &Path,
        children: Vec<FileStatus>,
        cx: &mut Context<Self>,
    ) {
        // Ignore a directory expansion that finished after the user switched
        // to another worktree, even when both use the same relative path.
        if self.worktree_path.as_deref() != Some(worktree) {
            return;
        }
        let Some(index) = self.rows.iter().position(|row| {
            row.path == path && row.kind() == FileChangeKind::UntrackedDirectory
        }) else {
            return;
        };
        let mut rows = self.rows.as_ref().clone();
        rows.remove(index);
        rows.extend(children.into_iter().map(|file| ChangeRow {
            path: file.path.clone(),
            staged: None,
            unstaged: Some(file),
        }));
        rows.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        self.update_counts(&rows);
        self.rows = Arc::new(rows);
        if self.selected_path.as_deref() == Some(path) {
            self.selected_path = None;
            self.selected_row = None;
        }
        self.rebuild_filter();
        cx.notify();
    }

    fn update_counts(&mut self, rows: &[ChangeRow]) {
        self.staged_count = rows
            .iter()
            .filter(|row| visual_state(row, &self.pending) != CheckState::Unchecked)
            .count();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn rebuild_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut visible: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                change_matches_filter(row, &needle, self.kind_filter, false)
                    && (!self.only_staged
                        || visual_state(row, &self.pending) != CheckState::Unchecked)
            })
            .map(|(index, _)| index)
            .collect();
        if self.sort_by_kind {
            visible.sort_unstable_by(|left, right| {
                let left_row = &self.rows[*left];
                let right_row = &self.rows[*right];
                left_row
                    .kind()
                    .short_code()
                    .cmp(right_row.kind().short_code())
                    .then_with(|| left_row.path.cmp(&right_row.path))
            });
        }
        let (state, action) = bulk_toggle_for_visible(&self.rows, &visible);
        self.header_state = if self.pending.is_empty() {
            state
        } else {
            visual_header_state(&self.rows, &visible, &self.pending)
        };
        self.header_action = action;
        if !self.selected_paths.is_empty() {
            let visible_paths: HashSet<&Path> = visible
                .iter()
                .map(|index| self.rows[*index].path.as_path())
                .collect();
            Arc::make_mut(&mut self.selected_paths)
                .retain(|path| visible_paths.contains(path.as_path()));
        }
        self.visible = Arc::new(visible);
        self.selected_row = self
            .selected_path
            .as_ref()
            .and_then(|path| {
                self.visible
                    .iter()
                    .position(|&index| self.rows[index].path == *path)
            });
    }

    fn send(&self, event: SidebarEvent, cx: &mut App) {
        self.sidebar
            .update(cx, |_, cx| cx.emit(event))
            .ok();
    }

    /// Display the user's intent immediately without mutating the Git snapshot.
    /// Rows with in-flight operations stay disabled until the index confirms
    /// them (or an operation failure rolls the visual state back).
    fn optimistic_stage_action(&mut self, event: &SidebarEvent, cx: &mut Context<Self>) {
        let (desired, paths): (CheckState, Option<&[String]>) = match event {
            SidebarEvent::StageFile(path) => {
                (CheckState::Checked, Some(std::slice::from_ref(path)))
            }
            SidebarEvent::UnstageFile(path) => {
                (CheckState::Unchecked, Some(std::slice::from_ref(path)))
            }
            SidebarEvent::StageFiles(paths) => (CheckState::Checked, Some(paths)),
            SidebarEvent::UnstageFiles(paths) => (CheckState::Unchecked, Some(paths)),
            SidebarEvent::StageAll => (CheckState::Checked, None),
            SidebarEvent::UnstageAll => (CheckState::Unchecked, None),
            _ => return,
        };

        let pending = Arc::make_mut(&mut self.pending);
        let mut changed = false;
        for row in self.rows.iter() {
            if row.has_conflict()
                || pending.contains_key(&row.path)
                || row.state() == desired
            {
                continue;
            }
            if paths.is_some_and(|paths| {
                !paths.iter().any(|path| row.path.as_path() == Path::new(path))
            }) {
                continue;
            }
            pending.insert(row.path.clone(), desired);
            changed = true;
        }
        if changed {
            self.staged_count = self
                .rows
                .iter()
                .filter(|row| visual_state(row, &self.pending) != CheckState::Unchecked)
                .count();
            self.rebuild_filter();
            cx.notify();
        }
    }

    fn dispatch_stage_action(&mut self, event: SidebarEvent, cx: &mut Context<Self>) {
        self.optimistic_stage_action(&event, cx);
        self.send(event, cx);
    }

    /// A failed Git operation must not leave a phantom checked/unchecked row.
    /// The existing error toast remains responsible for reporting the failure.
    pub fn rollback_pending(&mut self, stage: bool, cx: &mut Context<Self>) {
        let desired = if stage {
            CheckState::Checked
        } else {
            CheckState::Unchecked
        };
        let pending = Arc::make_mut(&mut self.pending);
        let before = pending.len();
        pending.retain(|_, target| *target != desired);
        if pending.len() != before {
            self.staged_count = self
                .rows
                .iter()
                .filter(|row| visual_state(row, &self.pending) != CheckState::Unchecked)
                .count();
            self.rebuild_filter();
            cx.notify();
        }
    }

    /// Regular click selects one row, Ctrl/Cmd toggles individual rows and
    /// Shift extends the range from the last unmodified click.
    fn select_visible_row(
        &mut self,
        row_number: usize,
        extend: bool,
        additive: bool,
        cx: &mut Context<Self>,
    ) {
        if row_number >= self.visible.len() {
            return;
        }
        let (selected, clicked_path) = selection_after_click(
            &self.rows,
            &self.visible,
            &self.selected_paths,
            self.selection_anchor.as_deref(),
            row_number,
            extend,
            additive,
        );
        if !extend || self.selection_anchor.is_none() {
            self.selection_anchor = Some(clicked_path.clone());
        }
        self.selected_paths = Arc::new(selected);
        self.selected_path = Some(clicked_path);
        self.selected_row = Some(row_number);
        cx.notify();
    }

    /// Clicking a checkbox within a multi-selection applies one operation to
    /// the group. Shift-clicking a checkbox also stages/unstages the range.
    fn toggle_checkbox(
        &mut self,
        row_number: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.visible.get(row_number).copied() else {
            return;
        };
        let path = self.rows[index].path.clone();
        if extend || !self.selected_paths.contains(&path) {
            self.select_visible_row(row_number, extend, false, cx);
        }
        let Some(row) = self.rows.get(index) else {
            return;
        };
        if row.has_conflict() || self.pending.contains_key(&path) {
            return;
        }
        // Keep the existing single-file path and its precise Git status
        // messages; only multi-selection needs a batch operation.
        if self.selected_paths.len() == 1 {
            let action = row.toggle_event();
            self.dispatch_stage_action(action, cx);
            return;
        }
        let target = if visual_state(row, &self.pending) == CheckState::Checked {
            CheckState::Unchecked
        } else {
            CheckState::Checked
        };
        if let Some(action) = selected_staging_action(
            &self.rows,
            &self.visible,
            &self.selected_paths,
            &self.pending,
            target,
        ) {
            self.dispatch_stage_action(action, cx);
        }
    }

    fn select_all_visible(&mut self, cx: &mut Context<Self>) {
        self.selected_paths = Arc::new(
            self.visible
                .iter()
                .map(|index| self.rows[*index].path.clone())
                .collect(),
        );
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected_paths = Arc::new(HashSet::new());
        self.selection_anchor = None;
        cx.notify();
    }

    fn activate_selected(&self, cx: &mut App) {
        if let Some(row) = self.selected_row.and_then(|index| self.visible.get(index)) {
            self.send(self.rows[*row].selected_event(), cx);
        }
    }

    fn toggle_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(row_number) = self.selected_row {
            self.toggle_checkbox(row_number, false, cx);
        }
    }

    fn select_offset(&mut self, direction: isize, extend: bool, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        let next = match (self.selected_row, direction) {
            (None, _) => 0,
            (Some(0), d) if d < 0 => 0,
            (Some(index), d) if d > 0 => (index + 1).min(last),
            (Some(index), _) => index.saturating_sub(1),
        };
        self.select_visible_row(next, extend, false, cx);
        self.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
    }
}

impl Render for ChangesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.colors().clone();
        let row_colors = colors.clone();
        let rows = self.rows.clone();
        let pending = self.pending.clone();
        let visible = self.visible.clone();
        let selected_paths = self.selected_paths.clone();
        let sidebar = self.sidebar.clone();
        let panel = cx.weak_entity();
        let show_line_stats = cx.global::<SettingsState>().settings().show_change_line_stats;
        let staged_count = self.staged_count;
        let header_state = self.header_state;
        let header_action = self.header_action.clone();
        let panel_header = panel.clone();
        let has_pending = !self.pending.is_empty();
        let file_count = rows.len();
        let kind_filter_button = panel.clone();
        let staged_filter_button = panel.clone();
        let sort_button = panel.clone();
        let kind_filter_label = match self.kind_filter {
            ChangeKindFilter::All => t!("changes.kind.all"),
            ChangeKindFilter::Modified => t!("changes.kind.modified"),
            ChangeKindFilter::Added => t!("changes.kind.added"),
            ChangeKindFilter::Deleted => t!("changes.kind.deleted"),
            ChangeKindFilter::Other => t!("changes.kind.other"),
        };
        let sort_label = if self.sort_by_kind {
            t!("changes.sort.status")
        } else {
            t!("changes.sort.path")
        };
        let staged_only = self.only_staged;
        let filtered_count = self.visible.len();

        let list = uniform_list(
            "changes-flat-list",
            visible.len(),
            move |range: Range<usize>, _window: &mut Window, _cx: &mut App| {
                range
                    .map(|row_number| {
                        let row = &rows[visible[row_number]];
                        let file_path = row.path.to_string_lossy().to_string();
                        let filename: SharedString = row
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| file_path.clone())
                            .into();
                        let parent: SharedString = row
                            .path
                            .parent()
                            .filter(|dir| !dir.as_os_str().is_empty())
                            .map(|dir| dir.to_string_lossy().to_string())
                            .unwrap_or_default()
                            .into();
                        let kind = row.kind();
                        let is_selected = selected_paths.contains(&row.path);
                        let check_state = visual_state(row, &pending);
                        let disabled = row.has_conflict() || pending.contains_key(&row.path);
                        let select_action = row.selected_event();
                        let staged_preview = row.staged_preview_event();
                        let stats = row.display_status().cloned();
                        let sidebar_select = sidebar.clone();
                        let weak_panel_toggle = panel.clone();
                        let sidebar_preview = sidebar.clone();
                        let weak_panel = panel.clone();
                        let path_tooltip = file_path.clone();
                        let row_bg = row_colors.ghost_element_selected;
                        let row_hover = row_colors.ghost_element_hover;
                        let accent = row_colors.text_accent;

                        div()
                            .id(ElementId::NamedInteger("changes-file-row".into(), row_number as u64))
                            .h_flex()
                            .w_full()
                            .h(px(28.))
                            .px(px(8.))
                            .gap(px(7.))
                            .items_center()
                            .border_l_2()
                            .border_color(row_colors.border_transparent)
                            .when(is_selected, |el| {
                                el.bg(row_bg).border_color(accent)
                            })
                            .hover(move |el| el.bg(row_hover))
                            .cursor_pointer()
                            .tooltip(Tooltip::text(path_tooltip))
                            .on_click(move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let modifiers = event.modifiers();
                                weak_panel
                                    .update(cx, |this, cx| {
                                        this.select_visible_row(
                                            row_number,
                                            modifiers.shift,
                                            modifiers.secondary(),
                                            cx,
                                        );
                                        this.focus_handle.focus(window, cx);
                                    })
                                    .ok();
                                if !modifiers.shift && !modifiers.secondary() {
                                    sidebar_select
                                        .update(cx, |_, cx| cx.emit(select_action.clone()))
                                        .ok();
                                }
                            })
                            .child(
                                Checkbox::new(
                                    ElementId::NamedInteger(
                                        "changes-stage-checkbox".into(),
                                        row_number as u64,
                                    ),
                                    check_state,
                                )
                                .disabled(disabled)
                                .on_click(move |event, _, cx| {
                                    let extend = event.modifiers().shift;
                                    cx.stop_propagation();
                                    weak_panel_toggle
                                        .update(cx, |this, cx| {
                                            this.toggle_checkbox(row_number, extend, cx);
                                        })
                                        .ok();
                                }),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(4.))
                                    .overflow_hidden()
                                    .child(
                                        Label::new(filename)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Default)
                                            .truncate(),
                                    )
                                    .child(
                                        Label::new(parent)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                            )
                            .child(
                                Label::new(kind.short_code())
                                    .size(LabelSize::XSmall)
                                    .color(change_color(kind)),
                            )
                            .when_some(staged_preview, |el, preview| {
                                el.child(
                                    Button::new(
                                        ElementId::NamedInteger(
                                            "changes-preview-staged".into(),
                                            row_number as u64,
                                        ),
                                        t!("changes.preview_staged"),
                                    )
                                    .size(ButtonSize::Compact)
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        sidebar_preview
                                            .update(cx, |_, cx| cx.emit(preview.clone()))
                                            .ok();
                                    }),
                                )
                            })
                            .when(show_line_stats && !disabled, |el| {
                                if let Some(ref stat) = stats {
                                    el.child(DiffStat::new(stat.additions, stat.deletions))
                                } else {
                                    el
                                }
                            })
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            },
        )
        .h_full()
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .track_scroll(&self.scroll);

        div()
            .id("changes-panel")
            .v_flex()
            .w_full()
            .h_full()
            .min_h_0()
            .bg(colors.panel_background)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !this.focus_handle.is_focused(window) {
                    cx.propagate();
                    return;
                }
                let modifiers = event.keystroke.modifiers;
                if modifiers.secondary() {
                    if event.keystroke.key == "a" {
                        this.select_all_visible(cx);
                    } else {
                        cx.propagate();
                    }
                    return;
                }
                if modifiers.alt {
                    cx.propagate();
                    return;
                }
                match event.keystroke.key.as_str() {
                    "up" | "k" => this.select_offset(-1, modifiers.shift, cx),
                    "down" | "j" => this.select_offset(1, modifiers.shift, cx),
                    "enter" => this.activate_selected(cx),
                    "space" | "s" => this.toggle_selected(cx),
                    "escape" => this.clear_selection(cx),
                    _ => cx.propagate(),
                }
            }))
            .child(
                div()
                    .h_flex()
                    .w_full()
                    .h(px(34.))
                    .px(px(8.))
                    .gap(px(8.))
                    .items_center()
                    .bg(colors.toolbar_background)
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        Checkbox::new("changes-check-all", header_state)
                            .disabled(header_action.is_none() || has_pending)
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                if let Some(ref action) = header_action {
                                    panel_header
                                        .update(cx, |this, cx| {
                                            this.dispatch_stage_action(action.clone(), cx);
                                        })
                                        .ok();
                                }
                            }),
                    )
                    .child(
                        Label::new(tp!("changes.header", file_count))
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format!("{staged_count}/{file_count}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(36.))
                    .px(px(8.))
                    .py(px(4.))
                    .child(self.filter_editor.clone()),
            )
            .child(
                div()
                    .h_flex()
                    .w_full()
                    .h(px(30.))
                    .px(px(8.))
                    .gap(px(4.))
                    .items_center()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        Button::new("changes-kind-filter", kind_filter_label)
                            .size(ButtonSize::Compact)
                            .on_click(move |_, _, cx| {
                                kind_filter_button
                                    .update(cx, |panel, cx| {
                                        panel.kind_filter = panel.kind_filter.next();
                                        panel.rebuild_filter();
                                        cx.notify();
                                    })
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new("changes-staged-filter", t!("changes.only_staged"))
                            .size(ButtonSize::Compact)
                            .selected(staged_only)
                            .on_click(move |_, _, cx| {
                                staged_filter_button
                                    .update(cx, |panel, cx| {
                                        panel.only_staged = !panel.only_staged;
                                        panel.rebuild_filter();
                                        cx.notify();
                                    })
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new("changes-sort", sort_label)
                            .size(ButtonSize::Compact)
                            .on_click(move |_, _, cx| {
                                sort_button
                                    .update(cx, |panel, cx| {
                                        panel.sort_by_kind = !panel.sort_by_kind;
                                        panel.rebuild_filter();
                                        cx.notify();
                                    })
                                    .ok();
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format!("{filtered_count}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when(file_count == 0, |el| {
                        el.child(
                            div()
                                .p(px(12.))
                                .child(
                                    Label::new(t!("sidebar.working_clean"))
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                        )
                    })
                    .when(file_count > 0 && self.visible.is_empty(), |el| {
                        el.child(
                            div()
                                .p(px(12.))
                                .child(
                                    Label::new(t!("changes.no_matches"))
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                        )
                    })
                    .when(!self.visible.is_empty(), |el| el.child(list)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, kind: FileChangeKind) -> FileStatus {
        FileStatus {
            path: PathBuf::from(path),
            kind,
            old_path: None,
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn ctrl_click_selects_non_adjacent_paths_and_toggles_them() {
        let rows = combine_changes(
            vec![],
            vec![
                file("a.rs", FileChangeKind::Modified),
                file("b.rs", FileChangeKind::Modified),
                file("c.rs", FileChangeKind::Modified),
            ],
        );
        let visible = vec![0, 1, 2];
        let (selection, _) =
            selection_after_click(&rows, &visible, &HashSet::new(), None, 0, false, false);
        let (selection, _) = selection_after_click(
            &rows,
            &visible,
            &selection,
            Some(Path::new("a.rs")),
            2,
            false,
            true,
        );
        assert_eq!(selection.len(), 2);
        assert!(selection.contains(Path::new("a.rs")));
        assert!(selection.contains(Path::new("c.rs")));
        let (selection, _) = selection_after_click(
            &rows,
            &visible,
            &selection,
            Some(Path::new("c.rs")),
            0,
            false,
            true,
        );
        assert_eq!(selection.len(), 1);
        assert!(selection.contains(Path::new("c.rs")));
    }

    #[test]
    fn shift_range_follows_visible_order_and_excludes_hidden_files() {
        let rows = combine_changes(
            vec![],
            vec![
                file("a.rs", FileChangeKind::Modified),
                file("hidden.rs", FileChangeKind::Modified),
                file("z.rs", FileChangeKind::Modified),
            ],
        );
        let visible = vec![2, 0];
        let (selection, _) = selection_after_click(
            &rows,
            &visible,
            &HashSet::new(),
            Some(Path::new("z.rs")),
            1,
            true,
            false,
        );
        assert_eq!(selection.len(), 2);
        assert!(!selection.contains(Path::new("hidden.rs")));
        assert!(selection.contains(Path::new("a.rs")));
        assert!(selection.contains(Path::new("z.rs")));
    }

    #[test]
    fn selected_checkbox_batches_stage_and_unstage_once() {
        let rows = combine_changes(
            vec![
                file("fully-staged.rs", FileChangeKind::Modified),
                file("staged.rs", FileChangeKind::Modified),
            ],
            vec![
                file("a.rs", FileChangeKind::Modified),
                file("b.rs", FileChangeKind::Modified),
                file("partial.rs", FileChangeKind::Modified),
                file("staged.rs", FileChangeKind::Modified),
                file("conflict.rs", FileChangeKind::Conflicted),
            ],
        );
        let visible: Vec<usize> = (0..rows.len()).collect();
        let selected: HashSet<PathBuf> = rows.iter().map(|r| r.path.clone()).collect();
        let pending = HashMap::from([(PathBuf::from("b.rs"), CheckState::Checked)]);
        assert_eq!(
            selected_staging_action(
                &rows,
                &visible,
                &selected,
                &pending,
                CheckState::Checked,
            ),
            Some(SidebarEvent::StageFiles(vec![
                "a.rs".into(),
                "partial.rs".into(),
                "staged.rs".into(),
            ]))
        );
        assert_eq!(
            selected_staging_action(
                &rows,
                &visible,
                &selected,
                &pending,
                CheckState::Unchecked,
            ),
            Some(SidebarEvent::UnstageFiles(vec![
                "fully-staged.rs".into(),
                "staged.rs".into(),
            ]))
        );
    }

    #[test]
    fn group_action_never_touches_hidden_selected_paths() {
        let rows = combine_changes(
            vec![],
            vec![
                file("hidden.rs", FileChangeKind::Modified),
                file("visible.rs", FileChangeKind::Modified),
            ],
        );
        let selected: HashSet<PathBuf> = rows.iter().map(|row| row.path.clone()).collect();
        assert_eq!(
            selected_staging_action(
                &rows,
                &[1],
                &selected,
                &HashMap::new(),
                CheckState::Checked,
            ),
            Some(SidebarEvent::StageFiles(vec!["visible.rs".into()]))
        );
    }

    #[test]
    fn optimistic_states_reconcile_only_after_index_confirms_them() {
        let rows = combine_changes(vec![], vec![file("a.rs", FileChangeKind::Modified)]);
        let mut pending = HashMap::from([(PathBuf::from("a.rs"), CheckState::Checked)]);
        assert_eq!(visual_state(&rows[0], &pending), CheckState::Checked);
        assert!(!reconcile_pending(&rows, &mut pending));
        assert_eq!(visual_header_state(&rows, &[0], &pending), CheckState::Checked);
        let confirmed = combine_changes(vec![file("a.rs", FileChangeKind::Modified)], vec![]);
        assert!(reconcile_pending(&confirmed, &mut pending));
        assert!(pending.is_empty());
    }

    #[test]
    fn optimistic_unstage_waits_for_actual_index_change() {
        let staged = combine_changes(vec![file("a.rs", FileChangeKind::Modified)], vec![]);
        let mut pending = HashMap::from([(PathBuf::from("a.rs"), CheckState::Unchecked)]);
        assert!(!reconcile_pending(&staged, &mut pending));
        assert_eq!(visual_state(&staged[0], &pending), CheckState::Unchecked);
        let unstaged = combine_changes(vec![], vec![file("a.rs", FileChangeKind::Modified)]);
        assert!(reconcile_pending(&unstaged, &mut pending));
        assert!(pending.is_empty());
    }

    #[test]
    fn filtering_bulk_selection_does_not_stage_hidden_paths() {
        let rows = combine_changes(
            vec![file("already.rs", FileChangeKind::Modified)],
            vec![
                file("visible.rs", FileChangeKind::Modified),
                file("hidden.rs", FileChangeKind::Modified),
            ],
        );
        let visible = rows
            .iter()
            .position(|row| row.path == PathBuf::from("visible.rs"))
            .unwrap();
        let (state, action) = bulk_toggle_for_visible(&rows, &[visible]);
        assert_eq!(state, CheckState::Unchecked);
        assert_eq!(action, Some(SidebarEvent::StageFiles(vec!["visible.rs".into()])));
    }

    #[test]
    fn filtered_bulk_unstage_is_limited_to_visible_staged_files() {
        let rows = combine_changes(
            vec![
                file("a.rs", FileChangeKind::Modified),
                file("b.rs", FileChangeKind::Modified),
            ],
            vec![],
        );
        let (state, action) = bulk_toggle_for_visible(&rows, &[1]);
        assert_eq!(state, CheckState::Checked);
        assert_eq!(action, Some(SidebarEvent::UnstageFiles(vec!["b.rs".into()])));
    }

    #[test]
    fn bulk_toggle_skips_conflicts_and_disables_conflict_only_lists() {
        let rows = combine_changes(
            vec![],
            vec![
                file("conflict.rs", FileChangeKind::Conflicted),
                file("normal.rs", FileChangeKind::Modified),
            ],
        );
        assert_eq!(
            bulk_toggle_for_visible(&rows, &[0]),
            (CheckState::Unchecked, None)
        );
        assert_eq!(
            bulk_toggle_for_visible(&rows, &[0, 1]).1,
            Some(SidebarEvent::StageFiles(vec!["normal.rs".into()]))
        );
    }

    #[test]
    fn flat_list_merges_partial_staging_and_sorts_by_path() {
        let rows = combine_changes(
            vec![file("z.rs", FileChangeKind::Modified)],
            vec![
                file("a.rs", FileChangeKind::Untracked),
                file("z.rs", FileChangeKind::Modified),
            ],
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, PathBuf::from("a.rs"));
        assert_eq!(rows[0].state(), CheckState::Unchecked);
        assert_eq!(rows[1].state(), CheckState::Indeterminate);
        assert_eq!(
            rows[1].toggle_event(),
            SidebarEvent::StageFile("z.rs".into())
        );
    }

    #[test]
    fn partially_staged_files_allow_staged_and_unstaged_previews() {
        let rows = combine_changes(
            vec![file("src/x.rs", FileChangeKind::Modified)],
            vec![file("src/x.rs", FileChangeKind::Modified)],
        );
        assert_eq!(
            rows[0].selected_event(),
            SidebarEvent::FileSelected {
                path: "src/x.rs".into(),
                staged: false,
            }
        );
        assert_eq!(
            rows[0].staged_preview_event(),
            Some(SidebarEvent::FileSelected {
                path: "src/x.rs".into(),
                staged: true,
            })
        );
    }

    #[test]
    fn checkbox_unstages_only_fully_staged_files() {
        let rows = combine_changes(vec![file("x.rs", FileChangeKind::Added)], vec![]);
        assert_eq!(rows[0].state(), CheckState::Checked);
        assert_eq!(
            rows[0].toggle_event(),
            SidebarEvent::UnstageFile("x.rs".into())
        );
    }

    #[test]
    fn status_and_staged_filters_preserve_partial_staging() {
        let rows = combine_changes(
            vec![file("both.rs", FileChangeKind::Modified)],
            vec![
                file("both.rs", FileChangeKind::Modified),
                file("only-new.rs", FileChangeKind::Untracked),
            ],
        );
        assert!(change_matches_filter(
            &rows[0],
            "",
            ChangeKindFilter::Modified,
            true
        ));
        assert!(!change_matches_filter(
            &rows[1],
            "",
            ChangeKindFilter::Modified,
            false
        ));
        assert!(!change_matches_filter(
            &rows[1],
            "",
            ChangeKindFilter::All,
            true
        ));
        assert_eq!(ChangeKindFilter::Other.next(), ChangeKindFilter::All);
    }

    #[test]
    fn untracked_directory_is_included_in_added_filter() {
        let placeholder = ChangeRow {
            path: PathBuf::from("generated"),
            staged: None,
            unstaged: Some(file("generated", FileChangeKind::UntrackedDirectory)),
        };
        assert!(change_matches_filter(
            &placeholder,
            "",
            ChangeKindFilter::Added,
            false
        ));
        assert!(!change_matches_filter(
            &placeholder,
            "",
            ChangeKindFilter::Other,
            false
        ));
    }

    #[test]
    fn folder_and_conflict_rows_keep_special_selection_routes() {
        let rows = combine_changes(
            vec![],
            vec![
                file("newdir", FileChangeKind::UntrackedDirectory),
                file("conflicted.rs", FileChangeKind::Conflicted),
            ],
        );
        assert_eq!(
            rows[1].selected_event(),
            SidebarEvent::ExpandUntrackedDirectory("newdir".into())
        );
        assert_eq!(
            rows[0].selected_event(),
            SidebarEvent::ConflictFileSelected("conflicted.rs".into())
        );
        assert!(rows[0].has_conflict());
    }
}
