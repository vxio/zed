use crate::{
    branch_picker, conflict_view,
    git_panel::{GitPanel, GitPanelAddon, GitStatusEntry},
    git_panel_settings::GitPanelSettings,
    picker_prompt,
};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, anyhow};
use buffer_diff::{BufferDiff, DiffHunkSecondaryStatus};
use collections::{HashMap, HashSet};
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, OrphanedReviewCommentSummary,
    RestoreOnlyDiffHunkDelegate, SelectionEffects, SplittableEditor,
    actions::{GoToHunk, GoToPreviousHunk, SendReviewToAgent},
    multibuffer_context_lines,
    scroll::Autoscroll,
};
use futures_lite::future::yield_now;
use git::repository::DiffType;

use git::{
    Commit, StageAll, StageAndNext, ToggleStaged, UnstageAll, UnstageAndNext, repository::RepoPath,
    status::FileStatus,
};
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, ClipboardItem, Entity,
    EventEmitter, FocusHandle, Focusable, PromptLevel, Render, Subscription, Task, WeakEntity,
    actions,
};
use language::{Anchor, Buffer, BufferId, Capability, OffsetRangeExt};
use multi_buffer::{MultiBuffer, PathKey};
use project::{
    ConflictSet, Project, ProjectPath,
    git_store::{
        Repository,
        branch_diff::{self, BranchDiffEvent, DiffBase},
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{
    GitDiffBaseSetting, GitPanelGroupBy, GitPanelSortBy, Settings, SettingsStore,
};
use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use theme::ActiveTheme;
use ui::{
    CommonAnimationExt as _, DiffStat, Divider, KeyBinding, PopoverMenu, Tooltip, prelude::*,
};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    CloseActiveItem, ItemNavHistory, SerializableItem, Toast, ToolbarItemEvent,
    ToolbarItemLocation, ToolbarItemView, Workspace,
    item::{Item, ItemEvent, ItemHandle, SaveOptions},
    notifications::{NotificationId, NotifyTaskExt},
    searchable::SearchableItemHandle,
};
use zed_actions::agent::ReviewBranchDiff;
use ztracing::instrument;

actions!(
    git,
    [
        /// Shows the diff between the working directory and the index.
        Diff,
        /// Toggles the git diff base between HEAD and the default branch.
        ToggleDiffBase,
        /// Adds files to the git staging area.
        Add,
        /// Shows the diff between the working directory and your default
        /// branch (typically main or master).
        BranchDiff,
        /// Opens a new agent thread with the branch diff for review.
        ReviewDiff,
        /// Refreshes the active diff view.
        RefreshDiff,
        LeaderAndFollower,
        /// Compare with a specific branch
        CompareWithBranch,
    ]
);

#[derive(PartialEq, Clone, Deserialize, Default, JsonSchema, Action)]
#[action(namespace = git, name = "DiffBranch")]
pub(crate) struct DeployBranchDiff;

struct BufferSubscriptions {
    _diff: Entity<BufferDiff>,
    _diff_subscription: Subscription,
    _conflict_set: Entity<ConflictSet>,
    _conflict_set_subscription: Subscription,
}

actions!(
    zed_review,
    [
        /// Archives all review comments and clears the current diff.
        ArchiveReviewComments,
        /// Archives user-authored review comments and clears them from the current diff.
        ArchiveUserReviewComments,
        /// Archives agent-authored review comments and clears them from the current diff.
        ArchiveAgentReviewComments,
        /// Archives review comments for a selected review round and clears them from the current diff.
        ArchiveReviewRoundComments,
        /// Deletes all review comments after confirmation by archiving them.
        DeleteReviewComments,
        /// Restores the newest archived review comment session.
        RestoreLatestReviewComments,
        /// Restores the newest archived user-authored review comment session.
        RestoreLatestUserReviewComments,
        /// Restores the newest archived agent-authored review comment session.
        RestoreLatestAgentReviewComments,
        /// Restores archived review comments for a selected review round.
        RestoreReviewRoundComments,
        /// Copies an Amp prompt for addressing unstamped review comments.
        CopyAmpPromptForUnstampedReviewComments,
    ]
);

pub struct ProjectDiff {
    project: Entity<Project>,
    multibuffer: Entity<MultiBuffer>,
    branch_diff: Entity<branch_diff::BranchDiff>,
    editor: Entity<SplittableEditor>,
    buffer_subscriptions: HashMap<Arc<RelPath>, BufferSubscriptions>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    pending_scroll: Option<PathKey>,
    review_comment_count: usize,
    loaded_review_comments_key: Option<String>,
    loaded_review_comments_json: Option<String>,
    last_observed_review_comments_db_json: Option<String>,
    last_review_comments_db_modified_on: Option<SystemTime>,
    restoring_review_comments: bool,
    review_comments_fully_restored: bool,
    copy_amp_prompt_in_progress: bool,
    orphaned_review_comments_expanded: bool,
    follows_default_branch: bool,
    _task: Task<Result<()>>,
    _review_comments_poll_task: Task<()>,
    _subscription: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshReason {
    DiffChanged,
    StatusesChanged,
    EditorSaved,
}

#[derive(Clone, Copy)]
enum ReviewCommentArchiveScope {
    All,
    User,
    Agent,
    Deleted,
    Round(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreLatestReviewCommentsResult {
    Restored,
    NoArchivedComments,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyAmpPromptResult {
    Copied,
    Saving,
    AlreadyInProgress,
    Failed(&'static str),
}

fn restore_latest_review_comments_notification_id() -> NotificationId {
    NotificationId::named("zed-review-restore-latest".into())
}

fn copy_amp_prompt_notification_id() -> NotificationId {
    NotificationId::named("zed-review-copy-amp-prompt".into())
}

impl ReviewCommentArchiveScope {
    fn archive_scope(self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::User => "user".to_string(),
            Self::Agent => "agent".to_string(),
            Self::Deleted => "deleted".to_string(),
            Self::Round(round) => format!("round:{round}"),
        }
    }

    fn includes(self, comment: &serde_json::Value) -> bool {
        match self {
            Self::All | Self::Deleted => true,
            Self::User => comment_author(comment) == "vxio",
            Self::Agent => comment_author(comment) == "amp",
            Self::Round(round) => review_comment_contains_round(comment, round),
        }
    }
}

fn comment_author(comment: &serde_json::Value) -> &str {
    comment
        .get("author")
        .and_then(|author| author.as_str())
        .unwrap_or("vxio")
}

fn review_comment_contains_round(comment: &serde_json::Value, round: u64) -> bool {
    comment_review_round(comment) == Some(round)
        || comment
            .get("replies")
            .and_then(|replies| replies.as_array())
            .is_some_and(|replies| {
                replies
                    .iter()
                    .any(|reply| comment_review_round(reply) == Some(round))
            })
}

fn comment_review_round(comment: &serde_json::Value) -> Option<u64> {
    comment.get("review_round").and_then(|round| round.as_u64())
}

impl ProjectDiff {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(Self::deploy);
        workspace.register_action(Self::deploy_branch_diff);
        workspace.register_action(Self::compare_with_branch);
        workspace.register_action(Self::refresh_active_diff);
        workspace.register_action(|workspace, _: &ArchiveReviewComments, window, cx| {
            Self::archive_active_review_comments(
                workspace,
                ReviewCommentArchiveScope::All,
                window,
                cx,
            );
        });
        workspace.register_action(|workspace, _: &ArchiveUserReviewComments, window, cx| {
            Self::archive_active_review_comments(
                workspace,
                ReviewCommentArchiveScope::User,
                window,
                cx,
            );
        });
        workspace.register_action(|workspace, _: &ArchiveAgentReviewComments, window, cx| {
            Self::archive_active_review_comments(
                workspace,
                ReviewCommentArchiveScope::Agent,
                window,
                cx,
            );
        });
        workspace.register_action(|workspace, _: &ArchiveReviewRoundComments, window, cx| {
            Self::archive_active_review_round_comments(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &DeleteReviewComments, window, cx| {
            Self::delete_active_review_comments(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &RestoreLatestReviewComments, window, cx| {
            Self::restore_latest_active_review_comments(workspace, None, window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleDiffBase, _window, cx| {
            settings::update_settings_file(
                workspace.app_state().fs.clone(),
                cx,
                move |settings, _| {
                    let git = settings.git.get_or_insert_default();
                    git.diff_base = Some(match git.diff_base.unwrap_or_default() {
                        GitDiffBaseSetting::Head => GitDiffBaseSetting::DefaultBranch,
                        GitDiffBaseSetting::DefaultBranch => GitDiffBaseSetting::Head,
                    });
                },
            );
        });
        workspace.register_action(
            |workspace, _: &RestoreLatestUserReviewComments, window, cx| {
                Self::restore_latest_active_review_comments(
                    workspace,
                    Some(ReviewCommentArchiveScope::User),
                    window,
                    cx,
                );
            },
        );
        workspace.register_action(
            |workspace, _: &RestoreLatestAgentReviewComments, window, cx| {
                Self::restore_latest_active_review_comments(
                    workspace,
                    Some(ReviewCommentArchiveScope::Agent),
                    window,
                    cx,
                );
            },
        );
        workspace.register_action(|workspace, _: &RestoreReviewRoundComments, window, cx| {
            Self::restore_active_review_round_comments(workspace, window, cx);
        });
        workspace.register_action(
            |workspace, _: &CopyAmpPromptForUnstampedReviewComments, window, cx| {
                Self::copy_active_amp_prompt_for_unstamped_review_comments(workspace, window, cx);
            },
        );
        workspace.register_action(|workspace, _: &Add, window, cx| {
            Self::deploy(workspace, &Diff, window, cx);
        });
        workspace::register_serializable_item::<ProjectDiff>(cx);
    }

    fn refresh_active_diff(
        workspace: &mut Workspace,
        _: &RefreshDiff,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
            return;
        };
        project_diff.update(cx, |project_diff, cx| {
            project_diff.refresh_diff(window, cx);
        });
    }

    fn archive_active_review_comments(
        workspace: &mut Workspace,
        scope: ReviewCommentArchiveScope,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(workspace_id) = workspace.database_id() else {
            return;
        };
        let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
            return;
        };
        project_diff.update(cx, |project_diff, cx| {
            project_diff.archive_review_comments(workspace_id, scope, window, cx);
        });
    }

    fn archive_active_review_round_comments(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(workspace_id) = workspace.database_id() else {
            return;
        };
        let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
            return;
        };
        let rounds = project_diff.update(cx, |project_diff, cx| {
            project_diff.active_review_comment_rounds(cx)
        });
        Self::prompt_for_review_round(
            workspace,
            rounds,
            "Archive review round",
            "No review rounds found in the current diff.",
            window,
            cx,
            move |round, window, cx| {
                project_diff.update(cx, |project_diff, cx| {
                    project_diff.archive_review_comments(
                        workspace_id,
                        ReviewCommentArchiveScope::Round(round),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn restore_latest_active_review_comments(
        workspace: &mut Workspace,
        scope: Option<ReviewCommentArchiveScope>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(workspace_id) = workspace.database_id() else {
            return;
        };
        let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
            return;
        };
        let result = project_diff.update(cx, |project_diff, cx| {
            project_diff.restore_latest_review_comments(workspace_id, scope, window, cx)
        });
        match result {
            RestoreLatestReviewCommentsResult::NoArchivedComments => {
                workspace.show_toast(
                    Toast::new(
                        restore_latest_review_comments_notification_id(),
                        "No archived review comments found for this diff.",
                    )
                    .autohide(),
                    cx,
                );
            }
            RestoreLatestReviewCommentsResult::Failed
            | RestoreLatestReviewCommentsResult::Restored => {}
        }
    }

    fn restore_active_review_round_comments(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(workspace_id) = workspace.database_id() else {
            return;
        };
        let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
            return;
        };
        let rounds = project_diff.update(cx, |project_diff, cx| {
            project_diff.archived_review_comment_rounds(workspace_id, cx)
        });
        Self::prompt_for_review_round(
            workspace,
            rounds,
            "Restore review round",
            "No archived review rounds found for this diff.",
            window,
            cx,
            move |round, window, cx| {
                let result = project_diff.update(cx, |project_diff, cx| {
                    project_diff.restore_latest_review_comments(
                        workspace_id,
                        Some(ReviewCommentArchiveScope::Round(round)),
                        window,
                        cx,
                    )
                });
                if result == RestoreLatestReviewCommentsResult::NoArchivedComments {
                    log::warn!("no archived Review #{round} comments found for this diff");
                }
            },
        );
    }

    fn prompt_for_review_round(
        workspace: &mut Workspace,
        rounds: Vec<u64>,
        title: &'static str,
        empty_message: &'static str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        on_select: impl FnOnce(u64, &mut Window, &mut App) + 'static,
    ) {
        if rounds.is_empty() {
            workspace.show_toast(
                Toast::new(
                    restore_latest_review_comments_notification_id(),
                    empty_message,
                )
                .autohide(),
                cx,
            );
            return;
        }

        let options = rounds
            .iter()
            .map(|round| format!("Review #{round}"))
            .map(SharedString::from)
            .collect::<Vec<_>>();

        let prompt = picker_prompt::prompt(title, options, workspace.weak_handle(), window, cx);
        cx.spawn_in(window, async move |_workspace, cx| {
            let Some(selection) = prompt.await else {
                return;
            };
            if let Some(round) = rounds.get(selection).copied() {
                cx.update(|window, cx| on_select(round, window, cx)).ok();
            }
        })
        .detach();
    }

    fn copy_active_amp_prompt_for_unstamped_review_comments(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(workspace_id) = workspace.database_id() else {
            Self::show_copy_amp_prompt_result(
                workspace,
                CopyAmpPromptResult::Failed("This workspace cannot save review comments."),
                cx,
            );
            return;
        };
        let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
            return;
        };
        let result = project_diff.update(cx, |project_diff, cx| {
            project_diff.copy_amp_prompt_for_unstamped_review_comments(workspace_id, window, cx)
        });

        Self::show_copy_amp_prompt_result(workspace, result, cx);
    }

    fn show_copy_amp_prompt_result(
        workspace: &mut Workspace,
        result: CopyAmpPromptResult,
        cx: &mut Context<Workspace>,
    ) {
        match result {
            CopyAmpPromptResult::Copied => {
                workspace.show_toast(
                    Toast::new(
                        copy_amp_prompt_notification_id(),
                        "Copied Amp prompt to clipboard.",
                    )
                    .autohide(),
                    cx,
                );
            }
            CopyAmpPromptResult::Saving => {
                workspace.show_toast(
                    Toast::new(
                        copy_amp_prompt_notification_id(),
                        "Saving review comments before copying Amp prompt…",
                    ),
                    cx,
                );
            }
            CopyAmpPromptResult::AlreadyInProgress => {
                workspace.show_toast(
                    Toast::new(
                        copy_amp_prompt_notification_id(),
                        "Still generating Amp prompt…",
                    ),
                    cx,
                );
            }
            CopyAmpPromptResult::Failed(message) => {
                workspace.show_toast(
                    Toast::new(copy_amp_prompt_notification_id(), message).autohide(),
                    cx,
                );
            }
        }
    }

    fn delete_active_review_comments(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(workspace_id) = workspace.database_id() else {
            return;
        };
        let Some(project_diff) = workspace.active_item_as::<ProjectDiff>(cx) else {
            return;
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            "Delete all review comments?",
            Some("Deleted comments cannot be restored."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |_workspace, cx| {
            if prompt.await == Ok(0) {
                project_diff
                    .update_in(cx, |project_diff, window, cx| {
                        project_diff.archive_review_comments(
                            workspace_id,
                            ReviewCommentArchiveScope::Deleted,
                            window,
                            cx,
                        );
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn deploy(
        workspace: &mut Workspace,
        _: &Diff,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::deploy_at(workspace, None, window, cx)
    }

    fn deploy_branch_diff(
        workspace: &mut Workspace,
        _: &BranchDiff,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        telemetry::event!("Git Branch Diff Opened");
        let project = workspace.project().clone();
        let Some(intended_repo) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("No active repository"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };

        let default_branch = intended_repo.update(cx, |repo, _| repo.default_branch(true));
        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let base_ref = default_branch
                    .await??
                    .context("Could not determine default branch")?;

                workspace.update_in(cx, |workspace, window, cx| {
                    Self::deploy_branch_diff_with_base_ref(
                        workspace,
                        project,
                        intended_repo,
                        base_ref,
                        true,
                        window,
                        cx,
                    );
                })?;

                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    fn compare_with_branch(
        workspace: &mut Workspace,
        _: &CompareWithBranch,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("No active repository"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };
        let selected_branch = workspace.active_item_as::<Self>(cx).and_then(|item| {
            match item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => Some(base_ref.clone()),
                DiffBase::Head => None,
            }
        });
        let workspace_handle = workspace.weak_handle();
        let on_select = Arc::new({
            let repository = repository.clone();
            let workspace = workspace_handle.clone();
            move |branch: git::repository::Branch, window: &mut Window, cx: &mut App| {
                let base_ref: SharedString = branch.name().to_owned().into();
                workspace
                    .update(cx, |workspace, cx| {
                        Self::deploy_branch_diff_with_base_ref(
                            workspace,
                            project.clone(),
                            repository.clone(),
                            base_ref,
                            false,
                            window,
                            cx,
                        );
                    })
                    .ok();
            }
        });

        workspace.toggle_modal(window, cx, |window, cx| {
            branch_picker::select_modal(
                workspace_handle,
                Some(repository),
                selected_branch,
                on_select,
                window,
                cx,
            )
        });
    }

    fn deploy_branch_diff_with_base_ref(
        workspace: &mut Workspace,
        project: Entity<Project>,
        intended_repo: Entity<Repository>,
        base_ref: SharedString,
        follows_default_branch: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace.items_of_type::<Self>(cx).find(|item| {
            let item = item.read(cx);
            matches!(
                item.diff_base(cx),
                DiffBase::Merge { base_ref: existing_base_ref } if existing_base_ref == &base_ref
            )
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);

            let needs_switch = existing
                .read(cx)
                .branch_diff
                .read(cx)
                .repo()
                .map_or(true, |current| {
                    current.read(cx).id != intended_repo.read(cx).id
                });

            if needs_switch {
                existing.update(cx, |project_diff, cx| {
                    project_diff.follows_default_branch = follows_default_branch;
                    project_diff.branch_diff.update(cx, |branch_diff, cx| {
                        branch_diff.set_repo(Some(intended_repo), cx);
                    });
                });
            } else {
                existing.update(cx, |project_diff, cx| {
                    project_diff.follows_default_branch = follows_default_branch;
                    cx.notify();
                });
            }

            return;
        }

        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let this = cx
                    .update(|window, cx| {
                        Self::new_with_branch_base(
                            project,
                            workspace.clone(),
                            base_ref,
                            intended_repo,
                            follows_default_branch,
                            window,
                            cx,
                        )
                    })?
                    .await?;
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(Box::new(this), None, true, window, cx);
                    })
                    .ok();
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    fn review_diff(&mut self, _: &ReviewDiff, window: &mut Window, cx: &mut Context<Self>) {
        let diff_base = self.diff_base(cx).clone();
        let DiffBase::Merge { base_ref } = diff_base else {
            return;
        };

        let Some(repo) = self.branch_diff.read(cx).repo().cloned() else {
            return;
        };

        let diff_receiver = repo.update(cx, |repo, cx| {
            repo.diff(
                DiffType::MergeBase {
                    base_ref: base_ref.clone(),
                },
                cx,
            )
        });

        let workspace = self.workspace.clone();

        window
            .spawn(cx, {
                let workspace = workspace.clone();
                async move |cx| {
                    let diff_text = diff_receiver.await??;

                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update_in(cx, |_workspace, window, cx| {
                            window.dispatch_action(
                                ReviewBranchDiff {
                                    diff_text: diff_text.into(),
                                    base_ref,
                                }
                                .boxed_clone(),
                                cx,
                            );
                        })?;
                    }

                    anyhow::Ok(())
                }
            })
            .detach_and_notify_err(workspace, window, cx);
    }

    pub fn deploy_at(
        workspace: &mut Workspace,
        entry: Option<GitStatusEntry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        telemetry::event!(
            "Git Diff Opened",
            source = if entry.is_some() {
                "Git Panel"
            } else {
                "Action"
            }
        );
        let intended_repo = workspace.project().read(cx).active_repository(cx);

        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| matches!(item.read(cx).diff_base(cx), DiffBase::Head));
        let project_diff = if let Some(existing) = existing {
            existing.update(cx, |project_diff, cx| {
                project_diff.move_to_beginning(window, cx);
            });

            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let workspace_handle = cx.entity();
            let project_diff =
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(
                Box::new(project_diff.clone()),
                None,
                true,
                window,
                cx,
            );
            project_diff
        };

        if let Some(intended) = &intended_repo {
            let needs_switch = project_diff
                .read(cx)
                .branch_diff
                .read(cx)
                .repo()
                .map_or(true, |current| current.read(cx).id != intended.read(cx).id);
            if needs_switch {
                project_diff.update(cx, |project_diff, cx| {
                    project_diff.branch_diff.update(cx, |branch_diff, cx| {
                        branch_diff.set_repo(Some(intended.clone()), cx);
                    });
                });
            }
        }

        if let Some(entry) = entry {
            project_diff.update(cx, |project_diff, cx| {
                project_diff.move_to_entry(entry, window, cx);
            })
        }
    }

    pub fn deploy_at_project_path(
        workspace: &mut Workspace,
        project_path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        telemetry::event!("Git Diff Opened", source = "Agent Panel");
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| matches!(item.read(cx).diff_base(cx), DiffBase::Head));
        let project_diff = if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let workspace_handle = cx.entity();
            let project_diff =
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(
                Box::new(project_diff.clone()),
                None,
                true,
                window,
                cx,
            );
            project_diff
        };
        project_diff.update(cx, |project_diff, cx| {
            project_diff.move_to_project_path(&project_path, window, cx);
        });
    }

    pub fn autoscroll(&self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.request_autoscroll(Autoscroll::fit(), cx);
            })
        })
    }

    fn review_comments_key(&self, cx: &App) -> Option<String> {
        let branch_key = self.review_comments_branch_key(cx)?;
        Some(format!(
            "{branch_key}\n{}",
            self.review_comments_view_suffix(cx)
        ))
    }

    fn review_comments_branch_key(&self, cx: &App) -> Option<String> {
        let repo = self.branch_diff.read(cx).repo()?.read(cx);
        let review_ref = repo
            .branch
            .as_ref()
            .or_else(|| repo.branch_list.iter().find(|branch| branch.is_head))
            .map(|branch| format!("branch:{}", branch.ref_name))
            .or_else(|| {
                repo.head_commit
                    .as_ref()
                    .map(|commit| format!("detached:{}", commit.sha))
            })
            .unwrap_or_else(|| "detached".to_string());
        Some(format!(
            "{}\n{}",
            repo.work_directory_abs_path.display(),
            review_ref
        ))
    }

    fn review_comments_view_suffix(&self, cx: &App) -> String {
        match self.diff_base(cx) {
            DiffBase::Head => "uncommitted".to_string(),
            DiffBase::Merge { base_ref } => format!("since:{base_ref}"),
        }
    }

    // The uncommitted and branch-diff views used to share a single branch-keyed
    // bucket. Only the branch-diff view inherits those older comments so they
    // don't get duplicated into both views after the split.
    fn legacy_review_comments_keys(&self, cx: &App) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(repo) = self.branch_diff.read(cx).repo() {
            let repo_path = repo.read(cx).work_directory_abs_path.display().to_string();
            if let Some(diff_base) = serde_json::to_string(self.diff_base(cx)).log_err() {
                keys.push(format!("{repo_path}\n{diff_base}"));
            }
        }
        if matches!(self.diff_base(cx), DiffBase::Merge { .. }) {
            if let Some(branch_key) = self.review_comments_branch_key(cx) {
                keys.push(branch_key);
            }
        }
        keys
    }

    fn persist_review_comments(&self, comments_json: String, cx: &mut Context<Self>) {
        let Some(workspace_id) = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).database_id())
        else {
            return;
        };
        let Some(review_key) = self.review_comments_key(cx) else {
            return;
        };

        let db = persistence::ProjectDiffDb::global(cx);
        let save = db.save_review_comments(workspace_id, review_key, comments_json);
        cx.background_spawn(save).detach_and_log_err(cx);
    }

    fn copy_amp_prompt_for_unstamped_review_comments(
        &mut self,
        workspace_id: workspace::WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CopyAmpPromptResult {
        let Some(review_key) = self.review_comments_key(cx) else {
            return CopyAmpPromptResult::Failed("No review view is available to copy.");
        };
        let Some(repo) = review_key
            .lines()
            .next()
            .filter(|repo| !repo.is_empty())
            .map(ToOwned::to_owned)
        else {
            return CopyAmpPromptResult::Failed("No repository is available to copy a prompt for.");
        };
        let Ok(comments_json) = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments_json(cx)
        else {
            return CopyAmpPromptResult::Failed("Failed to read the current review comments.");
        };

        if self.copy_amp_prompt_in_progress {
            return CopyAmpPromptResult::AlreadyInProgress;
        }

        if stamp_unstamped_review_comments(&comments_json).is_err() {
            return CopyAmpPromptResult::Failed(
                "No unstamped review comments are available to copy.",
            );
        }

        self.copy_amp_prompt_in_progress = true;
        cx.notify();

        let db = persistence::ProjectDiffDb::global(cx);
        let original_comments_json = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments_json(cx)
            .unwrap_or_default();
        let workspace = self.workspace.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
                let result = async {
                    let (persisted_comments_json, review_round) = db
                        .save_and_stamp_review_comments(
                            workspace_id,
                            review_key.clone(),
                            comments_json,
                        )
                        .await
                        .context("saving stamped review comments before copying Amp prompt")?;
                    let prompt = amp_prompt_for_review_round(
                        repo,
                        review_key,
                        &persisted_comments_json,
                        review_round,
                    )?;
                    cx.update_window(window_handle, |_, window, cx| {
                        this.update(cx, |this, cx| {
                            let editor = this.editor.read(cx).rhs_editor().clone();
                            if editor.read(cx).has_active_diff_review_input() {
                                return;
                            }
                            let restored_comments_json = editor
                                .read(cx)
                                .review_comments_json(cx)
                                .and_then(|live_comments_json| {
                                    preserve_live_review_comment_locations(
                                        &original_comments_json,
                                        &live_comments_json,
                                        &persisted_comments_json,
                                    )
                                })
                                .map(|comments_json| {
                                    comments_json
                                        .unwrap_or_else(|| persisted_comments_json.clone())
                                });
                            let restored_comments_json = match restored_comments_json {
                                Ok(restored_comments_json) => restored_comments_json,
                                Err(error) => {
                                    log::error!(
                                        "failed to preserve review comment locations while stamping: {error:#}"
                                    );
                                    return;
                                }
                            };
                            this.restoring_review_comments = true;
                            let restore_result = editor.update(cx, |editor, cx| {
                                editor.restore_review_comments_json(
                                    &restored_comments_json,
                                    window,
                                    cx,
                                )
                            });
                            this.restoring_review_comments = false;
                            if let Err(error) = restore_result {
                                log::error!(
                                    "failed to restore stamped review comments: {error:#}"
                                );
                                return;
                            }
                            this.loaded_review_comments_json = Some(restored_comments_json.clone());
                            this.last_observed_review_comments_db_json =
                                Some(persisted_comments_json.clone());
                        })
                        .ok();
                    })
                    .ok();
                    anyhow::Ok(prompt)
                }
                .await;

                this.update(cx, |this, cx| {
                    this.copy_amp_prompt_in_progress = false;
                    cx.notify();
                })
                .ok();

                match result {
                    Ok(prompt) => {
                        cx.update(|cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(prompt));
                        });
                        workspace.update(cx, |workspace, cx| {
                            Self::show_copy_amp_prompt_result(
                                workspace,
                                CopyAmpPromptResult::Copied,
                                cx,
                            );
                        })
                        .ok();
                    }
                    Err(error) => {
                        log::error!("{error:#}");
                        workspace.update(cx, |workspace, cx| {
                            workspace.show_toast(
                                Toast::new(
                                    copy_amp_prompt_notification_id(),
                                    "Failed to save review comments; Amp prompt was not copied.",
                                )
                                .autohide(),
                                cx,
                            );
                        })
                        .ok();
                    }
                }

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);

        CopyAmpPromptResult::Saving
    }

    fn archive_review_comments(
        &mut self,
        workspace_id: workspace::WorkspaceId,
        scope: ReviewCommentArchiveScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review_key) = self.review_comments_key(cx) else {
            return;
        };

        let Ok(comments_json) = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments_json(cx)
        else {
            return;
        };
        let Ok((archived_json, remaining_json, archived_count)) =
            partition_review_comments_json(&comments_json, scope)
        else {
            return;
        };
        if archived_count == 0 {
            return;
        }

        let archived_on = unix_time_millis();
        let archive_scope = scope.archive_scope();
        let session_id = format!("archived:{archived_on}:{archive_scope}");
        let db = persistence::ProjectDiffDb::global(cx);
        let archive = db.archive_review_comments(
            workspace_id,
            review_key.clone(),
            session_id,
            archived_json,
            remaining_json.clone(),
            archived_on,
            archive_scope,
        );
        cx.background_spawn(archive).detach_and_log_err(cx);

        self.loaded_review_comments_key = Some(review_key);
        self.loaded_review_comments_json = Some(remaining_json.clone());
        self.restoring_review_comments = true;
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                if let Err(error) = editor.restore_review_comments_json(&remaining_json, window, cx)
                {
                    log::error!("failed to clear archived review comments: {error:#}");
                }
            })
        });
        self.restoring_review_comments = false;
    }

    fn active_review_comment_rounds(&self, cx: &App) -> Vec<u64> {
        let Ok(comments_json) = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments_json(cx)
        else {
            return Vec::new();
        };
        review_comments_json_rounds(&comments_json)
    }

    fn archived_review_comment_rounds(
        &self,
        workspace_id: workspace::WorkspaceId,
        cx: &App,
    ) -> Vec<u64> {
        let Some(review_key) = self.review_comments_key(cx) else {
            return Vec::new();
        };
        let db = persistence::ProjectDiffDb::global(cx);
        match db.archived_review_comment_rounds(workspace_id, &review_key) {
            Ok(rounds) => rounds,
            Err(error) => {
                log::error!("failed to load archived review comment rounds: {error:#}");
                Vec::new()
            }
        }
    }

    fn restore_latest_review_comments(
        &mut self,
        workspace_id: workspace::WorkspaceId,
        scope: Option<ReviewCommentArchiveScope>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RestoreLatestReviewCommentsResult {
        let Some(review_key) = self.review_comments_key(cx) else {
            return RestoreLatestReviewCommentsResult::Failed;
        };

        let Ok(active_comments_json) = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments_json(cx)
        else {
            return RestoreLatestReviewCommentsResult::Failed;
        };

        let db = persistence::ProjectDiffDb::global(cx);
        let archive_scope = scope.map(|scope| scope.archive_scope());
        let Some((_session_id, comments_json)) = (match db.latest_archived_review_comments(
            workspace_id,
            &review_key,
            archive_scope.as_deref(),
        ) {
            Ok(result) => result,
            Err(error) => {
                log::error!("failed to load latest archived review comments: {error:#}");
                return RestoreLatestReviewCommentsResult::Failed;
            }
        }) else {
            return RestoreLatestReviewCommentsResult::NoArchivedComments;
        };

        let Ok((merged_json, restored_count)) =
            merge_review_comments_json(&active_comments_json, &comments_json)
        else {
            log::error!("failed to merge archived review comments into active comments");
            return RestoreLatestReviewCommentsResult::Failed;
        };
        if restored_count == 0 {
            return RestoreLatestReviewCommentsResult::NoArchivedComments;
        }

        let save = db.save_review_comments(workspace_id, review_key.clone(), merged_json.clone());
        cx.background_spawn(save).detach_and_log_err(cx);

        self.loaded_review_comments_key = Some(review_key);
        self.loaded_review_comments_json = Some(merged_json.clone());
        self.restoring_review_comments = true;
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                if let Err(error) = editor.restore_review_comments_json(&merged_json, window, cx) {
                    log::error!("failed to restore latest archived review comments: {error:#}");
                }
            })
        });
        self.restoring_review_comments = false;
        RestoreLatestReviewCommentsResult::Restored
    }

    fn restore_review_comments_if_needed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_review_comments_impl(false, window, cx)
    }

    // `reattach` re-runs the restore even when the persisted JSON is unchanged, so
    // comments that previously failed to attach (their file or line wasn't in the
    // diff yet) get another chance after the diff finishes loading. Only the
    // post-refresh path passes true; the periodic poll must not, otherwise an
    // unattachable comment would tear down and rebuild every overlay each tick.
    fn restore_review_comments_impl(
        &mut self,
        reattach: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace_id) = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).database_id())
        else {
            return;
        };
        let Some(review_key) = self.review_comments_key(cx) else {
            return;
        };

        let force_reattach = reattach && !self.review_comments_fully_restored;
        let db = persistence::ProjectDiffDb::global(cx);
        let db_modified_on = db.modified_on();
        if !force_reattach
            && db_modified_on.is_some()
            && self.loaded_review_comments_key.as_ref() == Some(&review_key)
            && self.last_review_comments_db_modified_on == db_modified_on
        {
            return;
        }
        let mut loaded_key = review_key.clone();
        let mut comments_json = match db.get_review_comments(workspace_id, &review_key) {
            Ok(comments_json) => comments_json,
            Err(error) => {
                log::error!("failed to load review comments: {error:#}");
                None
            }
        };
        if comments_json.is_none() {
            for legacy_key in self.legacy_review_comments_keys(cx) {
                if legacy_key == review_key {
                    continue;
                }
                match db.get_review_comments(workspace_id, &legacy_key) {
                    Ok(Some(json)) => {
                        comments_json = Some(json);
                        loaded_key = legacy_key;
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        log::error!("failed to load legacy review comments: {error:#}");
                    }
                }
            }
        }
        if !force_reattach
            && self.loaded_review_comments_key.as_ref() == Some(&review_key)
            && self.last_observed_review_comments_db_json.as_deref() == comments_json.as_deref()
        {
            self.last_review_comments_db_modified_on = db_modified_on;
            return;
        }
        if self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .has_active_diff_review_input()
        {
            return;
        }

        let observed_comments_json = comments_json.clone();
        let mut loaded = true;
        let mut locations_rebased = false;
        if let Some(comments_json) = comments_json.as_mut() {
            if loaded_key == review_key
                && self.loaded_review_comments_key.as_ref() == Some(&review_key)
                && let Some(base_comments_json) =
                    self.last_observed_review_comments_db_json.as_deref()
                && base_comments_json != comments_json
            {
                let live_comments_json = self
                    .editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .review_comments_json(cx);
                match live_comments_json.and_then(|live_comments_json| {
                    preserve_live_review_comment_locations(
                        base_comments_json,
                        &live_comments_json,
                        comments_json,
                    )
                }) {
                    Ok(Some(rebased)) => {
                        *comments_json = rebased;
                        locations_rebased = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        log::error!("failed to preserve live review comment locations: {error:#}")
                    }
                }
            }
        }
        self.restoring_review_comments = true;
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                if let Some(comments_json) = comments_json.as_deref() {
                    match editor.restore_review_comments_json(comments_json, window, cx) {
                        Ok(restored_count) => {
                            if restored_count
                                < review_comments_json_restorable_comment_count(comments_json)
                            {
                                loaded = false;
                            }
                        }
                        Err(error) => {
                            loaded = false;
                            log::error!("failed to restore review comments: {error:#}");
                        }
                    }
                } else {
                    editor.clear_review_comments(cx);
                }
            })
        });
        self.restoring_review_comments = false;
        if loaded
            && comments_json.is_some()
            && let Ok(restored_comments_json) = self
                .editor
                .read(cx)
                .rhs_editor()
                .read(cx)
                .review_comments_json(cx)
            && comments_json.as_deref() != Some(restored_comments_json.as_str())
        {
            comments_json = Some(restored_comments_json);
            locations_rebased = true;
        }
        // Cache the attempt even when some comments failed to attach; refresh()
        // retries attachment explicitly. Re-restoring from the poll instead would
        // rebuild every overlay once a second and leak the editors they create.
        self.review_comments_fully_restored = loaded;
        self.last_review_comments_db_modified_on = db_modified_on;
        if loaded_key != review_key {
            if let Some(comments_json) = comments_json.clone() {
                self.persist_review_comments(comments_json, cx);
            }
        } else if locations_rebased
            && let (Some(observed), Some(rebased)) =
                (observed_comments_json.clone(), comments_json.clone())
        {
            let save = db.replace_review_comments_if_unchanged(
                workspace_id,
                review_key.clone(),
                observed,
                rebased,
            );
            cx.background_spawn(save).detach_and_log_err(cx);
        }
        self.loaded_review_comments_key = Some(review_key);
        self.loaded_review_comments_json = comments_json;
        self.last_observed_review_comments_db_json = observed_comments_json;
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn new_with_default_branch(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(repo) = project.read(cx).git_store().read(cx).active_repository() else {
            return Task::ready(Err(anyhow!("No active repository")));
        };
        let main_branch = repo.update(cx, |repo, _| repo.default_branch(true));
        window.spawn(cx, async move |cx| {
            let main_branch = main_branch
                .await??
                .context("Could not determine default branch")?;

            let branch_diff = cx.new_window_entity(|window, cx| {
                let mut branch_diff = branch_diff::BranchDiff::new(
                    DiffBase::Merge {
                        base_ref: main_branch,
                    },
                    project.clone(),
                    window,
                    cx,
                );
                branch_diff.set_repo(Some(repo.clone()), cx);
                branch_diff
            })?;
            cx.new_window_entity(|window, cx| {
                Self::new_impl(branch_diff, project, workspace, true, window, cx)
            })
        })
    }

    fn new_with_branch_base(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        base_ref: SharedString,
        repo: Entity<Repository>,
        follows_default_branch: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let branch_diff = cx.new_window_entity(|window, cx| {
                let mut branch_diff = branch_diff::BranchDiff::new(
                    DiffBase::Merge { base_ref },
                    project.clone(),
                    window,
                    cx,
                );
                branch_diff.set_repo(Some(repo.clone()), cx);
                branch_diff
            })?;
            cx.new_window_entity(|window, cx| {
                Self::new_impl(
                    branch_diff,
                    project,
                    workspace,
                    follows_default_branch,
                    window,
                    cx,
                )
            })
        })
    }

    fn new(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch_diff =
            cx.new(|cx| branch_diff::BranchDiff::new(DiffBase::Head, project.clone(), window, cx));
        Self::new_impl(branch_diff, project, workspace, false, window, cx)
    }

    fn new_impl(
        branch_diff: Entity<branch_diff::BranchDiff>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        follows_default_branch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let editor = cx.new(|cx| {
            let diff_display_editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace.clone(),
                window,
                cx,
            );
            match branch_diff.read(cx).diff_base() {
                DiffBase::Head => {}
                DiffBase::Merge { .. } => diff_display_editor
                    .set_diff_hunk_delegate(Some(Arc::new(RestoreOnlyDiffHunkDelegate)), cx),
            }
            diff_display_editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_diff_review_button(true, cx);

                match branch_diff.read(cx).diff_base() {
                    DiffBase::Head => {
                        editor.register_addon(GitPanelAddon {
                            workspace: workspace.downgrade(),
                        });
                    }
                    DiffBase::Merge { .. } => {
                        editor.register_addon(BranchDiffAddon {
                            branch_diff: branch_diff.clone(),
                        });
                    }
                }
            });
            diff_display_editor
        });
        let editor_subscription = cx.subscribe_in(&editor, window, Self::handle_editor_event);

        let primary_editor = editor.read(cx).rhs_editor().clone();
        let review_comment_subscription =
            cx.subscribe(&primary_editor, |this, editor, event: &EditorEvent, cx| {
                if let EditorEvent::ReviewCommentsChanged {
                    total_count,
                    persist,
                } = event
                {
                    this.review_comment_count = *total_count;
                    if *persist && !this.restoring_review_comments {
                        match editor.read(cx).review_comments_json(cx) {
                            Ok(comments_json) => this.persist_review_comments(comments_json, cx),
                            Err(error) => {
                                log::error!("failed to serialize review comments: {error:#}")
                            }
                        }
                    }
                    cx.notify();
                }
            });

        let branch_diff_subscription = cx.subscribe_in(
            &branch_diff,
            window,
            move |this, _git_store, event, window, cx| match event {
                BranchDiffEvent::FileListChanged => {
                    this._task = window.spawn(cx, {
                        let this = cx.weak_entity();
                        async |cx| Self::refresh(this, cx).await
                    })
                }
                BranchDiffEvent::DiffBaseChanged => {
                    this.pending_scroll.take();
                    this._task = window.spawn(cx, {
                        let this = cx.weak_entity();
                        async |cx| Self::refresh(this, cx).await
                    })
                }
            },
        );

        let mut was_sort_by = GitPanelSettings::get_global(cx).sort_by;
        let mut was_group_by = GitPanelSettings::get_global(cx).group_by;
        let mut was_tree_view = GitPanelSettings::get_global(cx).tree_view;
        let mut was_collapse_untracked_diff =
            GitPanelSettings::get_global(cx).collapse_untracked_diff;
        cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
            let settings = GitPanelSettings::get_global(cx);
            let sort_by = settings.sort_by;
            let group_by = settings.group_by;
            let tree_view = settings.tree_view;
            let is_collapse_untracked_diff = settings.collapse_untracked_diff;
            if sort_by != was_sort_by
                || group_by != was_group_by
                || tree_view != was_tree_view
                || is_collapse_untracked_diff != was_collapse_untracked_diff
            {
                this._task = {
                    window.spawn(cx, {
                        let this = cx.weak_entity();
                        async |cx| Self::refresh(this, cx).await
                    })
                }
            }
            was_sort_by = sort_by;
            was_group_by = group_by;
            was_tree_view = tree_view;
            was_collapse_untracked_diff = is_collapse_untracked_diff;
        })
        .detach();

        let task = window.spawn(cx, {
            let this = cx.weak_entity();
            async |cx| Self::refresh(this, cx).await
        });
        let review_comments_poll_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.restore_review_comments_if_needed(window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            project,
            workspace: workspace.downgrade(),
            branch_diff,
            focus_handle,
            editor,
            multibuffer,
            buffer_subscriptions: Default::default(),
            pending_scroll: None,
            review_comment_count: 0,
            loaded_review_comments_key: None,
            loaded_review_comments_json: None,
            last_observed_review_comments_db_json: None,
            last_review_comments_db_modified_on: None,
            restoring_review_comments: false,
            review_comments_fully_restored: true,
            copy_amp_prompt_in_progress: false,
            orphaned_review_comments_expanded: false,
            follows_default_branch,
            _task: task,
            _review_comments_poll_task: review_comments_poll_task,
            _subscription: Subscription::join(
                branch_diff_subscription,
                Subscription::join(editor_subscription, review_comment_subscription),
            ),
        }
    }

    pub fn diff_base<'a>(&'a self, cx: &'a App) -> &'a DiffBase {
        self.branch_diff.read(cx).diff_base()
    }

    pub fn move_to_entry(
        &mut self,
        entry: GitStatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(git_repo) = self.branch_diff.read(cx).repo() else {
            return;
        };
        let repo = git_repo.read(cx);
        let path_key = project_diff_path_key(repo, &entry.repo_path, entry.status, cx);

        self.move_to_path(path_key, window, cx)
    }

    pub fn move_to_project_path(
        &mut self,
        project_path: &ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(git_repo) = self.branch_diff.read(cx).repo() else {
            return;
        };
        let Some(repo_path) = git_repo
            .read(cx)
            .project_path_to_repo_path(project_path, cx)
        else {
            return;
        };
        let status = git_repo
            .read(cx)
            .status_for_path(&repo_path)
            .map(|entry| entry.status)
            .unwrap_or(FileStatus::Untracked);
        let path_key = project_diff_path_key(&git_repo.read(cx), &repo_path, status, cx);
        self.move_to_path(path_key, window, cx)
    }

    fn move_to_beginning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.select_ranges(vec![multi_buffer::Anchor::Min..multi_buffer::Anchor::Min]);
                });
            });
        });
    }

    fn move_to_path(&mut self, path_key: PathKey, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(position) = self.multibuffer.read(cx).location_for_path(&path_key, cx) {
            self.editor.update(cx, |editor, cx| {
                editor.rhs_editor().update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::focused()),
                        window,
                        cx,
                        |s| {
                            s.select_ranges([position..position]);
                        },
                    )
                })
            });
        } else {
            self.pending_scroll = Some(path_key);
        }
    }

    pub fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    /// Returns the total count of review comments across all hunks/files.
    pub fn total_review_comment_count(&self) -> usize {
        self.review_comment_count
    }

    fn refresh_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_scroll.take();
        self.review_comments_fully_restored = false;
        if matches!(self.diff_base(cx), DiffBase::Merge { .. }) {
            if self.follows_default_branch
                && let Some(repo) = self.branch_diff.read(cx).repo().cloned()
            {
                let default_branch = repo.update(cx, |repo, _| repo.default_branch(true));
                self._task = cx.spawn_in(window, async move |this, cx| {
                    let default_branch = default_branch.await??;
                    this.update(cx, |this, cx| {
                        this.refresh_branch_diff(default_branch, cx);
                    })?;
                    Ok(())
                });
                cx.notify();
                return;
            }
            self.refresh_branch_diff(None, cx);
        } else {
            self._task = cx.spawn_in(window, async move |this, cx| Self::refresh(this, cx).await);
            cx.notify();
        }
    }

    fn refresh_branch_diff(
        &mut self,
        default_branch: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let Some(default_branch) = default_branch {
            let diff_base = self.diff_base(cx).clone();
            if !matches!(diff_base, DiffBase::Merge { ref base_ref } if base_ref == &default_branch)
            {
                self.branch_diff.update(cx, |branch_diff, cx| {
                    branch_diff.set_diff_base(
                        DiffBase::Merge {
                            base_ref: default_branch,
                        },
                        cx,
                    );
                });
                cx.notify();
                return;
            }
        }

        if matches!(self.diff_base(cx), DiffBase::Merge { .. }) {
            self.branch_diff.update(cx, |branch_diff, cx| {
                branch_diff.refresh(cx);
            });
        }
        cx.notify();
    }

    /// Returns a reference to the splittable editor.
    pub fn editor(&self) -> &Entity<SplittableEditor> {
        &self.editor
    }

    fn button_states(&self, cx: &App) -> ButtonStates {
        let editor = self.editor.read(cx).rhs_editor().read(cx);
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let prev_next = snapshot.diff_hunks().next().is_some();
        let mut selection = true;

        let mut ranges = editor
            .selections
            .disjoint_anchor_ranges()
            .collect::<Vec<_>>();
        if !ranges.iter().any(|range| range.start != range.end) {
            selection = false;
            let anchor = editor.selections.newest_anchor().head();
            if let Some((_, excerpt_range)) = snapshot.excerpt_containing(anchor..anchor)
                && let Some(range) = snapshot
                    .anchor_in_buffer(excerpt_range.context.start)
                    .zip(snapshot.anchor_in_buffer(excerpt_range.context.end))
                    .map(|(start, end)| start..end)
            {
                ranges = vec![range];
            } else {
                ranges = Vec::default();
            };
        }
        let mut has_staged_hunks = false;
        let mut has_unstaged_hunks = false;
        for hunk in editor.diff_hunks_in_ranges(&ranges, &snapshot) {
            match hunk.status.secondary {
                DiffHunkSecondaryStatus::HasSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkAdditionPending => {
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk => {
                    has_staged_hunks = true;
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::NoSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkRemovalPending => {
                    has_staged_hunks = true;
                }
            }
        }
        let mut stage_all = false;
        let mut unstage_all = false;
        self.workspace
            .read_with(cx, |workspace, cx| {
                if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                    let git_panel = git_panel.read(cx);
                    stage_all = git_panel.can_stage_all();
                    unstage_all = git_panel.can_unstage_all();
                }
            })
            .ok();

        ButtonStates {
            stage: has_unstaged_hunks,
            unstage: has_staged_hunks,
            prev_next,
            selection,
            stage_all,
            unstage_all,
        }
    }

    fn handle_editor_event(
        &mut self,
        editor: &Entity<SplittableEditor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::SelectionsChanged { local: true } => {
                let Some(project_path) = self.active_project_path(cx) else {
                    return;
                };
                self.workspace
                    .update(cx, |workspace, cx| {
                        if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                            git_panel.update(cx, |git_panel, cx| {
                                git_panel.select_entry_by_path(project_path, window, cx)
                            })
                        }
                    })
                    .ok();
            }
            EditorEvent::Saved => {
                self._task =
                    cx.spawn_in(window, async move |this, cx| Self::refresh(this, cx).await);
            }
            _ => {}
        }
        if editor.focus_handle(cx).contains_focused(window, cx)
            && self.multibuffer.read(cx).is_empty()
        {
            self.focus_handle.focus(window, cx)
        }
    }

    #[instrument(skip_all)]
    fn register_buffer(
        &mut self,
        path_key: PathKey,
        file_status: FileStatus,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        conflict_set: Entity<ConflictSet>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<BufferId> {
        let diff_subscription = cx.subscribe_in(&diff, window, {
            let path_key = path_key.clone();
            let buffer = buffer.clone();
            let diff = diff.clone();
            let conflict_set = conflict_set.clone();
            move |this, _, event, window, cx| match event {
                buffer_diff::BufferDiffEvent::DiffChanged(_) => {
                    this.buffer_ranges_changed(
                        path_key.clone(),
                        file_status,
                        buffer.clone(),
                        diff.clone(),
                        conflict_set.clone(),
                        window,
                        cx,
                    );
                }
                buffer_diff::BufferDiffEvent::BaseTextChanged => {}
            }
        });
        let conflict_set_subscription = cx.subscribe_in(&conflict_set, window, {
            let path_key = path_key.clone();
            let buffer = buffer.clone();
            let diff = diff.clone();
            let conflict_set = conflict_set.clone();
            move |this, _, _, window, cx| {
                this.buffer_ranges_changed(
                    path_key.clone(),
                    file_status,
                    buffer.clone(),
                    diff.clone(),
                    conflict_set.clone(),
                    window,
                    cx,
                )
            }
        });
        self.buffer_subscriptions.insert(
            path_key.path.clone(),
            BufferSubscriptions {
                _diff: diff.clone(),
                _diff_subscription: diff_subscription,
                _conflict_set: conflict_set.clone(),
                _conflict_set_subscription: conflict_set_subscription,
            },
        );

        let snapshot = buffer.read(cx).snapshot();
        let diff_snapshot = diff.read(cx).snapshot(cx);

        let excerpt_ranges = {
            let diff_hunk_ranges = diff_snapshot
                .hunks_intersecting_range(
                    Anchor::min_max_range_for_buffer(snapshot.remote_id()),
                    &snapshot,
                )
                .map(|diff_hunk| diff_hunk.buffer_range.to_point(&snapshot));
            let conflicts = conflict_set.read(cx).snapshot();
            let mut conflicts = conflicts
                .conflicts
                .iter()
                .map(|conflict| conflict.range.to_point(&snapshot))
                .peekable();

            if conflicts.peek().is_some() {
                conflicts.collect::<Vec<_>>()
            } else {
                diff_hunk_ranges.collect()
            }
        };

        let buffer_id = snapshot.text.remote_id();
        let mut needs_fold = false;

        let (was_empty, is_excerpt_newly_added) = self.editor.update(cx, |editor, cx| {
            let was_empty = editor.rhs_editor().read(cx).buffer().read(cx).is_empty();
            let is_newly_added = editor.update_excerpts_for_path(
                path_key.clone(),
                buffer,
                excerpt_ranges,
                multibuffer_context_lines(cx),
                diff,
                cx,
            );
            editor.rhs_editor().update(cx, |editor, cx| {
                conflict_view::buffer_ranges_updated(editor, conflict_set, cx);
            });
            (was_empty, is_newly_added)
        });

        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                if was_empty {
                    editor.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([
                                multi_buffer::Anchor::Min..multi_buffer::Anchor::Min
                            ])
                        },
                    );
                }
                if is_excerpt_newly_added
                    && (file_status.is_deleted()
                        || (file_status.is_untracked()
                            && GitPanelSettings::get_global(cx).collapse_untracked_diff))
                {
                    needs_fold = true;
                }
            })
        });

        if self.multibuffer.read(cx).is_empty()
            && self
                .editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            self.focus_handle.focus(window, cx);
        } else if self.focus_handle.is_focused(window) && !self.multibuffer.read(cx).is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        }
        if self.pending_scroll.as_ref() == Some(&path_key) {
            self.move_to_path(path_key, window, cx);
        }

        needs_fold.then_some(buffer_id)
    }

    fn buffer_ranges_changed(
        &mut self,
        path_key: PathKey,
        file_status: FileStatus,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        conflict_set: Entity<ConflictSet>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if buffer.read(cx).is_dirty() {
            return;
        }
        self.register_buffer(
            path_key,
            file_status,
            buffer,
            diff,
            conflict_set,
            window,
            cx,
        );
    }

    #[instrument(skip(this, cx))]
    pub async fn refresh(this: WeakEntity<Self>, cx: &mut AsyncWindowContext) -> Result<()> {
        let entries = this.update(cx, |this, cx| {
            let (repo, buffers_to_load) = this.branch_diff.update(cx, |branch_diff, cx| {
                let load_buffers = branch_diff.load_buffers(cx);
                (branch_diff.repo().cloned(), load_buffers)
            });
            let mut previous_paths = this
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .buffers_with_paths()
                .map(|(buffer_snapshot, path_key)| (path_key.clone(), buffer_snapshot.remote_id()))
                .collect::<HashMap<_, _>>();

            let mut entries = BTreeMap::new();
            if let Some(repo) = repo {
                let repo = repo.read(cx);
                for diff_buffer in buffers_to_load {
                    let path_key = project_diff_path_key(
                        &repo,
                        &diff_buffer.repo_path,
                        diff_buffer.file_status,
                        cx,
                    );
                    previous_paths.remove(&path_key);
                    entries.insert(path_key, diff_buffer);
                }
            }

            this.editor.update(cx, |editor, cx| {
                for (path, buffer_id) in previous_paths {
                    this.buffer_subscriptions.remove(&path.path);
                    editor.rhs_editor().update(cx, |editor, cx| {
                        conflict_view::buffers_removed(editor, &[buffer_id], cx);
                    });
                    let _span = ztracing::info_span!("remove_excerpts_for_path");
                    _span.enter();
                    editor.remove_excerpts_for_path(path, cx);
                }
            });

            entries
        })?;

        let mut buffers_to_fold = Vec::new();

        for (path_key, entry) in entries {
            if let Some((buffer, diff, conflict_set)) = entry.load.await.log_err() {
                // We might be lagging behind enough that all future entry.load futures are no longer pending.
                // If that is the case, this task will never yield, starving the foreground thread of execution time.
                yield_now().await;
                cx.update(|window, cx| {
                    this.update(cx, |this, cx| {
                        if let Some(buffer_id) = this.register_buffer(
                            path_key,
                            entry.file_status,
                            buffer,
                            diff,
                            conflict_set,
                            window,
                            cx,
                        ) {
                            buffers_to_fold.push(buffer_id);
                        }
                    })
                    .ok();
                })?;
            }
        }
        this.update(cx, |this, cx| {
            if !buffers_to_fold.is_empty() {
                this.editor.update(cx, |editor, cx| {
                    editor
                        .rhs_editor()
                        .update(cx, |editor, cx| editor.fold_buffers(buffers_to_fold, cx));
                });
            }
            this.pending_scroll.take();
            cx.notify();
        })?;
        let db = cx.update(|_, cx| persistence::ProjectDiffDb::global(cx))?;
        db.flush_writes().await?;
        cx.update(|window, cx| {
            this.update(cx, |this, cx| {
                this.restore_review_comments_impl(true, window, cx);
            })
        })??;

        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn excerpt_paths(&self, cx: &App) -> Vec<std::sync::Arc<util::rel_path::RelPath>> {
        let snapshot = self
            .editor()
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx);
        snapshot
            .excerpts()
            .map(|excerpt| {
                snapshot
                    .path_for_buffer(excerpt.context.start.buffer_id)
                    .unwrap()
                    .path
                    .clone()
            })
            .collect()
    }

    /// Returns the real (worktree-relative) path of each excerpted buffer, in
    /// the order the excerpts appear in the multibuffer. Unlike
    /// [`Self::excerpt_paths`], this resolves the buffer's actual `File` rather
    /// than the (possibly synthetic) `PathKey` path used for sorting.
    #[cfg(any(test, feature = "test-support"))]
    pub fn excerpt_file_paths(&self, cx: &App) -> Vec<String> {
        let multibuffer = self
            .editor()
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .clone();
        let snapshot = multibuffer.read(cx).snapshot(cx);
        let mut result = Vec::new();
        let mut last_buffer_id = None;
        for excerpt in snapshot.excerpts() {
            let buffer_id = excerpt.context.start.buffer_id;
            if last_buffer_id == Some(buffer_id) {
                continue;
            }
            last_buffer_id = Some(buffer_id);
            if let Some(buffer) = multibuffer.read(cx).buffer(buffer_id)
                && let Some(file) = buffer.read(cx).file()
            {
                result.push(file.path().as_unix_str().to_string());
            }
        }
        result
    }
}

