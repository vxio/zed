use crate::{
    branch_picker, conflict_view,
    git_panel::{GitPanel, GitPanelAddon, GitStatusEntry},
    git_panel_settings::GitPanelSettings,
};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, anyhow};
use buffer_diff::{BufferDiff, DiffHunkSecondaryStatus};
use collections::HashMap;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, SelectionEffects, SplittableEditor,
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
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, Render, Subscription, Task, WeakEntity, actions,
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
    vertical_divider,
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
        LeaderAndFollower,
        /// Compare with a specific branch
        CompareWithBranch,
    ]
);

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
        /// Deletes all review comments after confirmation by archiving them.
        DeleteReviewComments,
        /// Restores the newest archived review comment session.
        RestoreLatestReviewComments,
        /// Restores the newest archived user-authored review comment session.
        RestoreLatestUserReviewComments,
        /// Restores the newest archived agent-authored review comment session.
        RestoreLatestAgentReviewComments,
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
    last_review_comments_db_modified_on: Option<SystemTime>,
    restoring_review_comments: bool,
    _task: Task<Result<()>>,
    _review_comments_poll_task: Task<()>,
    _subscription: Subscription,
}

#[derive(serde::Deserialize)]
struct ReviewCommentsForDisplay {
    comments: Vec<ReviewCommentForDisplay>,
}

#[derive(serde::Deserialize)]
struct ReviewCommentForDisplay {
    author: String,
    file: String,
    line_start: u32,
    body: String,
    #[serde(default)]
    outdated: bool,
    #[serde(default)]
    outdated_reason: Option<String>,
}

#[derive(Clone)]
struct OutdatedReviewCommentSummary {
    location: String,
    body: String,
    reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshReason {
    DiffChanged,
    StatusesChanged,
    EditorSaved,
}

const CONFLICT_SORT_PREFIX: u64 = 1;
const TRACKED_SORT_PREFIX: u64 = 2;
const NEW_SORT_PREFIX: u64 = 3;

#[derive(Clone, Copy)]
enum ReviewCommentArchiveScope {
    All,
    User,
    Agent,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreLatestReviewCommentsResult {
    Restored,
    NoArchivedComments,
    Failed,
}

fn restore_latest_review_comments_notification_id() -> NotificationId {
    NotificationId::named("zed-review-restore-latest".into())
}

impl ReviewCommentArchiveScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::User => "user",
            Self::Agent => "agent",
            Self::Deleted => "deleted",
        }
    }

    fn includes(self, comment: &serde_json::Value) -> bool {
        match self {
            Self::All | Self::Deleted => true,
            Self::User => comment_author(comment) == "vxio",
            Self::Agent => comment_author(comment) == "amp",
        }
    }
}

fn comment_author(comment: &serde_json::Value) -> &str {
    comment
        .get("author")
        .and_then(|author| author.as_str())
        .unwrap_or("vxio")
}

impl ProjectDiff {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(Self::deploy);
        workspace.register_action(Self::deploy_branch_diff);
        workspace.register_action(Self::compare_with_branch);
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
        workspace.register_action(|workspace, _: &Add, window, cx| {
            Self::deploy(workspace, &Diff, window, cx);
        });
        workspace::register_serializable_item::<ProjectDiff>(cx);
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
                    project_diff.branch_diff.update(cx, |branch_diff, cx| {
                        branch_diff.set_repo(Some(intended_repo), cx);
                    });
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

