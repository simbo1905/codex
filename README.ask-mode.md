# /ask(1)

## NAME

`/ask` — conversation mode with no tool access

## SYNOPSIS

In the TUI composer, type:

```
/ask
```

## DESCRIPTION

`/ask` switches codex into **Ask mode**: a read-only conversation mode where
the model has no tools registered and cannot execute shell commands, read
files, or apply patches.

Use it when you want to think out loud with the model — architecture
questions, code review, trade-off analysis — without the risk of any
side-effects or the overhead of tool scaffolding in the context window.

## BEHAVIOUR

- All tools are unregistered for the duration of the mode.
- The model is instructed to answer questions, explain code, and discuss
  design; it will direct you to `/plan` or Shift+Tab if you need execution.
- The mode is available during an active task (it does not interrupt work
  in progress).
- Switching away: type `/plan` to enter Plan mode, or press Shift+Tab to
  cycle back to Default mode.

## IMPLEMENTATION

The mode is backed by `codex-rs/collaboration-mode-templates/templates/ask.md`,
which is injected as the system prompt when Ask mode is active.  It is one of
the collaboration modes defined in `codex-rs/tui/src/collaboration_modes.rs`
and dispatched from `codex-rs/tui/src/chatwidget/slash_dispatch.rs`.

## SEE ALSO

`/plan` — Plan mode (tools available, explicit approval required)

Shift+Tab — cycle collaboration mode