fn stamp_unstamped_review_comments(comments_json: &str) -> Result<(String, u64)> {
    let mut snapshot: serde_json::Value =
        serde_json::from_str(comments_json).context("deserializing review comments")?;
    let schema_version = snapshot
        .get("schema_version")
        .and_then(|version| version.as_u64())
        .unwrap_or(1);
    let comments = snapshot
        .get_mut("comments")
        .and_then(|comments| comments.as_array_mut())
        .context("review comments JSON is missing comments array")?;

    let current_round = review_comments_current_round(comments.iter());
    let next_round = current_round + 1;
    let mut stamped_any = false;
    for comment in comments.iter_mut() {
        if review_item_is_unstamped_by(comment, "vxio", "vxio") {
            comment["review_round"] = serde_json::json!(next_round);
            stamped_any = true;
        }
        if let Some(replies) = comment
            .get_mut("replies")
            .and_then(|replies| replies.as_array_mut())
        {
            for reply in replies {
                if review_item_is_unstamped_by(reply, "amp", "vxio") {
                    reply["review_round"] = serde_json::json!(next_round);
                    stamped_any = true;
                }
            }
        }
    }
    if !stamped_any {
        return Err(anyhow!("No unstamped review comments found"));
    }

    let stamped_json = serde_json::json!({
        "schema_version": schema_version,
        "comments": comments,
    })
    .to_string();
    Ok((stamped_json, next_round))
}

