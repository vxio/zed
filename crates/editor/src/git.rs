pub(super) mod blame;

use super::*;
use crate::display_map::{Crease, CreaseMetadata, FoldId};
use ::git::{
    Oid, Restore, blame::BlameEntry, commit::ParsedCommitMessage, repository::RepoPath,
    status::FileStatus,
};
use buffer_diff::{BufferDiff, DiffHunkStatus, DiffHunkStatusKind};
use fuzzy::{PathMatch, StringMatch, StringMatchCandidate};
use gpui::{InteractiveElement as _, PromptLevel};
use language::{CodeLabel, CodeLabelBuilder, HighlightId};
use ordered_float::OrderedFloat;
use project::{
    Completion, CompletionDisplayOptions, CompletionGroup, CompletionResponse,
    PathMatchCandidateSet, Project, ProjectPath, Symbol, WorktreeId, git_store::Repository,
    lsp_store::SymbolLocation,
};
use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::AtomicBool,
};
use text::{Anchor as TextAnchor, ToPoint as _};
use util::{
    paths::{PathStyle, UrlExt},
    rel_path::RelPath,
    truncate_and_remove_front,
};
use workspace::OpenOptions;

#[derive(Clone)]
pub struct ResolvedDiffHunk {
    pub buffer_range: Range<text::Anchor>,
    pub diff_base_byte_range: Range<usize>,
    pub status: DiffHunkStatus,
}

#[derive(Clone)]
pub struct ResolvedDiffHunks {
    pub diff: Entity<BufferDiff>,
    pub buffer_id: BufferId,
    pub buffer: Option<Entity<Buffer>>,
    pub hunks: Vec<ResolvedDiffHunk>,
}

pub trait DiffHunkDelegate {
    fn toggle(
        &self,
        hunks: Vec<ResolvedDiffHunks>,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut Context<Editor>,
    );

    fn stage_or_unstage(
        &self,
        stage: bool,
        hunks: Vec<ResolvedDiffHunks>,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut Context<Editor>,
    );

    fn restore(
        &self,
        hunks: Vec<ResolvedDiffHunks>,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        if hunks.is_empty() || editor.read_only(cx) {
            return;
        }
        self.stage_or_unstage(false, hunks.clone(), editor, window, cx);
        editor.transact(window, cx, |editor, window, cx| {
            editor.restore_diff_hunks(hunks, cx);
            let selections = editor
                .selections
                .all::<MultiBufferOffset>(&editor.display_snapshot(cx));
            editor.change_selections(
                SelectionEffects::no_scroll(),
                window,
                cx,
                |selections_state| {
                    selections_state.select(selections);
                },
            );
        });
    }

    fn render_hunk_controls(
        &self,
        row: u32,
        status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement;

    fn render_hunk_as_staged(&self, status: &DiffHunkStatus, _cx: &App) -> bool {
        !status.has_secondary_hunk()
    }
}

pub struct UncommittedDiffHunkDelegate;

impl DiffHunkDelegate for UncommittedDiffHunkDelegate {
    fn toggle(
        &self,
        hunks: Vec<ResolvedDiffHunks>,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let stage = hunks
            .iter()
            .flat_map(|hunks| hunks.hunks.iter())
            .any(|hunk| hunk.status.has_secondary_hunk());
        self.stage_or_unstage(stage, hunks, editor, window, cx);
    }

    fn stage_or_unstage(
        &self,
        stage: bool,
        hunks: Vec<ResolvedDiffHunks>,
        editor: &mut Editor,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let Some(project) = editor.project() else {
            return;
        };
        for hunks in hunks {
            let Some(buffer) = hunks.buffer else {
                continue;
            };

            let ranges = hunks
                .hunks
                .into_iter()
                .map(|hunk| hunk.buffer_range)
                .collect::<Vec<_>>();
            if ranges.is_empty() {
                continue;
            }
            let secondary_diff = hunks.diff.read(cx).secondary_diff();
            project
                .update(cx, |project, cx| {
                    if stage {
                        let Some(secondary_diff) = secondary_diff else {
                            return Err(anyhow::anyhow!("diff has no unstaged secondary"));
                        };
                        project.stage_hunks(buffer, secondary_diff, ranges, cx)
                    } else {
                        project.unstage_uncommitted_hunks(buffer, hunks.diff, ranges, cx)
                    }
                })
                .log_err();
        }
    }

    fn render_hunk_controls(
        &self,
        row: u32,
        status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        render_diff_hunk_controls(
            row,
            status,
            hunk_range,
            is_created_file,
            line_height,
            editor,
            window,
            cx,
        )
    }
}

pub struct RestoreOnlyDiffHunkDelegate;

impl DiffHunkDelegate for RestoreOnlyDiffHunkDelegate {
    fn toggle(
        &self,
        _hunks: Vec<ResolvedDiffHunks>,
        _editor: &mut Editor,
        _window: &mut Window,
        _cx: &mut Context<Editor>,
    ) {
    }

    fn stage_or_unstage(
        &self,
        _stage: bool,
        _hunks: Vec<ResolvedDiffHunks>,
        _editor: &mut Editor,
        _window: &mut Window,
        _cx: &mut Context<Editor>,
    ) {
    }

    fn restore(
        &self,
        _hunks: Vec<ResolvedDiffHunks>,
        _editor: &mut Editor,
        _window: &mut Window,
        _cx: &mut Context<Editor>,
    ) {
    }

    fn render_hunk_controls(
        &self,
        _row: u32,
        _status: &DiffHunkStatus,
        _hunk_range: Range<Anchor>,
        _is_created_file: bool,
        _line_height: Pixels,
        _editor: &Entity<Editor>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        gpui::Empty.into_any_element()
    }
}

pub struct RestoreOnlyUnstagedDiffHunkDelegate;

impl DiffHunkDelegate for RestoreOnlyUnstagedDiffHunkDelegate {
    fn toggle(
        &self,
        _hunks: Vec<ResolvedDiffHunks>,
        _editor: &mut Editor,
        _window: &mut Window,
        _cx: &mut Context<Editor>,
    ) {
    }

    fn stage_or_unstage(
        &self,
        _stage: bool,
        _hunks: Vec<ResolvedDiffHunks>,
        _editor: &mut Editor,
        _window: &mut Window,
        _cx: &mut Context<Editor>,
    ) {
    }

    fn render_hunk_controls(
        &self,
        _row: u32,
        _status: &DiffHunkStatus,
        _hunk_range: Range<Anchor>,
        _is_created_file: bool,
        _line_height: Pixels,
        _editor: &Entity<Editor>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        gpui::Empty.into_any_element()
    }

    fn render_hunk_as_staged(&self, _status: &DiffHunkStatus, _cx: &App) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DisplayDiffHunk {
    Folded {
        display_row: DisplayRow,
    },
    Unfolded {
        is_created_file: bool,
        diff_base_byte_range: Range<usize>,
        display_row_range: Range<DisplayRow>,
        multi_buffer_range: Range<Anchor>,
        status: DiffHunkStatus,
        word_diffs: Vec<Range<MultiBufferOffset>>,
    },
}

#[derive(Clone)]
pub(super) struct InlineBlamePopoverState {
    pub(super) scroll_handle: ScrollHandle,
    pub(super) commit_message: Option<ParsedCommitMessage>,
    pub(super) markdown: Entity<Markdown>,
}

pub(super) struct InlineBlamePopover {
    pub(super) position: gpui::Point<Pixels>,
    pub(super) hide_task: Option<Task<()>>,
    pub(super) popover_bounds: Option<Bounds<Pixels>>,
    pub(super) popover_state: InlineBlamePopoverState,
    pub(super) keyboard_grace: bool,
}

/// Represents a diff review button indicator that shows up when hovering over lines in the gutter
/// in diff view mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PhantomDiffReviewIndicator {
    /// The starting anchor of the selection (or the only row if not dragging).
    pub(super) start: Anchor,
    /// The ending anchor of the selection. Equal to start_anchor for single-line selection.
    pub(super) end: Anchor,
    /// There's a small debounce between hovering over the line and showing the indicator.
    /// We don't want to show the indicator when moving the mouse from editor to e.g. project panel.
    pub(super) is_active: bool,
}

#[derive(Clone, Debug)]
pub(super) struct DiffReviewDragState {
    start_anchor: Anchor,
    current_anchor: Anchor,
}

/// Identifies a specific hunk in the diff buffer.
/// Used as a key to group comments by their location.
#[derive(Clone, Debug)]
pub(super) struct DiffHunkKey {
    /// The file path (relative to worktree) this hunk belongs to.
    pub(super) file_path: Arc<util::rel_path::RelPath>,
    /// An anchor at the start of the hunk. This tracks position as the buffer changes.
    pub(super) hunk_start_anchor: Anchor,
}

#[derive(Clone)]
pub(super) struct StoredReviewComment {
    pub(super) id: usize,
    pub(super) author: String,
    pub(super) created_at: Option<String>,
    pub(super) deleted_on: Option<String>,
    pub(super) resolved_on: Option<String>,
    pub(super) comment: String,
    pub(super) range: Range<Anchor>,
    pub(super) replies: Vec<StoredReviewReply>,
    pub(super) is_editing: bool,
    pub(super) outdated: bool,
    pub(super) outdated_reason: Option<String>,
    pub(super) review_round: Option<u32>,
    /// Runtime-only: expands a resolved thread in place without unresolving it.
    pub(super) show_resolved: bool,
    location: ReviewCommentLocation,
}

#[derive(Clone)]
pub(super) struct StoredReviewReply {
    pub(super) id: usize,
    pub(super) author: String,
    pub(super) created_at: Option<String>,
    pub(super) comment: String,
    pub(super) is_editing: bool,
    pub(super) review_round: Option<u32>,
}

#[derive(Clone)]
pub(super) struct DiffReviewEditorCancel {
    parent: WeakEntity<Editor>,
    comment_id: usize,
    kind: DiffReviewEditorKind,
}

#[derive(Clone, Copy)]
enum DiffReviewEditorKind {
    Edit,
    ReplyEdit,
    Reply,
}

impl DiffReviewEditorCancel {
    fn edit(parent: WeakEntity<Editor>, comment_id: usize) -> Self {
        Self {
            parent,
            comment_id,
            kind: DiffReviewEditorKind::Edit,
        }
    }

    fn reply(parent: WeakEntity<Editor>, comment_id: usize) -> Self {
        Self {
            parent,
            comment_id,
            kind: DiffReviewEditorKind::Reply,
        }
    }

    fn reply_edit(parent: WeakEntity<Editor>, reply_id: usize) -> Self {
        Self {
            parent,
            comment_id: reply_id,
            kind: DiffReviewEditorKind::ReplyEdit,
        }
    }

