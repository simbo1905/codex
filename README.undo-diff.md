# /undo-diff(1)

## NAME

`/undo-diff` — show a coloured diff of what the agent changed in the last turn

## SYNOPSIS

In the TUI composer, type:

```
/undo-diff
```

## DESCRIPTION

`/undo-diff` compares the current working tree against the ghost-snapshot
commit that codex recorded at the start of the most recent agent turn.

A ghost snapshot is a detached git commit created by the undo subsystem
(`codex-git-utils`) before each turn begins.  By diffing the working tree
against that commit you get an exact, per-file view of every change the
agent made — identical to what `git diff <sha>` would show — displayed in
the TUI pager with ANSI colour.

## OUTPUT

The diff is rendered in the full-screen pager overlay using ANSI colour codes
(`git diff --color`).  Added lines are green, removed lines are red, context
lines are unchanged.  Press `q` or Escape to close the pager.

## REQUIREMENTS

- The working directory must be inside a git repository.
- At least one agent turn must have completed (so a ghost snapshot exists).
- If no snapshot SHA is available the command reports that no diff is ready.

## WHEN TO USE IT

- After an agent turn completes, to review exactly what changed before
  approving or undoing.
- As a lightweight alternative to opening a separate terminal and running
  `git diff` manually.
- To confirm that `/undo` rolled back everything you expected.

## IMPLEMENTATION

`/undo-diff` is a `SlashCommand::UndoDiff` variant dispatched in
`codex-rs/tui/src/chatwidget/slash_dispatch.rs`.  The diff itself is computed
by `get_undo_diff(sha)` in `codex-rs/tui/src/get_undo_diff.rs`, which shells
out to `git diff --color <sha>` asynchronously and pipes the output into the
pager overlay.

## SEE ALSO

`/undo` — revert the working tree to the ghost snapshot (discard agent changes)