fn amp_prompt_for_review_round(
    repo: String,
    review_key: String,
    comments_json: &str,
    review_round: u64,
) -> Result<String> {
    let snapshot: serde_json::Value =
        serde_json::from_str(comments_json).context("deserializing review comments")?;
    let comments = snapshot
        .get("comments")
        .and_then(|comments| comments.as_array())
        .context("review comments JSON is missing comments array")?;
    let queue_comments = comments
        .iter()
        .filter(|comment| review_comment_contains_round(comment, review_round))
        .map(compact_review_comment_for_prompt)
        .collect::<Vec<_>>();
    if queue_comments.is_empty() {
        return Err(anyhow!("No unstamped review comments found"));
    }

    let view = review_key_view_label(&repo, &review_key);
    let branch = review_key_branch(&review_key);
    let compact_packet = serde_json::json!({
        "repo": repo,
        "diff_base": review_key.strip_prefix(&format!("{repo}\n")).unwrap_or(&review_key),
        "view": view,
        "count": queue_comments.len(),
        "comments": queue_comments,
    });
    let prompt = [
        format!(
            "Address my newest Zed Git diff review comments for this repo — these are now Review #{review_round}."
        ),
        "".to_string(),
        format!(
            "These comments are for the repo/worktree at `{repo}`{}. Make all code edits in that exact directory — not in another repo or sibling worktree — and run any verification from there.",
            branch
                .as_ref()
                .map(|branch| format!(" (branch `{branch}`)"))
                .unwrap_or_default()
        ),
        "".to_string(),
        format!(
            "They live in the \"{view}\" diff view. Pass this exact repo path AND the reviewKey below (copy the reviewKey verbatim, including the line breaks) to every zed_review_* tool call so you read and reply on the right repo and view:"
        ),
        "```".to_string(),
        review_key.clone(),
        "```".to_string(),
        "".to_string(),
        format!(
            "Each comment/reply below carries a `review` round number. The threads are shown in full for context, but only act on the items whose `review` number is {review_round}. Treat every other item in each thread as settled context; do not redo it. Make the smallest correct code changes. If an item is a question or is not actionable, answer it in chat instead of inventing a change. After editing, run the narrowest useful verification."
        ),
        "".to_string(),
        format!(
            "Before changing code, load the `addressing-zed-review-comments` skill and follow it for marker capture, reply posting, reply verification, workflow-evolution, and refresh behavior. If you need to refresh the queue, call `zed_review_comments_list` with author `vxio`, the repo path and reviewKey above, and round `{review_round}` so you stay scoped to this round."
        ),
        "".to_string(),
        serde_json::to_string(&compact_packet).context("serializing Amp prompt packet")?,
    ]
    .join("\n");
    Ok(prompt)
}

fn review_item_author<'a>(item: &'a serde_json::Value, default_author: &'a str) -> &'a str {
    item.get("author")
        .and_then(|author| author.as_str())
        .unwrap_or(default_author)
}

fn review_item_is_unstamped_by(
    item: &serde_json::Value,
    default_author: &str,
    wanted_author: &str,
) -> bool {
    item.get("deleted_on").is_none()
        && review_item_author(item, default_author) == wanted_author
        && item.get("review_round").is_none()
}

fn review_comments_current_round<'a>(comments: impl Iterator<Item = &'a serde_json::Value>) -> u64 {
    comments
        .flat_map(|comment| {
            std::iter::once(comment).chain(
                comment
                    .get("replies")
                    .and_then(|replies| replies.as_array())
                    .into_iter()
                    .flatten(),
            )
        })
        .filter_map(comment_review_round)
        .max()
        .unwrap_or(0)
}

fn compact_review_comment_for_prompt(comment: &serde_json::Value) -> serde_json::Value {
    let mut compact = serde_json::Map::new();
    compact.insert("id".to_string(), comment["id"].clone());
    compact.insert(
        "by".to_string(),
        serde_json::Value::String(review_item_author(comment, "vxio").to_string()),
    );
    compact.insert("path".to_string(), comment["file"].clone());
    compact.insert("line".to_string(), comment["line_start"].clone());
    compact.insert("body".to_string(), comment["body"].clone());
    if let Some(created_at) = comment.get("created_at") {
        compact.insert("at".to_string(), created_at.clone());
    }
    if let Some(round) = comment_review_round(comment) {
        compact.insert("review".to_string(), serde_json::json!(round));
    }
    if comment.get("line_end") != comment.get("line_start") {
        compact.insert("end_line".to_string(), comment["line_end"].clone());
    }
    if comment
        .get("agent_feedback")
        .or_else(|| comment.get("agent_toolbox"))
        .and_then(|value| value.as_bool())
        == Some(true)
    {
        compact.insert("agent_feedback".to_string(), serde_json::Value::Bool(true));
    }
    let replies = comment
        .get("replies")
        .and_then(|replies| replies.as_array())
        .into_iter()
        .flatten()
        .filter(|reply| reply.get("deleted_on").is_none())
        .map(|reply| {
            let mut compact_reply = serde_json::Map::new();
            compact_reply.insert("id".to_string(), reply["id"].clone());
            compact_reply.insert(
                "by".to_string(),
                serde_json::Value::String(review_item_author(reply, "amp").to_string()),
            );
            if let Some(created_at) = reply.get("created_at") {
                compact_reply.insert("at".to_string(), created_at.clone());
            }
            if let Some(round) = comment_review_round(reply) {
                compact_reply.insert("review".to_string(), serde_json::json!(round));
            }
            compact_reply.insert("body".to_string(), reply["body"].clone());
            if reply
                .get("agent_feedback")
                .or_else(|| reply.get("agent_toolbox"))
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                compact_reply.insert("agent_feedback".to_string(), serde_json::Value::Bool(true));
            }
            serde_json::Value::Object(compact_reply)
        })
        .collect::<Vec<_>>();
    if !replies.is_empty() {
        compact.insert("replies".to_string(), serde_json::Value::Array(replies));
    }

    serde_json::Value::Object(compact)
}

fn review_key_view_label(repo: &str, review_key: &str) -> String {
    let suffix = review_key
        .strip_prefix(&format!("{repo}\n"))
        .unwrap_or(review_key);
    let Some((_ref, view)) = suffix.split_once('\n') else {
        return "Branch comments".to_string();
    };
    if view == "uncommitted" {
        "Uncommitted Changes".to_string()
    } else if let Some(base) = view.strip_prefix("since:") {
        format!("Changes since {base}")
    } else {
        view.to_string()
    }
}

fn review_key_branch(review_key: &str) -> Option<String> {
    let ref_line = review_key
        .lines()
        .nth(1)
        .or_else(|| review_key.lines().next())?;
    ref_line
        .strip_prefix("branch:refs/heads/")
        .map(ToOwned::to_owned)
}

const CONFLICT_SORT_PREFIX: u64 = 1;
const TRACKED_SORT_PREFIX: u64 = 2;
const NEW_SORT_PREFIX: u64 = 3;

/// Computes a stable [`PathKey`] for a buffer in the project diff.
///
/// The key is an intrinsic function of the file's own repo path and status; it
/// never depends on which other buffers happen to be present in the
/// multibuffer. This is required because the multibuffer uses the path key both
/// to order excerpts and to identify which excerpts belong to a given buffer, so
/// a key that shifted as files were added or removed would break that identity.
///
/// Status grouping is encoded in the `sort_prefix`, and the within-group order
/// is encoded in the (possibly synthetic) path so that `PathKey`'s natural
/// ordering reproduces the git panel's order. The path here is only ever used
/// for sorting and multibuffer identity; the path shown in the UI comes from the
/// buffer's own `File`.
fn project_diff_path_key(
    repo: &Repository,
    repo_path: &RepoPath,
    status: FileStatus,
    cx: &App,
) -> PathKey {
    let settings = GitPanelSettings::get_global(cx);
    let sort_prefix = if settings.group_by != GitPanelGroupBy::Status {
        TRACKED_SORT_PREFIX
    } else if repo.had_conflict_on_last_merge_head_change(repo_path) {
        CONFLICT_SORT_PREFIX
    } else if status.is_created() {
        NEW_SORT_PREFIX
    } else {
        TRACKED_SORT_PREFIX
    };
    let path = project_diff_sort_path(repo_path, settings.tree_view, settings.sort_by);
    PathKey::with_sort_prefix(sort_prefix, path)
}

