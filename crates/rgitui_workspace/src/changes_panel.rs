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
use rgitui_git::{CommitFileContents, CommitSelectedFile, FileChangeKind, FileStatus};
use rgitui_i18n::{key, t, tp};
use rgitui_settings::SettingsState;
use rgitui_theme::{ActiveTheme, Color, StyledExt};
use rgitui_ui::{
    Button, ButtonSize, CheckState, Checkbox, DiffStat, Label, LabelSize, TextInput,
    TextInputEvent, Tooltip,
};

use crate::{Sidebar, SidebarEvent};

/// A virtual checkbox changed; Commit uses this count rather than Git index status.
#[derive(Debug, Clone)]
pub enum ChangesPanelEvent {
    SelectionChanged(usize),
}

impl gpui::EventEmitter<ChangesPanelEvent> for ChangesPanel {}


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

/// A checkbox is a virtual include-in-commit selection. It never writes the
/// Git index. The initial display reflects existing staging, including partial
/// staging, while subsequent choices are explicit in-memory overrides.
fn visual_state(row: &ChangeRow, overrides: &HashMap<PathBuf, CheckState>) -> CheckState {
    overrides
        .get(&row.path)
        .copied()
        .unwrap_or_else(|| row.state())
}

fn visual_header_state(
    rows: &[ChangeRow],
    visible: &[usize],
    overrides: &HashMap<PathBuf, CheckState>,
) -> CheckState {
    let mut any = false;
    let mut all = true;
    let mut found = false;
    for row in visible.iter().filter_map(|index| rows.get(*index)) {
        if row.has_conflict() {
            continue;
        }
        found = true;
        match visual_state(row, overrides) {
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

/// Resolve a click against the path captured when the element was drawn.
/// Sorting, filtering, and watcher refreshes may reorder virtual-list row
/// numbers before an already queued click callback is dispatched.
fn resolve_visible_index(
    rows: &[ChangeRow],
    visible: &[usize],
    hint: usize,
    path: &Path,
) -> Option<usize> {
    if visible
        .get(hint)
        .and_then(|index| rows.get(*index))
        .is_some_and(|row| row.path.as_path() == path)
    {
        Some(hint)
    } else {
        visible
            .iter()
            .position(|index| rows[*index].path.as_path() == path)
    }
}

/// A highlighted multi-selection is applied once. All subsequent normal
/// checkbox clicks address individual files unless the user selects a range
/// or group again.
fn checkbox_target_paths(
    rows: &[ChangeRow],
    visible: &[usize],
    selected: &HashSet<PathBuf>,
    clicked: &Path,
) -> Vec<PathBuf> {
    if selected.len() > 1 && selected.contains(clicked) {
        selected_virtual_paths(rows, visible, selected)
    } else {
        vec![clicked.to_path_buf()]
    }
}

/// Resolve only visible selected rows. No stage/unstage operation is sent.
fn selected_virtual_paths(
    rows: &[ChangeRow],
    visible: &[usize],
    selected: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    visible
        .iter()
        .filter_map(|index| rows.get(*index))
        .filter(|row| selected.contains(&row.path) && !row.has_conflict())
        .map(|row| row.path.clone())
        .collect()
}

/// Build an explicit snapshot independent of the list filter and of Git
/// index mutations. This function is pure and safe to unit test directly.
fn build_commit_selection(
    rows: &[ChangeRow],
    overrides: &HashMap<PathBuf, CheckState>,
) -> (Vec<CommitSelectedFile>, Vec<PathBuf>) {
    let mut included = Vec::new();
    let mut excluded = Vec::new();
    let mut rename_sources = Vec::new();
    for row in rows.iter().filter(|row| !row.has_conflict()) {
        let state = visual_state(row, overrides);
        let contents = match state {
            CheckState::Checked => Some(CommitFileContents::WorkingTree),
            CheckState::Indeterminate => Some(CommitFileContents::Staged),
            CheckState::Unchecked => None,
        };
        if let Some(contents) = contents {
            included.push(CommitSelectedFile {
                path: row.path.clone(),
                contents,
            });
            // A rename is a pair: the original path must be removed when
            // the new one is selected, even if status only lists its new path.
            for status in row.staged.iter().chain(row.unstaged.iter()) {
                if status.kind == FileChangeKind::Renamed {
                    if let Some(old_path) = status.old_path.as_ref() {
                        rename_sources.push(CommitSelectedFile {
                            path: old_path.clone(),
                            contents,
                        });
                    }
                }
            }
        } else {
            excluded.push(row.path.clone());
        }
    }

    for old in rename_sources {
        if !included.iter().any(|file| file.path == old.path) {
            included.push(old);
        }
    }
    // Rename sources are included even if a stale separate status entry
    // listed the old path as excluded.
    excluded.retain(|path| !included.iter().any(|file| &file.path == path));
    (included, excluded)
}

/// GitHub Desktop-style working-tree list. Row and header checkboxes are
/// virtual, so fast toggles never queue Git index operations or status scans.

pub struct ChangesPanel {
    sidebar: WeakEntity<Sidebar>,
    rows: Arc<Vec<ChangeRow>>,
    overrides: Arc<HashMap<PathBuf, CheckState>>,
    /// Keep virtual selection separate for each inspected worktree.
    worktree_overrides: HashMap<PathBuf, HashMap<PathBuf, CheckState>>,
    visible: Arc<Vec<usize>>,
    selected_paths: Arc<HashSet<PathBuf>>,
    selection_anchor: Option<PathBuf>,
    /// Cached counts avoid scanning huge working trees on every 60 FPS render.
    staged_count: usize,
    commit_in_progress: bool,
    header_state: CheckState,
    header_enabled: bool,
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
            overrides: Arc::new(HashMap::new()),
            worktree_overrides: HashMap::new(),
            visible: Arc::new(Vec::new()),
            selected_paths: Arc::new(HashSet::new()),
            selection_anchor: None,
            staged_count: 0,
            commit_in_progress: false,
            header_state: CheckState::Unchecked,
            header_enabled: false,
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
            if let Some(previous) = self.worktree_path.take() {
                self.worktree_overrides
                    .insert(previous, self.overrides.as_ref().clone());
            }
            self.overrides = Arc::new(self.worktree_overrides.remove(&path).unwrap_or_default());
            self.worktree_path = Some(path);
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
        // A repository watcher may deliver an older or intermediate status
        // snapshot while the user is clicking. Never discard virtual choices
        // based on that snapshot: absent paths are ignored by commit_selection
        // and choices are removed explicitly after a successful commit.
        if self.rows.as_ref() == &updated {
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
        cx.emit(ChangesPanelEvent::SelectionChanged(self.staged_count));
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
        cx.emit(ChangesPanelEvent::SelectionChanged(self.staged_count));
        cx.notify();
    }

    fn update_counts(&mut self, rows: &[ChangeRow]) {
        self.staged_count = rows
            .iter()
            .filter(|row| {
                !row.has_conflict()
                    && visual_state(row, &self.overrides) != CheckState::Unchecked
            })
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
                        || visual_state(row, &self.overrides) != CheckState::Unchecked)
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
        self.header_state = visual_header_state(&self.rows, &visible, &self.overrides);
        self.header_enabled = visible
            .iter()
            .any(|index| !self.rows[*index].has_conflict());
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

    /// Capture virtual choices for all changed paths. Filtering affects only
    /// the visible list and never silently excludes hidden commit selections.
    pub fn commit_selection(&self) -> (Vec<CommitSelectedFile>, Vec<PathBuf>) {
        build_commit_selection(&self.rows, &self.overrides)
    }

    pub fn selected_commit_count(&self) -> usize {
        self.staged_count
    }

    pub fn set_commit_in_progress(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.commit_in_progress != active {
            self.commit_in_progress = active;
            cx.notify();
        }
    }

    fn set_virtual_paths(
        &mut self,
        paths: &[PathBuf],
        desired: CheckState,
        cx: &mut Context<Self>,
    ) {
        if self.commit_in_progress {
            return;
        }
        let mut changed = false;
        for path in paths {
            if let Ok(index) = self.rows.binary_search_by(|row| row.path.cmp(path)) {
                let row = &self.rows[index];
                if row.has_conflict() {
                    continue;
                }
                let previous = visual_state(row, &self.overrides);
                if previous == desired {
                    continue;
                }
                if previous != CheckState::Unchecked {
                    self.staged_count = self.staged_count.saturating_sub(1);
                }
                if desired != CheckState::Unchecked {
                    self.staged_count += 1;
                }
                Arc::make_mut(&mut self.overrides).insert(path.clone(), desired);
                changed = true;
            }
        }
        if changed {
            // Filtering by included files needs a new visible list; other
            // toggles only recompute the tiny header display.
            if self.only_staged {
                self.rebuild_filter();
            } else {
                self.header_state =
                    visual_header_state(&self.rows, &self.visible, &self.overrides);
            }
            cx.emit(ChangesPanelEvent::SelectionChanged(self.staged_count));
            cx.notify();
        }
    }

    /// A commit consumes its selected paths; a failed commit keeps the user's
    /// virtual choices so they can correct the error without selecting again.
    pub fn commit_succeeded(
        &mut self,
        worktree: &Path,
        paths: &[PathBuf],
        cx: &mut Context<Self>,
    ) {
        // A background commit may finish after the user inspects another
        // worktree. Never clear that new worktree's choices by accident.
        if self.worktree_path.as_deref() != Some(worktree) {
            if let Some(saved) = self.worktree_overrides.get_mut(worktree) {
                for path in paths {
                    saved.remove(path);
                }
            }
            return;
        }
        let overrides = Arc::make_mut(&mut self.overrides);
        let mut changed = false;
        for path in paths {
            changed |= overrides.remove(path).is_some();
        }
        if changed {
            let rows = self.rows.clone();
            self.update_counts(&rows);
            self.rebuild_filter();
            cx.emit(ChangesPanelEvent::SelectionChanged(self.staged_count));
            cx.notify();
        }
    }

    fn toggle_all_visible(&mut self, cx: &mut Context<Self>) {
        let desired = if self.header_state == CheckState::Checked {
            CheckState::Unchecked
        } else {
            CheckState::Checked
        };
        let paths = self.visible
            .iter()
            .filter_map(|index| self.rows.get(*index))
            .filter(|row| !row.has_conflict())
            .map(|row| row.path.clone())
            .collect::<Vec<_>>();
        self.set_virtual_paths(&paths, desired, cx);
    }

    fn visible_index_for_path(&self, hint: usize, path: &Path) -> Option<usize> {
        resolve_visible_index(&self.rows, &self.visible, hint, path)
    }

    fn select_visible_path(
        &mut self,
        path: &Path,
        hint: usize,
        extend: bool,
        additive: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = self.visible_index_for_path(hint, path) {
            self.select_visible_row(current, extend, additive, cx);
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

    /// Clicking a checkbox within a multi-selection toggles its virtual choices.
    /// Shift-clicking a checkbox applies the choice across the visible range.
    fn toggle_checkbox(
        &mut self,
        clicked_path: &Path,
        row_hint: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        if self.commit_in_progress {
            return;
        }
        let Some(row_number) = self.visible_index_for_path(row_hint, clicked_path) else {
            // The row disappeared after an in-flight click; do not toggle a
            // different file that happened to inherit its previous row number.
            return;
        };
        let index = self.visible[row_number];
        let path = self.rows[index].path.clone();
        if extend || !self.selected_paths.contains(&path) {
            self.select_visible_row(row_number, extend, false, cx);
        }
        let row = &self.rows[index];
        if row.has_conflict() {
            return;
        }
        let target = if visual_state(row, &self.overrides) == CheckState::Checked {
            CheckState::Unchecked
        } else {
            CheckState::Checked
        };
        let paths = checkbox_target_paths(
            &self.rows,
            &self.visible,
            &self.selected_paths,
            &path,
        );
        self.set_virtual_paths(&paths, target, cx);

        // A group checkbox click is a one-shot batch action. Keeping multiple
        // rows armed makes the very next rapid click reverse the entire group
        // again, which looks like all checkboxes randomly became unchecked.
        if paths.len() > 1 {
            self.selected_paths = Arc::new(HashSet::from([path.clone()]));
            self.selection_anchor = Some(path.clone());
            self.selected_path = Some(path);
            self.selected_row = Some(row_number);
            cx.notify();
        }
    }

    fn select_all_visible(&mut self, cx: &mut Context<Self>) {
        if self.selected_row.is_none() && !self.visible.is_empty() {
            let first_path = self.rows[self.visible[0]].path.clone();
            self.selected_path = Some(first_path.clone());
            self.selected_row = Some(0);
            self.selection_anchor = Some(first_path);
        }
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
            if let Some(index) = self.visible.get(row_number) {
                let path = self.rows[*index].path.clone();
                self.toggle_checkbox(&path, row_number, false, cx);
            }
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
        let overrides = self.overrides.clone();
        let visible = self.visible.clone();
        let selected_paths = self.selected_paths.clone();
        let sidebar = self.sidebar.clone();
        let panel = cx.weak_entity();
        let show_line_stats = cx.global::<SettingsState>().settings().show_change_line_stats;
        let staged_count = self.staged_count;
        let header_state = self.header_state;
        let has_selectable_rows = self.header_enabled && !self.commit_in_progress;
        let is_committing = self.commit_in_progress;
        let panel_header = panel.clone();
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
                        let check_state = visual_state(row, &overrides);
                        let disabled = row.has_conflict() || is_committing;
                        let select_action = row.selected_event();
                        let staged_preview = row.staged_preview_event();
                        let stats = row.display_status().cloned();
                        let sidebar_select = sidebar.clone();
                        let weak_panel_toggle = panel.clone();
                        let sidebar_preview = sidebar.clone();
                        let weak_panel = panel.clone();
                        let path_tooltip = file_path.clone();
                        let row_click_path = row.path.clone();
                        let checkbox_click_path = row.path.clone();
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
                            .on_click(move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let modifiers = event.modifiers();
                                weak_panel
                                    .update(cx, |this, cx| {
                                        this.select_visible_path(
                                            &row_click_path,
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
                                            this.toggle_checkbox(
                                                &checkbox_click_path,
                                                row_number,
                                                extend,
                                                cx,
                                            );
                                        })
                                        .ok();
                                }),
                            )
                            .child(
                                div()
                                    // GPUI's tooltip is available on a
                                    // stateful (ID-bearing) interactive element.
                                    // Keep this ID distinct from the file row
                                    // so hovering the checkbox cannot open it.
                                    .id(ElementId::NamedInteger(
                                        "changes-file-tooltip".into(),
                                        row_number as u64,
                                    ))
                                    .h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(4.))
                                    .overflow_hidden()
                                    // The checkbox must never be inside a
                                    // tooltip hit area: rapid pointer clicks
                                    // should only target the checkbox itself.
                                    .tooltip(Tooltip::text(path_tooltip))
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
                            .disabled(!has_selectable_rows)
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                panel_header
                                    .update(cx, |this, cx| this.toggle_all_visible(cx))
                                    .ok();
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
    fn rapid_checkbox_clicks_resolve_original_file_after_visible_reorder() {
        let rows = combine_changes(
            vec![],
            vec![
                file("a.rs", FileChangeKind::Modified),
                file("b.rs", FileChangeKind::Modified),
                file("c.rs", FileChangeKind::Modified),
            ],
        );
        let original = vec![0, 1, 2];
        assert_eq!(
            resolve_visible_index(&rows, &original, 0, Path::new("a.rs")),
            Some(0)
        );
        // A queued click for a.rs must not accidentally toggle c.rs after
        // the list is sorted differently or another row gets filtered out.
        let reordered = vec![2, 0, 1];
        assert_eq!(
            resolve_visible_index(&rows, &reordered, 0, Path::new("a.rs")),
            Some(1)
        );
        assert_eq!(
            resolve_visible_index(&rows, &[2, 1], 0, Path::new("a.rs")),
            None
        );
    }

    #[test]
    fn rapid_checkbox_click_after_batch_only_targets_clicked_file() {
        let rows = combine_changes(
            vec![],
            vec![
                file("a.rs", FileChangeKind::Modified),
                file("b.rs", FileChangeKind::Modified),
                file("c.rs", FileChangeKind::Modified),
            ],
        );
        let visible = vec![0, 1, 2];
        let multi = HashSet::from([PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
        let first = checkbox_target_paths(&rows, &visible, &multi, Path::new("a.rs"));
        assert_eq!(first, vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
        // The checkbox handler collapses selection to the clicked file after
        // batching. The next quick click cannot reverse all files at once.
        let single = HashSet::from([PathBuf::from("a.rs")]);
        let second = checkbox_target_paths(&rows, &visible, &single, Path::new("b.rs"));
        assert_eq!(second, vec![PathBuf::from("b.rs")]);
        let third = checkbox_target_paths(&rows, &visible, &single, Path::new("a.rs"));
        assert_eq!(third, vec![PathBuf::from("a.rs")]);
    }

    #[test]
    fn selected_virtual_paths_include_only_visible_nonconflicting_rows() {
        let rows = combine_changes(
            vec![],
            vec![
                file("a.rs", FileChangeKind::Modified),
                file("b.rs", FileChangeKind::Modified),
                file("conflict.rs", FileChangeKind::Conflicted),
                file("hidden.rs", FileChangeKind::Modified),
            ],
        );
        let selected: HashSet<PathBuf> = rows.iter().map(|row| row.path.clone()).collect();
        assert_eq!(
            selected_virtual_paths(&rows, &[0, 1, 2], &selected),
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
    }

    #[test]
    fn virtual_choices_survive_git_status_snapshots() {
        let unstaged = combine_changes(vec![], vec![file("a.rs", FileChangeKind::Modified)]);
        let overrides = HashMap::from([(PathBuf::from("a.rs"), CheckState::Checked)]);
        assert_eq!(visual_state(&unstaged[0], &overrides), CheckState::Checked);
        let staged = combine_changes(vec![file("a.rs", FileChangeKind::Modified)], vec![]);
        assert_eq!(visual_state(&staged[0], &overrides), CheckState::Checked);
        let unchecked =
            HashMap::from([(PathBuf::from("a.rs"), CheckState::Unchecked)]);
        assert_eq!(visual_state(&staged[0], &unchecked), CheckState::Unchecked);
        assert_eq!(
            visual_header_state(&staged, &[0], &unchecked),
            CheckState::Unchecked
        );
    }

    #[test]
    fn initial_partial_staging_remains_indeterminate() {
        let rows = combine_changes(
            vec![file("partial.rs", FileChangeKind::Modified)],
            vec![file("partial.rs", FileChangeKind::Modified)],
        );
        assert_eq!(
            visual_state(&rows[0], &HashMap::new()),
            CheckState::Indeterminate
        );
        let overrides =
            HashMap::from([(PathBuf::from("partial.rs"), CheckState::Checked)]);
        assert_eq!(visual_state(&rows[0], &overrides), CheckState::Checked);
    }

    #[test]
    fn commit_snapshot_includes_only_virtual_choices_and_rename_sources() {
        let mut renamed = file("new.rs", FileChangeKind::Renamed);
        renamed.old_path = Some(PathBuf::from("old.rs"));
        let rows = combine_changes(
            vec![file("staged.rs", FileChangeKind::Modified)],
            vec![
                renamed,
                file("skipped.rs", FileChangeKind::Modified),
                file("partial.rs", FileChangeKind::Modified),
            ],
        );
        let overrides = HashMap::from([
            (PathBuf::from("new.rs"), CheckState::Checked),
            (PathBuf::from("partial.rs"), CheckState::Checked),
            (PathBuf::from("staged.rs"), CheckState::Unchecked),
        ]);
        let (included, excluded) = build_commit_selection(&rows, &overrides);
        assert!(included.iter().any(|f| {
            f.path.as_path() == Path::new("new.rs") && f.contents == CommitFileContents::WorkingTree
        }));
        assert!(included.iter().any(|f| {
            f.path.as_path() == Path::new("old.rs") && f.contents == CommitFileContents::WorkingTree
        }));
        assert!(included.iter().any(|f| f.path.as_path() == Path::new("partial.rs")));
        assert!(!included.iter().any(|f| f.path.as_path() == Path::new("staged.rs")));
        assert!(excluded.contains(&PathBuf::from("skipped.rs")));
        assert!(excluded.contains(&PathBuf::from("staged.rs")));
    }

    #[test]
    fn virtual_header_respects_visible_rows_and_ignores_conflicts() {
        let rows = combine_changes(
            vec![],
            vec![
                file("conflict.rs", FileChangeKind::Conflicted),
                file("hidden.rs", FileChangeKind::Modified),
                file("visible.rs", FileChangeKind::Modified),
            ],
        );
        let mut overrides = HashMap::new();
        overrides.insert(PathBuf::from("visible.rs"), CheckState::Checked);
        assert_eq!(
            visual_header_state(&rows, &[2], &overrides),
            CheckState::Checked
        );
        assert_eq!(
            visual_header_state(&rows, &[0], &overrides),
            CheckState::Unchecked
        );
        assert_eq!(
            visual_header_state(&rows, &[0, 1, 2], &overrides),
            CheckState::Indeterminate
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