    pub(super) fn cancel(&self, window: &mut Window, cx: &mut Context<Editor>) {
        if let Some(parent) = self.parent.upgrade() {
            parent.update(cx, |editor, cx| match self.kind {
                DiffReviewEditorKind::Edit => {
                    editor.cancel_edit_review_comment(self.comment_id, window, cx)
                }
                DiffReviewEditorKind::ReplyEdit => {
                    editor.cancel_edit_review_reply(self.comment_id, window, cx)
                }
                DiffReviewEditorKind::Reply => {
                    editor.cancel_review_reply(self.comment_id, window, cx)
                }
            });
        }
    }
}

const DEFAULT_REVIEW_COMMENT_AUTHOR: &str = "vxio";

#[derive(Clone)]
enum ReviewCommentBodySegment {
    Text(String),
    Mention(ReviewCommentMention),
}

#[derive(Clone)]
struct ReviewCommentMention {
    label: String,
    url: String,
    tooltip: Option<SharedString>,
    icon: IconName,
    show_at_prefix: bool,
}

struct ReviewCommentEditorAddon;

impl Addon for ReviewCommentEditorAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        key_context.add("review_comment_editor");
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct ReviewCommentLocation {
    file: String,
    side: String,
    hunk_line: u32,
    line_start: u32,
    line_end: u32,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ReviewCommentsSnapshot {
    schema_version: u32,
    comments: Vec<ReviewCommentSnapshot>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(super) struct ReviewCommentSnapshot {
    id: usize,
    #[serde(default = "default_review_comment_author")]
    author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deleted_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_on: Option<String>,
    file: String,
    side: String,
    hunk_line: u32,
    line_start: u32,
    line_end: u32,
    body: String,
    #[serde(default, skip_serializing_if = "is_false")]
    outdated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outdated_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review_round: Option<u32>,
    #[serde(default)]
    replies: Vec<ReviewReplySnapshot>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct ReviewReplySnapshot {
    id: usize,
    #[serde(default = "default_review_comment_author")]
    author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review_round: Option<u32>,
}

fn default_review_comment_author() -> String {
    DEFAULT_REVIEW_COMMENT_AUTHOR.to_string()
}

fn is_false(value: &bool) -> bool {
    !value
}

fn current_review_comment_timestamp() -> Option<String> {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(format)
        .ok()
}

fn review_comment_timestamp_label(created_at: &str) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let Ok(created_at) = time::PrimitiveDateTime::parse(created_at, format) else {
        return created_at.to_string();
    };
    review_comment_timestamp_label_from(now, created_at.assume_offset(now.offset()))
}

fn review_comment_timestamp_label_from(
    now: time::OffsetDateTime,
    created_at: time::OffsetDateTime,
) -> String {
    let Ok(elapsed) = (now - created_at).try_into() else {
        return "now".to_string();
    };
    let elapsed: std::time::Duration = elapsed;
    let minutes = elapsed.as_secs() / 60;
    if minutes < 1 {
        "now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (60 * 24))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewMentionMode {
    File,
    Symbol,
}

impl ReviewMentionMode {
    fn keyword(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Symbol => "symbol",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::File => "Files & Directories",
            Self::Symbol => "Symbols",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::File => IconName::File,
            Self::Symbol => IconName::Code,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReviewMentionCompletion {
    source_range: Range<usize>,
    mode: Option<ReviewMentionMode>,
    argument: Option<String>,
}

impl ReviewMentionCompletion {
    fn try_parse(line: &str, offset_to_line: usize) -> Option<Self> {
        let mut last_mention_start = None;
        for (idx, _) in line.rmatch_indices('@') {
            if line[idx + 1..]
                .chars()
                .next()
                .is_some_and(|character| character.is_whitespace())
            {
                continue;
            }

            if idx > 0
                && line[..idx].chars().last().is_some_and(|character| {
                    !character.is_whitespace() && !matches!(character, '(' | '[' | '{')
                })
            {
                continue;
            }

            last_mention_start = Some(idx);
            break;
        }

        let last_mention_start = last_mention_start?;
        let rest_of_line = &line[last_mention_start + 1..];
        let mut mode = None;
        let mut argument = None;
        let mut parts = rest_of_line.split_whitespace();
        let mut end = last_mention_start + 1;

        if let Some(mode_text) = parts.next() {
            end += mode_text.len();
            match mode_text {
                "file" => mode = Some(ReviewMentionMode::File),
                "symbol" => mode = Some(ReviewMentionMode::Symbol),
                text => argument = Some(text.to_string()),
            }

            match rest_of_line[mode_text.len()..].find(|character: char| !character.is_whitespace())
            {
                Some(whitespace_count) => {
                    if let Some(argument_text) = parts.next() {
                        if mode.is_none() && !argument_text.is_empty() {
                            return None;
                        }

                        argument = Some(argument_text.to_string());
                        end += whitespace_count + argument_text.len();
                    }
                }
                None => {
                    end += rest_of_line.len() - mode_text.len();
                }
            }
        }

        Some(Self {
            source_range: last_mention_start + offset_to_line..end + offset_to_line,
            mode,
            argument,
        })
    }
}

enum ReviewMentionMatch {
    File(ReviewFileMatch),
    Symbol(Symbol),
    Mode(ReviewMentionMode),
}

struct ReviewFileMatch {
    mat: PathMatch,
    is_recent: bool,
}

struct ReviewMentionCompletionProvider {
    project: WeakEntity<Project>,
    editor: WeakEntity<Editor>,
}

impl ReviewMentionCompletionProvider {
    fn new(project: WeakEntity<Project>, editor: WeakEntity<Editor>) -> Self {
        Self { project, editor }
    }

    fn search_mentions(
        &self,
        mode: Option<ReviewMentionMode>,
        query: String,
        cancellation_flag: Arc<AtomicBool>,
        cx: &mut App,
    ) -> Task<Vec<ReviewMentionMatch>> {
        let Some(project) = self.project.upgrade() else {
            return Task::ready(Vec::new());
        };

        match mode {
            Some(ReviewMentionMode::File) => {
                let files_task = search_review_files(query, cancellation_flag, &project, cx);
                cx.background_spawn(async move {
                    files_task
                        .await
                        .into_iter()
                        .map(ReviewMentionMatch::File)
                        .collect()
                })
            }
            Some(ReviewMentionMode::Symbol) => {
                let symbols_task = search_review_symbols(query, cancellation_flag, &project, cx);
                cx.background_spawn(async move {
                    symbols_task
                        .await
                        .into_iter()
                        .map(ReviewMentionMatch::Symbol)
                        .collect()
                })
            }
            None if query.is_empty() => Task::ready(vec![
                ReviewMentionMatch::Mode(ReviewMentionMode::File),
                ReviewMentionMatch::Mode(ReviewMentionMode::Symbol),
            ]),
            None => {
                let files_task =
                    search_review_files(query.clone(), cancellation_flag.clone(), &project, cx);
                let symbols_task = search_review_symbols(query, cancellation_flag, &project, cx);
                cx.background_spawn(async move {
                    let mut matches = files_task
                        .await
                        .into_iter()
                        .map(ReviewMentionMatch::File)
                        .collect::<Vec<_>>();
                    matches.extend(
                        symbols_task
                            .await
                            .into_iter()
                            .map(ReviewMentionMatch::Symbol),
                    );
                    matches
                })
            }
        }
    }

    fn completion_for_mode(mode: ReviewMentionMode, source_range: Range<TextAnchor>) -> Completion {
        Completion {
            replace_range: source_range,
            new_text: format!("@{} ", mode.keyword()),
            label: CodeLabel::plain(mode.label().to_string(), None),
            documentation: None,
            source: project::CompletionSource::Custom,
            icon_path: Some(mode.icon().path().into()),
            match_start: None,
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: Some(Arc::new(|_, _, _| true)),
            group: None,
        }
    }

    fn completion_for_file(
        file_match: ReviewFileMatch,
        source_range: Range<TextAnchor>,
        project: &Entity<Project>,
        editor: WeakEntity<Editor>,
        label_max_chars: usize,
        cx: &mut App,
    ) -> Option<Completion> {
        let project_path = ProjectPath {
            worktree_id: WorktreeId::from_usize(file_match.mat.worktree_id),
            path: file_match.mat.path.clone(),
        };
        let project = project.read(cx);
        let path_style = project.path_style(cx);
        let (file_name, directory) = extract_review_file_name_and_directory(
            &project_path.path,
            &file_match.mat.path_prefix,
            path_style,
        );
        let label = build_review_code_label(
            &file_name,
            directory.as_ref().map(|directory| directory.as_ref()),
            None,
            label_max_chars,
            cx,
        );
        let abs_path = project.absolute_path(&project_path, cx)?;
        let mention_text = mention_link_for_path(&file_name, &abs_path, file_match.mat.is_dir)?;
        let content_len = mention_text.len();
        let icon_path = if file_match.is_recent {
            IconName::HistoryRerun.path().into()
        } else if file_match.mat.is_dir {
            file_icons::FileIcons::get_folder_icon(false, &abs_path, cx)
                .unwrap_or_else(|| IconName::Folder.path().into())
        } else {
            file_icons::FileIcons::get_icon(&abs_path, cx)
                .unwrap_or_else(|| IconName::File.path().into())
        };

        Some(Completion {
            replace_range: source_range.clone(),
            new_text: format!("{mention_text} "),
            label,
            documentation: None,
            source: project::CompletionSource::Custom,
            icon_path: Some(icon_path.clone()),
            match_start: None,
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: Some(confirm_review_mention_completion(
                source_range.start,
                content_len,
                file_name,
                icon_path.clone(),
                Some(abs_path.to_string_lossy().into_owned().into()),
                editor,
            )),
            group: None,
        })
    }

    fn completion_for_symbol(
        symbol: Symbol,
        source_range: Range<TextAnchor>,
        project: &Entity<Project>,
        editor: WeakEntity<Editor>,
        label_max_chars: usize,
        cx: &mut App,
    ) -> Option<Completion> {
        let project = project.read(cx);
        let (abs_path, file_name) = match &symbol.path {
            SymbolLocation::InProject(project_path) => (
                project.absolute_path(project_path, cx)?,
                project_path.path.file_name()?.to_string().into(),
            ),
            SymbolLocation::OutsideProject { abs_path, .. } => (
                PathBuf::from(abs_path.as_ref()),
                abs_path
                    .file_name()
                    .map(|file_name| file_name.to_string_lossy())?,
            ),
        };
        let label = build_review_code_label(
            &symbol.name,
            Some(&file_name),
            Some(symbol.range.start.0.row + 1),
            label_max_chars,
            cx,
        );
        let mention_text = mention_link_for_symbol(
            &symbol.name,
            &abs_path,
            symbol.range.start.0.row,
            symbol.range.end.0.row,
        )?;
        let content_len = mention_text.len();
        let tooltip = format!(
            "{}:{}-{}",
            abs_path.display(),
            symbol.range.start.0.row + 1,
            symbol.range.end.0.row + 1,
        );

        Some(Completion {
            replace_range: source_range.clone(),
            new_text: format!("{mention_text} "),
            label,
            documentation: None,
            source: project::CompletionSource::Custom,
            icon_path: Some(IconName::Code.path().into()),
            match_start: None,
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: Some(confirm_review_mention_completion(
                source_range.start,
                content_len,
                symbol.name.into(),
                IconName::Code.path().into(),
                Some(tooltip.into()),
                editor,
            )),
            group: None,
        })
    }
}

impl CompletionProvider for ReviewMentionCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: TextAnchor,
        _trigger: lsp::CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let Some(state) = buffer.update(cx, |buffer, _cx| {
            let position = buffer_position.to_point(buffer);
            let line_start = Point::new(position.row, 0);
            let offset_to_line = buffer.point_to_offset(line_start);
            let mut lines = buffer.text_for_range(line_start..position).lines();
            let line = lines.next()?;
            ReviewMentionCompletion::try_parse(line, offset_to_line)
        }) else {
            return Task::ready(Ok(Vec::new()));
        };

        let Some(project) = self.project.upgrade() else {
            return Task::ready(Ok(Vec::new()));
        };

        let snapshot = buffer.read(cx).snapshot();
        let source_range = snapshot.anchor_before(state.source_range.start)
            ..snapshot.anchor_after(state.source_range.end);
        let editor = self.editor.clone();
        let show_mode_groups = state.mode.is_none() && state.argument.is_none();
        let search_task = self.search_mentions(
            state.mode,
            state.argument.unwrap_or_default(),
            Arc::<AtomicBool>::default(),
            cx,
        );
        let label_max_chars = review_completion_label_max_chars(window, cx);

        cx.spawn(async move |_, cx| {
            let matches = search_task.await;
            let completions = cx.update(|cx| {
                matches
                    .into_iter()
                    .filter_map(|match_| {
                        let group = if show_mode_groups {
                            Some(CompletionGroup {
                                key: "review-context".into(),
                                label: None,
                            })
                        } else {
                            None
                        };
                        let mut completion = match match_ {
                            ReviewMentionMatch::File(file_match) => Self::completion_for_file(
                                file_match,
                                source_range.clone(),
                                &project,
                                editor.clone(),
                                label_max_chars,
                                cx,
                            ),
                            ReviewMentionMatch::Symbol(symbol) => Self::completion_for_symbol(
                                symbol,
                                source_range.clone(),
                                &project,
                                editor.clone(),
                                label_max_chars,
                                cx,
                            ),
                            ReviewMentionMatch::Mode(mode) => {
                                Some(Self::completion_for_mode(mode, source_range.clone()))
                            }
                        };
                        if let Some(completion) = &mut completion {
                            completion.group = group;
                        }
                        completion
                    })
                    .collect::<Vec<_>>()
            });

            Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions {
                    dynamic_width: true,
                },
                is_incomplete: true,
            }])
        })
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: TextAnchor,
        _text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        let buffer = buffer.read(cx);
        let position = position.to_point(buffer);
        let line_start = Point::new(position.row, 0);
        let offset_to_line = buffer.point_to_offset(line_start);
        let mut lines = buffer.text_for_range(line_start..position).lines();
        lines
            .next()
            .and_then(|line| ReviewMentionCompletion::try_parse(line, offset_to_line))
            .map(|completion| {
                let cursor_offset = offset_to_line + position.column as usize;
                completion.source_range.start <= cursor_offset
                    && completion.source_range.end >= cursor_offset
            })
            .unwrap_or(false)
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

fn search_review_files(
    query: String,
    cancellation_flag: Arc<AtomicBool>,
    project: &Entity<Project>,
    cx: &App,
) -> Task<Vec<ReviewFileMatch>> {
    if query.is_empty() {
        let project = project.read(cx);
        let worktrees = project.visible_worktrees(cx).collect::<Vec<_>>();
        let include_root_name = worktrees.len() > 1;
        let file_matches = worktrees.into_iter().flat_map(|worktree| {
            let worktree = worktree.read(cx);
            let path_prefix: Arc<RelPath> = if include_root_name {
                worktree.root_name().into()
            } else {
                RelPath::empty().into()
            };
            worktree
                .entries(false, 0)
                .map(move |entry| ReviewFileMatch {
                    mat: PathMatch {
                        score: 0.,
                        positions: Vec::new(),
                        worktree_id: worktree.id().to_usize(),
                        path: entry.path.clone(),
                        path_prefix: path_prefix.clone(),
                        distance_to_relative_ancestor: 0,
                        is_dir: entry.is_dir(),
                    },
                    is_recent: false,
                })
        });

        Task::ready(file_matches.collect())
    } else {
        let project = project.read(cx);
        let worktrees = project.visible_worktrees(cx).collect::<Vec<_>>();
        let include_root_name = worktrees.len() > 1;
        let candidate_sets = worktrees
            .into_iter()
            .map(|worktree| {
                let worktree = worktree.read(cx);
                PathMatchCandidateSet {
                    snapshot: worktree.snapshot(),
                    include_ignored: worktree.root_entry().is_some_and(|entry| entry.is_ignored),
                    include_root_name,
                    candidates: project::Candidates::Entries,
                }
            })
            .collect::<Vec<_>>();

        let executor = cx.background_executor().clone();
        cx.foreground_executor().spawn(async move {
            fuzzy::match_path_sets(
                candidate_sets.as_slice(),
                query.as_str(),
                &None,
                false,
                100,
                &cancellation_flag,
                executor,
            )
            .await
            .into_iter()
            .map(|mat| ReviewFileMatch {
                mat,
                is_recent: false,
            })
            .collect()
        })
    }
}

fn search_review_symbols(
    query: String,
    cancellation_flag: Arc<AtomicBool>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<Vec<Symbol>> {
    let symbols_task = project.update(cx, |project, cx| project.symbols(&query, cx));
    let project = project.clone();
    cx.spawn(async move |cx| {
        let Some(symbols) = symbols_task.await.log_err() else {
            return Vec::new();
        };
        let (visible_match_candidates, external_match_candidates): (Vec<_>, Vec<_>) = project
            .update(cx, |project, cx| {
                symbols
                    .iter()
                    .enumerate()
                    .map(|(id, symbol)| StringMatchCandidate::new(id, symbol.label.filter_text()))
                    .partition(|candidate| match &symbols[candidate.id].path {
                        SymbolLocation::InProject(project_path) => project
                            .entry_for_path(project_path, cx)
                            .is_some_and(|entry| !entry.is_ignored),
                        SymbolLocation::OutsideProject { .. } => false,
                    })
            });
        let query = query
            .rsplit_once("::")
            .map_or(&*query, |(_, suffix)| suffix)
            .to_owned();
        const MAX_MATCHES: usize = 100;
        let mut visible_matches = cx.foreground_executor().block_on(fuzzy::match_strings(
            &visible_match_candidates,
            &query,
            false,
            true,
            MAX_MATCHES,
            &cancellation_flag,
            cx.background_executor().clone(),
        ));
        let mut external_matches = cx.foreground_executor().block_on(fuzzy::match_strings(
            &external_match_candidates,
            &query,
            false,
            true,
            MAX_MATCHES - visible_matches.len().min(MAX_MATCHES),
            &cancellation_flag,
            cx.background_executor().clone(),
        ));
        let sort_key_for_match = |mat: &StringMatch| {
            let symbol = &symbols[mat.candidate_id];
            (Reverse(OrderedFloat(mat.score)), symbol.label.filter_text())
        };

        visible_matches.sort_unstable_by_key(sort_key_for_match);
        external_matches.sort_unstable_by_key(sort_key_for_match);
        let mut matches = visible_matches;
        matches.append(&mut external_matches);

        matches
            .into_iter()
            .map(|mat| symbols[mat.candidate_id].clone())
            .collect()
    })
}

fn confirm_review_mention_completion(
    start: TextAnchor,
    content_len: usize,
    label: SharedString,
    icon_path: SharedString,
    tooltip: Option<SharedString>,
    editor: WeakEntity<Editor>,
) -> Arc<dyn Fn(CompletionIntent, &mut Window, &mut App) -> bool + Send + Sync> {
    Arc::new(move |_, window, cx| {
        let label = label.clone();
        let icon_path = icon_path.clone();
        let tooltip = tooltip.clone();
        let editor = editor.clone();
        window.defer(cx, move |window, cx| {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| {
                    editor.insert_review_mention_crease(
                        start,
                        content_len,
                        label,
                        icon_path,
                        tooltip,
                        window,
                        cx,
                    );
                });
            }
        });
        false
    })
}

fn render_review_mention_fold_button(
    icon_path: SharedString,
    label: SharedString,
    url: Option<SharedString>,
    tooltip: Option<SharedString>,
    selection_editor: WeakEntity<Editor>,
    open_editor: WeakEntity<Editor>,
) -> Arc<dyn Send + Sync + Fn(FoldId, Range<Anchor>, &mut App) -> AnyElement> {
    Arc::new(move |_fold_id, fold_range, cx| {
        let is_selected = selection_editor
            .update(cx, |editor, cx| editor.is_range_selected(&fold_range, cx))
            .unwrap_or_default();
        let element = h_flex()
            .id(format!("diff-review-input-mention-{label}"))
            .gap_1()
            .items_center()
            .px_1p5()
            .h_5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(if is_selected {
                cx.theme().colors().element_selected
            } else {
                cx.theme().colors().element_background
            })
            .child(
                Icon::from_path(icon_path.clone())
                    .size(IconSize::XSmall)
                    .color(ui::Color::Muted),
            )
            .child(Label::new(label.clone()).size(LabelSize::Small));
        element
            .when_some(tooltip.clone(), |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            })
            .on_click({
                let open_editor = open_editor.clone();
                let url = url.clone();
                move |_, window, cx| {
                    if let (Some(url), Some(editor)) = (url.as_ref(), open_editor.upgrade()) {
                        editor.update(cx, |editor, cx| {
                            editor.open_review_comment_mention(url, window, cx);
                        });
                    }
                }
            })
            .into_any_element()
    })
}

fn parse_review_comment_body(comment: &str) -> Vec<Vec<ReviewCommentBodySegment>> {
    comment
        .split('\n')
        .map(|line| parse_review_comment_line(line))
        .collect()
}

fn parse_review_comment_line(line: &str) -> Vec<ReviewCommentBodySegment> {
    let mut segments = Vec::new();
    let mut remaining = line;

    while let Some(open_index) = remaining.find('[') {
        if open_index > 0 {
            segments.push(ReviewCommentBodySegment::Text(
                remaining[..open_index].to_string(),
            ));
        }

        let link_start = &remaining[open_index..];
        let Some(label_end) = find_review_comment_markdown_delimiter(link_start, '[', ']') else {
            segments.push(ReviewCommentBodySegment::Text(link_start.to_string()));
            return segments;
        };
        if link_start.get(label_end + 1..label_end + 2) != Some("(") {
            segments.push(ReviewCommentBodySegment::Text("[".to_string()));
            remaining = &link_start[1..];
            continue;
        }

        let url_start = label_end + 2;
        let Some(url_end_relative) =
            find_review_comment_markdown_delimiter(&link_start[label_end + 1..], '(', ')')
        else {
            segments.push(ReviewCommentBodySegment::Text(link_start.to_string()));
            return segments;
        };
        let url_end = label_end + 1 + url_end_relative;

        let raw_label = &link_start[1..label_end];
        let (label, show_at_prefix) = raw_label
            .strip_prefix('@')
            .map_or((raw_label, false), |label| (label, true));
        let url = &link_start[url_start..url_end];
        if let Some(mention) = build_review_comment_mention(label, url) {
            segments.push(ReviewCommentBodySegment::Mention(ReviewCommentMention {
                show_at_prefix,
                ..mention
            }));
        } else {
            segments.push(ReviewCommentBodySegment::Text(
                link_start[..url_end + 1].to_string(),
            ));
        }

        remaining = &link_start[url_end + 1..];
    }

    if !remaining.is_empty() || segments.is_empty() {
        segments.push(ReviewCommentBodySegment::Text(remaining.to_string()));
    }

    segments
}

fn find_review_comment_markdown_delimiter(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0;
    for (index, character) in text.char_indices() {
        if character == open {
            depth += 1;
        } else if character == close {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn build_review_comment_mention(label: &str, url: &str) -> Option<ReviewCommentMention> {
    if label.is_empty() {
        return None;
    }

    let parsed_url = url::Url::parse(url).ok()?;
    let icon = match parsed_url.scheme() {
        "file" if parsed_url.query_pairs().any(|(key, _)| key == "symbol") => IconName::Code,
        "file" if parsed_url.path().ends_with('/') => IconName::Folder,
        "file" => IconName::File,
        "zed" if parsed_url.path().contains("thread") => IconName::Thread,
        "zed" if parsed_url.path().contains("git-diff") => IconName::GitBranch,
        "zed" => IconName::Sparkle,
        "http" | "https" => IconName::ToolWeb,
        _ => return None,
    };

    let tooltip = if parsed_url.scheme() == "file" {
        parsed_url
            .to_file_path_ext(PathStyle::local())
            .ok()
            .map(|path| path.to_string_lossy().into_owned().into())
            .or_else(|| Some(url.to_string().into()))
    } else {
        Some(url.to_string().into())
    };

    Some(ReviewCommentMention {
        label: label.to_string(),
        url: url.to_string(),
        tooltip,
        icon,
        show_at_prefix: true,
    })
}

fn parse_review_comment_mention_point(url: &url::Url) -> Option<Point> {
    let fragment = url.fragment()?;
    let range = fragment.strip_prefix('L').unwrap_or(fragment);
    let start = range
        .split_once(':')
        .or_else(|| range.split_once('-'))
        .map_or(range, |(start, _)| start.strip_prefix('L').unwrap_or(start));
    let row = start.parse::<u32>().ok()?.checked_sub(1)?;
    let column = url
        .query_pairs()
        .find_map(|(key, value)| {
            (key == "column")
                .then(|| value.parse::<u32>().ok()?.checked_sub(1))
                .flatten()
        })
        .unwrap_or(0);
    Some(Point::new(row, column))
}

fn extract_review_file_name_and_directory(
    path: &RelPath,
    path_prefix: &RelPath,
    path_style: PathStyle,
) -> (SharedString, Option<SharedString>) {
    if path.is_empty() && !path_prefix.is_empty() {
        return (path_prefix.display(path_style).to_string().into(), None);
    }

    let full_path = path_prefix.join(path);
    let file_name = full_path.file_name().unwrap_or_default();
    let display_path = full_path.display(path_style);
    let (directory, file_name) = display_path.split_at(display_path.len() - file_name.len());
    (
        file_name.to_string().into(),
        Some(SharedString::new(directory)).filter(|directory| !directory.is_empty()),
    )
}

fn build_review_code_label(
    file: &str,
    directory: Option<&str>,
    line_number: Option<u32>,
    label_max_chars: usize,
    cx: &App,
) -> CodeLabel {
    let variable_highlight_id = cx
        .theme()
        .syntax()
        .highlight_id("variable")
        .map(HighlightId::new);
    let mut label = CodeLabelBuilder::default();

    label.push_str(file, None);
    label.push_str(" ", None);

    if let Some(directory) = directory {
        let file_name_chars = file.chars().count();
        let directory_max_chars = label_max_chars
            .saturating_sub(file_name_chars)
            .saturating_sub(1);
        let truncated_directory = truncate_and_remove_front(directory, directory_max_chars.max(5));
        label.push_str(&truncated_directory, variable_highlight_id);
    }
    if let Some(line_number) = line_number {
        label.push_str(&format!(" L{line_number}"), variable_highlight_id);
    }
    label.build()
}

fn review_completion_label_max_chars(window: &Window, cx: &App) -> usize {
    let style = window.text_style();
    let font_id = window.text_system().resolve_font(&style.font());
    let font_size = TextSize::Small.rems(cx).to_pixels(window.rem_size());
    let em_width = cx
        .text_system()
        .em_width(font_id, font_size)
        .unwrap_or(px(10.0));
    (f32::from(px(420.0)) / f32::from(em_width)) as usize
}

fn mention_link_for_path(label: &str, abs_path: &Path, is_directory: bool) -> Option<String> {
    let mut url = url::Url::parse("file:///").ok()?;
    let mut path = abs_path.to_string_lossy().into_owned();
    if is_directory && !path.ends_with('/') && !path.ends_with('\\') {
        path.push('/');
    }
    url.set_path(&path);
    Some(format!("[@{label}]({url})"))
}

fn mention_link_for_symbol(
    label: &str,
    abs_path: &Path,
    start_row: u32,
    end_row: u32,
) -> Option<String> {
    let mut url = url::Url::parse("file:///").ok()?;
    url.set_path(&abs_path.to_string_lossy());
    url.query_pairs_mut().append_pair("symbol", label);
    url.set_fragment(Some(&format!("L{}:{}", start_row + 1, end_row + 1)));
    Some(format!("[@{label}]({url})"))
}

/// Represents an active diff review overlay that appears when clicking the "Add Review" button.
pub(super) struct DiffReviewOverlay {
    pub(super) anchor_range: Range<Anchor>,
    pub(super) block_id: CustomBlockId,
    pub(super) prompt_editor: Entity<Editor>,
    pub(super) hunk_key: DiffHunkKey,
    pub(super) comments_expanded: bool,
    pub(super) prompt_visible: bool,
    pub(super) confirming_discard: bool,
    pub(super) inline_edit_editors: HashMap<usize, Entity<Editor>>,
    pub(super) inline_edit_subscriptions: HashMap<usize, Subscription>,
    pub(super) inline_reply_edit_editors: HashMap<usize, Entity<Editor>>,
    pub(super) inline_reply_edit_subscriptions: HashMap<usize, Subscription>,
    pub(super) reply_editors: HashMap<usize, Entity<Editor>>,
    pub(super) reply_subscriptions: HashMap<usize, Subscription>,
    pub(super) body_editors: HashMap<usize, Entity<Editor>>,
    pub(super) reply_body_editors: HashMap<usize, Entity<Editor>>,
    pub(super) user_avatar_uri: Option<SharedUri>,
    _subscription: Subscription,
}

impl DiffReviewDragState {
    pub(super) fn row_range(
        &self,
        snapshot: &DisplaySnapshot,
    ) -> std::ops::RangeInclusive<DisplayRow> {
        let start = self.start_anchor.to_display_point(snapshot).row();
        let current = self.current_anchor.to_display_point(snapshot).row();

        (start..=current).sorted()
    }
}

impl StoredReviewComment {
    fn new(
        id: usize,
        comment: String,
        anchor_range: Range<Anchor>,
        location: ReviewCommentLocation,
    ) -> Self {
        Self::new_with_author(
            id,
            DEFAULT_REVIEW_COMMENT_AUTHOR.to_string(),
            comment,
            anchor_range,
            Vec::new(),
            location,
        )
    }

    fn new_with_author(
        id: usize,
        author: String,
        comment: String,
        anchor_range: Range<Anchor>,
        replies: Vec<StoredReviewReply>,
        location: ReviewCommentLocation,
    ) -> Self {
        Self {
            id,
            author,
            created_at: current_review_comment_timestamp(),
            deleted_on: None,
            resolved_on: None,
            comment,
            range: anchor_range,
            replies,
            is_editing: false,
            outdated: false,
            outdated_reason: None,
            review_round: None,
            show_resolved: false,
            location,
        }
    }
}

impl Editor {
    fn review_comment_input_editor(window: &mut Window, cx: &mut Context<Self>) -> Editor {
        let mut editor = Editor::auto_height(1, 6, window, cx);
        Self::configure_review_comment_editor(&mut editor, cx);
        editor.register_addon(ReviewCommentEditorAddon);
        editor
    }

    fn configure_review_comment_editor(editor: &mut Editor, cx: &mut Context<Self>) {
        editor.set_show_completions_on_input(Some(true));
        editor.set_use_modal_editing(true);
        editor.set_show_indent_guides(false, cx);
        editor.set_context_menu_options(ContextMenuOptions {
            min_entries_visible: 12,
            max_entries_visible: 12,
            placement: None,
        });
    }

    fn attach_review_comment_completion_provider(
        project: Option<WeakEntity<Project>>,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = project else {
            return;
        };
        let provider = Rc::new(ReviewMentionCompletionProvider::new(
            project,
            editor.downgrade(),
        ));
        editor.update(cx, |editor, _cx| {
            editor.set_completion_provider(Some(provider));
        });
    }

    fn install_review_comment_insert_mode_handler(
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus_handle = editor.focus_handle(cx);
        let editor = editor.clone();
        cx.on_focus_in(&focus_handle, window, move |_, window, cx| {
            Self::focus_review_comment_editor(&editor, window, cx);
        })
        .detach();
    }

    fn review_comment_prompt_focus_out_subscription(
        hunk_key: DiffHunkKey,
        prompt_editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let focus_handle = prompt_editor.focus_handle(cx);
        cx.on_focus_out(&focus_handle, window, move |editor, _, window, cx| {
            editor.dismiss_empty_diff_review_prompt(&hunk_key, window, cx);
        })
    }

    fn review_comment_enter_subscription(
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
        mut confirm: impl FnMut(&mut Window, &mut App) + 'static,
    ) -> Subscription {
        let focus_handle = editor.focus_handle(cx);
        let editor = editor.downgrade();
        cx.intercept_keystrokes(move |event, window, cx| {
            if event.keystroke.key == "enter"
                && !event.keystroke.modifiers.modified()
                && focus_handle.contains_focused(window, cx)
            {
                let completion_open = editor
                    .read_with(cx, |editor, _| editor.context_menu_visible())
                    .unwrap_or(false);
                if completion_open {
                    return;
                }
                confirm(window, cx);
                cx.stop_propagation();
            }
        })
    }

    fn insert_review_mention_crease(
        &mut self,
        start: TextAnchor,
        content_len: usize,
        label: SharedString,
        icon_path: SharedString,
        tooltip: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_review_mention_crease_with_opener(
            start,
            content_len,
            label,
            icon_path,
            None,
            tooltip,
            cx.weak_entity(),
            window,
            cx,
        );
    }

    fn insert_review_mention_crease_with_opener(
        &mut self,
        start: TextAnchor,
        content_len: usize,
        label: SharedString,
        icon_path: SharedString,
        url: Option<SharedString>,
        tooltip: Option<SharedString>,
        open_editor: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer().read(cx).snapshot(cx);
        let Some(start) = snapshot.anchor_in_excerpt(start) else {
            return;
        };
        let start = start.bias_right(&snapshot);
        let end = snapshot.anchor_before(start.to_offset(&snapshot) + content_len);
        let range = start..end;
        let placeholder = FoldPlaceholder {
            render: render_review_mention_fold_button(
                icon_path.clone(),
                label.clone(),
                url,
                tooltip,
                cx.weak_entity(),
                open_editor,
            ),
            merge_adjacent: false,
            ..Default::default()
        };
        let crease = Crease::Inline {
            range,
            placeholder,
            render_toggle: None,
            render_trailer: None,
            metadata: Some(CreaseMetadata { icon_path, label }),
        };
        self.insert_creases(vec![crease.clone()], cx);
        self.fold_creases(vec![crease], false, window, cx);
    }

    fn focus_review_comment_editor(
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus_handle = editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        if let Ok(action) = cx.build_action("vim::SwitchToInsertMode", None) {
            focus_handle.dispatch_action(action.as_ref(), window, cx);
        }
        window.defer(cx, move |window, cx| {
            window.focus(&focus_handle, cx);
            if let Ok(action) = cx.build_action("vim::SwitchToInsertMode", None) {
                focus_handle.dispatch_action(action.as_ref(), window, cx);
            }
        });
    }

    pub(super) fn copy_review_comment_reference(
        &mut self,
        id: usize,
        is_reply: bool,
        cx: &mut Context<Self>,
    ) {
        let reference = if is_reply {
            format!("zed-review:reply:{id}")
        } else {
            format!("zed-review:comment:{id}")
        };
        cx.write_to_clipboard(ClipboardItem::new_string(reference));
    }

    pub(super) fn confirm_delete_review_reply(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = window.prompt(
            PromptLevel::Warning,
            "Delete review reply?",
            Some("This removes the reply from the current review."),
            &["Delete", "Cancel"],
            cx,
        );
        let editor = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            if prompt.await == Ok(0) {
                editor
                    .update_in(cx, |editor, window, cx| {
                        editor.delete_review_reply(id, window, cx);
                    })
                    .ok();
            }
        })
        .detach();
    }

    pub fn diff_hunks_in_ranges<'a>(
        &'a self,
        ranges: &'a [Range<Anchor>],
        buffer: &'a MultiBufferSnapshot,
    ) -> impl 'a + Iterator<Item = MultiBufferDiffHunk> {
        ranges.iter().flat_map(move |range| {
            let end_excerpt = buffer.excerpt_containing(range.end..range.end);
            let range = range.to_point(buffer);
            let mut peek_end = range.end;
            if range.end.row < buffer.max_row().0 {
                peek_end = Point::new(range.end.row + 1, 0);
            }
            buffer
                .diff_hunks_in_range(range.start..peek_end)
                .filter(move |hunk| {
                    if let Some((_, excerpt_range)) = &end_excerpt
                        && let Some(end_anchor) =
                            buffer.anchor_in_excerpt(excerpt_range.context.end)
                        && let Some(hunk_end_anchor) =
                            buffer.anchor_in_excerpt(hunk.excerpt_range.context.end)
                        && hunk_end_anchor.cmp(&end_anchor, buffer).is_gt()
                    {
                        false
                    } else {
                        true
                    }
                })
        })
    }