fn project_diff_sort_path(
    repo_path: &RelPath,
    tree_view: bool,
    sort_by: GitPanelSortBy,
) -> Arc<RelPath> {
    if tree_view {
        tree_sort_path(repo_path)
    } else {
        match sort_by {
            GitPanelSortBy::Path => repo_path.into_arc(),
            GitPanelSortBy::Name => name_sort_path(repo_path),
        }
    }
}

/// Builds a synthetic path that sorts by file name first, falling back to the
/// full path to keep the key unique per file.
fn name_sort_path(repo_path: &RelPath) -> Arc<RelPath> {
    let Some(file_name) = repo_path.file_name() else {
        return repo_path.into_arc();
    };
    let synthetic = format!("{}/{}", file_name, repo_path.as_unix_str());
    RelPath::from_unix_str(&synthetic)
        .map(|path| path.into_arc())
        .unwrap_or_else(|_| repo_path.into_arc())
}

/// Builds a synthetic path whose natural component-wise ordering reproduces a
/// folder-first tree order. Each directory component is prefixed with a NUL
/// byte, which can never appear in a real path component and sorts before every
/// printable character, so at each level directories sort before files.
fn tree_sort_path(repo_path: &RelPath) -> Arc<RelPath> {
    let components: Vec<&str> = repo_path.components().collect();
    if components.len() <= 1 {
        return repo_path.into_arc();
    }
    let last = components.len() - 1;
    let mut synthetic = String::new();
    for (index, component) in components.into_iter().enumerate() {
        if index > 0 {
            synthetic.push('/');
        }
        if index < last {
            synthetic.push('\0');
        }
        synthetic.push_str(component);
    }
    RelPath::from_unix_str(&synthetic)
        .map(|path| path.into_arc())
        .unwrap_or_else(|_| repo_path.into_arc())
}

fn partition_review_comments_json(
    comments_json: &str,
    scope: ReviewCommentArchiveScope,
) -> anyhow::Result<(String, String, usize)> {
    let snapshot: serde_json::Value =
        serde_json::from_str(comments_json).context("deserializing review comments")?;
    let schema_version = snapshot
        .get("schema_version")
        .and_then(|version| version.as_u64())
        .unwrap_or(1);
    let comments = snapshot
        .get("comments")
        .and_then(|comments| comments.as_array())
        .cloned()
        .unwrap_or_default();
    let mut archived = Vec::new();
    let mut remaining = Vec::new();
    for comment in comments {
        if scope.includes(&comment) {
            archived.push(comment);
        } else {
            remaining.push(comment);
        }
    }

    let archived_count = archived.len();
    let archived_json = serde_json::json!({
        "schema_version": schema_version,
        "comments": archived,
    })
    .to_string();
    let remaining_json = serde_json::json!({
        "schema_version": schema_version,
        "comments": remaining,
    })
    .to_string();
    Ok((archived_json, remaining_json, archived_count))
}

fn review_comments_json_rounds(comments_json: &str) -> Vec<u64> {
    let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(comments_json) else {
        return Vec::new();
    };
    let Some(comments) = snapshot
        .get("comments")
        .and_then(|comments| comments.as_array())
    else {
        return Vec::new();
    };

    let mut rounds = Vec::new();
    for comment in comments {
        if let Some(round) = comment_review_round(comment) {
            rounds.push(round);
        }
        if let Some(replies) = comment
            .get("replies")
            .and_then(|replies| replies.as_array())
        {
            rounds.extend(replies.iter().filter_map(comment_review_round));
        }
    }
    rounds.sort_unstable();
    rounds.dedup();
    rounds
}

fn merge_review_comments_json(
    active_comments_json: &str,
    archived_comments_json: &str,
) -> anyhow::Result<(String, usize)> {
    let active_snapshot: serde_json::Value =
        serde_json::from_str(active_comments_json).context("deserializing active comments")?;
    let archived_snapshot: serde_json::Value =
        serde_json::from_str(archived_comments_json).context("deserializing archived comments")?;
    let schema_version = active_snapshot
        .get("schema_version")
        .and_then(|version| version.as_u64())
        .or_else(|| {
            archived_snapshot
                .get("schema_version")
                .and_then(|version| version.as_u64())
        })
        .unwrap_or(1);
    let mut comments = active_snapshot
        .get("comments")
        .and_then(|comments| comments.as_array())
        .cloned()
        .unwrap_or_default();
    let archived_comments = archived_snapshot
        .get("comments")
        .and_then(|comments| comments.as_array())
        .cloned()
        .unwrap_or_default();

    let mut next_comment_id = comments
        .iter()
        .filter_map(|comment| comment.get("id").and_then(|id| id.as_i64()))
        .max()
        .unwrap_or(-1)
        + 1;
    let mut next_reply_id = comments
        .iter()
        .filter_map(|comment| {
            comment
                .get("replies")
                .and_then(|replies| replies.as_array())
        })
        .flatten()
        .filter_map(|reply| reply.get("id").and_then(|id| id.as_i64()))
        .max()
        .unwrap_or(999)
        + 1;

    let mut restored_count = 0;
    for mut comment in archived_comments {
        let Some(comment_object) = comment.as_object_mut() else {
            continue;
        };
        comment_object.insert("id".to_string(), serde_json::json!(next_comment_id));
        next_comment_id += 1;
        if let Some(replies) = comment_object
            .get_mut("replies")
            .and_then(|replies| replies.as_array_mut())
        {
            for reply in replies {
                if let Some(reply_object) = reply.as_object_mut() {
                    reply_object.insert("id".to_string(), serde_json::json!(next_reply_id));
                    next_reply_id += 1;
                }
            }
        }
        comments.push(comment);
        restored_count += 1;
    }

    comments.sort_by(|left, right| {
        let left_file = left
            .get("file")
            .and_then(|file| file.as_str())
            .unwrap_or("");
        let right_file = right
            .get("file")
            .and_then(|file| file.as_str())
            .unwrap_or("");
        let left_line = left
            .get("line_start")
            .and_then(|line| line.as_i64())
            .unwrap_or_default();
        let right_line = right
            .get("line_start")
            .and_then(|line| line.as_i64())
            .unwrap_or_default();
        let left_id = left
            .get("id")
            .and_then(|id| id.as_i64())
            .unwrap_or_default();
        let right_id = right
            .get("id")
            .and_then(|id| id.as_i64())
            .unwrap_or_default();
        left_file
            .cmp(right_file)
            .then(left_line.cmp(&right_line))
            .then(left_id.cmp(&right_id))
    });

    let merged_json = serde_json::json!({
        "schema_version": schema_version,
        "comments": comments,
    })
    .to_string();
    Ok((merged_json, restored_count))
}

fn merge_review_comments_for_save(
    existing_comments_json: Option<&str>,
    incoming_comments_json: &str,
) -> anyhow::Result<String> {
    let Some(existing_comments_json) = existing_comments_json else {
        return Ok(incoming_comments_json.to_string());
    };

    let existing_snapshot: serde_json::Value =
        serde_json::from_str(existing_comments_json).context("deserializing existing comments")?;
    let incoming_snapshot: serde_json::Value =
        serde_json::from_str(incoming_comments_json).context("deserializing incoming comments")?;
    let schema_version = incoming_snapshot
        .get("schema_version")
        .and_then(|version| version.as_u64())
        .or_else(|| {
            existing_snapshot
                .get("schema_version")
                .and_then(|version| version.as_u64())
        })
        .unwrap_or(1);
    let existing_comments = existing_snapshot
        .get("comments")
        .and_then(|comments| comments.as_array())
        .cloned()
        .unwrap_or_default();
    let mut incoming_comments = incoming_snapshot
        .get("comments")
        .and_then(|comments| comments.as_array())
        .cloned()
        .unwrap_or_default();

    let mut next_reply_id = existing_comments
        .iter()
        .chain(incoming_comments.iter())
        .filter_map(|comment| {
            comment
                .get("replies")
                .and_then(|replies| replies.as_array())
        })
        .flatten()
        .filter_map(|reply| reply.get("id").and_then(|id| id.as_i64()))
        .max()
        .unwrap_or(999)
        + 1;

    for existing_comment in existing_comments {
        let Some(existing_id) = existing_comment.get("id").and_then(|id| id.as_i64()) else {
            continue;
        };
        let Some(incoming_comment) = incoming_comments
            .iter_mut()
            .find(|comment| comment.get("id").and_then(|id| id.as_i64()) == Some(existing_id))
        else {
            incoming_comments.push(existing_comment);
            continue;
        };

        preserve_existing_review_round(incoming_comment, &existing_comment);
        merge_review_comment_replies(incoming_comment, existing_comment, &mut next_reply_id);
    }

    Ok(serde_json::json!({
        "schema_version": schema_version,
        "comments": incoming_comments,
    })
    .to_string())
}

fn merge_and_stamp_review_comments_for_save(
    existing_comments_json: Option<&str>,
    incoming_comments_json: &str,
) -> anyhow::Result<(String, u64)> {
    let merged_comments_json =
        merge_review_comments_for_save(existing_comments_json, incoming_comments_json)?;
    stamp_unstamped_review_comments(&merged_comments_json)
}

fn merge_review_comment_replies(
    incoming_comment: &mut serde_json::Value,
    existing_comment: serde_json::Value,
    next_reply_id: &mut i64,
) {
    let existing_replies = existing_comment
        .get("replies")
        .and_then(|replies| replies.as_array())
        .cloned()
        .unwrap_or_default();
    if existing_replies.is_empty() {
        return;
    }

    let Some(incoming_object) = incoming_comment.as_object_mut() else {
        return;
    };
    let incoming_replies = incoming_object
        .entry("replies")
        .or_insert_with(|| serde_json::json!([]));
    let Some(incoming_replies) = incoming_replies.as_array_mut() else {
        return;
    };

    let mut merged_replies = existing_replies;
    for incoming_reply in incoming_replies.drain(..) {
        let Some(incoming_id) = incoming_reply.get("id").and_then(|id| id.as_i64()) else {
            merged_replies.push(incoming_reply);
            continue;
        };
        if let Some(existing_reply) = merged_replies
            .iter_mut()
            .find(|reply| reply.get("id").and_then(|id| id.as_i64()) == Some(incoming_id))
        {
            if review_replies_match(existing_reply, &incoming_reply) {
                let mut incoming_reply = incoming_reply;
                preserve_existing_review_round(&mut incoming_reply, existing_reply);
                *existing_reply = incoming_reply;
            } else {
                let mut incoming_reply = incoming_reply;
                if let Some(reply_object) = incoming_reply.as_object_mut() {
                    reply_object.insert("id".to_string(), serde_json::json!(*next_reply_id));
                    *next_reply_id += 1;
                }
                merged_replies.push(incoming_reply);
            }
        } else {
            merged_replies.push(incoming_reply);
        }
    }

    *incoming_replies = merged_replies;
}

fn preserve_existing_review_round(
    incoming_item: &mut serde_json::Value,
    existing_item: &serde_json::Value,
) {
    if incoming_item.get("review_round").is_some() {
        return;
    }
    let Some(review_round) = existing_item.get("review_round").cloned() else {
        return;
    };
    if let Some(incoming_object) = incoming_item.as_object_mut() {
        incoming_object.insert("review_round".to_string(), review_round);
    }
}

fn review_replies_match(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    left.get("author") == right.get("author") && left.get("body") == right.get("body")
}

fn unix_time_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn review_comments_json_restorable_comment_count(comments_json: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(comments_json)
        .ok()
        .and_then(|json| {
            json.get("comments")
                .and_then(|comments| comments.as_array())
                .cloned()
        })
        .map(|comments| {
            comments
                .iter()
                .filter(|comment| {
                    comment.get("side").and_then(|side| side.as_str()) == Some("new")
                        && comment
                            .get("deleted_on")
                            .is_none_or(|deleted_on| deleted_on.is_null())
                })
                .count()
        })
        .unwrap_or(0)
}

fn preserve_live_review_comment_locations(
    base_comments_json: &str,
    live_comments_json: &str,
    incoming_comments_json: &str,
) -> anyhow::Result<Option<String>> {
    const LOCATION_FIELDS: [&str; 5] = ["file", "side", "hunk_line", "line_start", "line_end"];

    let base: serde_json::Value =
        serde_json::from_str(base_comments_json).context("deserializing base review comments")?;
    let live: serde_json::Value =
        serde_json::from_str(live_comments_json).context("deserializing live review comments")?;
    let mut incoming: serde_json::Value = serde_json::from_str(incoming_comments_json)
        .context("deserializing incoming review comments")?;
    let unique_ids = |snapshot: &serde_json::Value| {
        let mut unique = HashSet::default();
        let mut duplicates = HashSet::default();
        for id in snapshot
            .get("comments")
            .and_then(|comments| comments.as_array())
            .into_iter()
            .flatten()
            .filter_map(|comment| comment.get("id").and_then(|id| id.as_u64()))
        {
            if !unique.insert(id) {
                duplicates.insert(id);
            }
        }
        unique.retain(|id| !duplicates.contains(id));
        unique
    };
    let base_ids = unique_ids(&base);
    let live_ids = unique_ids(&live);
    let incoming_ids = unique_ids(&incoming);
    let stable_ids = base_ids
        .intersection(&live_ids)
        .copied()
        .filter(|id| incoming_ids.contains(id))
        .collect::<HashSet<_>>();
    let locations_by_id = |snapshot: &serde_json::Value| {
        snapshot
            .get("comments")
            .and_then(|comments| comments.as_array())
            .into_iter()
            .flatten()
            .filter_map(|comment| {
                let id = comment.get("id")?.as_u64()?;
                stable_ids
                    .contains(&id)
                    .then(|| (id, LOCATION_FIELDS.map(|field| comment.get(field).cloned())))
            })
            .collect::<HashMap<_, _>>()
    };
    let base_locations = locations_by_id(&base);
    let live_locations = locations_by_id(&live)
        .into_iter()
        .filter(|(id, location)| base_locations.get(id) != Some(location))
        .collect::<HashMap<_, _>>();

    let mut changed = false;
    if let Some(comments) = incoming
        .get_mut("comments")
        .and_then(|comments| comments.as_array_mut())
    {
        for comment in comments {
            let Some(id) = comment.get("id").and_then(|id| id.as_u64()) else {
                continue;
            };
            let Some(location) = live_locations.get(&id) else {
                continue;
            };
            let Some(comment) = comment.as_object_mut() else {
                continue;
            };
            for (field, value) in LOCATION_FIELDS.into_iter().zip(location) {
                match value {
                    Some(value) if comment.get(field) != Some(value) => {
                        comment.insert(field.to_string(), value.clone());
                        changed = true;
                    }
                    None if comment.remove(field).is_some() => changed = true,
                    _ => {}
                }
            }
        }
    }

    if changed {
        Ok(Some(
            serde_json::to_string_pretty(&incoming)
                .context("serializing rebased review comments")?,
        ))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
fn review_comments_json_comment_count(comments_json: &str) -> usize {
    serde_json::from_str::<serde_json::Value>(comments_json)
        .ok()
        .and_then(|json| {
            json.get("comments")
                .and_then(|comments| comments.as_array())
                .map(|comments| comments.len())
        })
        .unwrap_or(0)
}

impl EventEmitter<EditorEvent> for ProjectDiff {}

impl Focusable for ProjectDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Item for ProjectDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.deactivated(window, cx);
            })
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.navigate(data, window, cx)
            })
        })
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        match self.diff_base(cx) {
            DiffBase::Head => Some("Project Diff".into()),
            DiffBase::Merge { .. } => Some("Branch Diff".into()),
        }
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.branch_diff.read(cx).diff_base() {
            DiffBase::Head => "Uncommitted Changes".into(),
            DiffBase::Merge { base_ref } => format!("Changes since {}", base_ref).into(),
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Project Diff Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>, _cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        let editor = self.editor.read(cx).focused_editor().read(cx);
        let multibuffer = editor.buffer().read(cx);
        let position = editor.selections.newest_anchor().head();
        let snapshot = multibuffer.snapshot(cx);
        let (text_anchor, _) = snapshot.anchor_to_buffer_anchor(position)?;
        let buffer = multibuffer.buffer(text_anchor.buffer_id)?;

        let file = buffer.read(cx).file()?;
        Some(ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        })
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, _| {
                primary_editor.set_nav_history(Some(nav_history));
            })
        });
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        Task::ready(Some(cx.new(|cx| {
            ProjectDiff::new(self.project.clone(), workspace, window, cx)
        })))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.save(options, project, window, cx)
            })
        })
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |primary_editor, cx| {
                primary_editor.reload(project, window, cx)
            })
        })
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ProjectDiff {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.multibuffer.read(cx).is_empty();
        let is_loading = self.branch_diff.read(cx).is_tree_base_loading() || !self._task.is_ready();
        let orphaned_review_comments = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .orphaned_review_comment_summaries();

        let is_branch_diff_view = matches!(self.diff_base(cx), DiffBase::Merge { .. });

        div()
            .track_focus(&self.focus_handle)
            .key_context(if is_empty { "EmptyPane" } else { "GitDiff" })
            .when(is_branch_diff_view, |this| {
                this.on_action(cx.listener(Self::review_diff))
            })
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .when(is_empty && is_loading, |el| {
                let rems = TextSize::Large.rems(cx);
                el.child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Custom(rems))
                        .color(Color::Accent)
                        .with_rotate_animation(3)
                        .into_any_element(),
                )
            })
            .when(is_empty && !is_loading, |el| {
                let remote_button = if let Some(panel) = self
                    .workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<GitPanel>(cx))
                {
                    panel.update(cx, |panel, cx| panel.render_remote_button(cx))
                } else {
                    None
                };
                let keybinding_focus_handle = self.focus_handle(cx);
                el.child(
                    v_flex()
                        .size_full()
                        .gap_1()
                        .when(!orphaned_review_comments.is_empty(), |el| {
                            el.child(Self::render_orphaned_review_comments(
                                orphaned_review_comments.clone(),
                                self.orphaned_review_comments_expanded,
                                cx.weak_entity(),
                                cx,
                            ))
                        })
                        .child(
                            v_flex()
                                .flex_1()
                                .items_center()
                                .justify_center()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .justify_around()
                                        .child(Label::new("No uncommitted changes")),
                                )
                                .map(|el| match remote_button {
                                    Some(button) => {
                                        el.child(h_flex().justify_around().child(button))
                                    }
                                    None => el.child(
                                        h_flex()
                                            .justify_around()
                                            .child(Label::new("Remote up to date")),
                                    ),
                                })
                                .child(
                                    h_flex().justify_around().mt_1().child(
                                        Button::new("project-diff-close-button", "Close")
                                            .key_binding(KeyBinding::for_action_in(
                                                &CloseActiveItem::default(),
                                                &keybinding_focus_handle,
                                                cx,
                                            ))
                                            .on_click(move |_, window, cx| {
                                                window.focus(&keybinding_focus_handle, cx);
                                                window.dispatch_action(
                                                    Box::new(CloseActiveItem::default()),
                                                    cx,
                                                );
                                            }),
                                    ),
                                ),
                        ),
                )
            })
            .when(!is_empty, |el| {
                el.child(
                    v_flex()
                        .size_full()
                        .when(!orphaned_review_comments.is_empty(), |el| {
                            el.child(Self::render_orphaned_review_comments(
                                orphaned_review_comments,
                                self.orphaned_review_comments_expanded,
                                cx.weak_entity(),
                                cx,
                            ))
                        })
                        .child(self.editor.clone()),
                )
            })
    }
}

impl ProjectDiff {
    fn render_orphaned_review_comments(
        comments: Vec<OrphanedReviewCommentSummary>,
        expanded: bool,
        this: WeakEntity<ProjectDiff>,
        cx: &App,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let count = comments.len();
        let title = format!(
            "{} outdated review comment{} no longer {} a visible diff",
            count,
            if count == 1 { "" } else { "s" },
            if count == 1 { "has" } else { "have" }
        );

        v_flex()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .id("orphaned-review-comments-header")
                    .cursor_pointer()
                    .gap_2()
                    .items_center()
                    .on_click(move |_, _window, cx| {
                        this.update(cx, |this, cx| {
                            this.orphaned_review_comments_expanded =
                                !this.orphaned_review_comments_expanded;
                            cx.notify();
                        })
                        .log_err();
                    })
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted)),
            )
            .when(expanded, |el| {
                let mut list = v_flex().w_full().gap_1().max_h_32();
                gpui::InteractiveElement::interactivity(&mut list)
                    .base_style
                    .overflow
                    .y = Some(gpui::Overflow::Scroll);
                el.child(list.children(comments.into_iter().map(|comment| {
                    let line = if comment.line_start == comment.line_end {
                        format!("{}:{}", comment.file, comment.line_start)
                    } else {
                        format!(
                            "{}:{}-{}",
                            comment.file, comment.line_start, comment.line_end
                        )
                    };
                    let reason = match comment.outdated_reason.as_deref() {
                        Some("file_not_in_diff") => "file no longer has a visible diff",
                        Some("line_not_in_diff") => "line no longer has a visible diff",
                        Some("line_changed") => "commented text was changed",
                        _ => "change no longer has a visible diff",
                    };
                    let reply_label = match comment.replies.len() {
                        0 => None,
                        1 => Some("1 reply".to_string()),
                        count => Some(format!("{count} replies")),
                    };

                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_0p5()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.element_background)
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .child(Label::new(line).size(LabelSize::Small).color(Color::Muted))
                                .child(
                                    Label::new("Outdated")
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                )
                                .child(
                                    Label::new(reason)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .when_some(reply_label, |el, reply_label| {
                                    el.child(
                                        Label::new(reply_label)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }),
                        )
                        .child(Label::new(comment.body).size(LabelSize::Small))
                        .children(comment.replies.into_iter().map(|reply| {
                            v_flex()
                                .ml_4()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    Label::new(reply.author)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(reply.body).size(LabelSize::Small))
                        }))
                })))
            })
            .into_any_element()
    }
}

