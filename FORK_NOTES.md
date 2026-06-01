# Vince's Zed fork notes

This fork adds a GitHub-style review-comment workflow to Zed's project diff
view so Zed can replace Hunk as the fast local review surface while still
letting Amp inspect, add, and reply to review comments.

## Goals

- Open a Git diff in Zed and add a comment on any diff line.
- Persist comments across closing/reopening the diff view.
- Keep comments when switching branches, changing the diff base, or removing the
  original changed line/file.
- Edit, delete, archive, restore, and reply to comments.
- Distinguish human comments from Amp comments with `vxio` and `amp` authors.
- Let Amp list comments, address them, add new comments, and reply to existing
  threads.
- Let review comments reference code context with `@file` and `@symbol`
  completions in the comment editor.
- Keep the fork easy to rebase on top of daily upstream Zed changes.

## Zed implementation

Primary files:

- `crates/git_ui/src/project_diff.rs`
- `crates/editor/src/git.rs`
- `crates/editor/src/actions.rs`
- `crates/editor/src/editor.rs`
- `crates/editor/src/element.rs`
- `crates/editor/src/element/mouse.rs`
- `crates/editor/src/editor_tests.rs`
- `Makefile`

The project diff view renders persisted review threads inline with diff hunks.
Comments have author labels, relative timestamps, edit/delete/reply actions, and
visual styling that separates comment blocks from the surrounding diff.

Saved comment and reply bodies render as read-only embedded editors. This keeps
long text wrapping stable while allowing normal mouse selection and copy, and it
still folds stored `@file`, `@symbol`, and markdown links into clickable chips.

Review comment inputs use the editor completion menu for `@` context mentions:

- `@` shows context categories.
- `@file <query>` searches project files and directories.
- `@symbol <query>` searches project symbols.
- Accepted mentions are stored as markdown links for export compatibility but
  fold into inline chips while editing, matching the agent-thread input feel.

Review input editors are configured for Vim insert mode on focus, so adding a
new comment, editing, or replying lands in an immediately typeable input.

Each comment and reply has a copy-reference action. It writes a stable local
reference to the clipboard, e.g. `zed-review:comment:12` or
`zed-review:reply:18`. Delete actions ask for confirmation before removing a
comment or reply from the active review.

Comments are stored in Zed's SQLite database in the
`project_diff_review_comments` table. The active development build uses:

```text
~/Library/Application Support/Zed/db/0-dev/db.sqlite
```

The comment model stores enough diff metadata to keep showing comments after the
original diff changes:

- repository/workspace identity
- file path
- side (`old` / `new`)
- hunk and line position metadata
- line range
- body, author, and timestamps
- reply parentage
- archive/delete metadata
- an outdated/orphaned reason when the original diff is no longer present

## Commands

Commands use the `zed_review::` namespace to avoid colliding with Zed's built-in
Git commands.

User-facing commands include:

- `zed review: add comment`
- archive all review comments
- archive user comments
- archive agent comments
- restore latest archived comments
- restore latest user archived comments
- restore latest agent archived comments
- delete review comments after confirmation

Archive and delete intentionally differ:

- Archive removes comments from the active diff but keeps them restorable.
- Delete removes comments from the active diff and marks them as a non-restorable
  deleted batch. Comments are soft-deleted rather than physically removed.

Restore merges an archived batch into the active comments. Running restore twice
should not duplicate existing active comments.

## Amp integration

The Amp plugin lives outside this repo:

```text
~/.config/amp/plugins/zed-review.ts
```

The shared CLI lives outside this repo:

```text
~/.config/scripts/zed-review
~/.local/bin/zed-review -> ~/.config/scripts/zed-review
```

It exposes tools/commands for agents to:

- list current review comments
- ask an Amp agent to address human comments
- copy comments to the clipboard
- add an Amp-authored comment to a file/line/range
- reply to an existing review thread

Amp-authored comments use `author = "amp"`. Human comments use
`author = "vxio"`.

The plugin shells out to `zed-review` instead of owning database logic itself.
The CLI reads and writes the same SQLite table that Zed reads. Zed's diff pane
polls the DB file mtime and refreshes comments when the database changes, so
Amp-created comments appear without manually reopening the diff pane.

Useful CLI commands:

```fish
zed-review --repo $MOOV/accounts list --format compact
zed-review --repo $MOOV/accounts add --file internal/foo.go --line-start 42 --body "check this"
zed-review --repo $MOOV/accounts reply --comment-id 1 --body "fixed"
```

## Archive CLI/TUI

The archive browser lives outside this repo:

```text
~/.config/scripts/zed-review-archives.sh
~/.config/fish/functions/zra.fish
```

Use `zra` to browse archived review-comment sessions from the terminal. It
detects the current repo/branch context, lists archived batches, and previews the
comments before restoring.

## Fork sync workflow

The fork remote setup is:

```text
origin   https://github.com/vxio/zed.git
upstream https://github.com/zed-industries/zed.git
```

`upstream` push is disabled, and `origin` is the push default.

Use the fish helper from anywhere:

```fish
zed_sync_fork
```

The helper lives at:

```text
~/.config/fish/functions/zed_sync_fork.fish
```

It performs the daily sync flow:

1. Refuse to run with a dirty worktree.
2. Fetch `upstream` and `origin`.
3. Rebase the current branch on `upstream/main`.
4. Run `cargo fmt --check`.
5. Run `cargo check -p editor`.
6. Install the bundled app.
7. Push to `origin` with `--force-with-lease`.

The matching agent skill lives at:

```text
~/.config/agents/skills/syncing-zed-fork/SKILL.md
```

## Building and installing Zed Dev

The fork should be installed as `Zed Dev.app` rather than replacing stable Zed:

```fish
cd ~/code/zed
env TERM=xterm-256color FORCE_COLOR=1 ./script/bundle-mac -i
```

`TERM=xterm-256color FORCE_COLOR=1` avoids a `cargo-bundle` terminal-color panic
seen when running under non-interactive agent shells.

`script/bundle-mac -i` returns after installing `/Applications/Zed Dev.app` for
local `-i` builds, so `make build` does not continue into DMG packaging.

Do not update this fork through Zed's UI. Treat updates as source-controlled:
sync from upstream, rebuild, install, and push the fork.

The repo also has a tiny `Makefile` for the common local commands:

```fish
make run $MOOV/accounts
make run ARGS=$MOOV/accounts
make build
```

`make run` calls `cargo run -- ...`. `make build` runs the `bundle-mac -i`
install command above.

## Local tooling changes

Several personal tools were repointed to prefer `Zed Dev.app`:

- `~/.config/scripts/zed-projects.env` defines `ZED_APP_NAME` and `ZED_CLI`.
- `~/.config/scripts/zed-projects-open.sh` opens via `ZED_CLI` and activates
  `ZED_APP_NAME`.
- `~/.config/fish/functions/zed.fish` makes `zed ...` open Zed Dev from fish.
- Hammerspoon repo launcher window detection prefers `dev.zed.Zed-Dev`.
- Keyboard Maestro app targets were changed from stable Zed to Zed Dev.

These changes make the repo launcher, Amp agents TUI, fish helpers, and Keyboard
Maestro macros use the forked app by default, with stable Zed as a fallback in
some shell paths.

## Known tradeoffs and follow-ups

- This is a fork-only feature, so upstream changes in `project_diff.rs` and
  editor diff plumbing are the most likely conflict points during rebases.
- The review-comment DB schema is local to this fork and may need migration work
  if upstream changes Zed's database/domain migration patterns.
- The current Amp integration still uses Zed's SQLite database as the API. That
  is simple and fast, and the DB access is centralized in the `zed-review` CLI
  so Amp and terminal workflows share one implementation. A first-class Zed
  command/server API would still be cleaner if this became a long-lived
  integration.
- Review `@` mentions are stored as markdown links for persistence/export, then
  rendered as clickable chips in saved comments and replies. File and symbol
  chips open the target in Zed; hovering shows the target URL/path in both the
  input editor and saved comment view.
- Review comment inputs explicitly refocus after mount and switch Vim into
  insert mode so new, reply, and edit flows are ready for typing.
- Review inputs are auto-height editors: Enter submits, while Shift-Enter and
  Alt-Enter insert a newline into the comment body.
- Copying a review comment/reply reference writes directly to the clipboard
  without showing a persistent toast.
- `make build` installs `Zed Dev.app` and prints a successful build message
  after `script/bundle-mac -i` returns.
