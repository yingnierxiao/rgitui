use std::collections::HashMap;
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
    CheckState, Checkbox, DiffStat, Icon, IconName, IconSize, Label, LabelSize,
    TextInput, TextInputEvent, Tooltip,
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

/// GitHub Desktop-style working-tree list. The repository navigation Sidebar
/// retains sole ownership of Git actions; this panel forwards SidebarEvent
/// instead of implementing a second set of Stage/Unstage handlers.
pub struct ChangesPanel {
    sidebar: WeakEntity<Sidebar>,
    rows: Arc<Vec<ChangeRow>>,
    visible: Arc<Vec<usize>>,
    filter: String,
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
            visible: Arc::new(Vec::new()),
            filter: String::new(),
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
        self.rows = Arc::new(rows);
        if self.selected_path.as_deref() == Some(path) {
            self.selected_path = None;
            self.selected_row = None;
        }
        self.rebuild_filter();
        cx.notify();
    }

    fn rebuild_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = Arc::new(
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, row)| {
                    needle.is_empty()
                        || row.path.to_string_lossy().to_lowercase().contains(&needle)
                })
                .map(|(index, _)| index)
                .collect(),
        );
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

    fn activate_selected(&self, cx: &mut App) {
        if let Some(row) = self.selected_row.and_then(|index| self.visible.get(index)) {
            self.send(self.rows[*row].selected_event(), cx);
        }
    }

    fn toggle_selected(&self, cx: &mut App) {
        if let Some(row) = self.selected_row.and_then(|index| self.visible.get(index)) {
            let row = &self.rows[*row];
            if !row.has_conflict() {
                self.send(row.toggle_event(), cx);
            }
        }
    }

    fn select_offset(&mut self, direction: isize, cx: &mut Context<Self>) {
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
        self.selected_row = Some(next);
        self.selected_path = Some(self.rows[self.visible[next]].path.clone());
        self.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
        cx.notify();
    }
}

impl Render for ChangesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.colors().clone();
        let rows = self.rows.clone();
        let visible = self.visible.clone();
        let selected_path = self.selected_path.clone();
        let sidebar = self.sidebar.clone();
        let panel = cx.weak_entity();
        let show_line_stats = cx.global::<SettingsState>().settings().show_change_line_stats;
        let staged_count = rows.iter().filter(|row| row.staged.is_some()).count();
        let all_checked =
            !rows.is_empty() && rows.iter().all(|row| row.state() == CheckState::Checked);
        let selected_some = staged_count > 0;
        let header_state = if all_checked {
            CheckState::Checked
        } else if selected_some {
            CheckState::Indeterminate
        } else {
            CheckState::Unchecked
        };
        let sidebar_header = sidebar.clone();
        let file_count = rows.len();

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
                        let is_selected = selected_path.as_ref() == Some(&row.path);
                        let check_state = row.state();
                        let disabled = row.has_conflict();
                        let select_action = row.selected_event();
                        let toggle_action = row.toggle_event();
                        let stats = row.display_status().cloned();
                        let sidebar_select = sidebar.clone();
                        let sidebar_toggle = sidebar.clone();
                        let weak_panel = panel.clone();
                        let path_selected = row.path.clone();
                        let path_tooltip = file_path.clone();
                        let row_bg = colors.ghost_element_selected;
                        let row_hover = colors.ghost_element_hover;
                        let accent = colors.text_accent;

                        div()
                            .id(ElementId::NamedInteger("changes-file-row".into(), row_number as u64))
                            .h_flex()
                            .w_full()
                            .h(px(28.))
                            .px(px(8.))
                            .gap(px(7.))
                            .items_center()
                            .border_l_2()
                            .border_color(colors.border_transparent)
                            .when(is_selected, |el| {
                                el.bg(row_bg).border_color(accent)
                            })
                            .hover(move |el| el.bg(row_hover))
                            .cursor_pointer()
                            .tooltip(Tooltip::text(path_tooltip))
                            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                weak_panel
                                    .update(cx, |this, cx| {
                                        this.selected_path = Some(path_selected.clone());
                                        this.selected_row = Some(row_number);
                                        this.focus_handle.focus(window, cx);
                                        cx.notify();
                                    })
                                    .ok();
                                sidebar_select
                                    .update(cx, |_, cx| cx.emit(select_action.clone()))
                                    .ok();
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
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    sidebar_toggle
                                        .update(cx, |_, cx| cx.emit(toggle_action.clone()))
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
                                        Label::new(parent)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                    .child(
                                        Label::new(filename)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Default)
                                            .truncate(),
                                    ),
                            )
                            .child(
                                Label::new(kind.short_code())
                                    .size(LabelSize::XSmall)
                                    .color(change_color(kind)),
                            )
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
                match event.keystroke.key.as_str() {
                    "up" | "k" => this.select_offset(-1, cx),
                    "down" | "j" => this.select_offset(1, cx),
                    "enter" => this.activate_selected(cx),
                    "space" => this.toggle_selected(cx),
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
                            .disabled(file_count == 0)
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                sidebar_header
                                    .update(cx, |_, cx| {
                                        cx.emit(if all_checked {
                                            SidebarEvent::UnstageAll
                                        } else {
                                            SidebarEvent::StageAll
                                        });
                                    })
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
    fn checkbox_unstages_only_fully_staged_files() {
        let rows = combine_changes(vec![file("x.rs", FileChangeKind::Added)], vec![]);
        assert_eq!(rows[0].state(), CheckState::Checked);
        assert_eq!(
            rows[0].toggle_event(),
            SidebarEvent::UnstageFile("x.rs".into())
        );
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