impl SerializableItem for ProjectDiff {
    fn serialized_item_kind() -> &'static str {
        "ProjectDiff"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::ProjectDiffDb::global(cx);
        window.spawn(cx, async move |cx| {
            let (diff_base, follows_default_branch) = db.get_diff_base(item_id, workspace_id)?;

            let diff = cx.update(|window, cx| {
                let branch_diff = cx
                    .new(|cx| branch_diff::BranchDiff::new(diff_base, project.clone(), window, cx));
                let workspace = workspace.upgrade().context("workspace gone")?;
                anyhow::Ok(cx.new(|cx| {
                    ProjectDiff::new_impl(
                        branch_diff,
                        project,
                        workspace,
                        follows_default_branch,
                        window,
                        cx,
                    )
                }))
            })??;

            Ok(diff)
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let diff_base = self.diff_base(cx).clone();
        let follows_default_branch = self.follows_default_branch;

        let db = persistence::ProjectDiffDb::global(cx);
        Some(cx.background_spawn({
            async move {
                db.save_diff_base(
                    item_id,
                    workspace_id,
                    diff_base.clone(),
                    follows_default_branch,
                )
                .await
            }
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub(crate) mod persistence {

    use anyhow::Context as _;
    use db::{
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use project::git_store::{
        branch_diff::DiffBase, diff_buffer_list::DiffBase as ProjectDiffBase,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct ProjectDiffDb(ThreadSafeConnection);

    impl Domain for ProjectDiffDb {
        const NAME: &str = stringify!(ProjectDiffDb);

        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE project_diffs(
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    diff_base TEXT,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            sql!(
                CREATE TABLE project_diff_review_comments(
                    workspace_id INTEGER,
                    review_key TEXT,
                    comments_json TEXT,

                    PRIMARY KEY(workspace_id, review_key),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            sql!(
                CREATE TABLE project_diff_review_comments_v2(
                    workspace_id INTEGER,
                    review_key TEXT,
                    session_id TEXT,
                    comments_json TEXT,
                    created_on INTEGER,
                    archived_on INTEGER,
                    archive_scope TEXT,

                    PRIMARY KEY(workspace_id, review_key, session_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;

                INSERT INTO project_diff_review_comments_v2(
                    workspace_id,
                    review_key,
                    session_id,
                    comments_json,
                    created_on,
                    archived_on,
                    archive_scope
                )
                SELECT
                    workspace_id,
                    review_key,
                    "active",
                    comments_json,
                    CAST(strftime("%s", "now") AS INTEGER) * 1000,
                    NULL,
                    NULL
                FROM project_diff_review_comments;

                DROP TABLE project_diff_review_comments;
                ALTER TABLE project_diff_review_comments_v2 RENAME TO project_diff_review_comments;
            ),
            sql!(
                ALTER TABLE project_diffs ADD COLUMN follows_default_branch INTEGER DEFAULT 0;
                UPDATE project_diffs SET follows_default_branch = TRUE WHERE diff_base LIKE "%Merge%";
            ),
            sql!(UPDATE project_diffs SET follows_default_branch = FALSE;),
            sql!(
                UPDATE project_diffs SET follows_default_branch = TRUE
                WHERE diff_base LIKE "%origin/main%"
                   OR diff_base LIKE "%upstream/main%";
            ),
        ];
    }

    db::static_connection!(ProjectDiffDb, [WorkspaceDb]);

    impl ProjectDiffDb {
        pub async fn save_diff_base(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
            diff_base: DiffBase,
            follows_default_branch: bool,
        ) -> anyhow::Result<()> {
            self.write(move |connection| {
                let sql_stmt = sql!(
                    INSERT OR REPLACE INTO project_diffs(item_id, workspace_id, diff_base, follows_default_branch) VALUES (?, ?, ?, ?)
                );
                let diff_base_str = serde_json::to_string(&diff_base)?;
                let follows_default_branch = i64::from(follows_default_branch);
                let mut query = connection
                    .exec_bound::<(ItemId, WorkspaceId, String, i64)>(sql_stmt)?;
                query((
                    item_id,
                    workspace_id,
                    diff_base_str,
                    follows_default_branch,
                ))
                .context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    sql_stmt
                ))
            })
            .await
        }

        pub fn get_diff_base(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
        ) -> anyhow::Result<(DiffBase, bool)> {
            let sql_stmt = sql!(
                SELECT diff_base, follows_default_branch FROM project_diffs WHERE item_id =  ?AND workspace_id =  ?
            );
            let row = self.select_row_bound::<(ItemId, WorkspaceId), (String, i64)>(sql_stmt)?((
                item_id,
                workspace_id,
            ))
            .context(::std::format!(
                "Error in get_diff_base, select_row_bound failed to execute or parse for: {}",
                sql_stmt
            ))?;
            let Some((diff_base_str, follows_default_branch)) = row else {
                return Ok((DiffBase::Head, false));
            };
            let diff_base =
                serde_json::from_str(&diff_base_str).context("deserializing diff base")?;
            Ok((diff_base, follows_default_branch != 0))
        }

        pub fn get_project_diff_base(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
        ) -> anyhow::Result<ProjectDiffBase> {
            let (diff_base, _) = self.get_diff_base(item_id, workspace_id)?;
            let serialized = serde_json::to_string(&diff_base)?;
            serde_json::from_str(&serialized).context("converting persisted diff base")
        }

        pub async fn save_project_diff_base(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
            diff_base: ProjectDiffBase,
        ) -> anyhow::Result<()> {
            let serialized = serde_json::to_string(&diff_base)?;
            let diff_base =
                serde_json::from_str(&serialized).context("converting project diff base")?;
            self.save_diff_base(item_id, workspace_id, diff_base, false)
                .await
        }

        pub fn save_review_comments(
            &self,
            workspace_id: WorkspaceId,
            review_key: String,
            comments_json: String,
        ) -> impl Future<Output = anyhow::Result<String>> + use<> {
            let save =
                self.save_review_comments_inner(workspace_id, review_key, comments_json, false);
            async move { Ok(save.await?.0) }
        }

        pub fn save_and_stamp_review_comments(
            &self,
            workspace_id: WorkspaceId,
            review_key: String,
            comments_json: String,
        ) -> impl Future<Output = anyhow::Result<(String, u64)>> + use<> {
            let save =
                self.save_review_comments_inner(workspace_id, review_key, comments_json, true);
            async move {
                let (comments_json, review_round) = save.await?;
                Ok((
                    comments_json,
                    review_round.context("stamped save did not allocate a review round")?,
                ))
            }
        }

        fn save_review_comments_inner(
            &self,
            workspace_id: WorkspaceId,
            review_key: String,
            comments_json: String,
            stamp_unstamped: bool,
        ) -> impl Future<Output = anyhow::Result<(String, Option<u64>)>> + use<> {
            self.write(move |connection| {
                let select_sql = sql!(
                    SELECT comments_json FROM project_diff_review_comments
                    WHERE workspace_id = ? AND review_key = ? AND session_id = "active" AND archived_on IS NULL
                );
                let mut select_existing =
                    connection.select_bound::<(WorkspaceId, String), String>(select_sql)?;
                let existing_comments_json = select_existing((workspace_id, review_key.clone()))?
                    .into_iter()
                    .next();
                let (comments_json, review_round) = if stamp_unstamped {
                    let (comments_json, review_round) =
                        super::merge_and_stamp_review_comments_for_save(
                            existing_comments_json.as_deref(),
                            &comments_json,
                        )?;
                    (comments_json, Some(review_round))
                } else {
                    (
                        super::merge_review_comments_for_save(
                            existing_comments_json.as_deref(),
                            &comments_json,
                        )?,
                        None,
                    )
                };

                let sql_stmt = sql!(
                    INSERT INTO project_diff_review_comments(
                        workspace_id,
                        review_key,
                        session_id,
                        comments_json,
                        created_on,
                        archived_on,
                        archive_scope
                    ) VALUES (?, ?, "active", ?, CAST(strftime("%s", "now") AS INTEGER) * 1000, NULL, NULL)
                    ON CONFLICT(workspace_id, review_key, session_id) DO UPDATE SET
                        comments_json = excluded.comments_json,
                        archived_on = NULL,
                        archive_scope = NULL
                );
                let mut query = connection.exec_bound::<(WorkspaceId, String, String)>(sql_stmt)?;
                query((workspace_id, review_key, comments_json.clone())).context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    sql_stmt
                ))?;
                Ok((comments_json, review_round))
            })
        }

        pub fn replace_review_comments_if_unchanged(
            &self,
            workspace_id: WorkspaceId,
            review_key: String,
            expected_comments_json: String,
            replacement_comments_json: String,
        ) -> impl Future<Output = anyhow::Result<()>> + use<> {
            self.write(move |connection| {
                let sql_stmt = sql!(
                    UPDATE project_diff_review_comments
                    SET comments_json = ?
                    WHERE workspace_id = ?
                      AND review_key = ?
                      AND session_id = "active"
                      AND archived_on IS NULL
                      AND comments_json = ?
                );
                let mut query =
                    connection.exec_bound::<(String, WorkspaceId, String, String)>(sql_stmt)?;
                query((
                    replacement_comments_json,
                    workspace_id,
                    review_key,
                    expected_comments_json,
                ))
                .context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    sql_stmt
                ))
            })
        }

        pub fn flush_writes(&self) -> impl Future<Output = anyhow::Result<()>> + use<> {
            self.write(|_| Ok(()))
        }

        pub fn get_review_comments(
            &self,
            workspace_id: WorkspaceId,
            review_key: &str,
        ) -> anyhow::Result<Option<String>> {
            let sql_stmt = sql!(
                SELECT comments_json FROM project_diff_review_comments
                WHERE workspace_id = ? AND review_key = ? AND session_id = "active" AND archived_on IS NULL
            );
            self.select_row_bound::<(WorkspaceId, String), String>(sql_stmt)?((
                workspace_id,
                review_key.to_string(),
            ))
            .context(format!(
                "select_row_bound failed to execute or parse for: {}",
                sql_stmt
            ))
        }

        pub fn modified_on(&self) -> Option<std::time::SystemTime> {
            let db_path = db::db_path(db::database_dir(), *db::RELEASE_CHANNEL);
            let wal_path = {
                let mut path = db_path.clone().into_os_string();
                path.push("-wal");
                std::path::PathBuf::from(path)
            };
            [db_path, wal_path]
                .into_iter()
                .filter_map(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
                .max()
        }

        pub fn latest_archived_review_comments(
            &self,
            workspace_id: WorkspaceId,
            review_key: &str,
            archive_scope: Option<&str>,
        ) -> anyhow::Result<Option<(String, String)>> {
            if let Some(archive_scope) = archive_scope {
                let sql_stmt = sql!(
                    SELECT session_id, comments_json FROM project_diff_review_comments
                    WHERE workspace_id = ? AND review_key = ? AND archive_scope = ? AND archived_on IS NOT NULL
                    ORDER BY archived_on DESC, rowid DESC
                    LIMIT 1
                );
                self.select_row_bound::<(WorkspaceId, String, String), (String, String)>(sql_stmt)?(
                    (
                        workspace_id,
                        review_key.to_string(),
                        archive_scope.to_string(),
                    ),
                )
                .context(format!(
                    "select_row_bound failed to execute or parse for: {}",
                    sql_stmt
                ))
            } else {
                let sql_stmt = sql!(
                    SELECT session_id, comments_json FROM project_diff_review_comments
                    WHERE workspace_id = ? AND review_key = ? AND archived_on IS NOT NULL
                    AND (archive_scope IS NULL OR archive_scope != "deleted")
                    ORDER BY archived_on DESC, rowid DESC
                    LIMIT 1
                );
                self.select_row_bound::<(WorkspaceId, String), (String, String)>(sql_stmt)?((
                    workspace_id,
                    review_key.to_string(),
                ))
                .context(format!(
                    "select_row_bound failed to execute or parse for: {}",
                    sql_stmt
                ))
            }
        }

        pub fn archive_review_comments(
            &self,
            workspace_id: WorkspaceId,
            review_key: String,
            session_id: String,
            archived_comments_json: String,
            active_comments_json: String,
            archived_on: u128,
            archive_scope: String,
        ) -> impl Future<Output = anyhow::Result<()>> + use<> {
            self.write(move |connection| {
                let archived_on = i64::try_from(archived_on).unwrap_or(i64::MAX);
                let archive_sql = sql!(
                    INSERT INTO project_diff_review_comments(
                        workspace_id,
                        review_key,
                        session_id,
                        comments_json,
                        created_on,
                        archived_on,
                        archive_scope
                    ) VALUES (?, ?, ?, ?, ?, ?, ?)
                );
                let mut archive_query = connection.exec_bound::<(
                    WorkspaceId,
                    String,
                    String,
                    String,
                    i64,
                    i64,
                    String,
                )>(archive_sql)?;
                archive_query((
                    workspace_id,
                    review_key.clone(),
                    session_id,
                    archived_comments_json,
                    archived_on,
                    archived_on,
                    archive_scope,
                ))
                .context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    archive_sql
                ))?;

                let active_sql = sql!(
                    INSERT INTO project_diff_review_comments(
                        workspace_id,
                        review_key,
                        session_id,
                        comments_json,
                        created_on,
                        archived_on,
                        archive_scope
                    ) VALUES (?, ?, "active", ?, CAST(strftime("%s", "now") AS INTEGER) * 1000, NULL, NULL)
                    ON CONFLICT(workspace_id, review_key, session_id) DO UPDATE SET
                        comments_json = excluded.comments_json,
                        archived_on = NULL,
                        archive_scope = NULL
                );
                let mut active_query =
                    connection.exec_bound::<(WorkspaceId, String, String)>(active_sql)?;
                active_query((workspace_id, review_key, active_comments_json)).context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    active_sql
                ))
            })
        }

        #[cfg(test)]
        pub async fn insert_test_workspace(&self, workspace_id: WorkspaceId) -> anyhow::Result<()> {
            self.write(move |connection| {
                let sql_stmt = sql!(INSERT OR IGNORE INTO workspaces(workspace_id) VALUES (?));
                let mut query = connection.exec_bound::<WorkspaceId>(sql_stmt)?;
                query(workspace_id).context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    sql_stmt
                ))
            })
            .await
        }

        #[cfg(test)]
        pub async fn delete_test_workspace(&self, workspace_id: WorkspaceId) -> anyhow::Result<()> {
            self.write(move |connection| {
                let sql_stmt = sql!(DELETE FROM workspaces WHERE workspace_id = ?);
                let mut query = connection.exec_bound::<WorkspaceId>(sql_stmt)?;
                query(workspace_id).context(format!(
                    "exec_bound failed to execute or parse for: {}",
                    sql_stmt
                ))
            })
            .await
        }

        pub fn archived_review_comment_rounds(
            &self,
            workspace_id: WorkspaceId,
            review_key: &str,
        ) -> anyhow::Result<Vec<u64>> {
            let sql_stmt = sql!(
                SELECT DISTINCT archive_scope FROM project_diff_review_comments
                WHERE workspace_id = ? AND review_key = ? AND session_id != "active" AND archived_on IS NOT NULL AND archive_scope LIKE "round:%"
                ORDER BY archive_scope ASC
            );
            let rows = self.select_bound::<(WorkspaceId, String), String>(sql_stmt)?((
                workspace_id,
                review_key.to_string(),
            ))?;
            let mut rounds = rows
                .into_iter()
                .filter_map(|scope| scope.strip_prefix("round:")?.parse::<u64>().ok())
                .collect::<Vec<_>>();
            rounds.sort_unstable();
            rounds.dedup();
            Ok(rounds)
        }

        #[cfg(test)]
        pub fn archived_review_comments_count(
            &self,
            workspace_id: WorkspaceId,
            review_key: &str,
        ) -> anyhow::Result<usize> {
            let sql_stmt = sql!(
                SELECT COUNT(*) FROM project_diff_review_comments
                WHERE workspace_id = ? AND review_key = ? AND archived_on IS NOT NULL
            );
            let count = self.select_row_bound::<(WorkspaceId, String), usize>(sql_stmt)?((
                workspace_id,
                review_key.to_string(),
            ))?;
            Ok(count.unwrap_or(0))
        }
    }
}

pub struct ProjectDiffToolbar {
    project_diff: Option<WeakEntity<ProjectDiff>>,
    workspace: WeakEntity<Workspace>,
}

impl ProjectDiffToolbar {
    pub fn new(workspace: &Workspace, _: &mut Context<Self>) -> Self {
        Self {
            project_diff: None,
            workspace: workspace.weak_handle(),
        }
    }

    fn project_diff(&self, _: &App) -> Option<Entity<ProjectDiff>> {
        self.project_diff.as_ref()?.upgrade()
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project_diff) = self.project_diff(cx) {
            project_diff.focus_handle(cx).focus(window, cx);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }

    fn stage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<GitPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.stage_all(&Default::default(), window, cx);
                    });
                }
            })
            .ok();
    }

    fn unstage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                let Some(panel) = workspace.panel::<GitPanel>(cx) else {
                    return;
                };
                panel.update(cx, |panel, cx| {
                    panel.unstage_all(&Default::default(), window, cx);
                });
            })
            .ok();
    }
}

impl EventEmitter<ToolbarItemEvent> for ProjectDiffToolbar {}

impl ToolbarItemView for ProjectDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.project_diff = active_pane_item
            .and_then(|item| item.act_as::<ProjectDiff>(cx))
            .filter(|item| item.read(cx).diff_base(cx) == &DiffBase::Head)
            .map(|entity| entity.downgrade());
        if self.project_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

struct ButtonStates {
    stage: bool,
    unstage: bool,
    prev_next: bool,
    selection: bool,
    stage_all: bool,
    unstage_all: bool,
}

impl Render for ProjectDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project_diff) = self.project_diff(cx) else {
            return div();
        };
        let focus_handle = project_diff.focus_handle(cx);
        let button_states = project_diff.read(cx).button_states(cx);
        let review_count = project_diff.read(cx).total_review_comment_count();

        h_group_xl()
            .my_neg_1()
            .py_1()
            .items_center()
            .flex_wrap()
            .justify_between()
            .child(
                h_group_sm()
                    .when(button_states.selection, |el| {
                        el.child(
                            Button::new("stage", "Toggle Staged")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Toggle Staged",
                                    &ToggleStaged,
                                    &focus_handle,
                                ))
                                .disabled(!button_states.stage && !button_states.unstage)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&ToggleStaged, window, cx)
                                })),
                        )
                    })
                    .when(!button_states.selection, |el| {
                        el.child(
                            Button::new("stage", "Stage")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Stage and go to next hunk",
                                    &StageAndNext,
                                    &focus_handle,
                                ))
                                .disabled(
                                    !button_states.prev_next
                                        && !button_states.stage_all
                                        && !button_states.unstage_all,
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&StageAndNext, window, cx)
                                })),
                        )
                        .child(
                            Button::new("unstage", "Unstage")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Unstage and go to next hunk",
                                    &UnstageAndNext,
                                    &focus_handle,
                                ))
                                .disabled(
                                    !button_states.prev_next
                                        && !button_states.stage_all
                                        && !button_states.unstage_all,
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&UnstageAndNext, window, cx)
                                })),
                        )
                    }),
            )
            // n.b. the only reason these arrows are here is because we don't
            // support "undo" for staging so we need a way to go back.
            .child(
                h_group_sm()
                    .child(
                        IconButton::new("up", IconName::ArrowUp)
                            .shape(ui::IconButtonShape::Square)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to previous hunk",
                                &GoToPreviousHunk,
                                &focus_handle,
                            ))
                            .disabled(!button_states.prev_next)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToPreviousHunk, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("down", IconName::ArrowDown)
                            .shape(ui::IconButtonShape::Square)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to next hunk",
                                &GoToHunk,
                                &focus_handle,
                            ))
                            .disabled(!button_states.prev_next)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToHunk, window, cx)
                            })),
                    ),
            )
            .child(Divider::vertical())
            .child(
                h_group_sm()
                    .when(
                        button_states.unstage_all && !button_states.stage_all,
                        |el| {
                            el.child(
                                Button::new("unstage-all", "Unstage All")
                                    .tooltip(Tooltip::for_action_title_in(
                                        "Unstage all changes",
                                        &UnstageAll,
                                        &focus_handle,
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.unstage_all(window, cx)
                                    })),
                            )
                        },
                    )
                    .when(
                        !button_states.unstage_all || button_states.stage_all,
                        |el| {
                            el.child(
                                // todo make it so that changing to say "Unstaged"
                                // doesn't change the position.
                                div().child(
                                    Button::new("stage-all", "Stage All")
                                        .disabled(!button_states.stage_all)
                                        .tooltip(Tooltip::for_action_title_in(
                                            "Stage all changes",
                                            &StageAll,
                                            &focus_handle,
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.stage_all(window, cx)
                                        })),
                                ),
                            )
                        },
                    )
                    .child(
                        Button::new("commit", "Commit")
                            .tooltip(Tooltip::for_action_title_in(
                                "Commit",
                                &Commit,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&Commit, window, cx);
                            })),
                    ),
            )
            .when(review_count > 0, |el| {
                el.child(Divider::vertical()).child(
                    render_send_review_to_agent_button(review_count, &focus_handle).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&SendReviewToAgent, window, cx)
                        }),
                    ),
                )
            })
    }
}

pub(crate) fn render_send_review_to_agent_button(
    review_count: usize,
    focus_handle: &FocusHandle,
) -> Button {
    Button::new("send-review", format!("Copy Cmts ({})", review_count))
        .start_icon(
            Icon::new(IconName::ZedAssistant)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .tooltip(Tooltip::for_action_title_in(
            "Copy all review comments as JSON for an agent",
            &SendReviewToAgent,
            focus_handle,
        ))
}

pub struct BranchDiffToolbar {
    project_diff: Option<WeakEntity<ProjectDiff>>,
    _project_diff_subscription: Option<Subscription>,
}

impl BranchDiffToolbar {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            project_diff: None,
            _project_diff_subscription: None,
        }
    }

    fn project_diff(&self, _: &App) -> Option<Entity<ProjectDiff>> {
        self.project_diff.as_ref()?.upgrade()
    }

    fn set_project_diff(
        &mut self,
        project_diff: Option<Entity<ProjectDiff>>,
        cx: &mut Context<Self>,
    ) {
        self._project_diff_subscription = project_diff
            .as_ref()
            .map(|project_diff| cx.observe(project_diff, |_, _, cx| cx.notify()));
        self.project_diff = project_diff.map(|project_diff| project_diff.downgrade());
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project_diff) = self.project_diff(cx) {
            project_diff.focus_handle(cx).focus(window, cx);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }

    fn copy_amp_prompt(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_diff) = self.project_diff(cx) else {
            return;
        };
        let Some(workspace) = project_diff.read(cx).workspace.upgrade() else {
            return;
        };
        let Some(workspace_id) = workspace.read(cx).database_id() else {
            workspace.update(cx, |workspace, cx| {
                ProjectDiff::show_copy_amp_prompt_result(
                    workspace,
                    CopyAmpPromptResult::Failed("This workspace cannot save review comments."),
                    cx,
                );
            });
            return;
        };
        let result = project_diff.update(cx, |project_diff, cx| {
            project_diff.copy_amp_prompt_for_unstamped_review_comments(workspace_id, window, cx)
        });
        workspace.update(cx, |workspace, cx| {
            ProjectDiff::show_copy_amp_prompt_result(workspace, result, cx);
        });
    }
}

