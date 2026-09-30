# Plan mode

`claustrum run --permission-mode plan` (or Shift+Tab in the session) works as usual. Claude
Code names a plan file in its own plan directory, `~/.claude/plans/<slug>.md` (or
`$CLAUDE_CONFIG_DIR/plans`), and in plan mode refuses every tool that is not marked
read-only, so Claustrum's `Write`, `Edit` and `Bash` too. Claude writes the plan with
`WritePlan` and `EditPlan` instead:

- They take the plan file path and nothing else: a `<slug>.md` directly in the plan
  directory.
- The broker writes the file, never following a link. It only touches files that this
  workspace created, recorded in a ledger in the user state directory. Plans of other
  projects can be neither read nor changed.
- The plan directory is not mounted in the guest, and the confined worker may not read
  `~/.claude` at all.

Claude Code accepts a custom `plansDirectory` only inside the project, where the guest could
tamper with what Claude Code later reads and writes on the host, so Claustrum keeps the
default.

## Finding and disabling plans

- `claustrum plans` lists the plan files Claude wrote for the current workspace
  (`--workspace DIR` for another one).
- `[claude] plans = false` removes the plan tools and the plan mode tools.