    fn resolve_diff_hunks(
        &self,
        hunks: Vec<MultiBufferDiffHunk>,
        cx: &App,
    ) -> Vec<ResolvedDiffHunks> {
        let multibuffer = self.buffer().read(cx);
        let chunk_by = hunks.into_iter().chunk_by(|hunk| hunk.buffer_id);
        let mut resolved = Vec::new();

        for (source_buffer_id, hunks) in &chunk_by {
            let Some(diff) = multibuffer.diff_for(source_buffer_id) else {
                continue;
            };
            let diff_snapshot = diff.read(cx).snapshot(cx);
            let main_buffer_id = diff_snapshot.buffer_id();
            let buffer = multibuffer.buffer(main_buffer_id).or_else(|| {
                self.project
                    .as_ref()
                    .and_then(|project| project.read(cx).buffer_for_id(main_buffer_id, cx))
            });
            let mut resolved_hunks = Vec::new();

            for hunk in hunks {
                if hunk.buffer_id == main_buffer_id {
                    resolved_hunks.push(ResolvedDiffHunk {
                        buffer_range: hunk.buffer_range,
                        diff_base_byte_range: hunk.diff_base_byte_range.start.0
                            ..hunk.diff_base_byte_range.end.0,
                        status: hunk.status,
                    });
                } else {
                    let diff_base_byte_range =
                        hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0;
                    let Some(hunk) = diff_snapshot
                        .hunks_intersecting_base_text_range(
                            diff_base_byte_range.clone(),
                            diff_snapshot.buffer_snapshot(),
                        )
                        .find(|hunk| hunk.diff_base_byte_range == diff_base_byte_range)
                    else {
                        continue;
                    };
                    let kind = if hunk.buffer_range.start == hunk.buffer_range.end {
                        DiffHunkStatusKind::Deleted
                    } else if hunk.diff_base_byte_range.is_empty() {
                        DiffHunkStatusKind::Added
                    } else {
                        DiffHunkStatusKind::Modified
                    };
                    resolved_hunks.push(ResolvedDiffHunk {
                        buffer_range: hunk.buffer_range,
                        diff_base_byte_range: hunk.diff_base_byte_range,
                        status: DiffHunkStatus {
                            kind,
                            secondary: hunk.secondary_status,
                        },
                    });
                }
            }

            if !resolved_hunks.is_empty() {
                resolved.push(ResolvedDiffHunks {
                    diff,
                    buffer_id: main_buffer_id,
                    buffer,
                    hunks: resolved_hunks,
                });
            }
        }

        resolved
    }

    pub fn diff_hunk_delegate(&self) -> Arc<dyn DiffHunkDelegate> {
        self.diff_hunk_delegate
            .clone()
            .unwrap_or_else(|| Arc::new(UncommittedDiffHunkDelegate))
    }

    pub fn set_diff_hunk_delegate(
        &mut self,
        delegate: Option<Arc<dyn DiffHunkDelegate>>,
        cx: &mut Context<Self>,
    ) {
        let had_delegate = self.diff_hunk_delegate.is_some();
        let has_delegate = delegate.is_some();
        self.diff_hunk_delegate = delegate;

        if !had_delegate && has_delegate {
            self.load_diff_task.take();
        } else if had_delegate && !has_delegate {
            self.buffer.update(cx, |buffer, cx| {
                buffer.set_all_diff_hunks_collapsed(cx);
            });

            if let Some(project) = self.project.clone() {
                self.load_diff_task = Some(
                    self.update_uncommitted_diff_for_buffer(
                        &project,
                        self.buffer.read(cx).all_buffers(),
                        cx,
                    )
                    .shared(),
                );
            }
        }

        cx.notify();
    }

    pub fn git_blame_inline_enabled(&self) -> bool {
        self.git_blame_inline_enabled
    }

    pub fn blame(&self) -> Option<&Entity<GitBlame>> {
        self.blame.as_ref()
    }

    pub fn active_git_blame_entry(&self, cx: &mut App) -> Option<BlameEntry> {
        if !self.show_git_blame_inline
            || self.newest_selection_head_on_empty_line(cx)
            || !self.has_blame_entries(cx)
        {
            return None;
        }

        let blame = self.blame.as_ref()?;
        let snapshot = self.display_snapshot(cx);
        let cursor = self.selections.newest::<Point>(&snapshot).head();
        let (buffer, point) = snapshot.buffer_snapshot().point_to_buffer_point(cursor)?;

        blame
            .update(cx, |blame, cx| {
                blame
                    .blame_for_rows(
                        &[RowInfo {
                            buffer_id: Some(buffer.remote_id()),
                            buffer_row: Some(point.row),
                            ..Default::default()
                        }],
                        cx,
                    )
                    .next()
            })
            .flatten()
            .map(|(_, entry)| entry)
    }

    pub fn show_git_blame_gutter(&self) -> bool {
        self.show_git_blame_gutter
    }