    fn legacy_review_comments_key(&self, cx: &App) -> Option<String> {
        let repo = self.branch_diff.read(cx).repo()?.read(cx);
        let diff_base = serde_json::to_string(self.diff_base(cx)).log_err()?;
        Some(format!(
            "{}\n{}",
            repo.work_directory_abs_path.display(),
            diff_base
        ))
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
        let session_id = format!("archived:{archived_on}:{}", scope.as_str());
        let db = persistence::ProjectDiffDb::global(cx);
        let archive = db.archive_review_comments(
            workspace_id,
            review_key.clone(),
            session_id,
            archived_json,
            remaining_json.clone(),
            archived_on,
            scope.as_str().to_string(),
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
        let Some((_session_id, comments_json)) = (match db.latest_archived_review_comments(
            workspace_id,
            &review_key,
            scope.map(ReviewCommentArchiveScope::as_str),
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
        let db_modified_on = db.modified_on();
        if db_modified_on.is_some()
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
            if let Some(legacy_key) = self.legacy_review_comments_key(cx) {
                if legacy_key != review_key {
                    comments_json = match db.get_review_comments(workspace_id, &legacy_key) {
                        Ok(comments_json) => comments_json,
                        Err(error) => {
                            log::error!("failed to load legacy review comments: {error:#}");
                            None
                        }
                    };
                    if comments_json.is_some() {
                        loaded_key = legacy_key;
                    }
                }
            }
        }
        if self.loaded_review_comments_key.as_ref() == Some(&review_key)
            && self.loaded_review_comments_json.as_deref() == comments_json.as_deref()
        {
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

        let mut loaded = true;
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
        if loaded {
            self.last_review_comments_db_modified_on = db_modified_on;
            if loaded_key != review_key {
                if let Some(comments_json) = comments_json.clone() {
                    self.persist_review_comments(comments_json, cx);
                }
            }
            self.loaded_review_comments_key = Some(review_key);
            self.loaded_review_comments_json = comments_json;
        }
    }

    fn outdated_review_comment_summaries(&self, cx: &App) -> Vec<OutdatedReviewCommentSummary> {
        let Ok(comments_json) = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .review_comments_json(cx)
        else {
            return Vec::new();
        };
        let Ok(snapshot) = serde_json::from_str::<ReviewCommentsForDisplay>(&comments_json) else {
            return Vec::new();
        };

        snapshot
            .comments
            .into_iter()
            .filter(|comment| comment.outdated)
            .map(|comment| {
                let reason = match comment.outdated_reason.as_deref() {
                    Some("file_not_in_diff") => "file no longer has a visible diff",
                    Some("line_not_in_diff") => "line no longer has a visible diff",
                    _ => "change no longer has a visible diff",
                }
                .to_string();
                OutdatedReviewCommentSummary {
                    location: format!(
                        "{}:{} · {}",
                        comment.file, comment.line_start, comment.author
                    ),
                    body: comment.body,
                    reason,
                }
            })
            .collect()
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
                Self::new_impl(branch_diff, project, workspace, window, cx)
            })
        })
    }

    fn new_with_branch_base(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        base_ref: SharedString,
        repo: Entity<Repository>,
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
                Self::new_impl(branch_diff, project, workspace, window, cx)
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
        Self::new_impl(branch_diff, project, workspace, window, cx)
    }

    fn new_impl(
        branch_diff: Entity<branch_diff::BranchDiff>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
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
                DiffBase::Merge { .. } => diff_display_editor.disable_diff_hunk_controls(cx),
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
            last_review_comments_db_modified_on: None,
            restoring_review_comments: false,
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
                buffer_diff::BufferDiffEvent::BaseTextChanged
                | buffer_diff::BufferDiffEvent::HunksStagedOrUnstaged(_) => {}
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
                this.restore_review_comments_if_needed(window, cx);
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
    RelPath::unix(&synthetic)
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
    RelPath::unix(&synthetic)
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
                .filter(|comment| comment.get("side").and_then(|side| side.as_str()) == Some("new"))
                .count()
        })
        .unwrap_or(0)
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
        let outdated_comments = self.outdated_review_comment_summaries(cx);
        let outdated_comments_for_empty = outdated_comments.clone();

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
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_around()
                                .child(Label::new("No uncommitted changes")),
                        )
                        .when(!outdated_comments_for_empty.is_empty(), |el| {
                            let count = outdated_comments_for_empty.len();
                            el.child(
                                v_flex()
                                    .gap_1()
                                    .mt_2()
                                    .p_2()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .rounded_md()
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(
                                                Icon::new(IconName::Warning).size(IconSize::XSmall),
                                            )
                                            .child(Label::new(format!(
                                                "Outdated review comments ({count})"
                                            ))),
                                    )
                                    .children(outdated_comments_for_empty.into_iter().map(
                                        |comment| {
                                            v_flex()
                                                .gap_0p5()
                                                .child(
                                                    Label::new(format!(
                                                        "{} — {}",
                                                        comment.location, comment.reason
                                                    ))
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(comment.body).size(LabelSize::Small),
                                                )
                                        },
                                    )),
                            )
                        })
                        .map(|el| match remote_button {
                            Some(button) => el.child(h_flex().justify_around().child(button)),
                            None => el.child(
                                h_flex()
                                    .justify_around()
                                    .child(Label::new("Remote up to date")),
                            ),
                        })
                        .child(
                            h_flex().justify_around().mt_1().child(
                                Button::new("project-diff-close-button", "Close")
                                    // .style(ButtonStyle::Transparent)
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
                )
            })
            .when(!is_empty, |el| {
                el.child(
                    v_flex()
                        .size_full()
                        .when(!outdated_comments.is_empty(), |el| {
                            let count = outdated_comments.len();
                            el.child(
                                v_flex()
                                    .gap_1()
                                    .p_2()
                                    .border_b_1()
                                    .border_color(cx.theme().colors().border)
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(
                                                Icon::new(IconName::Warning).size(IconSize::XSmall),
                                            )
                                            .child(Label::new(format!(
                                                "Outdated review comments ({count})"
                                            ))),
                                    )
                                    .children(outdated_comments.into_iter().map(|comment| {
                                        h_flex()
                                            .gap_1()
                                            .child(
                                                Label::new(comment.location).size(LabelSize::Small),
                                            )
                                            .child(
                                                Label::new(comment.reason)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            )
                                            .child(Label::new(comment.body).size(LabelSize::Small))
                                    })),
                            )
                        })
                        .child(self.editor.clone()),
                )
            })
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
            let diff_base = db.get_diff_base(item_id, workspace_id)?;

            let diff = cx.update(|window, cx| {
                let branch_diff = cx
                    .new(|cx| branch_diff::BranchDiff::new(diff_base, project.clone(), window, cx));
                let workspace = workspace.upgrade().context("workspace gone")?;
                anyhow::Ok(
                    cx.new(|cx| ProjectDiff::new_impl(branch_diff, project, workspace, window, cx)),
                )
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

        let db = persistence::ProjectDiffDb::global(cx);
        Some(cx.background_spawn({
            async move {
                db.save_diff_base(item_id, workspace_id, diff_base.clone())
                    .await
            }
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

mod persistence {

    use anyhow::Context as _;
    use db::{
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use project::git_store::branch_diff::DiffBase;
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
        ];
    }

    db::static_connection!(ProjectDiffDb, [WorkspaceDb]);

    impl ProjectDiffDb {
        pub async fn save_diff_base(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
            diff_base: DiffBase,
        ) -> anyhow::Result<()> {
            self.write(move |connection| {
                let sql_stmt = sql!(
                    INSERT OR REPLACE INTO project_diffs(item_id, workspace_id, diff_base) VALUES (?, ?, ?)
                );
                let diff_base_str = serde_json::to_string(&diff_base)?;
                let mut query = connection.exec_bound::<(ItemId, WorkspaceId, String)>(sql_stmt)?;
                query((item_id, workspace_id, diff_base_str)).context(format!(
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
        ) -> anyhow::Result<DiffBase> {
            let sql_stmt =
                sql!(SELECT diff_base FROM project_diffs WHERE item_id =  ?AND workspace_id =  ?);
            let diff_base_str = self.select_row_bound::<(ItemId, WorkspaceId), String>(sql_stmt)?(
                (item_id, workspace_id),
            )
            .context(::std::format!(
                "Error in get_diff_base, select_row_bound failed to execute or parse for: {}",
                sql_stmt
            ))?;
            let Some(diff_base_str) = diff_base_str else {
                return Ok(DiffBase::Head);
            };
            serde_json::from_str(&diff_base_str).context("deserializing diff base")
        }

        pub fn save_review_comments(
            &self,
            workspace_id: WorkspaceId,
            review_key: String,
            comments_json: String,
        ) -> impl Future<Output = anyhow::Result<()>> + use<> {
            self.write(move |connection| {
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
                query((workspace_id, review_key, comments_json)).context(format!(
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
            std::fs::metadata(db::db_path(db::database_dir(), *db::RELEASE_CHANNEL))
                .and_then(|metadata| metadata.modified())
                .ok()
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
            .child(vertical_divider())
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
                el.child(vertical_divider()).child(
                    render_send_review_to_agent_button(review_count, &focus_handle).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&SendReviewToAgent, window, cx)
                        }),
                    ),
                )
            })
    }
}

fn render_send_review_to_agent_button(review_count: usize, focus_handle: &FocusHandle) -> Button {
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
}

impl BranchDiffToolbar {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { project_diff: None }
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
}

impl EventEmitter<ToolbarItemEvent> for BranchDiffToolbar {}

impl ToolbarItemView for BranchDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.project_diff = active_pane_item
            .and_then(|item| item.act_as::<ProjectDiff>(cx))
            .filter(|item| matches!(item.read(cx).diff_base(cx), DiffBase::Merge { .. }))
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

impl Render for BranchDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project_diff) = self.project_diff(cx) else {
            return div();
        };
        let focus_handle = project_diff.focus_handle(cx);
        let review_count = project_diff.read(cx).total_review_comment_count();
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
                this.child(vertical_divider()).child(
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
    use std::path::Path;
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

    #[gpui::test]
    async fn test_archive_user_review_comments_clears_visible_diff(cx: &mut TestAppContext) {
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
    async fn test_restore_latest_review_comments_restores_newest_once(cx: &mut TestAppContext) {
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

    #[gpui::test]
    async fn test_review_comments_survive_restore_clear_and_restore(cx: &mut TestAppContext) {
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

        editor.update(cx, |editor, cx| editor.clear_review_comments(cx));
        cx.run_until_parked();

        assert_eq!(
            db.get_review_comments(workspace_id, &review_key)
                .unwrap()
                .unwrap(),
            comments_json
        );

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

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        editor.update_in(cx, |editor, window, cx| {
            editor.show_diff_review_overlay(DisplayRow(0)..DisplayRow(0), window, cx);
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
                assert_eq!(
                    diff.loaded_review_comments_json.as_deref(),
                    Some(initial_comments_json.as_str())
                );
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
        init_test(cx);

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
    }

    #[gpui::test]
    async fn test_review_comments_keep_unmapped_locations(cx: &mut TestAppContext) {
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
                assert!(diff.loaded_review_comments_key.is_none());
            })
        });

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(restored.contains("restores first"));
        assert!(restored.contains("restores later"));
    }

    #[gpui::test]
    async fn test_review_comments_restore_waits_for_pending_save(cx: &mut TestAppContext) {
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
        let empty_comments_json = serde_json::json!({
            "schema_version": 1,
            "comments": []
        })
        .to_string();
        cx.update(|_, cx| {
            diff.update(cx, |diff, cx| {
                diff.persist_review_comments(empty_comments_json, cx);
            })
        });

        db.flush_writes().await.unwrap();
        assert_eq!(
            db.get_review_comments(workspace_id, &review_key)
                .unwrap()
                .unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "comments": []
            })
            .to_string()
        );

        cx.update(|window, cx| {
            diff.update(cx, |diff, cx| {
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            });
        });

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored = editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        assert!(!restored.contains("delete me"));
    }

    #[gpui::test]
    async fn test_review_comment_add_persists_and_restores(cx: &mut TestAppContext) {
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
            editor.show_diff_review_overlay(DisplayRow(1)..DisplayRow(1), window, cx);
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
    async fn test_review_comment_restores_as_outdated_after_diff_removed(cx: &mut TestAppContext) {
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
            editor.show_diff_review_overlay(DisplayRow(1)..DisplayRow(1), window, cx);
            let prompt_editor = editor.diff_review_prompt_editor().cloned().unwrap();
            prompt_editor.update(cx, |prompt_editor, cx| {
                prompt_editor.insert("removed diff comment", window, cx);
            });
            editor.submit_diff_review_comment(window, cx);
        });
        db.flush_writes().await.unwrap();

        fs.insert_file(path!("/project/foo.txt"), b"foo\n".to_vec())
            .await;
        cx.run_until_parked();

        let reopened_diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace.clone(), window, cx)
        });
        cx.run_until_parked();

        let reopened_editor =
            reopened_diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
        let restored =
            reopened_editor.read_with(cx, |editor, cx| editor.review_comments_json(cx).unwrap());
        let restored: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored["comments"][0]["body"], "removed diff comment");
        assert_eq!(restored["comments"][0]["outdated"], true);
        assert_eq!(
            restored["comments"][0]["outdated_reason"],
            "file_not_in_diff"
        );

        let summaries =
            reopened_diff.read_with(cx, |diff, cx| diff.outdated_review_comment_summaries(cx));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].body, "removed diff comment");
    }

    #[gpui::test]
    async fn test_review_comments_restore_after_diff_base_changes(cx: &mut TestAppContext) {
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
                assert_eq!(diff.review_comments_key(cx).as_ref(), Some(&review_key));
                diff.loaded_review_comments_key = None;
                diff.restore_review_comments_if_needed(window, cx);
            })
        });

        let editor = diff.read_with(cx, |diff, cx| diff.editor.read(cx).rhs_editor().clone());
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
        init_test(cx);

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
                "lib": { "foo.rs": "LIB FOO\n" },
                "src": { "foo.rs": "SRC FOO\n" },
                "m.rs": "M\n",
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
                ("lib/foo.rs", "lib foo\n".into()),
                ("src/foo.rs", "src foo\n".into()),
                ("m.rs", "m\n".into()),
            ],
        );
        cx.run_until_parked();

        // Sorted by file name, the two `foo.rs` files come before `m.rs`, and the
        // tie between them is broken by the full path (`lib/` before `src/`).
        // A plain path sort would instead order them `lib/foo.rs`, `m.rs`,
        // `src/foo.rs`.
        let paths = diff.read_with(cx, |diff, cx| diff.excerpt_file_paths(cx));
        assert_eq!(paths, vec!["lib/foo.rs", "src/foo.rs", "m.rs"]);
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