impl EventEmitter<ToolbarItemEvent> for BranchDiffToolbar {}

impl ToolbarItemView for BranchDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let project_diff = active_pane_item
            .and_then(|item| item.act_as::<ProjectDiff>(cx))
            .filter(|item| matches!(item.read(cx).diff_base(cx), DiffBase::Merge { .. }));
        self.set_project_diff(project_diff, cx);
        if self.project_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for BranchDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project_diff) = self.project_diff(cx) else {
            return div();
        };
        let focus_handle = project_diff.focus_handle(cx);
        let project_diff_state = project_diff.read(cx);
        let review_count = project_diff_state.total_review_comment_count();
        let copy_amp_prompt_in_progress = project_diff_state.copy_amp_prompt_in_progress;
        let copy_amp_prompt_disabled = review_count == 0 || copy_amp_prompt_in_progress;
        let (additions, deletions) = project_diff.read(cx).calculate_changed_lines(cx);
        let diff_base = project_diff.read(cx).diff_base(cx).clone();
        let DiffBase::Merge { base_ref } = diff_base else {
            return div();
        };
        let selected_base_ref = base_ref.clone();
        let base_ref_label = format!("Base: {base_ref}");
        let repository = project_diff.read(cx).branch_diff.read(cx).repo().cloned();
        let workspace = project_diff.read(cx).workspace.clone();
        let project_diff_for_picker = project_diff.downgrade();

        let is_multibuffer_empty = project_diff.read(cx).multibuffer.read(cx).is_empty();
        let is_ai_enabled = AgentSettings::get_global(cx).enabled(cx);

        let show_review_button = !is_multibuffer_empty && is_ai_enabled;

        h_group_xl()
            .my_neg_1()
            .py_1()
            .items_center()
            .flex_wrap()
            .justify_end()
            .gap_2()
            .child(
                IconButton::new("refresh-diff", IconName::RotateCw)
                    .shape(ui::IconButtonShape::Square)
                    .tooltip(Tooltip::for_action_title_in(
                        "Refresh Diff",
                        &RefreshDiff,
                        &focus_handle,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&RefreshDiff, window, cx);
                    })),
            )
            .child(
                div()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            if !copy_amp_prompt_disabled {
                                this.copy_amp_prompt(window, cx);
                            }
                        }),
                    )
                    .child(
                        IconButton::new("copy-amp-prompt", IconName::ToolCopy)
                            .shape(ui::IconButtonShape::Square)
                            .tooltip(Tooltip::for_action_title_in(
                                "Copy Amp Prompt for Unstamped Review Comments",
                                &CopyAmpPromptForUnstampedReviewComments,
                                &focus_handle,
                            ))
                            .disabled(copy_amp_prompt_disabled)
                            .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                                if matches!(event, gpui::ClickEvent::Keyboard(_)) {
                                    this.copy_amp_prompt(window, cx);
                                }
                            })),
                    ),
            )
            .child(
                PopoverMenu::new("branch-diff-base-branch-picker")
                    .menu(move |window, cx| {
                        let project_diff = project_diff_for_picker.clone();
                        let on_select = Arc::new(
                            move |branch: git::repository::Branch,
                                  _window: &mut Window,
                                  cx: &mut App| {
                                let base_ref: SharedString = branch.name().to_owned().into();
                                project_diff
                                    .update(cx, |project_diff, cx| {
                                        project_diff.follows_default_branch = false;
                                        let branch_diff = &mut project_diff.branch_diff;
                                        branch_diff.update(cx, |branch_diff, cx| {
                                            branch_diff
                                                .set_diff_base(DiffBase::Merge { base_ref }, cx);
                                        });
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        );
                        Some(branch_picker::select_popover(
                            workspace.clone(),
                            repository.clone(),
                            Some(selected_base_ref.clone()),
                            on_select,
                            window,
                            cx,
                        ))
                    })
                    .trigger_with_tooltip(
                        Button::new("branch-diff-base-branch", base_ref_label)
                            .color(Color::Muted)
                            .end_icon(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        Tooltip::text("Select base branch"),
                    ),
            )
            .when(!is_multibuffer_empty, |this| {
                this.child(DiffStat::new(
                    "branch-diff-stat",
                    additions as usize,
                    deletions as usize,
                ))
            })
            .when(show_review_button, |this| {
                let focus_handle = focus_handle.clone();
                this.child(Divider::vertical()).child(
                    Button::new("review-diff", "Review Diff")
                        .start_icon(
                            Icon::new(IconName::ZedAssistant)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .key_binding(KeyBinding::for_action_in(&ReviewDiff, &focus_handle, cx))
                        .tooltip(move |_, cx| {
                            Tooltip::with_meta_in(
                                "Review Diff",
                                Some(&ReviewDiff),
                                "Send this diff for your last agent to review.",
                                &focus_handle,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&ReviewDiff, window, cx);
                        })),
                )
            })
            .when(review_count > 0, |this| {
                this.child(Divider::vertical()).child(
                    render_send_review_to_agent_button(review_count, &focus_handle).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&SendReviewToAgent, window, cx)
                        }),
                    ),
                )
            })
    }
}

struct BranchDiffAddon {
    branch_diff: Entity<branch_diff::BranchDiff>,
}

impl Addon for BranchDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        cx: &App,
    ) -> Option<FileStatus> {
        self.branch_diff
            .read(cx)
            .status_for_buffer_id(buffer_id, cx)
    }
}

#[cfg(test)]
mod tests {
    use collections::HashMap;
    use db::indoc;
    use editor::display_map::DisplayRow;
    use editor::test::editor_test_context::{EditorTestContext, assert_state_with_diff};
    use git::status::{TrackedStatus, UnmergedStatus, UnmergedStatusCode};
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::{DiffViewStyle, GitPanelGroupBy, GitPanelSortBy, SettingsStore};
    use std::{cell::Cell, path::Path, rc::Rc};
    use unindent::Unindent as _;
    use util::{
        path,
        rel_path::{RelPath, rel_path},
    };

    use workspace::MultiWorkspace;

    use super::*;

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn init_review_db_test(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    }

