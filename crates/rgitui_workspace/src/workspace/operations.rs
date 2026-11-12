use std::time::Instant;

use gpui::Context;
use rgitui_git::{GitOperationKind, GitOperationUpdate};

use crate::ToastKind;

use super::{Workspace, OPERATION_OUTPUT_AUTO_HIDE_SECS};

impl Workspace {
    pub(super) fn schedule_operation_output_auto_hide(
        &mut self,
        timestamp: Instant,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(
                    OPERATION_OUTPUT_AUTO_HIDE_SECS,
                ))
                .await;
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if let Some(output) = &this.operations.last_operation_output {
                        if output.timestamp == timestamp && !output.expanded && !output.is_error {
                            this.operations.last_operation_output = None;
                            cx.notify();
                        }
                    }
                })
                .ok();
            })
        })
        .detach();
    }

    pub(super) fn retry_git_operation(
        &mut self,
        update: &GitOperationUpdate,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.active_project().cloned() else {
            self.show_toast("No active repository to retry.", ToastKind::Warning, cx);
            return;
        };

        // The checkout the operation actually ran in, carried on the update
        // itself. Resolving it here from what is being inspected *now* is
        // wrong whenever the failure arrives after the user has moved on —
        // and for a network operation, whose failure is a timeout or a
        // rejected push, arriving late is the normal case. Falling back to the
        // effective path covers operations that record no worktree.
        let worktree_path = update
            .worktree_path
            .clone()
            .unwrap_or_else(|| self.effective_worktree_path(cx));

        let handled = project.update(cx, |proj, cx| match update.kind {
            GitOperationKind::Fetch => {
                if let Some(remote_name) = update.remote_name.as_deref() {
                    proj.fetch_at(remote_name, &worktree_path, cx).detach();
                    true
                } else {
                    proj.fetch_default_at(&worktree_path, cx).detach();
                    true
                }
            }
            GitOperationKind::Pull => {
                if let Some(remote_name) = update.remote_name.as_deref() {
                    proj.pull_at(remote_name, &worktree_path, cx).detach();
                    true
                } else {
                    proj.pull_default_at(&worktree_path, cx).detach();
                    true
                }
            }
            GitOperationKind::Push | GitOperationKind::ForcePush => {
                let force = update.kind == GitOperationKind::ForcePush;
                if let Some(remote_name) = update.remote_name.as_deref() {
                    proj.push_at(remote_name, force, &worktree_path, cx)
                        .detach();
                    true
                } else {
                    proj.push_default_at(force, &worktree_path, cx).detach();
                    true
                }
            }
            GitOperationKind::Checkout => {
                if let Some(branch_name) = update.branch_name.as_deref() {
                    proj.checkout_branch(branch_name, cx).detach();
                    true
                } else {
                    false
                }
            }
            GitOperationKind::Merge => {
                if let Some(branch_name) = update.branch_name.as_deref() {
                    proj.merge_branch(branch_name, cx).detach();
                    true
                } else {
                    false
                }
            }
            GitOperationKind::RemoveRemote => {
                if let Some(remote_name) = update.remote_name.as_deref() {
                    proj.remove_remote(remote_name, cx).detach();
                    true
                } else {
                    false
                }
            }
            _ => false,
        });

        if !handled {
            self.show_toast(
                "This operation cannot be retried automatically.",
                ToastKind::Info,
                cx,
            );
        } else {
            self.operations.last_failed_git_operation = None;
        }
    }
}
