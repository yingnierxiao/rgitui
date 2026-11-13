use gpui::prelude::*;
use gpui::{
    div, px, Context, ElementId, Entity, EventEmitter, Render, SharedString, Window,
};
use rgitui_i18n::t;
use rgitui_theme::{ActiveTheme, Color, StyledExt};
use rgitui_ui::{
    Button, ButtonSize, ButtonStyle, CheckState, Checkbox, IconName, Label, LabelSize, TextInput,
    TextInputEvent,
};

use crate::keymap;
use crate::CommandId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddSubmoduleDialogEvent {
    Add {
        url: String,
        path: String,
        branch: Option<String>,
        recursive: bool,
    },
    ChangeUrl {
        name: String,
        path: String,
        url: String,
    },
    Dismissed,
}

#[derive(Clone)]
enum SubmoduleDialogMode {
    Add,
    ChangeUrl { name: String, path: String },
}

pub struct AddSubmoduleDialog {
    url_editor: Entity<TextInput>,
    path_editor: Entity<TextInput>,
    branch_editor: Entity<TextInput>,
    recursive: bool,
    mode: SubmoduleDialogMode,
    visible: bool,
    pending_focus: bool,
    error: Option<String>,
    submitting: bool,
}

impl EventEmitter<AddSubmoduleDialogEvent> for AddSubmoduleDialog {}

impl AddSubmoduleDialog {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let url_editor = cx.new(|cx| {
            let mut editor = TextInput::new(cx);
            editor.set_placeholder("https://example.com/repository.git");
            editor
        });
        let path_editor = cx.new(|cx| {
            let mut editor = TextInput::new(cx);
            editor.set_placeholder("libs/example");
            editor
        });
        let branch_editor = cx.new(|cx| {
            let mut editor = TextInput::new(cx);
            editor.set_placeholder("main (optional)");
            editor
        });
        for editor in [&url_editor, &path_editor, &branch_editor] {
            cx.subscribe(
                editor,
                |this: &mut Self, _, event: &TextInputEvent, cx| match event {
                    TextInputEvent::Changed(_) => {
                        this.error = None;
                        cx.notify();
                    }
                    TextInputEvent::Submit => this.submit(cx),
                    _ => {}
                },
            )
            .detach();
        }
        Self {
            url_editor,
            path_editor,
            branch_editor,
            recursive: false,
            mode: SubmoduleDialogMode::Add,
            visible: false,
            pending_focus: false,
            error: None,
            submitting: false,
        }
    }

    pub fn show_visible(&mut self, cx: &mut Context<Self>) {
        self.visible = true;
        self.pending_focus = true;
        self.submitting = false;
        self.recursive = false;
        self.mode = SubmoduleDialogMode::Add;
        self.error = None;
        for editor in [&self.url_editor, &self.path_editor, &self.branch_editor] {
            editor.update(cx, |editor, cx| editor.clear(cx));
        }
        cx.notify();
    }

    pub fn show_change_url(
        &mut self,
        name: String,
        path: String,
        url: String,
        cx: &mut Context<Self>,
    ) {
        self.visible = true;
        self.pending_focus = true;
        self.submitting = false;
        self.recursive = false;
        self.mode = SubmoduleDialogMode::ChangeUrl { name, path };
        self.error = None;
        self.url_editor
            .update(cx, |editor, cx| editor.set_text(url, cx));
        self.path_editor.update(cx, |editor, cx| editor.clear(cx));
        self.branch_editor.update(cx, |editor, cx| editor.clear(cx));
        cx.notify();
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            self.visible = false;
            self.submitting = false;
            self.error = None;
            cx.emit(AddSubmoduleDialogEvent::Dismissed);
            cx.notify();
        }
    }

    pub fn on_finished(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        if !self.submitting {
            return;
        }
        self.submitting = false;
        match result {
            Ok(()) => {
                self.visible = false;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.submitting {
            return;
        }
        let url = self.url_editor.read(cx).text().trim().to_string();
        if url.is_empty() {
            self.error = Some("Source URL is required".to_string());
            cx.notify();
            return;
        }
        self.submitting = true;
        self.error = None;
        match &self.mode {
            SubmoduleDialogMode::Add => {
                let path = self.path_editor.read(cx).text().trim().to_string();
                if path.is_empty() {
                    self.submitting = false;
                    self.error = Some("Local path is required".to_string());
                    cx.notify();
                    return;
                }
                let branch = self.branch_editor.read(cx).text().trim().to_string();
                cx.emit(AddSubmoduleDialogEvent::Add {
                    url,
                    path,
                    branch: (!branch.is_empty()).then_some(branch),
                    recursive: self.recursive,
                });
            }
            SubmoduleDialogMode::ChangeUrl { name, path } => {
                cx.emit(AddSubmoduleDialogEvent::ChangeUrl {
                    name: name.clone(),
                    path: path.clone(),
                    url,
                });
            }
        }
        cx.notify();
    }

    fn dispatch_command(
        &mut self,
        command: CommandId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            CommandId::Cancel => self.hide(cx),
            _ => cx.propagate(),
        }
    }
}