    #[test]
    fn test_external_reply_keeps_current_comment_lines() {
        let base = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "moved.go",
                "side": "new",
                "hunk_line": 4,
                "line_start": 5,
                "line_end": 6,
                "body": "review",
                "replies": []
            }]
        })
        .to_string();
        let live = serde_json::json!({
            "schema_version": 1,
            "comments": [
                {
                    "id": 0,
                    "author": "vxio",
                    "file": "moved.go",
                    "side": "new",
                    "hunk_line": 9,
                    "line_start": 10,
                    "line_end": 11,
                    "body": "review",
                    "replies": []
                },
                {
                    "id": 1,
                    "author": "vxio",
                    "file": "local.go",
                    "side": "new",
                    "hunk_line": 98,
                    "line_start": 99,
                    "line_end": 99,
                    "body": "local collision",
                    "replies": []
                }
            ]
        })
        .to_string();
        let incoming = serde_json::json!({
            "schema_version": 1,
            "comments": [
                {
                    "id": 0,
                    "author": "vxio",
                    "file": "moved.go",
                    "side": "new",
                    "hunk_line": 4,
                    "line_start": 5,
                    "line_end": 6,
                    "body": "review",
                    "resolved_on": "now",
                    "replies": [{ "id": 1000, "author": "amp", "body": "fixed" }]
                },
                {
                    "id": 1,
                    "author": "amp",
                    "file": "new.go",
                    "side": "new",
                    "hunk_line": 20,
                    "line_start": 21,
                    "line_end": 21,
                    "body": "external",
                    "replies": []
                }
            ]
        })
        .to_string();

        let merged = preserve_live_review_comment_locations(&base, &live, &incoming)
            .unwrap()
            .expect("locations should be rebased");
        let merged: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(merged["comments"][0]["hunk_line"], 9);
        assert_eq!(merged["comments"][0]["line_start"], 10);
        assert_eq!(merged["comments"][0]["line_end"], 11);
        assert_eq!(merged["comments"][0]["resolved_on"], "now");
        assert_eq!(merged["comments"][0]["replies"][0]["body"], "fixed");
        assert_eq!(merged["comments"][1]["line_start"], 21);
    }

    #[test]
    fn test_external_review_comment_merge_preserves_deletions_and_ambiguous_ids() {
        let comment = |id, line_start| {
            serde_json::json!({
                "id": id,
                "author": "vxio",
                "file": "file.go",
                "side": "new",
                "hunk_line": line_start - 1,
                "line_start": line_start,
                "line_end": line_start,
                "body": "review",
                "replies": []
            })
        };
        let base = serde_json::json!({
            "schema_version": 1,
            "comments": [comment(0, 5), comment(1, 10), comment(1, 11)]
        })
        .to_string();
        let live = serde_json::json!({
            "schema_version": 1,
            "comments": [comment(0, 15), comment(1, 20), comment(1, 21)]
        })
        .to_string();
        let incoming = serde_json::json!({
            "schema_version": 1,
            "comments": [comment(1, 30), comment(1, 31)]
        })
        .to_string();

        assert_eq!(
            preserve_live_review_comment_locations(&base, &live, &incoming).unwrap(),
            None,
            "an externally deleted comment must not be restored, and duplicate IDs must not be rebased"
        );
    }

    #[test]
    fn test_external_review_comment_merge_only_rebases_locations_changed_locally() {
        let base = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "file": "base.go",
                "side": "new",
                "hunk_line": 4,
                "line_start": 5,
                "line_end": 5
            }]
        })
        .to_string();
        let incoming = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "file": "incoming.go",
                "side": "new",
                "hunk_line": 8,
                "line_start": 9,
                "line_end": 9
            }]
        })
        .to_string();

        assert_eq!(
            preserve_live_review_comment_locations(&base, &base, &incoming).unwrap(),
            None,
            "incoming locations must win when the live anchor did not move"
        );
    }

    #[test]
    fn test_external_review_comment_merge_retries_from_last_raw_observation() {
        let snapshot = |line_start, body| {
            serde_json::json!({
                "schema_version": 1,
                "comments": [{
                    "id": 0,
                    "file": "file.go",
                    "side": "new",
                    "hunk_line": line_start - 1,
                    "line_start": line_start,
                    "line_end": line_start,
                    "body": body
                }]
            })
            .to_string()
        };
        let last_raw_observation = snapshot(5, "original");
        let live = snapshot(10, "first external update");
        let second_external_update = snapshot(5, "second external update");

        let merged = preserve_live_review_comment_locations(
            &last_raw_observation,
            &live,
            &second_external_update,
        )
        .unwrap()
        .expect("the live location should still differ from the raw DB observation");
        let merged: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(merged["comments"][0]["line_start"], 10);
        assert_eq!(merged["comments"][0]["body"], "second external update");
    }

    #[test]
    fn test_partition_review_comments_json_by_author() {
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 0, "author": "vxio", "file": "a.txt", "side": "new", "hunk_line": 1, "line_start": 1, "line_end": 1, "body": "user", "replies": [] },
                { "id": 1, "author": "amp", "file": "a.txt", "side": "new", "hunk_line": 2, "line_start": 2, "line_end": 2, "body": "agent", "replies": [] }
            ]
        })
        .to_string();

        let (archived_json, remaining_json, archived_count) =
            partition_review_comments_json(&comments_json, ReviewCommentArchiveScope::User)
                .unwrap();
        let archived: serde_json::Value = serde_json::from_str(&archived_json).unwrap();
        let remaining: serde_json::Value = serde_json::from_str(&remaining_json).unwrap();

        assert_eq!(archived_count, 1);
        assert_eq!(archived["comments"][0]["body"], "user");
        assert_eq!(remaining["comments"][0]["body"], "agent");
    }

    #[test]
    fn test_partition_review_comments_json_by_round() {
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 0, "author": "vxio", "review_round": 1, "file": "a.txt", "side": "new", "hunk_line": 1, "line_start": 1, "line_end": 1, "body": "review one", "replies": [] },
                { "id": 1, "author": "vxio", "review_round": 2, "file": "a.txt", "side": "new", "hunk_line": 2, "line_start": 2, "line_end": 2, "body": "review two", "replies": [] },
                { "id": 2, "author": "vxio", "review_round": 1, "file": "a.txt", "side": "new", "hunk_line": 3, "line_start": 3, "line_end": 3, "body": "follow-up thread", "replies": [{ "id": 1000, "author": "vxio", "review_round": 2, "body": "review two follow-up" }] }
            ]
        })
        .to_string();

        let (archived_json, remaining_json, archived_count) =
            partition_review_comments_json(&comments_json, ReviewCommentArchiveScope::Round(2))
                .unwrap();
        let archived: serde_json::Value = serde_json::from_str(&archived_json).unwrap();
        let remaining: serde_json::Value = serde_json::from_str(&remaining_json).unwrap();

        assert_eq!(archived_count, 2);
        assert_eq!(archived["comments"][0]["body"], "review two");
        assert_eq!(archived["comments"][1]["body"], "follow-up thread");
        assert_eq!(remaining["comments"][0]["body"], "review one");
    }

    #[test]
    fn test_review_comments_json_rounds_includes_replies() {
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 0, "author": "vxio", "review_round": 3, "file": "a.txt", "side": "new", "hunk_line": 1, "line_start": 1, "line_end": 1, "body": "three", "replies": [{ "id": 1000, "author": "vxio", "review_round": 2, "body": "two" }] },
                { "id": 1, "author": "vxio", "review_round": 1, "file": "a.txt", "side": "new", "hunk_line": 2, "line_start": 2, "line_end": 2, "body": "one", "replies": [] }
            ]
        })
        .to_string();

        assert_eq!(review_comments_json_rounds(&comments_json), vec![1, 2, 3]);
    }

    #[test]
    fn test_stamp_unstamped_review_comments_uses_next_round() {
        let repo = "/project".to_string();
        let review_key = "/project\nbranch:refs/heads/topic\nsince:origin/main".to_string();
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 0, "author": "vxio", "file": "a.txt", "side": "new", "hunk_line": 1, "line_start": 1, "line_end": 1, "body": "do this", "replies": [] },
                { "id": 1, "author": "vxio", "review_round": 1, "file": "b.txt", "side": "new", "hunk_line": 2, "line_start": 2, "line_end": 2, "body": "old", "replies": [] }
            ]
        })
        .to_string();

        let (stamped_json, review_round) = stamp_unstamped_review_comments(&comments_json).unwrap();
        let prompt =
            amp_prompt_for_review_round(repo, review_key, &stamped_json, review_round).unwrap();
        let stamped: serde_json::Value = serde_json::from_str(&stamped_json).unwrap();

        assert_eq!(stamped["comments"][0]["review_round"], 2);
        assert_eq!(stamped["comments"][1]["review_round"], 1);
        assert_eq!(review_round, 2);
        assert!(prompt.contains("do this"));
        assert!(!prompt.contains("\"body\":\"old\""));
    }

    #[test]
    fn test_prompt_includes_new_user_reply_to_agent_comment() {
        let comments_json = serde_json::json!({
            "schema_version": 3,
            "comments": [{
                "id": 0,
                "author": "amp",
                "review_round": 1,
                "file": "a.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "agent finding",
                "replies": [{ "id": 1000, "author": "vxio", "body": "please revisit" }]
            }]
        })
        .to_string();

        let (stamped_json, review_round) = stamp_unstamped_review_comments(&comments_json).unwrap();
        let prompt = amp_prompt_for_review_round(
            "/project".to_string(),
            "/project\nbranch:refs/heads/topic\nsince:origin/main".to_string(),
            &stamped_json,
            review_round,
        )
        .unwrap();

        assert_eq!(review_round, 2);
        assert!(prompt.contains("agent finding"));
        assert!(prompt.contains("please revisit"));
    }

    #[gpui::test]
    async fn test_archive_user_review_comments_clears_visible_diff(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 0, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "archive me", "replies": [] },
                { "id": 1, "author": "amp", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "keep me", "replies": [] }
            ]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key.clone(), comments_json)
            .await
            .unwrap();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(diff.clone()), None, true, window, cx);
        });
        cx.focus(&workspace);
        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.restore_review_comments_if_needed(window, cx)
            });
            window.dispatch_action(ArchiveUserReviewComments.boxed_clone(), cx);
        });

        db.flush_writes().await.unwrap();
        let active = db
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        assert!(!active.contains("archive me"));
        assert!(active.contains("keep me"));
        assert_eq!(
            db.archived_review_comments_count(workspace_id, &review_key)
                .unwrap(),
            1
        );

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let visible = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(!visible.contains("archive me"));
        assert!(visible.contains("keep me"));
    }

    #[gpui::test]
    async fn test_copy_amp_prompt_toolbar_updates_when_review_comments_change(
        cx: &mut TestAppContext,
    ) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            let Some(repository) = project.read(cx).active_repository(cx) else {
                panic!("expected active repository");
            };
            let branch_diff = cx.new(|cx| {
                let mut branch_diff = branch_diff::BranchDiff::new(
                    DiffBase::Merge {
                        base_ref: "origin/main".into(),
                    },
                    project.clone(),
                    window,
                    cx,
                );
                branch_diff.set_repo(Some(repository), cx);
                branch_diff
            });
            ProjectDiff::new_impl(
                branch_diff,
                project.clone(),
                workspace.clone(),
                true,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(diff.clone()), None, true, window, cx);
        });
        let workspace_id = workspace.read_with(cx, |workspace, _| workspace.database_id().unwrap());
        cx.update(|_, cx| persistence::ProjectDiffDb::global(cx))
            .insert_test_workspace(workspace_id)
            .await
            .unwrap();
        let toolbar = cx.replace_root_view(|_, cx| {
            let mut toolbar = BranchDiffToolbar::new(cx);
            toolbar.set_project_diff(Some(diff.clone()), cx);
            toolbar
        });
        let toolbar_notified = Rc::new(Cell::new(false));
        let _toolbar_subscription = cx.update(|_, cx| {
            let toolbar_notified = toolbar_notified.clone();
            cx.observe(&toolbar, move |_, _| toolbar_notified.set(true))
        });
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 0, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "first click", "replies": [] }
            ]
        })
        .to_string();
        diff.update_in(cx, |diff, window, cx| {
            let editor = diff.editor.read(cx).rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                editor
                    .restore_review_comments_json(&comments_json, window, cx)
                    .unwrap();
            });
        });
        assert!(toolbar_notified.get());

        let button_bounds = cx
            .debug_bounds("ICON-ToolCopy")
            .expect("Copy Amp Prompt button should be visible");
        cx.simulate_event(gpui::MouseDownEvent {
            position: button_bounds.center(),
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });

        cx.run_until_parked();
        let clipboard = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        assert!(clipboard.contains("first click"));

        let review_key = diff.read_with(cx, |diff, cx| diff.review_comments_key(cx).unwrap());
        let persisted = cx
            .update(|_, cx| persistence::ProjectDiffDb::global(cx))
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        assert!(persisted.contains("first click"));
        cx.simulate_event(gpui::MouseUpEvent {
            position: button_bounds.center(),
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 1,
        });
        let stamped = diff.read_with(cx, |diff, cx| {
            diff.editor
                .read(cx)
                .rhs_editor()
                .read(cx)
                .review_comments_json(cx)
                .unwrap()
        });
        let stamped: serde_json::Value = serde_json::from_str(&stamped).unwrap();
        assert_eq!(stamped["comments"][0]["review_round"], 1);

        let unstamped_comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 1, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "must persist", "replies": [] }
            ]
        })
        .to_string();
        diff.update_in(cx, |diff, window, cx| {
            let editor = diff.editor.read(cx).rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                editor
                    .restore_review_comments_json(&unstamped_comments_json, window, cx)
                    .unwrap();
            });
        });
        cx.update(|_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("unchanged".to_string()))
        });
        cx.update(|_, cx| persistence::ProjectDiffDb::global(cx))
            .delete_test_workspace(workspace_id)
            .await
            .unwrap();

        cx.simulate_event(gpui::MouseDownEvent {
            position: button_bounds.center(),
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("unchanged".to_string())
        );
        let after_failed_save = diff.read_with(cx, |diff, cx| {
            diff.editor
                .read(cx)
                .rhs_editor()
                .read(cx)
                .review_comments_json(cx)
                .unwrap()
        });
        let after_failed_save: serde_json::Value =
            serde_json::from_str(&after_failed_save).unwrap();
        assert!(
            after_failed_save["comments"][0]
                .get("review_round")
                .is_none()
        );
        cx.simulate_event(gpui::MouseUpEvent {
            position: button_bounds.center(),
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 1,
        });

        cx.update(|_, cx| persistence::ProjectDiffDb::global(cx))
            .insert_test_workspace(workspace_id)
            .await
            .unwrap();
        cx.simulate_event(gpui::MouseDownEvent {
            position: button_bounds.center(),
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
        let retried_prompt = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        assert!(retried_prompt.contains("must persist"));
        let persisted_after_retry = cx
            .update(|_, cx| persistence::ProjectDiffDb::global(cx))
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        let persisted_after_retry: serde_json::Value =
            serde_json::from_str(&persisted_after_retry).unwrap();
        assert_eq!(persisted_after_retry["comments"][0]["review_round"], 1);
    }

    #[gpui::test]
    async fn test_restore_latest_review_comments_restores_newest_once(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();

        let empty_json = serde_json::json!({ "schema_version": 1, "comments": [] }).to_string();
        let older_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{ "id": 0, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "older", "replies": [] }]
        })
        .to_string();
        let newer_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{ "id": 0, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "newer", "replies": [] }]
        })
        .to_string();
        let deleted_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{ "id": 0, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "deleted", "replies": [] }]
        })
        .to_string();
        db.archive_review_comments(
            workspace_id,
            review_key.clone(),
            "archived:1:user".to_string(),
            older_json,
            empty_json.clone(),
            1,
            "user".to_string(),
        )
        .await
        .unwrap();
        db.archive_review_comments(
            workspace_id,
            review_key.clone(),
            "archived:2:user".to_string(),
            newer_json,
            empty_json.clone(),
            2,
            "user".to_string(),
        )
        .await
        .unwrap();
        db.archive_review_comments(
            workspace_id,
            review_key.clone(),
            "archived:3:deleted".to_string(),
            deleted_json,
            empty_json.clone(),
            3,
            "deleted".to_string(),
        )
        .await
        .unwrap();

        let active_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{ "id": 0, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 1, "line_start": 1, "line_end": 1, "body": "active", "replies": [] }]
        })
        .to_string();
        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.editor.update(cx, |editor, cx| {
                    editor.rhs_editor().update(cx, |editor, cx| {
                        editor
                            .restore_review_comments_json(&active_json, window, cx)
                            .unwrap();
                    });
                });
            });
        });

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.restore_latest_review_comments(workspace_id, None, window, cx);
            })
        });

        db.flush_writes().await.unwrap();
        let active = db
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        assert!(active.contains("active"));
        assert!(active.contains("newer"));
        assert!(!active.contains("older"));
        assert!(!active.contains("deleted"));
        assert_eq!(review_comments_json_comment_count(&active), 2);
    }

    #[test]
    fn test_merge_review_comments_json_reassigns_restored_ids() {
        let active_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "active",
                "replies": [{ "id": 1000, "author": "amp", "body": "active reply" }]
            }]
        })
        .to_string();
        let archived_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "amp",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 2,
                "line_end": 2,
                "body": "restored",
                "replies": [{ "id": 1000, "author": "vxio", "body": "restored reply" }]
            }]
        })
        .to_string();

        let (merged, restored_count) =
            merge_review_comments_json(&active_json, &archived_json).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&merged).unwrap();
        let comments = merged
            .get("comments")
            .and_then(|comments| comments.as_array())
            .unwrap();

        assert_eq!(restored_count, 1);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].get("id").and_then(|id| id.as_i64()), Some(0));
        assert_eq!(comments[1].get("id").and_then(|id| id.as_i64()), Some(1));
        assert_eq!(
            comments[1]
                .get("replies")
                .and_then(|replies| replies.as_array())
                .and_then(|replies| replies.first())
                .and_then(|reply| reply.get("id"))
                .and_then(|id| id.as_i64()),
            Some(1001)
        );
    }

    #[test]
    fn test_merge_review_comments_for_save_preserves_external_replies() {
        let existing_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "review",
                "replies": [{ "id": 1000, "author": "amp", "body": "👍 fixed" }]
            }]
        })
        .to_string();
        let incoming_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "review",
                "replies": []
            }]
        })
        .to_string();

        let merged = merge_review_comments_for_save(Some(&existing_json), &incoming_json).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(merged["comments"][0]["replies"][0]["body"], "👍 fixed");
    }

    #[test]
    fn test_merge_review_comments_for_save_preserves_persisted_rounds() {
        let existing_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "review_round": 2,
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "review",
                "replies": [{ "id": 1000, "author": "amp", "review_round": 2, "body": "👍 fixed" }]
            }]
        })
        .to_string();
        let incoming_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "review",
                "replies": [
                    { "id": 1000, "author": "amp", "body": "👍 fixed" },
                    { "id": 1001, "author": "vxio", "body": "one more thing" }
                ]
            }]
        })
        .to_string();

        let merged = merge_review_comments_for_save(Some(&existing_json), &incoming_json).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(merged["comments"][0]["review_round"], 2);
        assert_eq!(merged["comments"][0]["replies"][0]["review_round"], 2);
        assert!(
            merged["comments"][0]["replies"][1]
                .get("review_round")
                .is_none()
        );
    }

    #[test]
    fn test_merge_and_stamp_review_comments_uses_latest_persisted_round() {
        let existing_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "review_round": 3,
                "file": "existing.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "already persisted",
                "replies": []
            }]
        })
        .to_string();
        let incoming_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 1,
                "author": "vxio",
                "file": "new.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "new review",
                "replies": []
            }]
        })
        .to_string();

        let (merged, review_round) =
            merge_and_stamp_review_comments_for_save(Some(&existing_json), &incoming_json).unwrap();
        let merged: serde_json::Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(review_round, 4);
        assert_eq!(merged["comments"][0]["review_round"], 4);
        assert_eq!(merged["comments"][1]["review_round"], 3);
    }

    #[test]
    fn test_merge_review_comments_for_save_keeps_both_reply_id_collisions() {
        let existing_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "review",
                "replies": [{ "id": 1000, "author": "amp", "body": "👍 fixed" }]
            }]
        })
        .to_string();
        let incoming_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 1,
                "line_start": 1,
                "line_end": 1,
                "body": "review",
                "replies": [{ "id": 1000, "author": "vxio", "review_round": 2, "body": "why didn't you reply?" }]
            }]
        })
        .to_string();

        let merged = merge_review_comments_for_save(Some(&existing_json), &incoming_json).unwrap();
        let prompt = amp_prompt_for_review_round(
            "/project".to_string(),
            "/project\nbranch:refs/heads/topic\nsince:origin/main".to_string(),
            &merged,
            2,
        )
        .unwrap();
        let merged_value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        let replies = merged_value["comments"][0]["replies"].as_array().unwrap();

        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["id"], 1000);
        assert_eq!(replies[0]["body"], "👍 fixed");
        assert_eq!(replies[1]["id"], 1001);
        assert_eq!(replies[1]["body"], "why didn't you reply?");
        assert!(prompt.contains("\"id\":1001"));
        assert!(prompt.contains("why didn't you reply?"));
    }

    #[gpui::test]
    async fn test_review_comments_survive_restore_clear_and_restore(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "persist me",
                "replies": []
            }]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key.clone(), comments_json.clone())
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            })
        });

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored.contains("persist me"));
        let restored: serde_json::Value = serde_json::from_str(&restored).unwrap();

        editor.update(cx, |editor, cx| editor.clear_review_comments(cx));
        cx.run_until_parked();

        let persisted = db
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(persisted, restored);

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            })
        });
        let restored_again =
            editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored_again.contains("persist me"));
    }

    #[gpui::test]
    async fn test_review_comments_reload_when_persisted_json_changes(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let initial_comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "first",
                "replies": []
            }]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key.clone(), initial_comments_json)
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.restore_review_comments_if_needed(window, cx);
            })
        });

        let updated_comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "first",
                "replies": [{ "id": 1000, "author": "amp", "body": "done" }]
            }]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key, updated_comments_json)
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.restore_review_comments_if_needed(window, cx);
            })
        });

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored.contains("done"));
    }

    #[gpui::test]
    async fn test_review_comments_reload_waits_for_active_comment_input(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let initial_comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "first",
                "replies": []
            }]
        })
        .to_string();
        db.save_review_comments(
            workspace_id,
            review_key.clone(),
            initial_comments_json.clone(),
        )
        .await
        .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.restore_review_comments_if_needed(window, cx);
            })
        });
        let loaded_before_draft =
            diff.read_with(cx, |diff, _| diff.loaded_review_comments_json.clone());

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.show_diff_review_overlay(DisplayRow(3)..DisplayRow(3), window, cx);
            let prompt_editor = editor.diff_review_prompt_editor().cloned().unwrap();
            prompt_editor.update(cx, |prompt_editor, cx| {
                prompt_editor.insert("draft", window, cx);
            });
        });

        let updated_comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "first",
                "replies": [{ "id": 1000, "author": "amp", "body": "done" }]
            }]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key, updated_comments_json)
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.restore_review_comments_if_needed(window, cx);
                assert_eq!(diff.loaded_review_comments_json, loaded_before_draft);
            })
        });

        editor.update(cx, |editor, _cx| {
            assert!(editor.has_active_diff_review_input());
            assert!(
                editor
                    .diff_review_prompt_editor()
                    .is_some_and(|prompt| prompt.read(_cx).text(_cx).contains("draft"))
            );
        });
    }

    #[gpui::test]
    async fn test_review_comments_restore_multiple_locations(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "go.mod": "line 1\nline 2 changed\nline 3 changed\nline 4\nline 5\n",
                "go.sum": "line 1\nline 2\nline 3 changed\nline 4\n",
                "internal": {
                    "validator": {
                        "validate_test.go": "line 1\nline 2 changed\nline 3 changed\nline 4\n"
                    }
                },
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[
                (
                    "go.mod",
                    "line 1\nline 2\nline 3\nline 4\nline 5\n".to_string(),
                ),
                ("go.sum", "line 1\nline 2\nline 3\nline 4\n".to_string()),
                (
                    "internal/validator/validate_test.go",
                    "line 1\nline 2\nline 3\nline 4\n".to_string(),
                ),
            ],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 1, "author": "vxio", "file": "go.sum", "side": "new", "hunk_line": 6, "line_start": 3, "line_end": 3, "body": "wow", "replies": [] },
                { "id": 2, "author": "vxio", "file": "go.sum", "side": "new", "hunk_line": 6, "line_start": 3, "line_end": 3, "body": "woah", "replies": [] },
                { "id": 4, "author": "vxio", "file": "go.sum", "side": "new", "hunk_line": 6, "line_start": 3, "line_end": 3, "body": "dope", "replies": [] },
                { "id": 6, "author": "vxio", "file": "go.sum", "side": "new", "hunk_line": 6, "line_start": 3, "line_end": 3, "body": "hi", "replies": [] },
                { "id": 3, "author": "vxio", "file": "go.mod", "side": "new", "hunk_line": 3, "line_start": 2, "line_end": 2, "body": "huh", "replies": [] },
                { "id": 8, "author": "vxio", "file": "go.mod", "side": "new", "hunk_line": 4, "line_start": 3, "line_end": 3, "body": "??", "replies": [] },
                { "id": 5, "author": "vxio", "file": "internal/validator/validate_test.go", "side": "new", "hunk_line": 9, "line_start": 2, "line_end": 2, "body": "wow", "replies": [] },
                { "id": 7, "author": "vxio", "file": "internal/validator/validate_test.go", "side": "new", "hunk_line": 10, "line_start": 3, "line_end": 3, "body": "weird", "replies": [] }
            ]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key.clone(), comments_json)
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            })
        });

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        let restored: serde_json::Value = serde_json::from_str(&restored).unwrap();
        let comments = restored["comments"].as_array().unwrap();

        assert_eq!(comments.len(), 8);
        for body in ["wow", "woah", "dope", "hi", "huh", "??", "weird"] {
            assert!(comments.iter().any(|comment| comment["body"] == body));
        }
        for (id, file) in [
            (1, "go.sum"),
            (2, "go.sum"),
            (4, "go.sum"),
            (6, "go.sum"),
            (3, "go.mod"),
            (8, "go.mod"),
            (5, "internal/validator/validate_test.go"),
            (7, "internal/validator/validate_test.go"),
        ] {
            let comment = comments
                .iter()
                .find(|comment| comment["id"] == id)
                .expect("restored comment");
            assert_eq!(comment["file"], file, "comment {id} changed files");
        }

        fs.insert_file(
            path!("/project/go.mod"),
            b"inserted\nline 1\nline 2 changed\nline 3 changed\nline 4\nline 5\n".to_vec(),
        )
        .await;
        fs.insert_file(
            path!("/project/internal/validator/validate_test.go"),
            b"line 1\ninserted\nline 2 changed\nline 3 changed\nline 4\n".to_vec(),
        )
        .await;
        cx.run_until_parked();

        let after_edits =
            editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        let after_edits: serde_json::Value = serde_json::from_str(&after_edits).unwrap();
        let comments = after_edits["comments"].as_array().unwrap();
        for (id, file) in [
            (1, "go.sum"),
            (2, "go.sum"),
            (4, "go.sum"),
            (6, "go.sum"),
            (3, "go.mod"),
            (8, "go.mod"),
            (5, "internal/validator/validate_test.go"),
            (7, "internal/validator/validate_test.go"),
        ] {
            let comment = comments
                .iter()
                .find(|comment| comment["id"] == id)
                .expect("comment after edits");
            assert_eq!(comment["file"], file, "comment {id} changed files");
        }
    }

    #[gpui::test]
    async fn test_review_comments_keep_unmapped_locations(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [
                { "id": 0, "author": "vxio", "file": "foo.txt", "side": "new", "hunk_line": 2, "line_start": 1, "line_end": 1, "body": "restores first", "replies": [] },
                { "id": 1, "author": "vxio", "file": "bar.txt", "side": "new", "hunk_line": 5, "line_start": 1, "line_end": 1, "body": "restores later", "replies": [] }
            ]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key.clone(), comments_json)
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
                // The attempt is cached so the periodic poll doesn't rebuild the
                // overlays every tick, but it's marked incomplete so refresh()
                // retries attaching the unmapped comment.
                assert_eq!(diff.loaded_review_comments_key.as_ref(), Some(&review_key));
                assert!(!diff.review_comments_fully_restored);
            })
        });

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored.contains("restores first"));
        assert!(restored.contains("restores later"));
    }

    #[gpui::test]
    async fn test_review_comments_restore_waits_for_pending_save(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "delete me",
                "replies": []
            }]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key.clone(), comments_json)
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            });
        });
        let deleted_comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "delete me",
                "deleted_on": "2026-07-23 20:00",
                "replies": []
            }]
        })
        .to_string();
        cx.update(|_, cx| {
            diff.update(cx, |diff, cx| {
                diff.persist_review_comments(deleted_comments_json.clone(), cx);
            })
        });

        db.flush_writes().await.unwrap();
        let persisted = db
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        let expected: serde_json::Value = serde_json::from_str(&deleted_comments_json).unwrap();
        assert_eq!(persisted, expected);

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            });
        });

        diff.read_with(cx, |diff, _| assert_eq!(diff.review_comment_count, 0));
    }

    #[gpui::test]
    async fn test_review_comment_add_persists_and_restores(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.show_diff_review_overlay(DisplayRow(1)..DisplayRow(1), window, cx);
            assert!(editor.diff_review_prompt_editor().is_none());
            editor.show_diff_review_overlay(DisplayRow(3)..DisplayRow(3), window, cx);
            let prompt_editor = editor.diff_review_prompt_editor().cloned().unwrap();
            prompt_editor.update(cx, |prompt_editor, cx| {
                prompt_editor.insert("new comment", window, cx);
            });
            editor.submit_diff_review_comment(window, cx);
        });

        db.flush_writes().await.unwrap();
        let saved_comments = db
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        assert!(saved_comments.contains("new comment"));

        editor.update(cx, |editor, cx| editor.clear_review_comments(cx));
        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            })
        });
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored.contains("new comment"));
    }

    #[gpui::test]
    async fn test_review_comment_restores_in_reopened_diff(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, db) = cx.update(|_, cx| {
            (
                workspace.read(cx).database_id().unwrap(),
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.show_diff_review_overlay(DisplayRow(3)..DisplayRow(3), window, cx);
            let prompt_editor = editor.diff_review_prompt_editor().cloned().unwrap();
            prompt_editor.update(cx, |prompt_editor, cx| {
                prompt_editor.insert("reopened comment", window, cx);
            });
            editor.submit_diff_review_comment(window, cx);
        });
        db.flush_writes().await.unwrap();

        let reopened_diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let reopened_editor =
            reopened_diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored =
            reopened_editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored.contains("reopened comment"));
    }

    #[gpui::test]
    async fn test_review_comment_does_not_move_to_similarly_named_file(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
                "foo_test.txt": "TEST\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("foo.txt", "foo\n".to_string()),
                ("foo_test.txt", "test\n".to_string()),
            ],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, db) = cx.update(|_, cx| {
            (
                workspace.read(cx).database_id().unwrap(),
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.show_diff_review_overlay(DisplayRow(3)..DisplayRow(3), window, cx);
            let prompt_editor = editor.diff_review_prompt_editor().cloned().unwrap();
            prompt_editor.update(cx, |prompt_editor, cx| {
                prompt_editor.insert("removed diff comment", window, cx);
            });
            editor.submit_diff_review_comment(window, cx);
        });
        db.flush_writes().await.unwrap();
        let review_key = diff.read_with(cx, |diff, cx| diff.review_comments_key(cx).unwrap());

        fs.insert_file(path!("/project/foo.txt"), b"foo\n".to_vec())
            .await;
        cx.run_until_parked();
        db.flush_writes().await.unwrap();

        let comments_json = db
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        let mut comments: serde_json::Value = serde_json::from_str(&comments_json).unwrap();
        comments["comments"][0]["replies"] = serde_json::json!([
            { "id": 1000, "author": "amp", "body": "reply on removed diff" }
        ]);
        db.save_review_comments(workspace_id, review_key, comments.to_string())
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.restore_review_comments_if_needed(window, cx);
            })
        });
        cx.run_until_parked();

        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        let restored: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored["comments"][0]["body"], "removed diff comment");
        assert_eq!(restored["comments"][0]["outdated"], true);
        assert_eq!(
            restored["comments"][0]["outdated_reason"],
            "file_not_in_diff"
        );
        assert_eq!(
            restored["comments"][0]["replies"][0]["body"],
            "reply on removed diff"
        );

        diff.read_with(cx, |diff, _cx| {
            assert_eq!(diff.review_comment_count, 1);
        });
        editor.read_with(cx, |editor, _cx| {
            let orphaned = editor.orphaned_review_comment_summaries();
            assert_eq!(orphaned.len(), 1);
            assert_eq!(orphaned[0].file, "foo.txt");
            assert_eq!(orphaned[0].line_start, 1);
            assert_eq!(orphaned[0].body, "removed diff comment");
            assert_eq!(orphaned[0].replies.len(), 1);
            assert_eq!(orphaned[0].replies[0].body, "reply on removed diff");
            assert_eq!(
                orphaned[0].outdated_reason.as_deref(),
                Some("file_not_in_diff")
            );
        });
    }

    #[gpui::test]
    async fn test_review_comments_are_scoped_per_diff_view(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("foo.txt", "foo\n".to_string())],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let (workspace_id, review_key, db) = cx.update(|_, cx| {
            let workspace_id = workspace.read(cx).database_id().unwrap();
            let review_key = diff.read(cx).review_comments_key(cx).unwrap();
            (
                workspace_id,
                review_key,
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": [{
                "id": 0,
                "author": "vxio",
                "file": "foo.txt",
                "side": "new",
                "hunk_line": 2,
                "line_start": 1,
                "line_end": 1,
                "body": "base agnostic",
                "replies": []
            }]
        })
        .to_string();
        db.save_review_comments(workspace_id, review_key.clone(), comments_json)
            .await
            .unwrap();

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.branch_diff.update(cx, |branch_diff, cx| {
                    branch_diff.set_diff_base(
                        DiffBase::Merge {
                            base_ref: "changed-base".into(),
                        },
                        cx,
                    );
                });
                assert_ne!(diff.review_comments_key(cx).as_ref(), Some(&review_key));
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            })
        });
        cx.run_until_parked();

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let in_branch_view =
            editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(!in_branch_view.contains("base agnostic"));

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.branch_diff.update(cx, |branch_diff, cx| {
                    branch_diff.set_diff_base(DiffBase::Head, cx);
                });
                assert_eq!(diff.review_comments_key(cx).as_ref(), Some(&review_key));
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            })
        });
        cx.run_until_parked();

        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored.contains("base agnostic"));
    }

    #[gpui::test]
    async fn test_save_after_restore(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;

        fs.set_head_for_repo(
            path!("/project/.git").as_ref(),
            &[("foo.txt", "foo\n".into())],
            "deadbeef",
        );
        fs.set_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("foo.txt", "foo\n".into())],
        );

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        assert_state_with_diff(
            &editor,
            cx,
            &"
                - ˇfoo
                + FOO
            "
            .unindent(),
        );

        editor
            .update_in(cx, |editor, window, cx| {
                editor.git_restore(&Default::default(), window, cx);
                editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        assert_state_with_diff(&editor, cx, &"ˇ".unindent());

        let text = String::from_utf8(fs.read_file_sync("/project/foo.txt").unwrap()).unwrap();
        assert_eq!(text, "foo\n");
    }

    #[gpui::test]
    async fn test_scroll_to_beginning_with_deletion(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "bar": "BAR\n",
                "foo": "FOO\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("bar", "bar\n".into()), ("foo", "foo\n".into())],
        );
        cx.run_until_parked();

        let editor = cx.update_window_entity(&diff, |diff, window, cx| {
            diff.move_to_path(
                PathKey::with_sort_prefix(2, rel_path("foo").into_arc()),
                window,
                cx,
            );
            diff.editor.read(cx).rhs_editor().clone()
        });
        assert_state_with_diff(
            &editor,
            cx,
            &"
                - bar
                + BAR

                - ˇfoo
                + FOO
            "
            .unindent(),
        );

        let editor = cx.update_window_entity(&diff, |diff, window, cx| {
            diff.move_to_path(
                PathKey::with_sort_prefix(2, rel_path("bar").into_arc()),
                window,
                cx,
            );
            diff.editor.read(cx).rhs_editor().clone()
        });
        assert_state_with_diff(
            &editor,
            cx,
            &"
                - ˇbar
                + BAR

                - foo
                + FOO
            "
            .unindent(),
        );
    }

    #[gpui::test]
    async fn test_hunks_after_restore_then_modify(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo": "modified\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        fs.set_head_for_repo(
            path!("/project/.git").as_ref(),
            &[("foo", "original\n".into())],
            "deadbeef",
        );

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/foo"), cx)
            })
            .await
            .unwrap();
        let buffer_editor = cx.new_window_entity(|window, cx| {
            Editor::for_buffer(buffer, Some(project.clone()), window, cx)
        });
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        let diff_editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());

        assert_state_with_diff(
            &diff_editor,
            cx,
            &"
                - ˇoriginal
                + modified
            "
            .unindent(),
        );

        let prev_buffer_hunks =
            cx.update_window_entity(&buffer_editor, |buffer_editor, window, cx| {
                let snapshot = buffer_editor.snapshot(window, cx);
                let snapshot = &snapshot.buffer_snapshot();
                let prev_buffer_hunks = buffer_editor
                    .diff_hunks_in_ranges(&[editor::Anchor::Min..editor::Anchor::Max], snapshot)
                    .collect::<Vec<_>>();
                buffer_editor.git_restore(&Default::default(), window, cx);
                prev_buffer_hunks
            });
        assert_eq!(prev_buffer_hunks.len(), 1);
        cx.run_until_parked();

        let new_buffer_hunks =
            cx.update_window_entity(&buffer_editor, |buffer_editor, window, cx| {
                let snapshot = buffer_editor.snapshot(window, cx);
                let snapshot = &snapshot.buffer_snapshot();
                buffer_editor
                    .diff_hunks_in_ranges(&[editor::Anchor::Min..editor::Anchor::Max], snapshot)
                    .collect::<Vec<_>>()
            });
        assert_eq!(new_buffer_hunks.as_slice(), &[]);

        cx.update_window_entity(&buffer_editor, |buffer_editor, window, cx| {
            buffer_editor.set_text("different\n", window, cx);
            buffer_editor.save(
                SaveOptions {
                    format: false,
                    force_format: false,
                    autosave: false,
                },
                project.clone(),
                window,
                cx,
            )
        })
        .await
        .unwrap();

        cx.run_until_parked();

        cx.update_window_entity(&buffer_editor, |buffer_editor, window, cx| {
            buffer_editor.expand_all_diff_hunks(&Default::default(), window, cx);
        });

        assert_state_with_diff(
            &buffer_editor,
            cx,
            &"
                - original
                + different
                  ˇ"
            .unindent(),
        );

        assert_state_with_diff(
            &diff_editor,
            cx,
            &"
                - ˇoriginal
                + different
            "
            .unindent(),
        );
    }

    use crate::project_diff::{self, ProjectDiff};

    #[gpui::test]
    async fn test_go_to_prev_hunk_multibuffer(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/a"),
            json!({
                ".git": {},
                "a.txt": "created\n",
                "b.txt": "really changed\n",
                "c.txt": "unchanged\n"
            }),
        )
        .await;

        fs.set_head_and_index_for_repo(
            Path::new(path!("/a/.git")),
            &[
                ("b.txt", "before\n".to_string()),
                ("c.txt", "unchanged\n".to_string()),
                ("d.txt", "deleted\n".to_string()),
            ],
        );

        let project = Project::test(fs, [Path::new(path!("/a"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(project_diff::Diff.boxed_clone(), cx);
        });

        cx.run_until_parked();

        let item = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ProjectDiff>(cx).unwrap()
        });
        cx.focus(&item);
        let editor = item.read_with(cx, |item, cx| item.editor.read(cx).rhs_editor().clone());

        let mut cx = EditorTestContext::for_editor_in(editor, cx).await;

        cx.set_selections_state(indoc!(
            "
            before
            really changed

            deleted

            ˇcreated
        "
        ));

        cx.dispatch_action(editor::actions::GoToPreviousHunk);

        cx.assert_excerpts_with_selections(indoc!(
            "
            [EXCERPT]
            before
            really changed
            [EXCERPT]
            ˇ[FOLDED]
            [EXCERPT]
            created
        "
        ));

        cx.dispatch_action(editor::actions::GoToPreviousHunk);

        cx.assert_excerpts_with_selections(indoc!(
            "
            [EXCERPT]
            ˇbefore
            really changed
            [EXCERPT]
            [FOLDED]
            [EXCERPT]
            created
        "
        ));
    }

    #[gpui::test]
    async fn test_excerpts_splitting_after_restoring_the_middle_excerpt(cx: &mut TestAppContext) {
        init_test(cx);

        let git_contents = indoc! {r#"
            #[rustfmt::skip]
            fn main() {
                let x = 0.0; // this line will be removed
                // 1
                // 2
                // 3
                let y = 0.0; // this line will be removed
                // 1
                // 2
                // 3
                let arr = [
                    0.0, // this line will be removed
                    0.0, // this line will be removed
                    0.0, // this line will be removed
                    0.0, // this line will be removed
                ];
            }
        "#};
        let buffer_contents = indoc! {"
            #[rustfmt::skip]
            fn main() {
                // 1
                // 2
                // 3
                // 1
                // 2
                // 3
                let arr = [
                ];
            }
        "};

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/a"),
            json!({
                ".git": {},
                "main.rs": buffer_contents,
            }),
        )
        .await;

        fs.set_head_and_index_for_repo(
            Path::new(path!("/a/.git")),
            &[("main.rs", git_contents.to_owned())],
        );

        let project = Project::test(fs, [Path::new(path!("/a"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(project_diff::Diff.boxed_clone(), cx);
        });

        cx.run_until_parked();

        let item = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ProjectDiff>(cx).unwrap()
        });
        cx.focus(&item);
        let editor = item.read_with(cx, |item, cx| item.editor.read(cx).rhs_editor().clone());

        let mut cx = EditorTestContext::for_editor_in(editor, cx).await;

        cx.assert_excerpts_with_selections(&format!("[EXCERPT]\nˇ{git_contents}"));

        cx.dispatch_action(editor::actions::GoToHunk);
        cx.dispatch_action(editor::actions::GoToHunk);
        cx.dispatch_action(git::Restore);
        cx.dispatch_action(editor::actions::MoveToBeginning);

        cx.assert_excerpts_with_selections(&format!("[EXCERPT]\nˇ{git_contents}"));
    }

    #[gpui::test(iterations = 50)]
    async fn test_split_diff_conflict_path_transition_with_dirty_buffer_invalid_anchor_panics(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
        });

        let build_conflict_text: fn(usize) -> String = |tag: usize| {
            let mut lines = (0..80)
                .map(|line_index| format!("line {line_index}"))
                .collect::<Vec<_>>();
            for offset in [5usize, 20, 37, 61] {
                lines[offset] = format!("base-{tag}-line-{offset}");
            }
            format!("{}\n", lines.join("\n"))
        };
        let initial_conflict_text = build_conflict_text(0);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "helper.txt": "same\n",
                "conflict.txt": initial_conflict_text,
            }),
        )
        .await;
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state
                .refs
                .insert("MERGE_HEAD".into(), "conflict-head".into());
        })
        .unwrap();
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "conflict.txt",
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                }),
            )],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("conflict.txt", build_conflict_text(1)),
                ("helper.txt", "same\n".to_string()),
            ],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let _project_diff = cx
            .update(|window, cx| {
                ProjectDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/conflict.txt"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "dirty\n")], None, cx));
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        cx.run_until_parked();

        cx.update(|window, cx| {
            let fs = fs.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
                        state.refs.insert("HEAD".into(), "head-1".into());
                        state.refs.remove("MERGE_HEAD");
                    })
                    .unwrap();
                    fs.set_status_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            (
                                "conflict.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                            (
                                "helper.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                        ],
                    );
                    // FakeFs assigns deterministic OIDs by entry position; flipping order churns
                    // conflict diff identity without reaching into ProjectDiff internals.
                    fs.set_merge_base_content_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            ("helper.txt", "helper-base\n".to_string()),
                            ("conflict.txt", build_conflict_text(2)),
                        ],
                    );
                })
                .detach();
        });

        cx.update(|window, cx| {
            let buffer = buffer.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    for edit_index in 0..10 {
                        if edit_index > 0 {
                            cx.background_executor().simulate_random_delay().await;
                        }
                        buffer.update(cx, |buffer, cx| {
                            let len = buffer.len();
                            if edit_index % 2 == 0 {
                                buffer.edit(
                                    [(0..0, format!("status-burst-head-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            } else {
                                buffer.edit(
                                    [(len..len, format!("status-burst-tail-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            }
                        });
                    }
                })
                .detach();
        });

        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_new_hunk_in_modified_file(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "
                    one
                    two
                    three
                    four
                    five
                    six
                    seven
                    eight
                    nine
                    ten
                    ELEVEN
                    twelve
                ".unindent()
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[(
                "foo.txt",
                "
                    one
                    two
                    three
                    four
                    five
                    six
                    seven
                    eight
                    nine
                    ten
                    eleven
                    twelve
                "
                .unindent(),
            )],
        );
        cx.run_until_parked();

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());

        assert_state_with_diff(
            &editor,
            cx,
            &"
                  ˇnine
                  ten
                - eleven
                + ELEVEN
                  twelve
            "
            .unindent(),
        );

        // The project diff updates its excerpts when a new hunk appears in a buffer that already has a diff.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/foo.txt"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| {
            buffer.edit_via_marked_text(
                &"
                    one
                    «TWO»
                    three
                    four
                    five
                    six
                    seven
                    eight
                    nine
                    ten
                    ELEVEN
                    twelve
                "
                .unindent(),
                None,
                cx,
            );
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();

        assert_state_with_diff(
            &editor,
            cx,
            &"
                  one
                - two
                + TWO
                  three
                  four
                  five
                  ˇnine
                  ten
                - eleven
                + ELEVEN
                  twelve
            "
            .unindent(),
        );
    }

    #[gpui::test]
    async fn test_sort_by_name_tie_breaks_on_path(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let git_panel = settings.git_panel.get_or_insert_default();
                    git_panel.sort_by = Some(GitPanelSortBy::Name);
                    git_panel.group_by = Some(GitPanelGroupBy::None);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.rs": { "src": { "foo.rs": "NESTED FOO\n" } },
                "src": { "foo.rs": "SRC FOO\n" },
                "m.rs": "M\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let (workspace_id, db) = cx.update(|_, cx| {
            (
                workspace.read(cx).database_id().unwrap(),
                persistence::ProjectDiffDb::global(cx),
            )
        });
        db.insert_test_workspace(workspace_id).await.unwrap();
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("foo.rs/src/foo.rs", "nested foo\n".into()),
                ("src/foo.rs", "src foo\n".into()),
                ("m.rs", "m\n".into()),
            ],
        );
        cx.run_until_parked();

        // Sorted by file name, the two `foo.rs` files come before `m.rs`, and the
        // tie between them is broken by the full path.
        let paths = diff.read_with(cx, |diff, cx| diff.excerpt_file_paths(cx));
        assert_eq!(paths, vec!["foo.rs/src/foo.rs", "src/foo.rs", "m.rs"]);

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.show_diff_review_overlay(DisplayRow(3)..DisplayRow(3), window, cx);
            let prompt_editor = editor.diff_review_prompt_editor().cloned().unwrap();
            prompt_editor.update(cx, |prompt_editor, cx| {
                prompt_editor.insert("stable path", window, cx);
            });
            editor.submit_diff_review_comment(window, cx);
        });
        let comments = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        let comments: serde_json::Value = serde_json::from_str(&comments).unwrap();
        assert_eq!(comments["schema_version"], 3);
        assert_eq!(comments["comments"][0]["file"], "foo.rs/src/foo.rs");
        editor.update_in(cx, |editor, window, cx| {
            editor
                .restore_review_comments_json(&comments.to_string(), window, cx)
                .unwrap();
        });
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        let restored: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored["comments"][0]["file"], "foo.rs/src/foo.rs");

        let mut legacy_comments = comments.clone();
        legacy_comments["schema_version"] = serde_json::json!(1);
        editor.update_in(cx, |editor, window, cx| {
            assert_eq!(
                editor
                    .restore_review_comments_json(&legacy_comments.to_string(), window, cx)
                    .unwrap(),
                0
            );
            assert_eq!(editor.orphaned_review_comment_summaries().len(), 1);
            editor
                .restore_review_comments_json(&comments.to_string(), window, cx)
                .unwrap();
        });

        db.flush_writes().await.unwrap();
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.git_panel.get_or_insert_default().sort_by = Some(GitPanelSortBy::Path);
                });
            });
        });
        cx.run_until_parked();

        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        let restored: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored["comments"][0]["file"], "foo.rs/src/foo.rs");
        assert_ne!(restored["comments"][0]["outdated"], true, "{restored:#}");

        db.flush_writes().await.unwrap();
        let review_key = diff.read_with(cx, |diff, cx| diff.review_comments_key(cx).unwrap());
        let persisted = db
            .get_review_comments(workspace_id, &review_key)
            .unwrap()
            .unwrap();
        let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(persisted["schema_version"], 3);
        assert_eq!(persisted["comments"][0]["file"], "foo.rs/src/foo.rs");
        assert_ne!(persisted["comments"][0]["outdated"], true);

        let legacy_comments = legacy_comments.to_string();
        db.save_review_comments(workspace_id, review_key.clone(), legacy_comments)
            .await
            .unwrap();
        for _ in 0..2 {
            cx.update(|window, cx| {
                diff.update(cx, |diff, cx| {
                    diff.loaded_review_comments_key = None;
                    diff.restore_review_comments_if_needed(window, cx);
                    assert!(!diff.review_comments_fully_restored);
                })
            });
            editor.read_with(cx, |editor, _| {
                assert_eq!(editor.orphaned_review_comment_summaries().len(), 1);
            });
            db.flush_writes().await.unwrap();
            let persisted = db
                .get_review_comments(workspace_id, &review_key)
                .unwrap()
                .unwrap();
            let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap();
            assert_eq!(persisted["schema_version"], 1);
        }
    }

    #[gpui::test]
    async fn test_tree_view_orders_directories_before_files(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let git_panel = settings.git_panel.get_or_insert_default();
                    git_panel.tree_view = Some(true);
                    git_panel.group_by = Some(GitPanelGroupBy::None);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "src": {
                    "a.rs": "A\n",
                    "m.rs": "M\n",
                    "sub": { "b.rs": "B\n" },
                },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        fs.set_head_and_index_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("src/a.rs", "a\n".into()),
                ("src/m.rs", "m\n".into()),
                ("src/sub/b.rs", "b\n".into()),
            ],
        );
        cx.run_until_parked();

        // In tree view the `src/sub/` directory sorts before the files directly
        // in `src/`. A plain path sort would interleave them as `src/a.rs`,
        // `src/m.rs`, `src/sub/b.rs`.
        let paths = diff.read_with(cx, |diff, cx| diff.excerpt_file_paths(cx));
        assert_eq!(paths, vec!["src/sub/b.rs", "src/a.rs", "src/m.rs"]);
    }

    #[gpui::test]
    async fn test_branch_diff(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "C",
                "b.txt": "new",
                "c.txt": "in-merge-base-and-work-tree",
                "d.txt": "created-in-head",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx
            .update(|window, cx| {
                ProjectDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "B".into()), ("d.txt", "created-in-head".into())],
            "sha",
        );
        // fs.set_index_for_repo(dot_git, index_state);
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("a.txt", "A".into()),
                ("c.txt", "in-merge-base-and-work-tree".into()),
            ],
        );
        cx.run_until_parked();

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());

        assert_state_with_diff(
            &editor,
            cx,
            &"
                - A
                + ˇC
                + new
                + created-in-head"
                .unindent(),
        );

        let statuses: HashMap<Arc<RelPath>, Option<FileStatus>> =
            editor.update(cx, |editor, cx| {
                editor
                    .buffer()
                    .read(cx)
                    .all_buffers()
                    .iter()
                    .map(|buffer| {
                        (
                            buffer.read(cx).file().unwrap().path().clone(),
                            editor.status_for_buffer_id(buffer.read(cx).remote_id(), cx),
                        )
                    })
                    .collect()
            });

        assert_eq!(
            statuses,
            HashMap::from_iter([
                (
                    rel_path("a.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Modified,
                        worktree_status: git::status::StatusCode::Modified
                    }))
                ),
                (rel_path("b.txt").into_arc(), Some(FileStatus::Untracked)),
                (
                    rel_path("d.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Added,
                        worktree_status: git::status::StatusCode::Added
                    }))
                )
            ])
        );
    }

    #[gpui::test]
    async fn test_branch_diff_action_matches_existing_item_by_base_ref(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "changed",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let target_branch_diff = cx
            .update(|window, cx| {
                let Some(repository) = project.read(cx).active_repository(cx) else {
                    return Task::ready(Err(anyhow!("No active repository")));
                };
                ProjectDiff::new_with_branch_base(
                    project.clone(),
                    workspace.clone(),
                    "topic".into(),
                    repository,
                    false,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(target_branch_diff.clone()),
                None,
                true,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(BranchDiff.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let (active_base_ref, mut base_refs) = workspace.update(cx, |workspace, cx| {
            let active_item = workspace.active_item_as::<ProjectDiff>(cx).unwrap();
            let active_base_ref = match active_item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => base_ref.to_string(),
                DiffBase::Head => panic!("expected active item to be a branch diff"),
            };
            let base_refs = workspace
                .items_of_type::<ProjectDiff>(cx)
                .filter_map(|item| match item.read(cx).diff_base(cx) {
                    DiffBase::Merge { base_ref } => Some(base_ref.to_string()),
                    DiffBase::Head => None,
                })
                .collect::<Vec<_>>();
            (active_base_ref, base_refs)
        });
        base_refs.sort();

        assert_eq!(active_base_ref, "origin/main");
        assert_eq!(base_refs, vec!["origin/main", "topic"]);
    }

    #[gpui::test]
    async fn test_refresh_default_branch_diff_updates_base_ref(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "changed",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx
            .update(|window, cx| {
                let Some(repository) = project.read(cx).active_repository(cx) else {
                    return Task::ready(Err(anyhow!("No active repository")));
                };
                ProjectDiff::new_with_branch_base(
                    project.clone(),
                    workspace,
                    "origin/main".into(),
                    repository,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.default_branch = Some("origin/dev".into());
        })
        .unwrap();
        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.refresh_diff(window, cx);
            });
        });
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| match diff.diff_base(cx) {
            DiffBase::Merge { base_ref } => assert_eq!(base_ref.as_ref(), "origin/dev"),
            DiffBase::Head => panic!("expected branch diff"),
        });
    }

    #[gpui::test]
    async fn test_refresh_explicit_branch_diff_keeps_base_ref(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "changed",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx
            .update(|window, cx| {
                let Some(repository) = project.read(cx).active_repository(cx) else {
                    return Task::ready(Err(anyhow!("No active repository")));
                };
                ProjectDiff::new_with_branch_base(
                    project.clone(),
                    workspace,
                    "origin/main".into(),
                    repository,
                    false,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();

        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.default_branch = Some("origin/dev".into());
        })
        .unwrap();
        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.refresh_diff(window, cx);
            });
        });
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| match diff.diff_base(cx) {
            DiffBase::Merge { base_ref } => assert_eq!(base_ref.as_ref(), "origin/main"),
            DiffBase::Head => panic!("expected branch diff"),
        });
    }

    #[gpui::test]
    async fn test_project_diff_persistence_keeps_default_branch_tracking(cx: &mut TestAppContext) {
        init_review_db_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ ".git": {} }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());

        let (db, workspace_id) = cx.update(|_, cx| {
            (
                persistence::ProjectDiffDb::global(cx),
                workspace.read(cx).database_id().unwrap(),
            )
        });
        let item_id = 1;
        let diff_base = DiffBase::Merge {
            base_ref: "origin/main".into(),
        };

        db.insert_test_workspace(workspace_id).await.unwrap();
        db.save_diff_base(item_id, workspace_id, diff_base.clone(), true)
            .await
            .unwrap();

        assert_eq!(
            db.get_diff_base(item_id, workspace_id).unwrap(),
            (diff_base, true)
        );
    }

    #[gpui::test]
    async fn test_update_on_uncommit(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "README.md": "# My cool project\n".to_owned()
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("README.md", "# My cool project\n".to_owned())],
        );
        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let _editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path("README.md")), None, true, window, cx)
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();

        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(project_diff::Diff.boxed_clone(), cx);
        });
        cx.run_until_parked();
        let item = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ProjectDiff>(cx).unwrap()
        });
        cx.focus(&item);
        let editor = item.read_with(cx, |item, cx| item.editor.read(cx).rhs_editor().clone());

        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[(
                "README.md",
                "# My cool project\nDetails to come.\n".to_owned(),
            )],
        );
        cx.run_until_parked();

        let mut cx = EditorTestContext::for_editor_in(editor, cx).await;

        cx.assert_excerpts_with_selections("[EXCERPT]\nˇ# My cool project\nDetails to come.\n");
    }

    #[gpui::test]
    async fn test_deploy_at_respects_active_repository_selection(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project_a"),
            json!({
                ".git": {},
                "a.txt": "CHANGED_A\n",
            }),
        )
        .await;
        fs.insert_tree(
            path!("/project_b"),
            json!({
                ".git": {},
                "b.txt": "CHANGED_B\n",
            }),
        )
        .await;

        fs.set_head_and_index_for_repo(
            Path::new(path!("/project_a/.git")),
            &[("a.txt", "original_a\n".to_string())],
        );
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project_b/.git")),
            &[("b.txt", "original_b\n".to_string())],
        );

        let project = Project::test(
            fs.clone(),
            [
                Path::new(path!("/project_a")),
                Path::new(path!("/project_b")),
            ],
            cx,
        )
        .await;

        let (worktree_a_id, worktree_b_id) = project.read_with(cx, |project, cx| {
            let mut worktrees: Vec<_> = project.worktrees(cx).collect();
            worktrees.sort_by_key(|w| w.read(cx).abs_path());
            (worktrees[0].read(cx).id(), worktrees[1].read(cx).id())
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        // Select project A explicitly and open the diff.
        workspace.update(cx, |workspace, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            git_store.update(cx, |git_store, cx| {
                git_store.set_active_repo_for_worktree(worktree_a_id, cx);
            });
        });
        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(project_diff::Diff.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let diff_item = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ProjectDiff>(cx).unwrap()
        });
        let paths_a = diff_item.read_with(cx, |diff, cx| diff.excerpt_paths(cx));
        assert_eq!(paths_a.len(), 1);
        assert_eq!(*paths_a[0], *"a.txt");

        // Switch the explicit active repository to project B and re-run the diff action.
        workspace.update(cx, |workspace, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            git_store.update(cx, |git_store, cx| {
                git_store.set_active_repo_for_worktree(worktree_b_id, cx);
            });
        });
        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(project_diff::Diff.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let same_diff_item = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<ProjectDiff>(cx).unwrap()
        });
        assert_eq!(diff_item.entity_id(), same_diff_item.entity_id());

        let paths_b = diff_item.read_with(cx, |diff, cx| diff.excerpt_paths(cx));
        assert_eq!(paths_b.len(), 1);
        assert_eq!(*paths_b[0], *"b.txt");
    }
}
