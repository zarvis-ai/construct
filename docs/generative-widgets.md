# Generative widgets

Generative widgets are lightweight, session-scoped UI panels backed by Markdown
files. They let an agent show compact task state, decisions, and action links
without turning that UI into model-facing transcript history.

Widgets are designed to be:

- **Durable enough for UI state**: they live on disk with the session and can be
  rehydrated after a client reconnects.
- **Ephemeral enough for task UX**: agents should update, consolidate, or delete
  widgets as the task changes.
- **Renderer-owned**: agents provide semantic Markdown; the TUI and web UI own
  layout, focus, scrolling, and hide/show controls.

## File-backed widgets

Agents create widgets by writing UTF-8 Markdown files with a `.md` extension to
the current session's widget directory. Agents should not guess this path: call
`agentd_context` and use `session_widgets.dir` from the response.

```text
widgets/
  task-status.md
  review.md
```

Each Markdown file becomes one widget. The filename is used as the stable widget
id, and the file stem becomes the title fallback (`task-status.md` renders as
`task status`). Updating the file updates the widget; deleting the file removes
it.

Widgets are **UI state**, not transcript history. Clients can restore current
widgets after reconnect without replaying the model conversation.

## Markdown subset

Widgets use "agentd Markdown": normal Markdown plus a small set of semantic
extensions. Renderers parse the pieces they understand and degrade the rest to
plain text.

Common Markdown works well:

```markdown
# Build status

- [x] Compile
- [~] Run checks
- [ ] Merge PR

[Open PR](agentd:action/open-pr)
```

### Timeline blocks

`agentd_context` advertises supported `widget_markdown_extensions`. The current
special extension is `timeline`, which renders top-level bullets/checklists as a
vertical timeline with nested detail lines.

```markdown
:::timeline
- [x] Prepare branch
  - [x] Create worktree
  - [x] Commit change
- [~] Validate
  - [x] cargo build
  - [ ] CI
- [ ] Merge and clean up
:::
```

Supported top-level markers:

| Marker | Meaning |
|---|---|
| `[x]` | Done |
| `[~]` | Active/current |
| `[ ]` | Todo |
| `[!]` | Blocked/warning |
| plain bullet | Milestone |

Nested bullets render under their parent at arbitrary depth.

## Action links

Widgets can include action links with the `agentd:action/` scheme:

```markdown
[Run checks](agentd:action/run-checks)
[Open PR](agentd:action/open-pr?key=o)
```

When a user activates an action, the owning session receives a normal
observation such as:

```text
OBSERVATION: ui.action {"panel_id":"task-status","action_id":"run-checks","label":"Run checks"}
```

Keyboard shortcuts are opt-in with `?key=<key>`. They are only active while the
widget/card is focused. Action links are intent signals only; they do not bypass
normal tool approvals, safety policy, or user confirmation requirements.

## Example

A typical task status widget:

```markdown
# PR cleanup

:::timeline
- [x] Wait for CI
  - [x] Build & test passed
- [~] Merge PR
  - [ ] Squash merge
- [ ] Cleanup
  - [ ] Remove worktree
  - [ ] Pull main
  - [ ] cargo build
:::

[Open PR](agentd:action/open-pr?key=p)
```

Write it to the current session's widget directory:

```bash
cat >"$AGENTD_SESSION_WIDGETS_DIR/pr-cleanup.md" <<'EOF'
# PR cleanup

:::timeline
- [x] Wait for CI
- [~] Merge PR
- [ ] Cleanup
:::

[Open PR](agentd:action/open-pr?key=p)
EOF
```