    pub fn expand_selected_diff_hunks(&mut self, cx: &mut Context<Self>) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        self.buffer
            .update(cx, |buffer, cx| buffer.expand_diff_hunks(ranges, cx))
    }

    pub fn toggle_git_blame(
        &mut self,
        _: &::git::Blame,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_git_blame_gutter = !self.show_git_blame_gutter;

        if self.show_git_blame_gutter && !self.has_blame_entries(cx) {
            self.start_git_blame(true, window, cx);
        }

        cx.notify();
    }

    pub fn toggle_git_blame_inline(
        &mut self,
        _: &ToggleGitBlameInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_git_blame_inline_internal(true, window, cx);
        cx.notify();
    }

    /// Hides the inline blame popover element, in case it's already visible, or
    /// interrupts the task meant to show it, in case the task is running.
    ///
    /// When `ignore_timeout` is set to `true`, the popover is hidden
    /// immediately, otherwise it'll be hidden after a short delay.
    ///
    /// Returns `true` if the popover was visible and was hidden, `false`
    /// otherwise.
    pub fn hide_blame_popover(&mut self, ignore_timeout: bool, cx: &mut Context<Self>) -> bool {
        self.inline_blame_popover_show_task.take();

        if let Some(state) = &mut self.inline_blame_popover {
            if ignore_timeout {
                self.inline_blame_popover.take();
                cx.notify();
            } else {
                state.hide_task = Some(cx.spawn(async move |editor, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;

                    editor
                        .update(cx, |editor, cx| {
                            editor.inline_blame_popover.take();
                            cx.notify();
                        })
                        .ok();
                }));
            }

            true
        } else {
            false
        }
    }

    pub fn git_restore(&mut self, _: &Restore, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only(cx) {
            return;
        }
        let selections = self
            .selections
            .all(&self.display_snapshot(cx))
            .into_iter()
            .map(|s| s.range())
            .collect();
        self.restore_hunks_in_ranges(selections, window, cx);
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        if let Some(status) = self
            .addons
            .iter()
            .find_map(|(_, addon)| addon.override_status_for_buffer_id(buffer_id, cx))
        {
            return Some(status);
        }
        self.project
            .as_ref()?
            .read(cx)
            .status_for_buffer_id(buffer_id, cx)
    }

    pub fn go_to_hunk_before_or_after_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        direction: Direction,
        wrap_around: bool,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let row = if direction == Direction::Next {
            self.hunk_after_position(snapshot, position, wrap_around)
                .map(|hunk| hunk.row_range.start)
        } else {
            self.hunk_before_position(snapshot, position, wrap_around)
        };

        if let Some(row) = row {
            let destination = Point::new(row.0, 0);
            let autoscroll = Autoscroll::center();

            self.unfold_ranges(&[destination..destination], false, false, cx);
            self.change_selections(SelectionEffects::scroll(autoscroll), window, cx, |s| {
                s.select_ranges([destination..destination]);
            });
        }
    }

    pub fn set_expand_all_diff_hunks(&mut self, cx: &mut App) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_all_diff_hunks_expanded(cx);
        });
    }

    pub fn expand_all_diff_hunks(
        &mut self,
        _: &ExpandAllDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.expand_diff_hunks(vec![Anchor::Min..Anchor::Max], cx)
        });
    }

    pub fn show_diff_review_overlay(
        &mut self,
        display_range: Range<DisplayRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_diff_review_overlay_with_prompt(display_range, true, true, window, cx);
    }

    fn show_diff_review_overlay_with_prompt(
        &mut self,
        display_range: Range<DisplayRow>,
        prompt_visible: bool,
        comments_expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Range { start, end } = display_range.sorted();

        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let editor_snapshot = self.snapshot(window, cx);

        // Convert display rows to multibuffer points
        let start_point = editor_snapshot
            .display_snapshot
            .display_point_to_point(start.as_display_point(), Bias::Left);
        let end_point = editor_snapshot
            .display_snapshot
            .display_point_to_point(end.as_display_point(), Bias::Left);
        let end_multi_buffer_row = MultiBufferRow(end_point.row);

        // Create anchor range for the selected lines (start of first line to end of last line)
        let line_end = Point::new(
            end_point.row,
            buffer_snapshot.line_len(end_multi_buffer_row),
        );
        let anchor_range =
            buffer_snapshot.anchor_after(start_point)..buffer_snapshot.anchor_before(line_end);

        // Compute the hunk key for this display row
        let file_path = buffer_snapshot
            .diff_hunks_in_range(start_point..line_end)
            .next()
            .and_then(|hunk| {
                buffer_snapshot
                    .buffers_with_paths()
                    .find(|(snapshot, _)| snapshot.remote_id() == hunk.buffer_id)
                    .map(|(_, path_key)| path_key.path.clone())
            })
            .or_else(|| {
                buffer_snapshot
                    .range_to_buffer_ranges(start_point..line_end)
                    .into_iter()
                    .next()
                    .and_then(|(buffer_snapshot, _, _)| {
                        buffer_snapshot
                            .file()
                            .map(|file| file.path().clone())
                            .or_else(|| {
                                let buffer_id = buffer_snapshot.remote_id();
                                self.buffer
                                    .read(cx)
                                    .snapshot(cx)
                                    .buffers_with_paths()
                                    .find(|(snapshot, _)| snapshot.remote_id() == buffer_id)
                                    .map(|(_, path_key)| path_key.path.clone())
                            })
                    })
            })
            .or_else(|| {
                buffer_snapshot
                    .file_at(start_point)
                    .map(|file: &Arc<dyn language::File>| file.path().clone())
            })
            .unwrap_or_else(|| Arc::from(util::rel_path::RelPath::empty()));
        let hunk_start_anchor = buffer_snapshot.anchor_before(start_point);
        let new_hunk_key = DiffHunkKey {
            file_path,
            hunk_start_anchor,
        };

        if let Some(existing_overlay) = self.diff_review_overlays.iter_mut().find(|overlay| {
            Self::hunk_keys_match(&overlay.hunk_key, &new_hunk_key, &buffer_snapshot)
        }) {
            if prompt_visible {
                existing_overlay.prompt_visible = true;
                Self::focus_review_comment_editor(&existing_overlay.prompt_editor, window, cx);
            }
            self.refresh_diff_review_overlay_height(&new_hunk_key, window, cx);
            return;
        }

        self.dismiss_overlays_without_comments(cx);

        // Get the current user's avatar URI from the project's user_store
        let user_avatar_uri = self.project.as_ref().and_then(|project| {
            let user_store = project.read(cx).user_store();
            user_store
                .read(cx)
                .current_user()
                .map(|user| user.avatar_uri.clone())
        });

        // Create anchor at the end of the last row so the block appears immediately below it
        // Use multibuffer coordinates for anchor creation
        let line_len = buffer_snapshot.line_len(end_multi_buffer_row);
        let anchor = buffer_snapshot.anchor_after(Point::new(end_multi_buffer_row.0, line_len));

        // Use the hunk key we already computed
        let hunk_key = new_hunk_key;

        let completion_project = self.project.as_ref().map(Entity::downgrade);
        let prompt_editor = cx.new(|cx| {
            let mut editor = Self::review_comment_input_editor(window, cx);
            editor.set_placeholder_text("Add a review comment...", window, cx);
            editor
        });
        Self::attach_review_comment_completion_provider(completion_project, &prompt_editor, cx);
        Self::install_review_comment_insert_mode_handler(&prompt_editor, window, cx);

        let parent_editor = cx.entity().downgrade();
        let enter_subscription = Self::review_comment_enter_subscription(&prompt_editor, cx, {
            let parent_editor = parent_editor.clone();
            move |window, cx| {
                if let Some(editor) = parent_editor.upgrade() {
                    editor.update(cx, |editor, cx| {
                        editor.submit_diff_review_comment(window, cx);
                    });
                }
            }
        });
        let focus_out_subscription = Self::review_comment_prompt_focus_out_subscription(
            hunk_key.clone(),
            &prompt_editor,
            window,
            cx,
        );
        let subscription = prompt_editor.update(cx, |prompt_editor, _cx| {
            let confirm_subscription = prompt_editor.register_action({
                let parent_editor = parent_editor.clone();
                move |_: &menu::Confirm, window, cx| {
                    if let Some(editor) = parent_editor.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.submit_diff_review_comment(window, cx);
                        });
                    }
                }
            });
            let cancel_subscription = prompt_editor.register_action({
                let parent_editor = parent_editor.clone();
                let hunk_key = hunk_key.clone();
                move |_: &crate::actions::Cancel, window, cx| {
                    if let Some(editor) = parent_editor.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.cancel_diff_review_prompt(&hunk_key, window, cx);
                        });
                    }
                }
            });
            Subscription::join(
                Subscription::join(
                    Subscription::join(confirm_subscription, cancel_subscription),
                    enter_subscription,
                ),
                focus_out_subscription,
            )
        });

        let initial_height = self.calculate_overlay_height(
            &hunk_key,
            comments_expanded,
            prompt_visible,
            &buffer_snapshot,
            cx,
        );

        // Create the overlay block
        let prompt_editor_for_render = prompt_editor.clone();
        let hunk_key_for_render = hunk_key.clone();
        let editor_handle = cx.entity().downgrade();
        let block = BlockProperties {
            style: BlockStyle::Sticky,
            placement: BlockPlacement::Below(anchor),
            height: Some(initial_height),
            render: Arc::new(move |cx| {
                Self::render_diff_review_overlay(
                    &prompt_editor_for_render,
                    &hunk_key_for_render,
                    &editor_handle,
                    cx,
                )
            }),
            priority: 0,
        };

        let block_ids = self.insert_blocks([block], None, cx);
        let Some(block_id) = block_ids.into_iter().next() else {
            log::error!("Failed to insert diff review overlay block");
            return;
        };

        self.diff_review_overlays.push(DiffReviewOverlay {
            anchor_range,
            block_id,
            prompt_editor: prompt_editor.clone(),
            hunk_key: hunk_key.clone(),
            comments_expanded,
            prompt_visible,
            confirming_discard: false,
            inline_edit_editors: HashMap::default(),
            inline_edit_subscriptions: HashMap::default(),
            inline_reply_edit_editors: HashMap::default(),
            inline_reply_edit_subscriptions: HashMap::default(),
            reply_editors: HashMap::default(),
            reply_subscriptions: HashMap::default(),
            body_editors: HashMap::default(),
            reply_body_editors: HashMap::default(),
            user_avatar_uri,
            _subscription: subscription,
        });

        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);

        if prompt_visible {
            Self::focus_review_comment_editor(&prompt_editor, window, cx);
        }

        cx.notify();
    }

    /// Stores the diff review comment locally.
    /// Comments are stored per-hunk and can later be batch-submitted to the Agent panel.
    pub fn submit_diff_review_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hunk_key = self
            .diff_review_overlays
            .iter()
            .find(|overlay| overlay.prompt_editor.focus_handle(cx).is_focused(window))
            .map(|overlay| overlay.hunk_key.clone());

        if let Some(hunk_key) = hunk_key {
            self.submit_diff_review_comment_for_hunk(&hunk_key, window, cx);
        }
    }

    fn submit_diff_review_comment_for_hunk(
        &mut self,
        hunk_key: &DiffHunkKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let overlay_index = self
            .diff_review_overlays
            .iter()
            .position(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot));
        let Some(overlay_index) = overlay_index else {
            return;
        };

        let comment_text = self.diff_review_overlays[overlay_index]
            .prompt_editor
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if comment_text.is_empty() {
            return;
        }

        let anchor_range = self.diff_review_overlays[overlay_index]
            .anchor_range
            .clone();
        let hunk_key = self.diff_review_overlays[overlay_index].hunk_key.clone();

        self.add_review_comment(hunk_key.clone(), comment_text, anchor_range, cx);

        if let Some(overlay) = self.diff_review_overlays.get(overlay_index) {
            overlay.prompt_editor.update(cx, |editor, cx| {
                editor.clear(window, cx);
            });
        }
        if let Some(overlay) = self.diff_review_overlays.get_mut(overlay_index) {
            overlay.prompt_visible = false;
        }

        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);

        cx.notify();
    }

    /// Returns the prompt editor for the diff review overlay, if one is active.
    /// This is primarily used for testing.
    pub fn diff_review_prompt_editor(&self) -> Option<&Entity<Editor>> {
        self.diff_review_overlays
            .first()
            .map(|overlay| &overlay.prompt_editor)
    }

    pub fn has_active_diff_review_input(&self) -> bool {
        self.diff_review_overlays.iter().any(|overlay| {
            overlay.prompt_visible
                || !overlay.inline_edit_editors.is_empty()
                || !overlay.inline_reply_edit_editors.is_empty()
                || !overlay.reply_editors.is_empty()
        })
    }

    /// Sets whether the comments section is expanded in the diff review overlay.
    /// This is primarily used for testing.
    pub fn set_diff_review_comments_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        for overlay in &mut self.diff_review_overlays {
            overlay.comments_expanded = expanded;
        }
        cx.notify();
    }

    pub(super) fn total_review_comment_count(&self) -> usize {
        self.orphaned_review_comments
            .iter()
            .filter(|comment| comment.deleted_on.is_none())
            .count()
            + self
                .stored_review_comments
                .iter()
                .map(|(_, comments)| {
                    comments
                        .iter()
                        .filter(|comment| comment.deleted_on.is_none())
                        .count()
                })
                .sum::<usize>()
    }

    pub(super) fn refresh_review_comment_outdated_markers(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let mut changed = false;
        let stale_reasons = self
            .stored_review_comments
            .iter()
            .map(|(key, comments)| {
                comments
                    .iter()
                    .map(|comment| Self::outdated_review_comment_reason(key, comment, &snapshot))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        for ((_, comments), reasons) in self.stored_review_comments.iter_mut().zip(stale_reasons) {
            for (comment, reason) in comments.iter_mut().zip(reasons) {
                let outdated = reason.is_some();
                if comment.outdated != outdated || comment.outdated_reason != reason {
                    comment.outdated = outdated;
                    comment.outdated_reason = reason;
                    changed = true;
                }
            }
        }

        if changed {
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: self.total_review_comment_count(),
                persist: true,
            });
            cx.notify();
        }

        changed
    }

    fn outdated_review_comment_reason(
        key: &DiffHunkKey,
        comment: &StoredReviewComment,
        snapshot: &MultiBufferSnapshot,
    ) -> Option<String> {
        let file_has_visible_diff = snapshot
            .buffers_with_paths()
            .any(|(_, path_key)| path_key.path == key.file_path);
        if !file_has_visible_diff {
            return Some("file_not_in_diff".to_string());
        }

        if !comment.range.start.is_valid(snapshot) || !comment.range.end.is_valid(snapshot) {
            return Some("line_not_in_diff".to_string());
        }

        let start = comment.range.start.to_point(snapshot);
        let end = comment.range.end.to_point(snapshot);
        let query_end = if end.row < snapshot.max_point().row {
            Point::new(end.row + 1, 0)
        } else {
            snapshot.max_point()
        };
        let has_hunk = snapshot.diff_hunks_in_range(start..query_end).any(|hunk| {
            snapshot
                .buffers_with_paths()
                .any(|(buffer_snapshot, path_key)| {
                    buffer_snapshot.remote_id() == hunk.buffer_id && path_key.path == key.file_path
                })
        });

        (!has_hunk).then(|| "line_not_in_diff".to_string())
    }

    pub(super) fn add_review_comment(
        &mut self,
        hunk_key: DiffHunkKey,
        comment: String,
        anchor_range: Range<Anchor>,
        cx: &mut Context<Self>,
    ) -> usize {
        let id = self.next_review_comment_id;
        self.next_review_comment_id += 1;

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let location = Self::review_comment_location(&hunk_key, &anchor_range, &snapshot);
        let stored_comment = StoredReviewComment::new(id, comment, anchor_range, location);
        let key_point = hunk_key.hunk_start_anchor.to_point(&snapshot);

        if let Some((_, comments)) = self.stored_review_comments.iter_mut().find(|(k, _)| {
            k.file_path == hunk_key.file_path
                && k.hunk_start_anchor.to_point(&snapshot) == key_point
        }) {
            comments.push(stored_comment);
        } else {
            self.stored_review_comments
                .push((hunk_key, vec![stored_comment]));
        }

        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.total_review_comment_count(),
            persist: true,
        });
        cx.notify();
        id
    }

    pub(super) fn blame_hover(
        &mut self,
        _: &BlameHover,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let just_started = self.blame.is_none();
        if just_started {
            self.start_git_blame(true, window, cx);
        }
        let Some(blame) = self.blame.as_ref() else {
            return;
        };

        if just_started && !blame.read(cx).has_generated_entries() {
            let subscription = cx.observe_in(blame, window, |editor, blame, window, cx| {
                if blame.read(cx).has_generated_entries() {
                    editor.pending_blame_hover_observation.take();
                    editor.show_blame_hover_popover(window, cx);
                }
            });
            self.pending_blame_hover_observation = Some(subscription);
            return;
        }

        self.show_blame_hover_popover(window, cx);
    }

    fn show_blame_hover_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.snapshot(window, cx);
        let cursor = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();
        let Some((buffer, point)) = snapshot.buffer_snapshot().point_to_buffer_point(cursor) else {
            return;
        };

        let Some(blame) = self.blame.as_ref() else {
            return;
        };

        let row_info = RowInfo {
            buffer_id: Some(buffer.remote_id()),
            buffer_row: Some(point.row),
            ..Default::default()
        };
        let Some((buffer, blame_entry)) = blame
            .update(cx, |blame, cx| blame.blame_for_rows(&[row_info], cx).next())
            .flatten()
        else {
            return;
        };

        let anchor = self.selections.newest_anchor().head();
        let position = self.to_pixel_point(anchor, &snapshot, window, cx);
        if let (Some(position), Some(last_bounds)) = (position, self.last_bounds) {
            self.show_blame_popover(
                buffer,
                &blame_entry,
                position + last_bounds.origin,
                true,
                cx,
            );
        };
    }

    pub(super) fn restore_file(
        &mut self,
        _: &::git::RestoreFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let mut buffer_ids = HashSet::default();
        let snapshot = self.buffer().read(cx).snapshot(cx);
        for selection in self
            .selections
            .all::<MultiBufferOffset>(&self.display_snapshot(cx))
        {
            buffer_ids.extend(snapshot.buffer_ids_for_range(selection.range()))
        }

        let ranges = buffer_ids
            .into_iter()
            .flat_map(|buffer_id| snapshot.range_for_buffer(buffer_id))
            .collect::<Vec<_>>();

        self.restore_hunks_in_ranges(ranges, window, cx);
    }

    /// Restores the diff hunks in the editor's selections and moves the cursor
    /// to the next diff hunk. Wraps around to the beginning of the buffer if
    /// not all diff hunks are expanded.
    pub(super) fn restore_and_next(
        &mut self,
        _: &::git::RestoreAndNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let selections = self
            .selections
            .all(&self.display_snapshot(cx))
            .into_iter()
            .map(|selection| selection.range())
            .collect();

        self.restore_hunks_in_ranges(selections, window, cx);

        let all_diff_hunks_expanded = self.buffer().read(cx).all_diff_hunks_expanded();
        let wrap_around = !all_diff_hunks_expanded;
        let snapshot = self.snapshot(window, cx);
        let position = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();

        self.go_to_hunk_before_or_after_position(
            &snapshot,
            position,
            Direction::Next,
            wrap_around,
            window,
            cx,
        );
    }

    pub fn restore_diff_hunks(&mut self, hunks: Vec<ResolvedDiffHunks>, cx: &mut Context<Self>) {
        let mut revert_changes = Vec::new();
        for hunks in hunks {
            let Some(buffer) = hunks.buffer else {
                continue;
            };
            let diff_snapshot = hunks.diff.read(cx).snapshot(cx);
            let changes = hunks
                .hunks
                .into_iter()
                .filter_map(|hunk| {
                    if hunk.diff_base_byte_range == (0..0)
                        && hunk.buffer_range.start.is_min()
                        && hunk.buffer_range.end.is_max()
                    {
                        return None;
                    }
                    let original_text = diff_snapshot
                        .base_text()
                        .as_rope()
                        .slice(hunk.diff_base_byte_range.start..hunk.diff_base_byte_range.end);
                    Some((hunk.buffer_range, original_text))
                })
                .collect::<Vec<_>>();
            if !changes.is_empty() {
                revert_changes.push((buffer, changes));
            }
        }

        for (buffer, changes) in revert_changes {
            buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    changes
                        .into_iter()
                        .map(|(range, text)| (range, text.to_string())),
                    None,
                    cx,
                );
            });
        }
    }

    pub(super) fn go_to_next_hunk(
        &mut self,
        _: &GoToHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let selection = self.selections.newest::<Point>(&self.display_snapshot(cx));
        self.go_to_hunk_before_or_after_position(
            &snapshot,
            selection.head(),
            Direction::Next,
            true,
            window,
            cx,
        );
    }

    pub(super) fn collapse_all_diff_hunks(
        &mut self,
        _: &CollapseAllDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.collapse_diff_hunks(vec![Anchor::Min..Anchor::Max], cx)
        });
    }

    pub fn toggle_all_diff_hunks(
        &mut self,
        _: &ToggleAllDiffHunks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_any_expanded_diff_hunks(cx) {
            self.collapse_all_diff_hunks(&CollapseAllDiffHunks, window, cx);
        } else {
            self.expand_all_diff_hunks(&ExpandAllDiffHunks, window, cx);
        }
    }

    pub(super) fn toggle_selected_diff_hunks(
        &mut self,
        _: &ToggleSelectedDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        self.toggle_diff_hunks_in_ranges(ranges, cx);
    }

    pub(super) fn show_diff_review_button(&self) -> bool {
        self.show_diff_review_button
    }

    pub(super) fn add_review_comment_action(
        &mut self,
        _: &AddComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let row = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head()
            .to_display_point(&snapshot.display_snapshot)
            .row();
        self.show_diff_review_overlay(row..row, window, cx);
    }

    pub(super) fn go_to_next_review_comment_thread(
        &mut self,
        _: &NextCommentThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.go_to_review_comment_thread(Direction::Next, window, cx);
    }

    pub(super) fn go_to_previous_review_comment_thread(
        &mut self,
        _: &PreviousCommentThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.go_to_review_comment_thread(Direction::Prev, window, cx);
    }

    pub(super) fn current_review_comment_id(&self, cx: &App) -> Option<usize> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let cursor_row = self
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row;
        let mut best: Option<(u32, usize)> = None;
        for (_, comments) in &self.stored_review_comments {
            for comment in comments {
                if comment.deleted_on.is_some() {
                    continue;
                }
                let range = comment.range.to_point(&snapshot);
                let distance = if cursor_row < range.start.row {
                    range.start.row - cursor_row
                } else if cursor_row > range.end.row {
                    cursor_row - range.end.row
                } else {
                    0
                };
                if best.map_or(true, |(best_distance, _)| distance < best_distance) {
                    best = Some((distance, comment.id));
                }
            }
        }
        best.map(|(_, id)| id)
    }

    pub(super) fn reply_to_current_review_comment(
        &mut self,
        _: &ReplyToComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.current_review_comment_id(cx) {
            self.reply_to_review_comment(&ReplyToReviewComment { id }, window, cx);
        }
    }

    pub(super) fn edit_current_review_comment(
        &mut self,
        _: &EditComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.current_review_comment_id(cx) {
            self.edit_review_comment(&EditReviewComment { id }, window, cx);
        }
    }

    pub(super) fn delete_current_review_comment(
        &mut self,
        _: &DeleteComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.current_review_comment_id(cx) {
            self.delete_review_comment(&DeleteReviewComment { id }, window, cx);
        }
    }

    pub(super) fn copy_current_review_comment_reference(
        &mut self,
        _: &CopyCommentReference,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.current_review_comment_id(cx) {
            self.copy_review_comment_reference(id, false, cx);
        }
    }

    fn go_to_review_comment_thread(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let current_position = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let threads = self.review_comment_thread_positions(&buffer_snapshot);
        let Some((_, destination, hunk_key)) =
            Self::review_comment_thread_after_or_before(&threads, current_position, direction)
        else {
            return;
        };

        self.unfold_ranges(&[destination..destination], false, false, cx);
        self.change_selections(
            SelectionEffects::scroll(Autoscroll::center()),
            window,
            cx,
            |s| {
                s.select_ranges([destination..destination]);
            },
        );

        let snapshot = self.buffer.read(cx).snapshot(cx);
        if let Some(overlay) = self
            .diff_review_overlays
            .iter_mut()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
        {
            overlay.comments_expanded = true;
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }

    fn review_comment_thread_positions(
        &self,
        snapshot: &MultiBufferSnapshot,
    ) -> Vec<(usize, Point, DiffHunkKey)> {
        let mut threads = Vec::new();

        for (hunk_key, comments) in &self.stored_review_comments {
            for comment in comments {
                if comment.deleted_on.is_none() && comment.range.start.is_valid(snapshot) {
                    threads.push((
                        comment.id,
                        comment.range.start.to_point(snapshot),
                        hunk_key.clone(),
                    ));
                }
            }
        }

        threads.sort_by_key(|(id, point, _)| (point.row, point.column, *id));
        threads
    }

    fn review_comment_thread_after_or_before(
        threads: &[(usize, Point, DiffHunkKey)],
        position: Point,
        direction: Direction,
    ) -> Option<(usize, Point, DiffHunkKey)> {
        match direction {
            Direction::Next => threads
                .iter()
                .find(|(_, point, _)| {
                    point.row > position.row
                        || point.row == position.row && point.column > position.column
                })
                .or_else(|| threads.first())
                .cloned(),
            Direction::Prev => threads
                .iter()
                .rev()
                .find(|(_, point, _)| {
                    point.row < position.row
                        || point.row == position.row && point.column < position.column
                })
                .or_else(|| threads.last())
                .cloned(),
        }
    }

    pub(super) fn render_diff_review_button(
        &self,
        display_row: DisplayRow,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text_color = cx.theme().colors().text;
        let icon_color = cx.theme().colors().icon_accent;

        h_flex()
            .id("diff_review_button")
            .cursor_pointer()
            .w(width - px(1.))
            .h(relative(0.9))
            .justify_center()
            .rounded_sm()
            .border_1()
            .border_color(text_color.opacity(0.1))
            .bg(text_color.opacity(0.15))
            .hover(|s| {
                s.bg(icon_color.opacity(0.4))
                    .border_color(icon_color.opacity(0.5))
            })
            .child(Icon::new(IconName::Plus).size(IconSize::Small))
            .tooltip(Tooltip::text("Add Review (drag to select multiple lines)"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |editor, _event: &gpui::MouseDownEvent, window, cx| {
                    editor.start_diff_review_drag(display_row, window, cx);
                }),
            )
    }

    pub(super) fn start_diff_review_drag(
        &mut self,
        display_row: DisplayRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let point = snapshot
            .display_snapshot
            .display_point_to_point(DisplayPoint::new(display_row, 0), Bias::Left);
        let anchor = snapshot.buffer_snapshot().anchor_before(point);
        self.diff_review_drag_state = Some(DiffReviewDragState {
            start_anchor: anchor,
            current_anchor: anchor,
        });
        cx.notify();
    }

    pub(super) fn update_diff_review_drag(
        &mut self,
        display_row: DisplayRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff_review_drag_state.is_none() {
            return;
        }
        let snapshot = self.snapshot(window, cx);
        let point = snapshot
            .display_snapshot
            .display_point_to_point(display_row.as_display_point(), Bias::Left);
        let anchor = snapshot.buffer_snapshot().anchor_before(point);
        if let Some(drag_state) = &mut self.diff_review_drag_state {
            drag_state.current_anchor = anchor;
            cx.notify();
        }
    }

    pub(super) fn end_diff_review_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag_state) = self.diff_review_drag_state.take() {
            let snapshot = self.snapshot(window, cx);
            let range = drag_state.row_range(&snapshot.display_snapshot);
            self.show_diff_review_overlay(*range.start()..*range.end(), window, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_diff_review_drag(&mut self, cx: &mut Context<Self>) {
        self.diff_review_drag_state = None;
        cx.notify();
    }

    /// Dismisses all diff review overlays.
    pub(super) fn dismiss_all_diff_review_overlays(&mut self, cx: &mut Context<Self>) {
        if self.diff_review_overlays.is_empty() {
            return;
        }
        let block_ids: HashSet<_> = self
            .diff_review_overlays
            .drain(..)
            .map(|overlay| overlay.block_id)
            .collect();
        self.remove_blocks(block_ids, None, cx);
        cx.notify();
    }

    pub(super) fn cancel_diff_review_prompt(
        &mut self,
        hunk_key: &DiffHunkKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay_index) = self
            .diff_review_overlays
            .iter()
            .position(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
        else {
            return;
        };
        let overlay = &mut self.diff_review_overlays[overlay_index];
        let has_unsaved_text = !overlay.prompt_editor.read(cx).text(cx).trim().is_empty();
        if !has_unsaved_text {
            self.dismiss_diff_review_prompt(hunk_key, window, cx);
            return;
        }
        if overlay.confirming_discard {
            return;
        }
        overlay.confirming_discard = true;

        let prompt = window.prompt(
            PromptLevel::Warning,
            "Discard review comment?",
            Some("This comment hasn't been saved yet."),
            &["Discard", "Keep editing"],
            cx,
        );
        let editor = cx.entity().downgrade();
        let hunk_key = hunk_key.clone();
        cx.spawn_in(window, async move |_, cx| {
            let discard = prompt.await == Ok(0);
            editor
                .update_in(cx, |editor, window, cx| {
                    if discard {
                        editor.dismiss_diff_review_prompt(&hunk_key, window, cx);
                    } else if let Some(overlay) = editor.diff_review_overlay_mut(&hunk_key, cx) {
                        overlay.confirming_discard = false;
                    }
                })
                .ok();
        })
        .detach();
    }

    fn diff_review_overlay_mut(
        &mut self,
        hunk_key: &DiffHunkKey,
        cx: &App,
    ) -> Option<&mut DiffReviewOverlay> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let overlay_index = self
            .diff_review_overlays
            .iter()
            .position(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))?;
        self.diff_review_overlays.get_mut(overlay_index)
    }

    pub(super) fn dismiss_diff_review_prompt(
        &mut self,
        hunk_key: &DiffHunkKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay_index) = self
            .diff_review_overlays
            .iter()
            .position(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
        else {
            return;
        };

        let has_comments = self.hunk_comment_count(hunk_key, &snapshot) > 0;
        if has_comments {
            self.diff_review_overlays[overlay_index].prompt_visible = false;
            self.refresh_diff_review_overlay_height(hunk_key, window, cx);
        } else {
            let overlay = self.diff_review_overlays.remove(overlay_index);
            self.remove_blocks(HashSet::from_iter([overlay.block_id]), None, cx);
            cx.notify();
        }
    }

    pub(super) fn dismiss_empty_diff_review_prompt(
        &mut self,
        hunk_key: &DiffHunkKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay_index) = self
            .diff_review_overlays
            .iter()
            .position(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
        else {
            return false;
        };

        let overlay = &self.diff_review_overlays[overlay_index];
        if !overlay.prompt_visible || !overlay.prompt_editor.read(cx).text(cx).trim().is_empty() {
            return false;
        }

        self.dismiss_diff_review_prompt(hunk_key, window, cx);
        true
    }

    pub(super) fn dismiss_focused_diff_review_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let hunk_key = self
            .diff_review_overlays
            .iter()
            .find(|overlay| {
                overlay.prompt_visible && overlay.prompt_editor.focus_handle(cx).is_focused(window)
            })
            .map(|overlay| overlay.hunk_key.clone());
        if let Some(hunk_key) = hunk_key {
            self.cancel_diff_review_prompt(&hunk_key, window, cx);
            true
        } else {
            false
        }
    }

    pub(super) fn has_visible_review_comments(&self, cx: &App) -> bool {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        self.diff_review_overlays
            .iter()
            .any(|overlay| self.hunk_comment_count(&overlay.hunk_key, &snapshot) > 0)
    }

    /// Action handler for SubmitDiffReviewComment.
    pub(super) fn submit_diff_review_comment_action(
        &mut self,
        _: &SubmitDiffReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_diff_review_comment(window, cx);
    }

    /// Returns comments for a specific hunk, ordered by creation time.
    pub(super) fn comments_for_hunk(
        &self,
        key: &DiffHunkKey,
        snapshot: &MultiBufferSnapshot,
    ) -> Vec<StoredReviewComment> {
        let key_point = key.hunk_start_anchor.to_point(snapshot);
        self.stored_review_comments
            .iter()
            .find(|(k, _)| {
                k.file_path == key.file_path && k.hunk_start_anchor.to_point(snapshot) == key_point
            })
            .map(|(_, comments)| {
                comments
                    .iter()
                    .filter(|comment| comment.deleted_on.is_none())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns the count of comments for a specific hunk.
    pub(super) fn hunk_comment_count(
        &self,
        key: &DiffHunkKey,
        snapshot: &MultiBufferSnapshot,
    ) -> usize {
        let key_point = key.hunk_start_anchor.to_point(snapshot);
        self.stored_review_comments
            .iter()
            .find(|(k, _)| {
                k.file_path == key.file_path && k.hunk_start_anchor.to_point(snapshot) == key_point
            })
            .map(|(_, comments)| {
                comments
                    .iter()
                    .filter(|comment| comment.deleted_on.is_none())
                    .count()
            })
            .unwrap_or(0)
    }

    pub(super) fn drop_review_comment_body_editors(
        &mut self,
        hunk_key: &DiffHunkKey,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay_index) = self
            .diff_review_overlays
            .iter()
            .position(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
        else {
            return;
        };

        self.diff_review_overlays[overlay_index]
            .body_editors
            .clear();
        self.diff_review_overlays[overlay_index]
            .reply_body_editors
            .clear();
    }

    fn hunk_key_for_review_comment(&self, id: usize) -> Option<DiffHunkKey> {
        self.stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|comment| comment.id == id) {
                    Some(key.clone())
                } else {
                    None
                }
            })
    }

    fn hunk_key_for_review_reply(&self, id: usize) -> Option<DiffHunkKey> {
        self.stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments
                    .iter()
                    .any(|comment| comment.replies.iter().any(|reply| reply.id == id))
                {
                    Some(key.clone())
                } else {
                    None
                }
            })
    }

    pub(super) fn add_review_reply(
        &mut self,
        comment_id: usize,
        reply: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let id = self.next_review_reply_id;
        self.next_review_reply_id += 1;

        if let Some(comment) = self
            .orphaned_review_comments
            .iter_mut()
            .find(|comment| comment.id == comment_id)
        {
            comment.replies.push(ReviewReplySnapshot {
                id,
                author: DEFAULT_REVIEW_COMMENT_AUTHOR.to_string(),
                created_at: current_review_comment_timestamp(),
                body: reply,
                review_round: None,
            });
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: self.total_review_comment_count(),
                persist: true,
            });
            cx.notify();
            return true;
        }

        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|comment| comment.id == comment_id) {
                comment.replies.push(StoredReviewReply {
                    id,
                    author: DEFAULT_REVIEW_COMMENT_AUTHOR.to_string(),
                    created_at: current_review_comment_timestamp(),
                    comment: reply,
                    is_editing: false,
                    review_round: None,
                });
                cx.emit(EditorEvent::ReviewCommentsChanged {
                    total_count: self.total_review_comment_count(),
                    persist: true,
                });
                cx.notify();
                return true;
            }
        }

        self.next_review_reply_id = self.next_review_reply_id.saturating_sub(1);
        false
    }

    pub(super) fn remove_review_comment(&mut self, id: usize, cx: &mut Context<Self>) -> bool {
        if let Some(comment) = self
            .orphaned_review_comments
            .iter()
            .find(|comment| comment.id == id)
            && comment.deleted_on.is_some()
        {
            return false;
        }
        if let Some(comment) = self
            .orphaned_review_comments
            .iter_mut()
            .find(|comment| comment.id == id)
        {
            comment.deleted_on = current_review_comment_timestamp();
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: self.total_review_comment_count(),
                persist: true,
            });
            cx.notify();
            return true;
        }

        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|comment| comment.id == id) {
                if comment.deleted_on.is_some() {
                    return false;
                }
                comment.deleted_on = current_review_comment_timestamp();
                cx.emit(EditorEvent::ReviewCommentsChanged {
                    total_count: self.total_review_comment_count(),
                    persist: true,
                });
                cx.notify();
                return true;
            }
        }
        false
    }

    pub(super) fn restore_latest_deleted_review_comment(
        &mut self,
        _: &RestoreLatestDeletedComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut latest_deleted: Option<(String, usize)> = None;

        for comment in &self.orphaned_review_comments {
            if let Some(deleted_on) = &comment.deleted_on
                && latest_deleted
                    .as_ref()
                    .is_none_or(|latest| (deleted_on, comment.id) > (&latest.0, latest.1))
            {
                latest_deleted = Some((deleted_on.clone(), comment.id));
            }
        }

        for (_, comments) in &self.stored_review_comments {
            for comment in comments {
                if let Some(deleted_on) = &comment.deleted_on
                    && latest_deleted
                        .as_ref()
                        .is_none_or(|latest| (deleted_on, comment.id) > (&latest.0, latest.1))
                {
                    latest_deleted = Some((deleted_on.clone(), comment.id));
                }
            }
        }

        let Some((_, comment_id)) = latest_deleted else {
            return;
        };

        if let Some(comment) = self
            .orphaned_review_comments
            .iter_mut()
            .find(|comment| comment.id == comment_id)
        {
            comment.deleted_on = None;
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: self.total_review_comment_count(),
                persist: true,
            });
            cx.notify();
            return;
        }

        let mut restored_hunk_key = None;
        for (hunk_key, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|comment| comment.id == comment_id) {
                comment.deleted_on = None;
                restored_hunk_key = Some(hunk_key.clone());
                break;
            }
        }

        if let Some(hunk_key) = restored_hunk_key {
            let display_snapshot = self.display_snapshot(cx);
            let snapshot = self.buffer.read(cx).snapshot(cx);
            let display_range = self
                .stored_review_comments
                .iter()
                .find(|(key, _)| Self::hunk_keys_match(key, &hunk_key, &snapshot))
                .and_then(|(_, comments)| comments.iter().find(|comment| comment.id == comment_id))
                .map(|comment| {
                    let start = comment
                        .range
                        .start
                        .to_display_point(&display_snapshot)
                        .row();
                    let end = comment.range.end.to_display_point(&display_snapshot).row();
                    start..end
                });
            if let Some(display_range) = display_range {
                self.show_diff_review_overlay_with_prompt(display_range, false, true, window, cx);
            }
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: self.total_review_comment_count(),
                persist: true,
            });
            cx.notify();
        }
    }

    pub(super) fn remove_review_reply(&mut self, id: usize, cx: &mut Context<Self>) -> bool {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(replies) = comments.iter_mut().find_map(|comment| {
                comment
                    .replies
                    .iter()
                    .position(|reply| reply.id == id)
                    .map(|index| (&mut comment.replies, index))
            }) {
                replies.0.remove(replies.1);
                cx.emit(EditorEvent::ReviewCommentsChanged {
                    total_count: self.total_review_comment_count(),
                    persist: true,
                });
                cx.notify();
                return true;
            }
        }
        false
    }

    pub(super) fn update_review_comment(
        &mut self,
        id: usize,
        new_comment: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(comment) = self
            .orphaned_review_comments
            .iter_mut()
            .find(|comment| comment.id == id)
        {
            comment.body = new_comment;
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: self.total_review_comment_count(),
                persist: true,
            });
            cx.notify();
            return true;
        }

        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|c| c.id == id) {
                comment.comment = new_comment;
                comment.is_editing = false;
                cx.emit(EditorEvent::ReviewCommentsChanged {
                    total_count: self.total_review_comment_count(),
                    persist: true,
                });
                cx.notify();
                return true;
            }
        }
        false
    }

    pub(super) fn update_review_reply(
        &mut self,
        id: usize,
        new_comment: String,
        cx: &mut Context<Self>,
    ) -> bool {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            for comment in comments {
                if let Some(reply) = comment.replies.iter_mut().find(|reply| reply.id == id) {
                    reply.comment = new_comment;
                    reply.is_editing = false;
                    cx.emit(EditorEvent::ReviewCommentsChanged {
                        total_count: self.total_review_comment_count(),
                        persist: true,
                    });
                    cx.notify();
                    return true;
                }
            }
        }
        false
    }

    /// Sets a comment's editing state.
    pub(super) fn set_comment_editing(
        &mut self,
        id: usize,
        is_editing: bool,
        cx: &mut Context<Self>,
    ) {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|c| c.id == id) {
                comment.is_editing = is_editing;
                cx.notify();
                return;
            }
        }
    }

    pub(super) fn set_reply_editing(
        &mut self,
        id: usize,
        is_editing: bool,
        cx: &mut Context<Self>,
    ) {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            for comment in comments {
                if let Some(reply) = comment.replies.iter_mut().find(|reply| reply.id == id) {
                    reply.is_editing = is_editing;
                    cx.notify();
                    return;
                }
            }
        }
    }

    pub(super) fn send_review_to_agent(
        &mut self,
        _: &SendReviewToAgent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_review_comments_json(cx) {
            Ok(json) => cx.write_to_clipboard(ClipboardItem::new_string(json)),
            Err(error) => log::error!("failed to serialize review comments: {error:#}"),
        }
    }

    pub fn review_comments_json(&self, _cx: &App) -> anyhow::Result<String> {
        self.review_comments_json_with_deleted(true)
    }

    fn active_review_comments_json(&self, _cx: &App) -> anyhow::Result<String> {
        self.review_comments_json_with_deleted(false)
    }

    fn review_comments_json_with_deleted(&self, include_deleted: bool) -> anyhow::Result<String> {
        let mut comments = self.orphaned_review_comments.clone();
        if !include_deleted {
            comments.retain(|comment| comment.deleted_on.is_none());
        }

        for (_, hunk_comments) in &self.stored_review_comments {
            for comment in hunk_comments {
                if include_deleted || comment.deleted_on.is_none() {
                    comments.push(Self::review_comment_snapshot(comment));
                }
            }
        }

        serde_json::to_string_pretty(&ReviewCommentsSnapshot {
            schema_version: 1,
            comments,
        })
        .context("serializing review comments")
    }

    fn review_comment_location(
        hunk_key: &DiffHunkKey,
        anchor_range: &Range<Anchor>,
        snapshot: &MultiBufferSnapshot,
    ) -> ReviewCommentLocation {
        let hunk_line = if hunk_key.hunk_start_anchor.is_valid(snapshot) {
            hunk_key.hunk_start_anchor.to_point(snapshot).row + 1
        } else {
            1
        };

        let (line_start, line_end) =
            if anchor_range.start.is_valid(snapshot) && anchor_range.end.is_valid(snapshot) {
                let start_point = anchor_range.start.to_point(snapshot);
                let end_point = anchor_range.end.to_point(snapshot);
                snapshot
                    .range_to_buffer_ranges(start_point..end_point)
                    .into_iter()
                    .next()
                    .map(|(buffer_snapshot, range, _)| {
                        let start_line = buffer_snapshot.offset_to_point(range.start.0).row + 1;
                        let end_offset = if range.end.0 > range.start.0 {
                            range.end.0 - 1
                        } else {
                            range.start.0
                        };
                        let end_line = buffer_snapshot.offset_to_point(end_offset).row + 1;
                        (start_line, end_line)
                    })
                    .unwrap_or((start_point.row + 1, end_point.row + 1))
            } else {
                (1, 1)
            };

        ReviewCommentLocation {
            file: hunk_key.file_path.as_unix_str().to_string(),
            side: "new".to_string(),
            hunk_line,
            line_start,
            line_end,
        }
    }

    fn review_comment_snapshot(comment: &StoredReviewComment) -> ReviewCommentSnapshot {
        let location = comment.location.clone();

        ReviewCommentSnapshot {
            id: comment.id,
            author: comment.author.clone(),
            created_at: comment.created_at.clone(),
            deleted_on: comment.deleted_on.clone(),
            resolved_on: comment.resolved_on.clone(),
            file: location.file,
            side: location.side,
            hunk_line: location.hunk_line,
            line_start: location.line_start,
            line_end: location.line_end,
            body: comment.comment.clone(),
            outdated: comment.outdated,
            outdated_reason: comment.outdated_reason.clone(),
            review_round: comment.review_round,
            replies: comment
                .replies
                .iter()
                .map(|reply| ReviewReplySnapshot {
                    id: reply.id,
                    author: reply.author.clone(),
                    created_at: reply.created_at.clone(),
                    body: reply.comment.clone(),
                    review_round: reply.review_round,
                })
                .collect(),
        }
    }

    pub fn restore_review_comments_json(
        &mut self,
        json: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<usize> {
        let restored: ReviewCommentsSnapshot =
            serde_json::from_str(json).context("deserializing review comments")?;

        // Reloads (e.g. an agent writing a reply to the DB) rebuild every overlay from
        // scratch. Remember each thread's current expand/collapse state so a reload doesn't
        // silently re-collapse a thread the user had open.
        let prior_snapshot = self.buffer.read(cx).snapshot(cx);
        let prior_comments_expanded: HashMap<(Arc<util::rel_path::RelPath>, u32), bool> = self
            .diff_review_overlays
            .iter()
            .map(|overlay| {
                let row = overlay
                    .hunk_key
                    .hunk_start_anchor
                    .to_point(&prior_snapshot)
                    .row;
                (
                    (overlay.hunk_key.file_path.clone(), row),
                    overlay.comments_expanded,
                )
            })
            .collect();

        self.dismiss_all_diff_review_overlays(cx);
        self.stored_review_comments.clear();
        self.orphaned_review_comments.clear();
        self.next_review_comment_id = 0;
        self.next_review_reply_id = 0;

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let mut restored_count = 0;
        for mut saved_comment in restored.comments {
            if saved_comment.side != "new" {
                continue;
            }
            self.next_review_comment_id = self
                .next_review_comment_id
                .max(saved_comment.id.saturating_add(1));
            for reply in &saved_comment.replies {
                self.next_review_reply_id =
                    self.next_review_reply_id.max(reply.id.saturating_add(1));
            }

            let file_path = util::rel_path::RelPath::unix(saved_comment.file.as_str())?;
            let attachment = (|| {
                let Some((buffer_snapshot, path_key)) = snapshot
                    .buffers_with_paths()
                    .find(|(_, path_key)| path_key.path.as_ref() == file_path)
                else {
                    return Err("file_not_in_diff");
                };

                let start_row = saved_comment
                    .line_start
                    .checked_sub(1)
                    .ok_or("line_not_in_diff")?;
                let end_row = saved_comment
                    .line_end
                    .checked_sub(1)
                    .ok_or("line_not_in_diff")?;
                if start_row > buffer_snapshot.max_point().row
                    || end_row > buffer_snapshot.max_point().row
                {
                    return Err("line_not_in_diff");
                }

                let line_end = Point::new(end_row, buffer_snapshot.line_len(end_row));
                let buffer_anchor_range = buffer_snapshot.anchor_before(Point::new(start_row, 0))
                    ..buffer_snapshot.anchor_after(line_end);
                let anchor_range = snapshot
                    .anchor_range_in_buffer(buffer_anchor_range)
                    .ok_or("line_not_in_diff")?;

                Ok((
                    DiffHunkKey {
                        file_path: path_key.path.clone(),
                        hunk_start_anchor: anchor_range.start,
                    },
                    anchor_range,
                ))
            })();

            let (hunk_key, anchor_range) = match attachment {
                Ok(attachment) => attachment,
                Err(reason) => {
                    saved_comment.outdated = true;
                    saved_comment.outdated_reason = Some(reason.to_string());
                    self.orphaned_review_comments.push(saved_comment);
                    continue;
                }
            };

            let location = ReviewCommentLocation {
                file: saved_comment.file,
                side: saved_comment.side,
                hunk_line: saved_comment.hunk_line,
                line_start: saved_comment.line_start,
                line_end: saved_comment.line_end,
            };
            let replies = saved_comment
                .replies
                .into_iter()
                .map(|reply| StoredReviewReply {
                    id: reply.id,
                    author: reply.author,
                    created_at: reply.created_at,
                    comment: reply.body,
                    is_editing: false,
                    review_round: reply.review_round,
                })
                .collect();
            let comment = StoredReviewComment::new_with_author(
                saved_comment.id,
                saved_comment.author,
                saved_comment.body,
                anchor_range.clone(),
                replies,
                location,
            );
            let comment = StoredReviewComment {
                created_at: saved_comment.created_at,
                deleted_on: saved_comment.deleted_on,
                resolved_on: saved_comment.resolved_on,
                outdated: saved_comment.outdated,
                outdated_reason: saved_comment.outdated_reason,
                review_round: saved_comment.review_round,
                ..comment
            };

            let key_point = hunk_key.hunk_start_anchor.to_point(&snapshot);
            if let Some((_, comments)) = self.stored_review_comments.iter_mut().find(|(key, _)| {
                key.file_path == hunk_key.file_path
                    && key.hunk_start_anchor.to_point(&snapshot) == key_point
            }) {
                comments.push(comment);
            } else {
                self.stored_review_comments.push((hunk_key, vec![comment]));
            }
            restored_count += 1;
        }

        let restored_overlays: Vec<_> = self
            .stored_review_comments
            .iter()
            .filter_map(|(hunk_key, comments)| {
                comments
                    .iter()
                    .find(|comment| comment.deleted_on.is_none())
                    .map(|comment| (hunk_key.clone(), comment.range.clone()))
            })
            .collect();
        // Restored reviews open expanded by default. Threads the user already collapsed keep their
        // state instead of falling back to this default.
        let default_comments_expanded = true;
        for (hunk_key, range) in restored_overlays {
            let display_snapshot = self.display_snapshot(cx);
            let start = range.start.to_display_point(&display_snapshot).row();
            let end = range.end.to_display_point(&display_snapshot).row();
            let row = hunk_key.hunk_start_anchor.to_point(&snapshot).row;
            let comments_expanded = prior_comments_expanded
                .get(&(hunk_key.file_path.clone(), row))
                .copied()
                .unwrap_or(default_comments_expanded);
            self.show_diff_review_overlay_with_prompt(
                start..end,
                false,
                comments_expanded,
                window,
                cx,
            );
        }

        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.total_review_comment_count(),
            persist: false,
        });
        cx.notify();
        Ok(restored_count)
    }

    pub fn clear_review_comments(&mut self, cx: &mut Context<Self>) {
        self.dismiss_all_diff_review_overlays(cx);
        self.stored_review_comments.clear();
        self.orphaned_review_comments.clear();
        self.next_review_comment_id = 0;
        self.next_review_reply_id = 0;
        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: 0,
            persist: false,
        });
        cx.notify();
    }

    /// Toggles the expanded state of the comments section in the overlay.
    pub(super) fn toggle_review_comments_expanded(
        &mut self,
        _: &ToggleReviewCommentsExpanded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Find the overlay that currently has focus, or use the first one
        let overlay_info = self.diff_review_overlays.iter_mut().find_map(|overlay| {
            if overlay.prompt_editor.focus_handle(cx).is_focused(window) {
                overlay.comments_expanded = !overlay.comments_expanded;
                Some(overlay.hunk_key.clone())
            } else {
                None
            }
        });

        // If no focused overlay found, toggle the first one
        let hunk_key = overlay_info.or_else(|| {
            self.diff_review_overlays.first_mut().map(|overlay| {
                overlay.comments_expanded = !overlay.comments_expanded;
                overlay.hunk_key.clone()
            })
        });

        if let Some(hunk_key) = hunk_key {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
            cx.notify();
        }
    }

    /// Expands every review comment thread, or collapses them all when each one is already open.
    pub(super) fn toggle_all_review_comments(
        &mut self,
        _: &ToggleAllComments,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff_review_overlays.is_empty() {
            return;
        }
        let expand = !self
            .diff_review_overlays
            .iter()
            .all(|overlay| overlay.comments_expanded);
        let hunk_keys: Vec<DiffHunkKey> = self
            .diff_review_overlays
            .iter_mut()
            .map(|overlay| {
                overlay.comments_expanded = expand;
                overlay.hunk_key.clone()
            })
            .collect();
        for hunk_key in hunk_keys {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
        cx.notify();
    }

    /// Toggles the expanded state of a specific overlay, identified by its hunk. Used by the
    /// comments-section header so clicking it always targets that overlay rather than the
    /// focused or first one.
    pub(super) fn toggle_review_comments_expanded_for_hunk(
        &mut self,
        hunk_key: &DiffHunkKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay) = self
            .diff_review_overlays
            .iter_mut()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
        else {
            return;
        };
        overlay.comments_expanded = !overlay.comments_expanded;
        self.refresh_diff_review_overlay_height(hunk_key, window, cx);
        cx.notify();
    }

    /// Handles the EditReviewComment action - sets a comment into editing mode.
    pub(super) fn edit_review_comment(
        &mut self,
        action: &EditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_id = action.id;

        self.set_comment_editing(comment_id, true, cx);

        let hunk_key = self.hunk_key_for_review_comment(comment_id);
        let completion_project = self.project.as_ref().map(Entity::downgrade);

        let snapshot = self.buffer.read(cx).snapshot(cx);
        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    overlay.inline_edit_editors.entry(comment_id)
                {
                    let comment_text = self
                        .stored_review_comments
                        .iter()
                        .flat_map(|(_, comments)| comments)
                        .find(|c| c.id == comment_id)
                        .map(|c| c.comment.clone())
                        .unwrap_or_default();

                    let parent_editor = cx.entity().downgrade();
                    let inline_editor = cx.new(|cx| {
                        let mut editor = Self::review_comment_input_editor(window, cx);
                        editor.set_text(&*comment_text, window, cx);
                        editor.move_to_end(&crate::actions::MoveToEnd, window, cx);
                        editor.diff_review_editor_cancel = Some(DiffReviewEditorCancel::edit(
                            parent_editor.clone(),
                            comment_id,
                        ));
                        editor
                    });
                    Self::attach_review_comment_completion_provider(
                        completion_project.clone(),
                        &inline_editor,
                        cx,
                    );
                    Self::install_review_comment_insert_mode_handler(&inline_editor, window, cx);

                    let enter_subscription =
                        Self::review_comment_enter_subscription(&inline_editor, cx, {
                            let parent_editor = parent_editor.clone();
                            move |window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.confirm_edit_review_comment(comment_id, window, cx);
                                    });
                                }
                            }
                        });
                    let subscription = inline_editor.update(cx, |inline_editor, _cx| {
                        let confirm_subscription = inline_editor.register_action({
                            let parent_editor = parent_editor.clone();
                            move |_: &menu::Confirm, window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.confirm_edit_review_comment(comment_id, window, cx);
                                    });
                                }
                            }
                        });
                        let cancel_subscription = inline_editor.register_action({
                            let parent_editor = parent_editor.clone();
                            move |_: &crate::actions::Cancel, window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.cancel_edit_review_comment(comment_id, window, cx);
                                    });
                                }
                            }
                        });
                        Subscription::join(
                            Subscription::join(confirm_subscription, cancel_subscription),
                            enter_subscription,
                        )
                    });

                    overlay
                        .inline_edit_subscriptions
                        .insert(comment_id, subscription);

                    Self::focus_review_comment_editor(&inline_editor, window, cx);

                    entry.insert(inline_editor);
                }
            }
        }

        cx.notify();
    }

    pub(super) fn reply_to_review_comment(
        &mut self,
        action: &ReplyToReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_id = action.id;
        let Some(hunk_key) = self.hunk_key_for_review_comment(comment_id) else {
            return;
        };

        let completion_project = self.project.as_ref().map(Entity::downgrade);
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let Some(overlay) = self
            .diff_review_overlays
            .iter_mut()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
        else {
            return;
        };

        if let std::collections::hash_map::Entry::Vacant(entry) =
            overlay.reply_editors.entry(comment_id)
        {
            let parent_editor = cx.entity().downgrade();
            let reply_editor = cx.new(|cx| {
                let mut editor = Self::review_comment_input_editor(window, cx);
                editor.set_placeholder_text("Reply...", window, cx);
                editor.diff_review_editor_cancel = Some(DiffReviewEditorCancel::reply(
                    parent_editor.clone(),
                    comment_id,
                ));
                editor
            });
            Self::attach_review_comment_completion_provider(completion_project, &reply_editor, cx);
            Self::install_review_comment_insert_mode_handler(&reply_editor, window, cx);

            let enter_subscription = Self::review_comment_enter_subscription(&reply_editor, cx, {
                let parent_editor = parent_editor.clone();
                move |window, cx| {
                    if let Some(editor) = parent_editor.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.submit_review_reply(comment_id, window, cx);
                        });
                    }
                }
            });
            let subscription = reply_editor.update(cx, |reply_editor, _cx| {
                let submit_subscription = reply_editor.register_action({
                    let parent_editor = parent_editor.clone();
                    move |_: &menu::Confirm, window, cx| {
                        if let Some(editor) = parent_editor.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.submit_review_reply(comment_id, window, cx);
                            });
                        }
                    }
                });
                let cancel_subscription = reply_editor.register_action({
                    let parent_editor = parent_editor.clone();
                    move |_: &crate::actions::Cancel, window, cx| {
                        if let Some(editor) = parent_editor.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.cancel_review_reply(comment_id, window, cx);
                            });
                        }
                    }
                });
                Subscription::join(
                    Subscription::join(submit_subscription, cancel_subscription),
                    enter_subscription,
                )
            });

            overlay.reply_subscriptions.insert(comment_id, subscription);
            Self::focus_review_comment_editor(&reply_editor, window, cx);
            entry.insert(reply_editor);
        }

        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        cx.notify();
    }

    pub(super) fn submit_review_reply(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hunk_key) = self.hunk_key_for_review_comment(comment_id) else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let reply_text = self
            .diff_review_overlays
            .iter()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            .and_then(|overlay| overlay.reply_editors.get(&comment_id))
            .map(|editor| editor.read(cx).text(cx).trim().to_string());

        if let Some(reply_text) = reply_text {
            if !reply_text.is_empty() {
                self.add_review_reply(comment_id, reply_text, cx);
            }
        }

        self.cancel_review_reply(comment_id, window, cx);
    }

    pub(super) fn cancel_review_reply(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hunk_key) = self.hunk_key_for_review_comment(comment_id) else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot(cx);
        if let Some(overlay) = self
            .diff_review_overlays
            .iter_mut()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
        {
            overlay.reply_editors.remove(&comment_id);
            overlay.reply_subscriptions.remove(&comment_id);
        }

        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        cx.notify();
    }

    pub(super) fn cancel_active_diff_review_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let inline_comment_id = self
            .diff_review_overlays
            .iter()
            .find_map(|overlay| overlay.inline_edit_editors.keys().next().copied());
        if let Some(comment_id) = inline_comment_id {
            self.cancel_edit_review_comment(comment_id, window, cx);
            return true;
        }

        let inline_reply_id = self
            .diff_review_overlays
            .iter()
            .find_map(|overlay| overlay.inline_reply_edit_editors.keys().next().copied());
        if let Some(reply_id) = inline_reply_id {
            self.cancel_edit_review_reply(reply_id, window, cx);
            return true;
        }

        let reply_comment_id = self
            .diff_review_overlays
            .iter()
            .find_map(|overlay| overlay.reply_editors.keys().next().copied());
        if let Some(comment_id) = reply_comment_id {
            self.cancel_review_reply(comment_id, window, cx);
            return true;
        }

        false
    }

    /// Confirms an inline edit of a review comment.
    pub(super) fn confirm_edit_review_comment(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let hunk_key = self.hunk_key_for_review_comment(comment_id);

        let new_text = hunk_key
            .as_ref()
            .and_then(|hunk_key| {
                self.diff_review_overlays
                    .iter()
                    .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
            })
            .as_ref()
            .and_then(|overlay| overlay.inline_edit_editors.get(&comment_id))
            .map(|editor| editor.read(cx).text(cx).trim().to_string());

        if let Some(new_text) = new_text {
            if !new_text.is_empty() {
                self.update_review_comment(comment_id, new_text, cx);
            }
        }

        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_edit_editors.remove(&comment_id);
                overlay.inline_edit_subscriptions.remove(&comment_id);
            }
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }

        self.set_comment_editing(comment_id, false, cx);
    }

    /// Cancels an inline edit of a review comment.
    pub(super) fn cancel_edit_review_comment(
        &mut self,
        comment_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunk_key = self.hunk_key_for_review_comment(comment_id);
        self.set_comment_editing(comment_id, false, cx);

        if let Some(hunk_key) = hunk_key {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_edit_editors.remove(&comment_id);
                overlay.inline_edit_subscriptions.remove(&comment_id);
            }
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }

    pub(super) fn confirm_edit_review_reply(
        &mut self,
        reply_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let hunk_key = self.hunk_key_for_review_reply(reply_id);

        let new_text = hunk_key
            .as_ref()
            .and_then(|hunk_key| {
                self.diff_review_overlays
                    .iter()
                    .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
            })
            .as_ref()
            .and_then(|overlay| overlay.inline_reply_edit_editors.get(&reply_id))
            .map(|editor| editor.read(cx).text(cx).trim().to_string());

        if let Some(new_text) = new_text {
            if !new_text.is_empty() {
                self.update_review_reply(reply_id, new_text, cx);
            }
        }

        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_reply_edit_editors.remove(&reply_id);
                overlay.inline_reply_edit_subscriptions.remove(&reply_id);
            }
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }

        self.set_reply_editing(reply_id, false, cx);
    }

    pub(super) fn cancel_edit_review_reply(
        &mut self,
        reply_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunk_key = self.hunk_key_for_review_reply(reply_id);
        self.set_reply_editing(reply_id, false, cx);

        if let Some(hunk_key) = hunk_key {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_reply_edit_editors.remove(&reply_id);
                overlay.inline_reply_edit_subscriptions.remove(&reply_id);
            }
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }

    /// Action handler for ConfirmEditReviewComment.
    pub(super) fn confirm_edit_review_comment_action(
        &mut self,
        action: &ConfirmEditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_edit_review_comment(action.id, window, cx);
    }

    /// Action handler for CancelEditReviewComment.
    pub(super) fn cancel_edit_review_comment_action(
        &mut self,
        action: &CancelEditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_edit_review_comment(action.id, window, cx);
    }

    pub(super) fn confirm_edit_review_reply_action(
        &mut self,
        action: &ConfirmEditReviewReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_edit_review_reply(action.id, window, cx);
    }

    pub(super) fn cancel_edit_review_reply_action(
        &mut self,
        action: &CancelEditReviewReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_edit_review_reply(action.id, window, cx);
    }

    pub(super) fn delete_review_reply(
        &mut self,
        reply_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunk_key = self.hunk_key_for_review_reply(reply_id);
        self.remove_review_reply(reply_id, cx);

        if let Some(hunk_key) = hunk_key {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }

    /// Handles the DeleteReviewComment action - removes a comment.
    pub(super) fn delete_review_comment(
        &mut self,
        action: &DeleteReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_id = action.id;
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        self.remove_review_comment(action.id, cx);

        if let Some(hunk_key) = hunk_key {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            let overlay_index = self
                .diff_review_overlays
                .iter()
                .position(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot));
            let should_remove_overlay = overlay_index.is_some_and(|index| {
                self.hunk_comment_count(&hunk_key, &snapshot) == 0
                    && !self.diff_review_overlays[index].prompt_visible
            });
            if should_remove_overlay {
                if let Some(index) = overlay_index {
                    let overlay = self.diff_review_overlays.remove(index);
                    self.remove_blocks(HashSet::from_iter([overlay.block_id]), None, cx);
                    cx.notify();
                }
            } else {
                self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
            }
        }
    }

    fn set_review_comment_resolved(&mut self, id: usize, resolved: bool, cx: &mut Context<Self>) {
        let timestamp = if resolved {
            current_review_comment_timestamp()
        } else {
            None
        };
        if let Some(comment) = self
            .orphaned_review_comments
            .iter_mut()
            .find(|comment| comment.id == id)
        {
            comment.resolved_on = timestamp;
        } else if let Some(comment) = self
            .stored_review_comments
            .iter_mut()
            .flat_map(|(_, comments)| comments)
            .find(|comment| comment.id == id)
        {
            comment.resolved_on = timestamp;
            if !resolved {
                comment.show_resolved = false;
            }
        } else {
            return;
        }
        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.total_review_comment_count(),
            persist: true,
        });
        cx.notify();
    }

    pub(super) fn resolve_review_comment(
        &mut self,
        action: &ResolveReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunk_key = self.hunk_key_for_review_comment(action.id);
        self.set_review_comment_resolved(action.id, true, cx);
        if let Some(hunk_key) = hunk_key {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }

    pub(super) fn unresolve_review_comment(
        &mut self,
        action: &UnresolveReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunk_key = self.hunk_key_for_review_comment(action.id);
        self.set_review_comment_resolved(action.id, false, cx);
        if let Some(hunk_key) = hunk_key {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }

    pub(super) fn toggle_show_resolved_review_comment(
        &mut self,
        action: &ToggleResolvedReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunk_key = self.hunk_key_for_review_comment(action.id);
        if let Some(comment) = self
            .stored_review_comments
            .iter_mut()
            .flat_map(|(_, comments)| comments)
            .find(|comment| comment.id == action.id && comment.resolved_on.is_some())
        {
            comment.show_resolved = !comment.show_resolved;
            cx.notify();
            if let Some(hunk_key) = hunk_key {
                self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
            }
        }
    }

    pub(super) fn resolve_current_review_comment(
        &mut self,
        _: &ResolveThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.current_review_comment_id(cx) {
            self.resolve_review_comment(&ResolveReviewComment { id }, window, cx);
        }
    }

    pub(super) fn unresolve_current_review_comment(
        &mut self,
        _: &UnresolveThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.current_review_comment_id(cx) {
            self.unresolve_review_comment(&UnresolveReviewComment { id }, window, cx);
        }
    }

    pub(super) fn toggle_resolved_current_review_comment(
        &mut self,
        _: &ToggleResolvedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.current_review_comment_id(cx) {
            self.toggle_show_resolved_review_comment(
                &ToggleResolvedReviewComment { id },
                window,
                cx,
            );
        }
    }

    pub(super) fn edit_review_reply(
        &mut self,
        reply_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_reply_editing(reply_id, true, cx);

        let hunk_key = self.hunk_key_for_review_reply(reply_id);
        let reply_text = self
            .stored_review_comments
            .iter()
            .flat_map(|(_, comments)| comments)
            .flat_map(|comment| &comment.replies)
            .find(|reply| reply.id == reply_id)
            .map(|reply| reply.comment.clone())
            .unwrap_or_default();

        let completion_project = self.project.as_ref().map(Entity::downgrade);
        let snapshot = self.buffer.read(cx).snapshot(cx);
        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    overlay.inline_reply_edit_editors.entry(reply_id)
                {
                    let parent_editor = cx.entity().downgrade();
                    let inline_editor = cx.new(|cx| {
                        let mut editor = Self::review_comment_input_editor(window, cx);
                        editor.set_text(&*reply_text, window, cx);
                        editor.select_all(&crate::actions::SelectAll, window, cx);
                        editor.diff_review_editor_cancel = Some(
                            DiffReviewEditorCancel::reply_edit(parent_editor.clone(), reply_id),
                        );
                        editor
                    });
                    Self::attach_review_comment_completion_provider(
                        completion_project.clone(),
                        &inline_editor,
                        cx,
                    );
                    Self::install_review_comment_insert_mode_handler(&inline_editor, window, cx);

                    let enter_subscription =
                        Self::review_comment_enter_subscription(&inline_editor, cx, {
                            let parent_editor = parent_editor.clone();
                            move |window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.confirm_edit_review_reply(reply_id, window, cx);
                                    });
                                }
                            }
                        });
                    let subscription = inline_editor.update(cx, |inline_editor, _cx| {
                        let confirm_subscription = inline_editor.register_action({
                            let parent_editor = parent_editor.clone();
                            move |_: &menu::Confirm, window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.confirm_edit_review_reply(reply_id, window, cx);
                                    });
                                }
                            }
                        });
                        let cancel_subscription = inline_editor.register_action({
                            let parent_editor = parent_editor.clone();
                            move |_: &crate::actions::Cancel, window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.cancel_edit_review_reply(reply_id, window, cx);
                                    });
                                }
                            }
                        });
                        Subscription::join(
                            Subscription::join(confirm_subscription, cancel_subscription),
                            enter_subscription,
                        )
                    });

                    overlay
                        .inline_reply_edit_subscriptions
                        .insert(reply_id, subscription);
                    Self::focus_review_comment_editor(&inline_editor, window, cx);
                    entry.insert(inline_editor);
                }
            }

            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
            cx.notify();
        }
    }

    pub(super) fn copy_permalink_to_line(
        &mut self,
        _: &CopyPermalinkToLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let permalink_task = self.get_permalink_to_line(cx);
        let workspace = self.workspace();

        cx.spawn_in(window, async move |_, cx| match permalink_task.await {
            Ok(permalink) => {
                cx.update(|_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(permalink.to_string()));
                })
                .ok();
            }
            Err(err) => {
                let message = format!("Failed to copy permalink: {err}");

                anyhow::Result::<()>::Err(err).log_err();

                if let Some(workspace) = workspace {
                    workspace
                        .update_in(cx, |workspace, _, cx| {
                            struct CopyPermalinkToLine;

                            workspace.show_toast(
                                Toast::new(
                                    NotificationId::unique::<CopyPermalinkToLine>(),
                                    message,
                                ),
                                cx,
                            )
                        })
                        .ok();
                }
            }
        })
        .detach();
    }

    pub(super) fn open_permalink_to_line(
        &mut self,
        _: &OpenPermalinkToLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let permalink_task = self.get_permalink_to_line(cx);
        let workspace = self.workspace();

        cx.spawn_in(window, async move |_, cx| match permalink_task.await {
            Ok(permalink) => {
                cx.update(|_, cx| {
                    cx.open_url(permalink.as_ref());
                })
                .ok();
            }
            Err(err) => {
                let message = format!("Failed to open permalink: {err}");

                anyhow::Result::<()>::Err(err).log_err();

                if let Some(workspace) = workspace {
                    workspace.update(cx, |workspace, cx| {
                        struct OpenPermalinkToLine;

                        workspace.show_toast(
                            Toast::new(NotificationId::unique::<OpenPermalinkToLine>(), message),
                            cx,
                        )
                    });
                }
            }
        })
        .detach();
    }

    pub(super) fn toggle_staged_selected_diff_hunks(
        &mut self,
        _: &::git::ToggleStaged,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        let task = self.save_buffers_for_ranges_if_needed(&ranges, cx);
        cx.spawn_in(window, async move |this, cx| {
            task.await?;
            this.update_in(cx, |this, window, cx| {
                let snapshot = this.buffer.read(cx).snapshot(cx);
                let hunks = this.diff_hunks_in_ranges(&ranges, &snapshot).collect();
                this.apply_toggle(hunks, window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    pub(super) fn stage_and_next(
        &mut self,
        _: &::git::StageAndNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_stage_or_unstage_and_next(true, window, cx);
    }

    pub(super) fn unstage_and_next(
        &mut self,
        _: &::git::UnstageAndNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.do_stage_or_unstage_and_next(false, window, cx);
    }

    pub fn apply_toggle(
        &mut self,
        hunks: Vec<MultiBufferDiffHunk>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut hunks = self.resolve_diff_hunks(hunks, cx);
        if self.diff_hunk_delegate.is_none() {
            hunks.retain(|hunks| hunks.diff.read(cx).is_stageable());
        }
        if hunks.is_empty() {
            return;
        }
        let delegate = self.diff_hunk_delegate();
        delegate.toggle(hunks, self, window, cx);
    }

    pub fn apply_stage_or_unstage(
        &mut self,
        stage: bool,
        hunks: Vec<MultiBufferDiffHunk>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut hunks = self.resolve_diff_hunks(hunks, cx);
        if self.diff_hunk_delegate.is_none() {
            hunks.retain(|hunks| hunks.diff.read(cx).is_stageable());
        }
        if hunks.is_empty() {
            return;
        }
        let delegate = self.diff_hunk_delegate();
        delegate.stage_or_unstage(stage, hunks, self, window, cx);
    }

    pub fn apply_restore(
        &mut self,
        hunks: Vec<MultiBufferDiffHunk>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut hunks = self.resolve_diff_hunks(hunks, cx);
        if self.diff_hunk_delegate.is_none() {
            hunks.retain(|hunks| hunks.diff.read(cx).is_stageable());
        }
        if hunks.is_empty() {
            return;
        }
        let delegate = self.diff_hunk_delegate();
        delegate.restore(hunks, self, window, cx);
    }

    pub(super) fn clear_expanded_diff_hunks(&mut self, cx: &mut Context<Self>) -> bool {
        self.buffer.update(cx, |buffer, cx| {
            let ranges = vec![Anchor::Min..Anchor::Max];
            if !buffer.all_diff_hunks_expanded()
                && buffer.has_expanded_diff_hunks_in_ranges(&ranges, cx)
            {
                buffer.collapse_diff_hunks(ranges, cx);
                true
            } else {
                false
            }
        })
    }

    pub(super) fn has_any_expanded_diff_hunks(&self, cx: &App) -> bool {
        if self.buffer.read(cx).all_diff_hunks_expanded() {
            return true;
        }
        let ranges = vec![Anchor::Min..Anchor::Max];
        self.buffer
            .read(cx)
            .has_expanded_diff_hunks_in_ranges(&ranges, cx)
    }

    pub(super) fn toggle_single_diff_hunk(&mut self, range: Range<Anchor>, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.toggle_single_diff_hunk(range, cx);
        })
    }

    pub(super) fn apply_all_diff_hunks(
        &mut self,
        _: &ApplyAllDiffHunks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }

        let buffers = self.buffer.read(cx).all_buffers();
        for branch_buffer in buffers {
            branch_buffer.update(cx, |branch_buffer, cx| {
                branch_buffer.merge_into_base(Vec::new(), cx);
            });
        }

        if let Some(project) = self.project.clone() {
            self.save(
                SaveOptions {
                    format: true,
                    force_format: false,
                    autosave: false,
                },
                project,
                window,
                cx,
            )
            .detach_and_log_err(cx);
        }
    }

    pub(super) fn apply_selected_diff_hunks(
        &mut self,
        _: &ApplyDiffHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let snapshot = self.snapshot(window, cx);
        let hunks = snapshot.hunks_for_ranges(
            self.selections
                .all(&snapshot.display_snapshot)
                .into_iter()
                .map(|selection| selection.range()),
        );
        let mut ranges_by_buffer = HashMap::default();
        self.transact(window, cx, |editor, _window, cx| {
            for hunk in hunks {
                if let Some(buffer) = editor.buffer.read(cx).buffer(hunk.buffer_id) {
                    ranges_by_buffer
                        .entry(buffer.clone())
                        .or_insert_with(Vec::new)
                        .push(hunk.buffer_range.to_offset(buffer.read(cx)));
                }
            }

            for (buffer, ranges) in ranges_by_buffer {
                buffer.update(cx, |buffer, cx| {
                    buffer.merge_into_base(ranges, cx);
                });
            }
        });

        if let Some(project) = self.project.clone() {
            self.save(
                SaveOptions {
                    format: true,
                    force_format: false,
                    autosave: false,
                },
                project,
                window,
                cx,
            )
            .detach_and_log_err(cx);
        }
    }

    pub(super) fn open_git_blame_commit(
        &mut self,
        _: &OpenGitBlameCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_git_blame_commit_internal(window, cx);
    }

    pub(super) fn toggle_git_blame_inline_internal(
        &mut self,
        user_triggered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git_blame_inline_enabled {
            self.git_blame_inline_enabled = false;
            self.show_git_blame_inline = false;
            self.show_git_blame_inline_delay_task.take();
        } else {
            self.git_blame_inline_enabled = true;
            self.start_git_blame_inline(user_triggered, window, cx);
        }

        cx.notify();
    }

    pub(super) fn start_git_blame_inline(
        &mut self,
        user_triggered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_git_blame(user_triggered, window, cx);

        if ProjectSettings::get_global(cx)
            .git
            .inline_blame_delay()
            .is_some()
        {
            self.start_inline_blame_timer(window, cx);
        } else {
            self.show_git_blame_inline = true
        }
    }

    pub(super) fn render_git_blame_gutter(&self, cx: &App) -> bool {
        !self.mode().is_minimap() && self.show_git_blame_gutter && self.has_blame_entries(cx)
    }

    pub(super) fn render_git_blame_inline(&self, window: &Window, cx: &App) -> bool {
        ProjectSettings::get_global(cx).git.inline_blame.location
            == project::project_settings::InlineBlameLocation::Inline
            && self.show_git_blame_inline
            && (self.focus_handle.is_focused(window) || self.inline_blame_popover.is_some())
            && !self.newest_selection_head_on_empty_line(cx)
            && self.has_blame_entries(cx)
    }

    pub(super) fn start_inline_blame_timer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(delay) = ProjectSettings::get_global(cx).git.inline_blame_delay() {
            self.show_git_blame_inline = false;

            self.show_git_blame_inline_delay_task =
                Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(delay).await;

                    this.update(cx, |this, cx| {
                        this.show_git_blame_inline = true;
                        cx.notify();
                    })
                    .log_err();
                }));
        }
    }

    pub(super) fn show_blame_popover(
        &mut self,
        buffer: BufferId,
        blame_entry: &BlameEntry,
        position: gpui::Point<Pixels>,
        ignore_timeout: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &mut self.inline_blame_popover {
            state.hide_task.take();
        } else {
            let blame_popover_delay = EditorSettings::get_global(cx).hover_popover_delay.0;
            let blame_entry = blame_entry.clone();
            let show_task = cx.spawn(async move |editor, cx| {
                if !ignore_timeout {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(blame_popover_delay))
                        .await;
                }
                editor
                    .update(cx, |editor, cx| {
                        editor.inline_blame_popover_show_task.take();
                        let Some(blame) = editor.blame.as_ref() else {
                            return;
                        };
                        let blame = blame.read(cx);
                        let details = blame.details_for_entry(buffer, &blame_entry);
                        let markdown = cx.new(|cx| {
                            Markdown::new(
                                details
                                    .as_ref()
                                    .map(|message| message.message.clone())
                                    .unwrap_or_default(),
                                None,
                                None,
                                cx,
                            )
                        });
                        editor.inline_blame_popover = Some(InlineBlamePopover {
                            position,
                            hide_task: None,
                            popover_bounds: None,
                            popover_state: InlineBlamePopoverState {
                                scroll_handle: ScrollHandle::new(),
                                commit_message: details,
                                markdown,
                            },
                            keyboard_grace: ignore_timeout,
                        });
                        cx.notify();
                    })
                    .ok();
            });
            self.inline_blame_popover_show_task = Some(show_task);
        }
    }

    pub(super) fn go_to_prev_hunk(
        &mut self,
        _: &GoToPreviousHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let selection = self.selections.newest::<Point>(&snapshot.display_snapshot);
        self.go_to_hunk_before_or_after_position(
            &snapshot,
            selection.head(),
            Direction::Prev,
            true,
            window,
            cx,
        );
    }

    pub(super) fn calculate_overlay_height(
        &self,
        hunk_key: &DiffHunkKey,
        comments_expanded: bool,
        prompt_visible: bool,
        snapshot: &MultiBufferSnapshot,
        cx: &App,
    ) -> u32 {
        fn text_height(text: &str) -> u32 {
            const ESTIMATED_WRAP_COLUMNS: usize = 48;

            text.lines()
                .map(|line| {
                    let character_count = line.chars().count().max(1);
                    character_count.div_ceil(ESTIMATED_WRAP_COLUMNS) as u32
                })
                .sum::<u32>()
                .clamp(1, 12)
                + 1
        }

        fn editor_height(editor: &Entity<Editor>, cx: &App) -> u32 {
            text_height(&editor.read(cx).text(cx))
        }

        let comments = self.comments_for_hunk(hunk_key, snapshot);
        let comment_count = comments.len();
        let overlay = self
            .diff_review_overlays
            .iter()
            .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, snapshot));
        let prompt_height: u32 = if prompt_visible {
            overlay
                .map(|overlay| editor_height(&overlay.prompt_editor, cx))
                .unwrap_or(2)
        } else {
            0
        };

        if comment_count == 0 {
            prompt_height
        } else if comments_expanded {
            let comment_height = comments
                .iter()
                .map(|comment| {
                    overlay
                        .and_then(|overlay| overlay.inline_edit_editors.get(&comment.id))
                        .map(|editor| editor_height(editor, cx))
                        .unwrap_or_else(|| text_height(&comment.comment))
                        + comment
                            .replies
                            .iter()
                            .map(|reply| {
                                overlay
                                    .and_then(|overlay| {
                                        overlay.inline_reply_edit_editors.get(&reply.id)
                                    })
                                    .map(|editor| editor_height(editor, cx))
                                    .unwrap_or_else(|| text_height(&reply.comment))
                            })
                            .sum::<u32>()
                })
                .sum::<u32>();
            let reply_editor_height = overlay
                .map(|overlay| {
                    overlay
                        .reply_editors
                        .values()
                        .map(|editor| editor_height(editor, cx))
                        .sum::<u32>()
                })
                .unwrap_or(0);
            prompt_height + 1 + comment_height + reply_editor_height
        } else {
            prompt_height + 1
        }
    }

    pub fn stage_or_unstage_diff_hunks(
        &mut self,
        stage: bool,
        ranges: Vec<Range<Anchor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = self.save_buffers_for_ranges_if_needed(&ranges, cx);
        cx.spawn_in(window, async move |this, cx| {
            task.await?;
            this.update_in(cx, |this, window, cx| {
                let snapshot = this.buffer.read(cx).snapshot(cx);
                let hunks = this.diff_hunks_in_ranges(&ranges, &snapshot).collect();
                this.apply_stage_or_unstage(stage, hunks, window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn restore_diff_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let hunks = self.diff_hunks_in_ranges(&ranges, &snapshot).collect();
        self.apply_restore(hunks, window, cx);
    }

    fn toggle_diff_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        cx: &mut Context<Editor>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            let expand = !buffer.has_expanded_diff_hunks_in_ranges(&ranges, cx);
            buffer.expand_or_collapse_diff_hunks(ranges, expand, cx);
        })
    }

    pub fn set_blame(
        &mut self,
        blame: Entity<GitBlame>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.blame_subscription = Some(cx.observe_in(&blame, window, |_, _, _, cx| cx.notify()));
        self.blame = Some(blame);
        self.show_git_blame_gutter = true;
        cx.notify();
    }

    fn start_git_blame(
        &mut self,
        user_triggered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .blame
            .as_ref()
            .is_some_and(|blame| blame.read(cx).is_static())
        {
            return;
        }
        if let Some(project) = self.project() {
            if let Some(buffer) = self.buffer().read(cx).as_singleton()
                && buffer.read(cx).file().is_none_or(|file| {
                    matches!(file.disk_state(), language::DiskState::Historic { .. })
                })
            {
                return;
            }

            let focused = self.focus_handle(cx).contains_focused(window, cx);

            let project = project.clone();
            let blame = cx
                .new(|cx| GitBlame::new(self.buffer.clone(), project, user_triggered, focused, cx));
            self.blame_subscription =
                Some(cx.observe_in(&blame, window, |_, _, _, cx| cx.notify()));
            self.blame = Some(blame);
        }
    }

    fn restore_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Point>>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let hunks = self.snapshot(window, cx).hunks_for_ranges(ranges);
        self.apply_restore(hunks, window, cx);
    }

    fn save_buffers_for_ranges_if_needed(
        &mut self,
        ranges: &[Range<Anchor>],
        cx: &mut Context<Editor>,
    ) -> Task<Result<()>> {
        let multibuffer = self.buffer.read(cx);
        let snapshot = multibuffer.read(cx);
        let buffer_ids: HashSet<_> = ranges
            .iter()
            .flat_map(|range| snapshot.buffer_ids_for_range(range.clone()))
            .collect();
        drop(snapshot);

        let mut buffers = HashSet::default();
        for buffer_id in buffer_ids {
            if let Some(buffer_entity) = multibuffer.buffer(buffer_id) {
                let buffer = buffer_entity.read(cx);
                if buffer.file().is_some_and(|file| file.disk_state().exists()) && buffer.is_dirty()
                {
                    buffers.insert(buffer_entity);
                }
            }
        }

        if let Some(project) = &self.project {
            project.update(cx, |project, cx| project.save_buffers(buffers, cx))
        } else {
            Task::ready(Ok(()))
        }
    }

    fn do_stage_or_unstage_and_next(
        &mut self,
        stage: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges = self.selections.disjoint_anchor_ranges().collect::<Vec<_>>();

        if ranges.iter().any(|range| range.start != range.end) {
            self.stage_or_unstage_diff_hunks(stage, ranges, window, cx);
            return;
        }

        self.stage_or_unstage_diff_hunks(stage, ranges, window, cx);

        let all_diff_hunks_expanded = self.buffer().read(cx).all_diff_hunks_expanded();
        let wrap_around = !all_diff_hunks_expanded;
        let snapshot = self.snapshot(window, cx);
        let position = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();

        self.go_to_hunk_before_or_after_position(
            &snapshot,
            position,
            Direction::Next,
            wrap_around,
            window,
            cx,
        );
    }

    fn open_git_blame_commit_internal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let (blame_entry, repo) = self.blame_entry_at_cursor(window, cx)?;
        let renderer = cx.global::<GlobalBlameRenderer>().0.clone();
        let workspace = self.workspace()?.downgrade();
        renderer.open_blame_commit(blame_entry, repo, workspace, window, cx);
        None
    }

    fn blame_entry_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(BlameEntry, Entity<Repository>)> {
        let blame = self.blame.clone()?;
        let snapshot = self.snapshot(window, cx);
        let cursor = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();
        let (buffer, point) = snapshot.buffer_snapshot().point_to_buffer_point(cursor)?;
        let (_, blame_entry) = blame
            .update(cx, |blame, cx| {
                blame
                    .blame_for_rows(
                        &[RowInfo {
                            buffer_id: Some(buffer.remote_id()),
                            buffer_row: Some(point.row),
                            ..Default::default()
                        }],
                        cx,
                    )
                    .next()
            })
            .flatten()?;
        let repository = blame.read(cx).repository(cx, buffer.remote_id())?;
        Some((blame_entry, repository))
    }

    pub(crate) fn blame_revision_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(RepoPath, Oid, Entity<Repository>)> {
        let (blame_entry, repository) = self.blame_entry_at_cursor(window, cx)?;
        let highlighted_sha = self
            .blame
            .as_ref()
            .and_then(|blame| blame.read(cx).highlighted_sha());
        let (revision, path) = blame_entry.revision_target(highlighted_sha)?;
        Some((path, revision, repository))
    }

    pub(crate) fn blame_previous_revision_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(RepoPath, Oid, Entity<Repository>)> {
        let (blame_entry, repository) = self.blame_entry_at_cursor(window, cx)?;
        let (revision, path) = blame_entry.previous_revision_target()?;
        Some((path, revision, repository))
    }

    pub(super) fn blame_revision(
        &mut self,
        _: &BlameRevision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, revision, repository)) = self.blame_revision_target(window, cx) else {
            return;
        };
        self.open_blame_revision(path, revision, repository, window, cx);
    }

    pub(super) fn blame_previous_revision(
        &mut self,
        _: &BlamePreviousRevision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, revision, repository)) = self.blame_previous_revision_target(window, cx)
        else {
            return;
        };
        self.open_blame_revision(path, revision, repository, window, cx);
    }

    fn open_blame_revision(
        &mut self,
        path: RepoPath,
        revision: Oid,
        repository: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let renderer = cx.global::<GlobalBlameRenderer>().0.clone();
        renderer.open_blame_revision(
            path,
            revision,
            repository,
            workspace.downgrade(),
            window,
            cx,
        );
    }

    fn has_blame_entries(&self, cx: &App) -> bool {
        self.blame()
            .is_some_and(|blame| blame.read(cx).has_generated_entries())
    }

    fn newest_selection_head_on_empty_line(&self, cx: &App) -> bool {
        let cursor_anchor = self.selections.newest_anchor().head();

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let buffer_row = MultiBufferRow(cursor_anchor.to_point(&snapshot).row);

        snapshot.line_len(buffer_row) == 0
    }
    fn hunk_after_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        wrap_around: bool,
    ) -> Option<MultiBufferDiffHunk> {
        let result = snapshot
            .buffer_snapshot()
            .diff_hunks_in_range(position..snapshot.buffer_snapshot().max_point())
            .find(|hunk| hunk.row_range.start.0 > position.row);

        if wrap_around {
            result.or_else(|| {
                snapshot
                    .buffer_snapshot()
                    .diff_hunks_in_range(Point::zero()..position)
                    .find(|hunk| hunk.row_range.end.0 < position.row)
            })
        } else {
            result
        }
    }

    fn hunk_before_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        wrap_around: bool,
    ) -> Option<MultiBufferRow> {
        let result = snapshot.buffer_snapshot().diff_hunk_before(position);

        if wrap_around {
            result.or_else(|| snapshot.buffer_snapshot().diff_hunk_before(Point::MAX))
        } else {
            result
        }
    }

    /// Dismisses overlays that have no comments stored for their hunks.
    /// Keeps overlays that have at least one comment.
    fn dismiss_overlays_without_comments(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);

        let overlays_to_keep: Vec<bool> = self
            .diff_review_overlays
            .iter()
            .map(|overlay| {
                self.hunk_comment_count(&overlay.hunk_key, &snapshot) > 0
                    || (overlay.prompt_visible
                        && !overlay.prompt_editor.read(cx).text(cx).trim().is_empty())
            })
            .collect();

        let mut block_ids_to_remove = HashSet::default();
        let mut index = 0;
        self.diff_review_overlays.retain(|overlay| {
            let should_keep = overlays_to_keep[index];
            index += 1;
            if !should_keep {
                block_ids_to_remove.insert(overlay.block_id);
            }
            should_keep
        });

        if !block_ids_to_remove.is_empty() {
            self.remove_blocks(block_ids_to_remove, None, cx);
            cx.notify();
        }
    }

    /// Refreshes the diff review overlay block to update its height and render function.
    /// Uses resize_blocks and replace_blocks to avoid visual flicker from remove+insert.
    fn refresh_diff_review_overlay_height(
        &mut self,
        hunk_key: &DiffHunkKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_review_comment_body_editors(hunk_key, window, cx);

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let (comments_expanded, prompt_visible, block_id, prompt_editor) = {
            let Some(overlay) = self
                .diff_review_overlays
                .iter()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
            else {
                return;
            };

            (
                overlay.comments_expanded,
                overlay.prompt_visible,
                overlay.block_id,
                overlay.prompt_editor.clone(),
            )
        };

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let new_height = self.calculate_overlay_height(
            hunk_key,
            comments_expanded,
            prompt_visible,
            &snapshot,
            cx,
        );

        let mut heights = HashMap::default();
        heights.insert(block_id, new_height);
        self.resize_blocks(heights, None, cx);

        let hunk_key_for_render = hunk_key.clone();
        let editor_handle = cx.entity().downgrade();
        let render: Arc<dyn Fn(&mut BlockContext) -> AnyElement + Send + Sync> =
            Arc::new(move |cx| {
                Self::render_diff_review_overlay(
                    &prompt_editor,
                    &hunk_key_for_render,
                    &editor_handle,
                    cx,
                )
            });

        let mut renderers = HashMap::default();
        renderers.insert(block_id, render);
        self.replace_blocks(renderers, None, cx);
    }

    /// Compares two DiffHunkKeys for equality by resolving their anchors.
    fn hunk_keys_match(a: &DiffHunkKey, b: &DiffHunkKey, snapshot: &MultiBufferSnapshot) -> bool {
        a.file_path == b.file_path
            && a.hunk_start_anchor.to_point(snapshot) == b.hunk_start_anchor.to_point(snapshot)
    }

    fn render_diff_review_overlay(
        prompt_editor: &Entity<Editor>,
        hunk_key: &DiffHunkKey,
        editor_handle: &WeakEntity<Editor>,
        cx: &mut BlockContext,
    ) -> AnyElement {
        fn format_line_ranges(ranges: &[(u32, u32)]) -> Option<String> {
            if ranges.is_empty() {
                return None;
            }
            let formatted: Vec<String> = ranges
                .iter()
                .map(|(start, end)| {
                    let start_line = start + 1;
                    let end_line = end + 1;
                    if start_line == end_line {
                        format!("Line {start_line}")
                    } else {
                        format!("Lines {start_line}-{end_line}")
                    }
                })
                .collect();
            // Don't show label for single line in single excerpt
            if ranges.len() == 1 && ranges[0].0 == ranges[0].1 {
                return None;
            }
            Some(formatted.join(" ⋯ "))
        }

        let theme = cx.theme();
        let colors = theme.colors();

        let (
            comments,
            comments_expanded,
            prompt_visible,
            inline_editors,
            inline_reply_editors,
            reply_editors,
            body_editors,
            reply_body_editors,
            user_avatar_uri,
            line_ranges,
        ) = editor_handle
            .upgrade()
            .map(|editor| {
                let editor = editor.read(cx);
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let comments = editor.comments_for_hunk(hunk_key, &snapshot);
                let (
                    expanded,
                    prompt_visible,
                    editors,
                    inline_reply_editors,
                    reply_editors,
                    body_editors,
                    reply_body_editors,
                    avatar_uri,
                    line_ranges,
                ) = editor
                    .diff_review_overlays
                    .iter()
                    .find(|overlay| Editor::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
                    .map(|o| {
                        let start_point = o.anchor_range.start.to_point(&snapshot);
                        let end_point = o.anchor_range.end.to_point(&snapshot);
                        let buffer_ranges = snapshot.range_to_buffer_ranges(start_point..end_point);
                        let ranges: Vec<(u32, u32)> = buffer_ranges
                            .iter()
                            .map(|(buffer_snapshot, range, _)| {
                                let start = buffer_snapshot.offset_to_point(range.start.0).row;
                                let end = buffer_snapshot.offset_to_point(range.end.0).row;
                                (start, end)
                            })
                            .collect();
                        (
                            o.comments_expanded,
                            o.prompt_visible,
                            o.inline_edit_editors.clone(),
                            o.inline_reply_edit_editors.clone(),
                            o.reply_editors.clone(),
                            o.body_editors.clone(),
                            o.reply_body_editors.clone(),
                            o.user_avatar_uri.clone(),
                            if ranges.is_empty() {
                                None
                            } else {
                                Some(ranges)
                            },
                        )
                    })
                    .unwrap_or((
                        true,
                        false,
                        HashMap::default(),
                        HashMap::default(),
                        HashMap::default(),
                        HashMap::default(),
                        HashMap::default(),
                        None,
                        None,
                    ));
                (
                    comments,
                    expanded,
                    prompt_visible,
                    editors,
                    inline_reply_editors,
                    reply_editors,
                    body_editors,
                    reply_body_editors,
                    avatar_uri,
                    line_ranges,
                )
            })
            .unwrap_or((
                Vec::new(),
                true,
                false,
                HashMap::default(),
                HashMap::default(),
                HashMap::default(),
                HashMap::default(),
                HashMap::default(),
                None,
                None,
            ));

        let comment_count = comments.len();
        let avatar_size = px(20.);
        let action_icon_size = IconSize::Custom(rems(20. / 16.));

        v_flex()
            .w_full()
            .min_w_0()
            .bg(colors.editor_background)
            .border_b_1()
            .border_color(colors.border)
            .px_2()
            .pb_2()
            .gap_2()
            // Line range indicator (only shown for multi-line selections or multiple excerpts)
            .when_some(line_ranges, |el, ranges| {
                let label = format_line_ranges(&ranges);
                if let Some(label) = label {
                    el.child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                } else {
                    el
                }
            })
            .when(prompt_visible, |el| {
                el.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .bg(colors.surface_background)
                        .on_mouse_down_out({
                            let editor_handle = editor_handle.clone();
                            let hunk_key = hunk_key.clone();
                            move |_, window, cx| {
                                if let Some(editor) = editor_handle.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.dismiss_empty_diff_review_prompt(
                                            &hunk_key, window, cx,
                                        );
                                    });
                                }
                            }
                        })
                        .child(
                            div()
                                .size(avatar_size)
                                .flex_shrink_0()
                                .rounded_full()
                                .overflow_hidden()
                                .child(if let Some(ref avatar_uri) = user_avatar_uri {
                                    Avatar::new(avatar_uri.clone())
                                        .size(avatar_size)
                                        .into_any_element()
                                } else {
                                    Icon::new(IconName::Person)
                                        .size(IconSize::Small)
                                        .color(ui::Color::Muted)
                                        .into_any_element()
                                }),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .border_1()
                                .border_color(colors.border)
                                .rounded_md()
                                .bg(colors.editor_background)
                                .px_2()
                                .py_1()
                                .child(prompt_editor.clone()),
                        )
                        .child(
                            h_flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(
                                    IconButton::new("diff-review-close", IconName::Close)
                                        .icon_color(ui::Color::Muted)
                                        .icon_size(action_icon_size)
                                        .size(ButtonSize::Medium)
                                        .tooltip(Tooltip::text("Close"))
                                        .on_click({
                                            let editor_handle = editor_handle.clone();
                                            let hunk_key = hunk_key.clone();
                                            move |_, window, cx| {
                                                if let Some(editor) = editor_handle.upgrade() {
                                                    editor.update(cx, |editor, cx| {
                                                        editor.dismiss_diff_review_prompt(
                                                            &hunk_key, window, cx,
                                                        );
                                                    });
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    IconButton::new("diff-review-add", IconName::Return)
                                        .icon_color(ui::Color::Muted)
                                        .icon_size(action_icon_size)
                                        .size(ButtonSize::Medium)
                                        .tooltip(Tooltip::text("Add comment"))
                                        .on_click({
                                            let editor_handle = editor_handle.clone();
                                            let hunk_key = hunk_key.clone();
                                            move |_, window, cx| {
                                                if let Some(editor) = editor_handle.upgrade() {
                                                    editor.update(cx, |editor, cx| {
                                                        editor.submit_diff_review_comment_for_hunk(
                                                            &hunk_key, window, cx,
                                                        );
                                                    });
                                                }
                                            }
                                        }),
                                ),
                        ),
                )
            })
            .when(comment_count > 0, |el| {
                el.child(Self::render_comments_section(
                    comments,
                    hunk_key.clone(),
                    comments_expanded,
                    inline_editors,
                    inline_reply_editors,
                    reply_editors,
                    body_editors,
                    reply_body_editors,
                    user_avatar_uri,
                    editor_handle.clone(),
                    avatar_size,
                    action_icon_size,
                    colors,
                ))
            })
            .into_any_element()
    }

    fn render_comments_section(
        comments: Vec<StoredReviewComment>,
        hunk_key: DiffHunkKey,
        expanded: bool,
        inline_editors: HashMap<usize, Entity<Editor>>,
        inline_reply_editors: HashMap<usize, Entity<Editor>>,
        reply_editors: HashMap<usize, Entity<Editor>>,
        body_editors: HashMap<usize, Entity<Editor>>,
        reply_body_editors: HashMap<usize, Entity<Editor>>,
        user_avatar_uri: Option<SharedUri>,
        editor_handle: WeakEntity<Editor>,
        avatar_size: Pixels,
        action_icon_size: IconSize,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        let comment_count = comments.len();
        // Custom blocks render without a per-overlay element id, so a constant header id would
        // collide across overlays and GPUI would drop clicks on all but one. Key it to the
        // thread's first (stable) comment id so every header is independently clickable.
        let header_id = format!(
            "review-comments-header-{}",
            comments.first().map_or(0, |c| c.id)
        );

        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .child(
                h_flex()
                    .id(header_id)
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .rounded_md()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click({
                        let editor_handle = editor_handle.clone();
                        let hunk_key = hunk_key.clone();
                        move |_, window: &mut Window, cx| {
                            if let Some(editor) = editor_handle.upgrade() {
                                editor.update(cx, |editor, cx| {
                                    editor.toggle_review_comments_expanded_for_hunk(
                                        &hunk_key, window, cx,
                                    );
                                });
                            }
                        }
                    })
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::Small)
                        .color(ui::Color::Muted),
                    )
                    .child(
                        Label::new(format!(
                            "{} Comment{}",
                            comment_count,
                            if comment_count == 1 { "" } else { "s" }
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .when(expanded, |el| {
                el.children(comments.into_iter().map(|comment| {
                    let inline_editor = inline_editors.get(&comment.id).cloned();
                    let inline_reply_editors = inline_reply_editors.clone();
                    let reply_editor = reply_editors.get(&comment.id).cloned();
                    let body_editor = body_editors.get(&comment.id).cloned();
                    let reply_body_editors = reply_body_editors.clone();
                    Self::render_comment_row(
                        comment,
                        inline_editor,
                        inline_reply_editors,
                        reply_editor,
                        body_editor,
                        reply_body_editors,
                        user_avatar_uri.clone(),
                        editor_handle.clone(),
                        avatar_size,
                        action_icon_size,
                        colors,
                    )
                }))
            })
    }

    fn render_comment_row(
        comment: StoredReviewComment,
        inline_editor: Option<Entity<Editor>>,
        inline_reply_editors: HashMap<usize, Entity<Editor>>,
        reply_editor: Option<Entity<Editor>>,
        body_editor: Option<Entity<Editor>>,
        reply_body_editors: HashMap<usize, Entity<Editor>>,
        user_avatar_uri: Option<SharedUri>,
        editor_handle: WeakEntity<Editor>,
        avatar_size: Pixels,
        action_icon_size: IconSize,
        colors: &theme::ThemeColors,
    ) -> AnyElement {
        let comment_id = comment.id;
        let author = comment.author.clone();
        let created_at = comment.created_at.clone();
        let review_round = comment.review_round;
        let is_outdated = comment.outdated;
        let outdated_reason = comment.outdated_reason.clone();
        let is_editing = inline_editor.is_some();
        let is_resolved = comment.resolved_on.is_some();
        let show_resolved = comment.show_resolved;
        let reply_count = comment.replies.len();
        let cancel_editor_handle = editor_handle.clone();
        let confirm_editor_handle = editor_handle.clone();
        let edit_editor_handle = editor_handle.clone();
        let reply_editor_handle = editor_handle.clone();
        let copy_editor_handle = editor_handle.clone();
        let body_editor_handle = editor_handle.clone();
        let cancel_reply_editor_handle = editor_handle.clone();
        let submit_reply_editor_handle = editor_handle.clone();
        let delete_editor_handle = editor_handle.clone();
        let resolve_editor_handle = editor_handle.clone();
        let unresolve_editor_handle = editor_handle.clone();
        let toggle_resolved_editor_handle = editor_handle.clone();
        let reply_row_editor_handle = editor_handle;
        let review_comment_background = colors.element_background;
        let review_comment_border = colors.border_variant;

        if is_resolved && !show_resolved && !is_editing {
            let collapsed_preview = comment
                .comment
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            return v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(review_comment_border)
                        .bg(review_comment_background)
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .items_center()
                                .gap_1()
                                .child(Self::render_author_badge(
                                    author,
                                    user_avatar_uri.clone(),
                                    avatar_size,
                                    colors,
                                ))
                                .child(Self::render_resolved_badge(colors))
                                .when_some(created_at.clone(), |el, created_at| {
                                    el.child(Self::render_review_timestamp(created_at))
                                })
                                .when(reply_count > 0, |el| {
                                    el.child(
                                        Label::new(format!(
                                            "{reply_count} repl{}",
                                            if reply_count == 1 { "y" } else { "ies" }
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    )
                                })
                                .child(div().mx_1p5().w_px().h_4().bg(review_comment_border))
                                .child(
                                    IconButton::new(
                                        format!("diff-review-show-thread-{comment_id}"),
                                        IconName::Eye,
                                    )
                                    .icon_color(ui::Color::Info)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::None)
                                    .tooltip(Tooltip::text("Show thread"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) =
                                                toggle_resolved_editor_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.toggle_show_resolved_review_comment(
                                                        &ToggleResolvedReviewComment {
                                                            id: comment_id,
                                                        },
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                                .child(
                                    IconButton::new(
                                        format!("diff-review-unresolve-{comment_id}"),
                                        IconName::RotateCcw,
                                    )
                                    .icon_color(ui::Color::Success)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::None)
                                    .tooltip(Tooltip::text("Unresolve thread"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) = unresolve_editor_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.unresolve_review_comment(
                                                        &UnresolveReviewComment { id: comment_id },
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                ),
                        )
                        .child(
                            div().min_w_0().child(
                                Label::new(collapsed_preview)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        ),
                )
                .into_any_element();
        }

        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .relative()
                    .items_start()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(review_comment_border)
                    .bg(review_comment_background)
                    .child(if is_editing {
                        Self::render_author_badge(
                            author,
                            user_avatar_uri.clone(),
                            avatar_size,
                            colors,
                        )
                        .into_any_element()
                    } else {
                        h_flex()
                            .absolute()
                            .top_1()
                            .left_2()
                            .gap_1p5()
                            .items_center()
                            .child(Self::render_author_badge(
                                author,
                                user_avatar_uri.clone(),
                                avatar_size,
                                colors,
                            ))
                            .when_some(review_round, |el, round| {
                                el.child(Self::render_review_round_badge(round, colors))
                            })
                            .when_some(created_at.clone(), |el, created_at| {
                                el.child(Self::render_review_timestamp(created_at))
                            })
                            .when(is_resolved, |el| {
                                el.child(Self::render_resolved_badge(colors))
                            })
                            .when(is_outdated, |el| {
                                let reason = match outdated_reason.as_deref() {
                                    Some("file_not_in_diff") => "file no longer has a visible diff",
                                    Some("line_not_in_diff") => "line no longer has a visible diff",
                                    _ => "change no longer has a visible diff",
                                };
                                let mut badge = h_flex()
                                    .px_1p5()
                                    .h_5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border_variant)
                                    .bg(colors.element_background)
                                    .child(
                                        Label::new("Outdated")
                                            .size(LabelSize::Small)
                                            .color(Color::Warning),
                                    );
                                gpui::InteractiveElement::interactivity(&mut badge)
                                    .tooltip(Tooltip::text(reason));
                                el.child(badge)
                            })
                            .child(div().mx_1p5().w_px().h_4().bg(review_comment_border))
                            .child(
                                IconButton::new(
                                    format!("diff-review-copy-reference-{comment_id}"),
                                    IconName::Copy,
                                )
                                .icon_color(ui::Color::Info)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Copy comment reference"))
                                .on_click(move |_, _window, cx| {
                                    if let Some(editor) = copy_editor_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor.copy_review_comment_reference(
                                                comment_id, false, cx,
                                            );
                                        });
                                    }
                                }),
                            )
                            .child(
                                IconButton::new(
                                    format!("diff-review-reply-{comment_id}"),
                                    IconName::ReplyArrowRight,
                                )
                                .icon_color(ui::Color::Default)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Reply"))
                                .on_click(move |_, window, cx| {
                                    if let Some(editor) = reply_editor_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor.reply_to_review_comment(
                                                &crate::actions::ReplyToReviewComment {
                                                    id: comment_id,
                                                },
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                            )
                            .child(
                                IconButton::new(
                                    format!("diff-review-edit-{comment_id}"),
                                    IconName::Pencil,
                                )
                                .icon_color(ui::Color::Modified)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Edit comment"))
                                .on_click(move |_, window, cx| {
                                    if let Some(editor) = edit_editor_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor.edit_review_comment(
                                                &crate::actions::EditReviewComment {
                                                    id: comment_id,
                                                },
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                            )
                            .child(
                                IconButton::new(
                                    format!("diff-review-delete-{comment_id}"),
                                    IconName::Trash,
                                )
                                .icon_color(ui::Color::Error)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Delete comment"))
                                .on_click(move |_, window, cx| {
                                    if let Some(editor) = delete_editor_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor.delete_review_comment(
                                                &DeleteReviewComment { id: comment_id },
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                            )
                            .when(!is_resolved, |el| {
                                el.child(
                                    IconButton::new(
                                        format!("diff-review-resolve-{comment_id}"),
                                        IconName::Check,
                                    )
                                    .icon_color(ui::Color::Success)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::None)
                                    .tooltip(Tooltip::text("Resolve thread"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) = resolve_editor_handle.upgrade() {
                                                editor.update(cx, |editor, cx| {
                                                    editor.resolve_review_comment(
                                                        &ResolveReviewComment { id: comment_id },
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                            })
                            .when(is_resolved, |el| {
                                el.child(
                                    IconButton::new(
                                        format!("diff-review-hide-thread-{comment_id}"),
                                        IconName::EyeOff,
                                    )
                                    .icon_color(ui::Color::Info)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::None)
                                    .tooltip(Tooltip::text("Collapse resolved thread"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) =
                                                toggle_resolved_editor_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.toggle_show_resolved_review_comment(
                                                        &ToggleResolvedReviewComment {
                                                            id: comment_id,
                                                        },
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                                .child(
                                    IconButton::new(
                                        format!("diff-review-unresolve-expanded-{comment_id}"),
                                        IconName::RotateCcw,
                                    )
                                    .icon_color(ui::Color::Success)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::None)
                                    .tooltip(Tooltip::text("Unresolve thread"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) = unresolve_editor_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.unresolve_review_comment(
                                                        &UnresolveReviewComment { id: comment_id },
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                            })
                            .into_any_element()
                    })
                    .child(if let Some(editor) = inline_editor {
                        div()
                            .min_w_0()
                            .flex_1()
                            .border_1()
                            .border_color(colors.border)
                            .rounded_md()
                            .bg(colors.editor_background)
                            .px_2()
                            .py_1()
                            .child(editor)
                            .into_any_element()
                    } else {
                        div()
                            .min_w_0()
                            .flex_1()
                            .pt(px(28.))
                            .child(if let Some(body_editor) = body_editor {
                                body_editor.into_any_element()
                            } else {
                                Self::render_review_comment_body(
                                    comment.comment,
                                    body_editor_handle,
                                    colors,
                                )
                            })
                            .into_any_element()
                    })
                    .when(is_editing, |row| {
                        row.child(
                            h_flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(
                                    IconButton::new(
                                        format!("diff-review-cancel-edit-{comment_id}"),
                                        IconName::Close,
                                    )
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::Medium)
                                    .tooltip(Tooltip::text("Cancel"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) = cancel_editor_handle.upgrade() {
                                                editor.update(cx, |editor, cx| {
                                                    editor.cancel_edit_review_comment(
                                                        comment_id, window, cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                                .child(
                                    IconButton::new(
                                        format!("diff-review-confirm-edit-{comment_id}"),
                                        IconName::Return,
                                    )
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::Medium)
                                    .tooltip(Tooltip::text("Confirm"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) = confirm_editor_handle.upgrade() {
                                                editor.update(cx, |editor, cx| {
                                                    editor.confirm_edit_review_comment(
                                                        comment_id, window, cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                ),
                        )
                    }),
            )
            .children(comment.replies.into_iter().map(|reply| {
                let reply_id = reply.id;
                let reply_review_round = reply.review_round;
                let reply_created_at = reply.created_at.clone();
                let inline_reply_editor = inline_reply_editors.get(&reply_id).cloned();
                let is_editing_reply = inline_reply_editor.is_some();
                let reply_to_thread_handle = reply_row_editor_handle.clone();
                let copy_reply_handle = reply_row_editor_handle.clone();
                let edit_reply_handle = reply_row_editor_handle.clone();
                let delete_reply_handle = reply_row_editor_handle.clone();
                let cancel_reply_edit_handle = reply_row_editor_handle.clone();
                let confirm_reply_edit_handle = reply_row_editor_handle.clone();
                let reply_body_editor_handle = reply_row_editor_handle.clone();
                let reply_body_editor = reply_body_editors.get(&reply_id).cloned();
                h_flex()
                    .ml_4()
                    .min_w_0()
                    .relative()
                    .items_start()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(review_comment_border)
                    .bg(review_comment_background)
                    .child(if is_editing_reply {
                        Self::render_author_badge(
                            reply.author,
                            user_avatar_uri.clone(),
                            avatar_size,
                            colors,
                        )
                        .into_any_element()
                    } else {
                        h_flex()
                            .absolute()
                            .top_1()
                            .left_2()
                            .gap_1p5()
                            .items_center()
                            .child(Self::render_author_badge(
                                reply.author,
                                user_avatar_uri.clone(),
                                avatar_size,
                                colors,
                            ))
                            .when_some(reply_review_round, |el, round| {
                                el.child(Self::render_review_round_badge(round, colors))
                            })
                            .when_some(reply_created_at.clone(), |el, created_at| {
                                el.child(Self::render_review_timestamp(created_at))
                            })
                            .child(div().mx_1p5().w_px().h_4().bg(review_comment_border))
                            .child(
                                IconButton::new(
                                    format!("diff-review-copy-reply-reference-{reply_id}"),
                                    IconName::Copy,
                                )
                                .icon_color(ui::Color::Info)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Copy reply reference"))
                                .on_click(move |_, _window, cx| {
                                    if let Some(editor) = copy_reply_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor
                                                .copy_review_comment_reference(reply_id, true, cx);
                                        });
                                    }
                                }),
                            )
                            .child(
                                IconButton::new(
                                    format!("diff-review-reply-to-reply-{reply_id}"),
                                    IconName::ReplyArrowRight,
                                )
                                .icon_color(ui::Color::Default)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Reply"))
                                .on_click(move |_, window, cx| {
                                    if let Some(editor) = reply_to_thread_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor.reply_to_review_comment(
                                                &crate::actions::ReplyToReviewComment {
                                                    id: comment_id,
                                                },
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                            )
                            .child(
                                IconButton::new(
                                    format!("diff-review-edit-reply-{reply_id}"),
                                    IconName::Pencil,
                                )
                                .icon_color(ui::Color::Modified)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Edit reply"))
                                .on_click(move |_, window, cx| {
                                    if let Some(editor) = edit_reply_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor.edit_review_reply(reply_id, window, cx);
                                        });
                                    }
                                }),
                            )
                            .child(
                                IconButton::new(
                                    format!("diff-review-delete-reply-{reply_id}"),
                                    IconName::Trash,
                                )
                                .icon_color(ui::Color::Error)
                                .icon_size(action_icon_size)
                                .size(ButtonSize::None)
                                .tooltip(Tooltip::text("Delete reply"))
                                .on_click(move |_, window, cx| {
                                    if let Some(editor) = delete_reply_handle.upgrade() {
                                        editor.update(cx, |editor, cx| {
                                            editor
                                                .confirm_delete_review_reply(reply_id, window, cx);
                                        });
                                    }
                                }),
                            )
                            .into_any_element()
                    })
                    .child(if let Some(editor) = inline_reply_editor {
                        div()
                            .min_w_0()
                            .flex_1()
                            .border_1()
                            .border_color(colors.border)
                            .rounded_md()
                            .bg(colors.editor_background)
                            .px_2()
                            .py_1()
                            .child(editor)
                            .into_any_element()
                    } else {
                        div()
                            .min_w_0()
                            .flex_1()
                            .pt(px(28.))
                            .child(if let Some(reply_body_editor) = reply_body_editor {
                                reply_body_editor.into_any_element()
                            } else {
                                Self::render_review_comment_body(
                                    reply.comment,
                                    reply_body_editor_handle,
                                    colors,
                                )
                            })
                            .into_any_element()
                    })
                    .when(is_editing_reply, |row| {
                        row.child(
                            h_flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(
                                    IconButton::new(
                                        format!("diff-review-cancel-reply-edit-{reply_id}"),
                                        IconName::Close,
                                    )
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::Medium)
                                    .tooltip(Tooltip::text("Cancel"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) = cancel_reply_edit_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.cancel_edit_review_reply(
                                                        reply_id, window, cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                                .child(
                                    IconButton::new(
                                        format!("diff-review-confirm-reply-edit-{reply_id}"),
                                        IconName::Return,
                                    )
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::Medium)
                                    .tooltip(Tooltip::text("Confirm"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) =
                                                confirm_reply_edit_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.confirm_edit_review_reply(
                                                        reply_id, window, cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                ),
                        )
                    })
            }))
            .when_some(reply_editor, |el, reply_editor| {
                el.child(
                    h_flex()
                        .ml_4()
                        .min_w_0()
                        .items_start()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .bg(colors.surface_background)
                        .child(Self::render_author_badge(
                            DEFAULT_REVIEW_COMMENT_AUTHOR.to_string(),
                            user_avatar_uri.clone(),
                            avatar_size,
                            colors,
                        ))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .border_1()
                                .border_color(colors.border)
                                .rounded_md()
                                .bg(colors.editor_background)
                                .px_2()
                                .py_1()
                                .child(reply_editor),
                        )
                        .child(
                            h_flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(
                                    IconButton::new(
                                        format!("diff-review-cancel-reply-{comment_id}"),
                                        IconName::Close,
                                    )
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::Medium)
                                    .tooltip(Tooltip::text("Cancel"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) =
                                                cancel_reply_editor_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.cancel_review_reply(
                                                        comment_id, window, cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                                .child(
                                    IconButton::new(
                                        format!("diff-review-submit-reply-{comment_id}"),
                                        IconName::Return,
                                    )
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .size(ButtonSize::Medium)
                                    .tooltip(Tooltip::text("Add reply"))
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(editor) =
                                                submit_reply_editor_handle.upgrade()
                                            {
                                                editor.update(cx, |editor, cx| {
                                                    editor.submit_review_reply(
                                                        comment_id, window, cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_review_comment_body(
        comment: String,
        editor_handle: WeakEntity<Editor>,
        colors: &theme::ThemeColors,
    ) -> AnyElement {
        let text_color = colors.text;
        let border_color = colors.border_variant;
        let background = colors.element_background;

        v_flex()
            .w_full()
            .min_w_0()
            .gap_0p5()
            .children(parse_review_comment_body(&comment).into_iter().map(|line| {
                if let [ReviewCommentBodySegment::Text(text)] = line.as_slice() {
                    return div()
                        .w_full()
                        .min_w_0()
                        .text_sm()
                        .text_color(text_color)
                        .whitespace_normal()
                        .child(text.clone())
                        .into_any_element();
                }

                h_flex()
                    .w_full()
                    .min_w_0()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .children(line.into_iter().map(|segment| {
                        match segment {
                            ReviewCommentBodySegment::Text(text) => div()
                                .text_sm()
                                .text_color(text_color)
                                .whitespace_normal()
                                .child(text)
                                .into_any_element(),
                            ReviewCommentBodySegment::Mention(mention) => {
                                Self::render_review_mention(
                                    mention,
                                    editor_handle.clone(),
                                    border_color,
                                    background,
                                )
                            }
                        }
                    }))
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_review_mention(
        mention: ReviewCommentMention,
        editor_handle: WeakEntity<Editor>,
        border_color: Hsla,
        background: Hsla,
    ) -> AnyElement {
        let label = mention.label;
        let url = mention.url;
        let tooltip = mention.tooltip;
        let label = if mention.show_at_prefix {
            format!("@{label}")
        } else {
            label
        };

        let chip = h_flex()
            .id(format!("diff-review-mention-{url}"))
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h_5()
            .px_1p5()
            .rounded_md()
            .border_1()
            .border_color(border_color)
            .bg(background)
            .cursor_pointer()
            .child(
                Icon::new(mention.icon)
                    .size(IconSize::XSmall)
                    .color(ui::Color::Muted),
            )
            .child(Label::new(label).size(LabelSize::Small))
            .on_click(move |_, window, cx| {
                if let Some(editor) = editor_handle.upgrade() {
                    editor.update(cx, |editor, cx| {
                        editor.open_review_comment_mention(&url, window, cx);
                    });
                }
            });

        chip.when_some(tooltip, |this, tooltip| {
            this.tooltip(Tooltip::text(tooltip))
        })
        .into_any_element()
    }

    fn open_review_comment_mention(
        &mut self,
        mention_url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(url) = url::Url::parse(mention_url) else {
            return;
        };

        if url.scheme() != "file" {
            cx.open_url(mention_url);
            return;
        }

        let Some(workspace) = self.workspace() else {
            cx.open_url(mention_url);
            return;
        };

        workspace.update(cx, |workspace, cx| {
            let path_style = workspace.path_style(cx);
            let Ok(abs_path) = url.to_file_path_ext(path_style) else {
                cx.open_url(mention_url);
                return;
            };
            let point = parse_review_comment_mention_point(&url);
            let project = workspace.project();
            let item = if let Some(project_path) =
                project.update(cx, |project, cx| project.find_project_path(&abs_path, cx))
            {
                workspace.open_path(project_path, None, true, window, cx)
            } else if abs_path.exists() {
                workspace.open_abs_path(
                    abs_path,
                    OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            } else {
                cx.open_url(mention_url);
                return;
            };

            if let Some(point) = point {
                window
                    .spawn(cx, async move |cx| {
                        let Some(editor) = item.await?.downcast::<Editor>() else {
                            return anyhow::Ok(());
                        };
                        editor.update_in(cx, |editor, window, cx| {
                            editor.change_selections(
                                SelectionEffects::scroll(Autoscroll::center()),
                                window,
                                cx,
                                |selections| selections.select_ranges([point..point]),
                            );
                        })?;
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
            } else {
                item.detach_and_log_err(cx);
            }
        });
    }

    fn render_review_timestamp(created_at: String) -> impl IntoElement {
        let mut timestamp = div().child(
            Label::new(review_comment_timestamp_label(&created_at))
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
        gpui::InteractiveElement::interactivity(&mut timestamp).tooltip(Tooltip::text(created_at));
        timestamp
    }

    fn render_review_round_badge(round: u32, colors: &theme::ThemeColors) -> impl IntoElement {
        h_flex()
            .px_1p5()
            .h_5()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                Label::new(format!("Review #{round}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_resolved_badge(colors: &theme::ThemeColors) -> impl IntoElement {
        h_flex()
            .px_1p5()
            .h_5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(Color::Success),
            )
            .child(
                Label::new("Resolved")
                    .size(LabelSize::Small)
                    .color(Color::Success),
            )
    }

    fn render_author_badge(
        author: String,
        user_avatar_uri: Option<SharedUri>,
        avatar_size: Pixels,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        let is_amp = author.eq_ignore_ascii_case("amp");
        let is_vxio = author.eq_ignore_ascii_case("vxio");

        div()
            .min_w(px(36.))
            .h(avatar_size)
            .px_1p5()
            .flex()
            .items_center()
            .gap_1()
            .justify_center()
            .flex_shrink_0()
            .rounded_full()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .child(if is_amp {
                Label::new("🤖").size(LabelSize::XSmall).into_any_element()
            } else if is_vxio {
                Avatar::new(
                    user_avatar_uri
                        .unwrap_or_else(|| SharedUri::from("https://github.com/vxio.png?size=128")),
                )
                .size(avatar_size)
                .into_any_element()
            } else {
                Icon::new(IconName::Person)
                    .size(IconSize::XSmall)
                    .color(ui::Color::Muted)
                    .into_any_element()
            })
            .child(
                Label::new(author)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn get_permalink_to_line(&self, cx: &mut Context<Self>) -> Task<Result<url::Url>> {
        let buffer_and_selection = maybe!({
            let selection = self.selections.newest::<Point>(&self.display_snapshot(cx));
            let selection_range = selection.range();

            let multi_buffer = self.buffer().read(cx);
            let multi_buffer_snapshot = multi_buffer.snapshot(cx);
            let buffer_ranges = multi_buffer_snapshot
                .range_to_buffer_ranges(selection_range.start..selection_range.end);

            let (buffer_snapshot, range, _) = if selection.reversed {
                buffer_ranges.first()
            } else {
                buffer_ranges.last()
            }?;

            let buffer_range = range.to_point(buffer_snapshot);
            let buffer = multi_buffer.buffer(buffer_snapshot.remote_id())?;

            let Some(buffer_diff) = multi_buffer.diff_for(buffer_snapshot.remote_id()) else {
                return Some((buffer, buffer_range.start.row..buffer_range.end.row));
            };

            let buffer_diff_snapshot = buffer_diff.read(cx).snapshot(cx);
            let start = buffer_diff_snapshot
                .buffer_point_to_base_text_point(buffer_range.start, &buffer_snapshot);
            let end = buffer_diff_snapshot
                .buffer_point_to_base_text_point(buffer_range.end, &buffer_snapshot);

            Some((buffer, start.row..end.row))
        });

        let Some((buffer, selection)) = buffer_and_selection else {
            return Task::ready(Err(anyhow!("failed to determine buffer and selection")));
        };

        let Some(project) = self.project() else {
            return Task::ready(Err(anyhow!("editor does not have project")));
        };

        project.update(cx, |project, cx| {
            project.get_permalink_to_line(&buffer, selection, cx)
        })
    }
}

#[cfg(test)]
impl Editor {
    /// Returns the line range for the first diff review overlay, if one is active.
    /// Returns (start_row, end_row) as physical line numbers in the underlying file.
    pub(super) fn diff_review_line_range(&self, cx: &App) -> Option<(u32, u32)> {
        let overlay = self.diff_review_overlays.first()?;
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let start_point = overlay.anchor_range.start.to_point(&snapshot);
        let end_point = overlay.anchor_range.end.to_point(&snapshot);
        let start_row = snapshot
            .point_to_buffer_point(start_point)
            .map(|(_, p)| p.row)
            .unwrap_or(start_point.row);
        let end_row = snapshot
            .point_to_buffer_point(end_point)
            .map(|(_, p)| p.row)
            .unwrap_or(end_point.row);
        Some((start_row, end_row))
    }

    /// Takes all stored comments from all hunks, clearing the storage.
    /// Returns a Vec of (hunk_key, comments) pairs.
    pub(super) fn take_all_review_comments(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<(DiffHunkKey, Vec<StoredReviewComment>)> {
        self.dismiss_all_diff_review_overlays(cx);
        let comments = std::mem::take(&mut self.stored_review_comments);
        self.orphaned_review_comments.clear();
        self.next_review_comment_id = 0;
        self.next_review_reply_id = 0;
        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: 0,
            persist: false,
        });
        cx.notify();
        comments
    }
}

impl EditorSnapshot {
    pub(super) fn display_diff_hunks_for_rows<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
        folded_buffers: &'a HashSet<BufferId>,
    ) -> impl 'a + Iterator<Item = DisplayDiffHunk> {
        let buffer_start = DisplayPoint::new(display_rows.start, 0).to_point(self);
        let buffer_end = DisplayPoint::new(display_rows.end, 0).to_point(self);

        self.buffer_snapshot()
            .diff_hunks_in_range(buffer_start..buffer_end)
            .filter_map(|hunk| {
                if folded_buffers.contains(&hunk.buffer_id)
                    || (hunk.row_range.is_empty() && self.buffer.all_diff_hunks_expanded())
                {
                    return None;
                }

                let hunk_start_point = Point::new(hunk.row_range.start.0, 0);
                let hunk_end_point = if hunk.row_range.end > hunk.row_range.start {
                    let last_row = MultiBufferRow(hunk.row_range.end.0 - 1);
                    let line_len = self.buffer_snapshot().line_len(last_row);
                    Point::new(last_row.0, line_len)
                } else {
                    Point::new(hunk.row_range.end.0, 0)
                };

                let hunk_display_start = self.point_to_display_point(hunk_start_point, Bias::Left);
                let hunk_display_end = self.point_to_display_point(hunk_end_point, Bias::Right);

                let display_hunk = if hunk_display_start.column() != 0 {
                    DisplayDiffHunk::Folded {
                        display_row: hunk_display_start.row(),
                    }
                } else {
                    let mut end_row = hunk_display_end.row();
                    if hunk.row_range.end > hunk.row_range.start || hunk_display_end.column() > 0 {
                        end_row.0 += 1;
                    }
                    let is_created_file = hunk.is_created_file();
                    let multi_buffer_range = hunk.multi_buffer_range.clone();

                    DisplayDiffHunk::Unfolded {
                        status: hunk.status(),
                        diff_base_byte_range: hunk.diff_base_byte_range.start.0
                            ..hunk.diff_base_byte_range.end.0,
                        word_diffs: hunk.word_diffs,
                        display_row_range: hunk_display_start.row()..end_row,
                        multi_buffer_range,
                        is_created_file,
                    }
                };

                Some(display_hunk)
            })
    }

    fn hunks_for_ranges(
        &self,
        ranges: impl IntoIterator<Item = Range<Point>>,
    ) -> Vec<MultiBufferDiffHunk> {
        let mut hunks = Vec::new();
        let mut processed_buffer_rows: HashMap<BufferId, HashSet<Range<text::Anchor>>> =
            HashMap::default();
        for query_range in ranges {
            let query_rows =
                MultiBufferRow(query_range.start.row)..MultiBufferRow(query_range.end.row + 1);
            for hunk in self.buffer_snapshot().diff_hunks_in_range(
                Point::new(query_rows.start.0, 0)..Point::new(query_rows.end.0, 0),
            ) {
                // Include deleted hunks that are adjacent to the query range, because
                // otherwise they would be missed.
                let mut intersects_range = hunk.row_range.overlaps(&query_rows);
                if hunk.status().is_deleted() {
                    intersects_range |= hunk.row_range.start == query_rows.end;
                    intersects_range |= hunk.row_range.end == query_rows.start;
                }
                if intersects_range {
                    if !processed_buffer_rows
                        .entry(hunk.buffer_id)
                        .or_default()
                        .insert(hunk.buffer_range.start..hunk.buffer_range.end)
                    {
                        continue;
                    }
                    hunks.push(hunk);
                }
            }
        }

        hunks
    }
}

pub fn set_blame_renderer(renderer: impl BlameRenderer + 'static, cx: &mut App) {
    cx.set_global(GlobalBlameRenderer(Arc::new(renderer)));
}

pub fn render_diff_hunk_controls(
    row: u32,
    status: &DiffHunkStatus,
    hunk_range: Range<Anchor>,
    is_created_file: bool,
    line_height: Pixels,
    editor: &Entity<Editor>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let stageable = hunk_range
        .start
        .buffer_id()
        .and_then(|buffer_id| editor.read(cx).buffer().read(cx).diff_for(buffer_id))
        .is_some_and(|diff| diff.read(cx).is_stageable());
    let show_stage_restore = stageable
        && ProjectSettings::get_global(cx)
            .git
            .show_stage_restore_buttons;

    h_flex()
        .h(line_height)
        .mr_1()
        .gap_1()
        .px_0p5()
        .pb_1()
        .border_x_1()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .rounded_b_lg()
        .bg(cx.theme().colors().editor_background)
        .gap_1()
        .block_mouse_except_scroll()
        .shadow_md()
        .when(show_stage_restore, |el| {
            el.child(if status.has_secondary_hunk() {
                Button::new(("stage", row as u64), "Stage")
                    .alpha(if status.is_pending() { 0.66 } else { 1.0 })
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Stage Hunk",
                                &::git::ToggleStaged,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                editor.stage_or_unstage_diff_hunks(
                                    true,
                                    vec![hunk_range.start..hunk_range.start],
                                    window,
                                    cx,
                                );
                            });
                        }
                    })
            } else {
                Button::new(("unstage", row as u64), "Unstage")
                    .alpha(if status.is_pending() { 0.66 } else { 1.0 })
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Unstage Hunk",
                                &::git::ToggleStaged,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                editor.stage_or_unstage_diff_hunks(
                                    false,
                                    vec![hunk_range.start..hunk_range.start],
                                    window,
                                    cx,
                                );
                            });
                        }
                    })
            })
        })
        .when(show_stage_restore, |el| {
            el.child(
                Button::new(("restore", row as u64), "Restore")
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Restore Hunk",
                                &::git::Restore,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.snapshot(window, cx);
                                let point = hunk_range.start.to_point(&snapshot.buffer_snapshot());
                                editor.restore_hunks_in_ranges(vec![point..point], window, cx);
                            });
                        }
                    })
                    .disabled(is_created_file),
            )
        })
        .when(
            !editor.read(cx).buffer().read(cx).all_diff_hunks_expanded(),
            |el| {
                el.child(
                    IconButton::new(("next-hunk", row as u64), IconName::ArrowDown)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        // .disabled(!has_multiple_hunks)
                        .tooltip({
                            let focus_handle = editor.focus_handle(cx);
                            move |_window, cx| {
                                Tooltip::for_action_in("Next Hunk", &GoToHunk, &focus_handle, cx)
                            }
                        })
                        .on_click({
                            let editor = editor.clone();
                            move |_event, window, cx| {
                                editor.update(cx, |editor, cx| {
                                    let snapshot = editor.snapshot(window, cx);
                                    let position =
                                        hunk_range.end.to_point(&snapshot.buffer_snapshot());
                                    editor.go_to_hunk_before_or_after_position(
                                        &snapshot,
                                        position,
                                        Direction::Next,
                                        true,
                                        window,
                                        cx,
                                    );
                                    editor.expand_selected_diff_hunks(cx);
                                });
                            }
                        }),
                )
                .child(
                    IconButton::new(("prev-hunk", row as u64), IconName::ArrowUp)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        // .disabled(!has_multiple_hunks)
                        .tooltip({
                            let focus_handle = editor.focus_handle(cx);
                            move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Previous Hunk",
                                    &GoToPreviousHunk,
                                    &focus_handle,
                                    cx,
                                )
                            }
                        })
                        .on_click({
                            let editor = editor.clone();
                            move |_event, window, cx| {
                                editor.update(cx, |editor, cx| {
                                    let snapshot = editor.snapshot(window, cx);
                                    let point =
                                        hunk_range.start.to_point(&snapshot.buffer_snapshot());
                                    editor.go_to_hunk_before_or_after_position(
                                        &snapshot,
                                        point,
                                        Direction::Prev,
                                        true,
                                        window,
                                        cx,
                                    );
                                    editor.expand_selected_diff_hunks(cx);
                                });
                            }
                        }),
                )
            },
        )
        .into_any_element()
}

impl Editor {
    pub(super) fn update_uncommitted_diff_for_buffer(
        &mut self,
        project: &Entity<Project>,
        buffers: impl IntoIterator<Item = Entity<Buffer>>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let mut tasks = Vec::new();
        project.update(cx, |project, cx| {
            let git_store = project.git_store().clone();
            git_store.update(cx, |git_store, cx| {
                for buffer in buffers {
                    if project::File::from_dyn(buffer.read(cx).file()).is_some() {
                        tasks.push(git_store.open_display_diff(buffer, cx));
                    }
                }
            });
        });

        let editor = cx.entity();
        let buffer = self.buffer.clone();
        cx.spawn(async move |_, cx| {
            let diffs = future::join_all(tasks).await;
            if editor.read_with(cx, |editor, _cx| editor.diff_hunk_delegate.is_some()) {
                return;
            }

            buffer.update(cx, |buffer, cx| {
                for diff in diffs.into_iter().flatten() {
                    buffer.add_diff(diff, cx);
                }
            });
        })
    }
}