impl Render for AddSubmoduleDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().id("add-submodule-dialog").into_any_element();
        }
        if self.pending_focus {
            self.pending_focus = false;
            self.url_editor
                .update(cx, |editor, cx| editor.focus(window, cx));
        }
        let colors = cx.colors().clone();
        let mut modal = div()
            .id("add-submodule-dialog-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., 0.35))
            .occlude()
            .child(
                div()
                    .id("add-submodule-dialog-content")
                    .v_flex()
                    .w(px(560.))
                    .gap(px(12.))
                    .p(px(16.))
                    .bg(colors.panel_background)
                    .border_1()
                    .border_color(colors.border_variant)
                    .rounded(px(6.))
                    .child(
                        Label::new(match self.mode {
                            SubmoduleDialogMode::Add => "添加子模块",
                            SubmoduleDialogMode::ChangeUrl { .. } => "更改子模块源地址",
                        })
                        .size(LabelSize::Large)
                        .color(Color::Default),
                    )
                    .child(self.field("源路径 / URL", self.url_editor.clone()))
                    .when(matches!(self.mode, SubmoduleDialogMode::Add), |el| {
                        el.child(self.field("本地相关路径", self.path_editor.clone()))
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(
                                        Label::new("源分支")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(div().flex_1().child(self.branch_editor.clone())),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(8.))
                                    .items_center()
                                    .child(
                                        Checkbox::new(
                                            ElementId::Name("add-submodule-recursive".into()),
                                            if self.recursive {
                                                CheckState::Checked
                                            } else {
                                                CheckState::Unchecked
                                            },
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.recursive = !this.recursive;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        Label::new("递归子模块")
                                            .size(LabelSize::Small)
                                            .color(Color::Default),
                                    ),
                            )
                    })
                    .when_some(self.error.clone(), |el, error| {
                        el.child(
                            Label::new(SharedString::from(error))
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    })
                    .child(
                        div()
                            .h_flex()
                            .justify_end()
                            .gap(px(8.))
                            .pt(px(8.))
                            .border_t_1()
                            .border_color(colors.border_variant)
                            .child(
                                Button::new("add-submodule-cancel", t!("common.cancel"))
                                    .size(ButtonSize::Default)
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, _, cx| this.hide(cx))),
                            )
                            .child(
                                Button::new(
                                    "add-submodule-submit",
                                    match self.mode {
                                        SubmoduleDialogMode::Add => "添加子模块",
                                        SubmoduleDialogMode::ChangeUrl { .. } => "更新源地址",
                                    },
                                )
                                .icon(IconName::Plus)
                                .size(ButtonSize::Default)
                                .style(ButtonStyle::Filled)
                                .disabled(self.submitting)
                                .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                            ),
                    ),
            );
        modal = modal.map(|element| {
            keymap::bind_actions(
                element,
                "AddSubmoduleDialog",
                &["Workspace", "TextInput"],
                cx,
                Self::dispatch_command,
            )
        });
        modal.into_any_element()
    }
}

impl AddSubmoduleDialog {
    fn field(&self, label: &'static str, editor: Entity<TextInput>) -> gpui::Div {
        div()
            .h_flex()
            .gap(px(8.))
            .items_center()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(div().flex_1().child(editor))
    }
}
