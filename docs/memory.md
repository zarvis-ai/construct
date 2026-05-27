# Memory

agentd gives agents a small, durable memory surface for facts that should carry
across turns, sessions, or future work in the same project. Memory is plain
Markdown so it stays readable, editable, and easy to audit.

Memory is shared across all agentd harness types in the same scope, so Codex,
Claude Code, Zarvis, and other agents can build on the same durable context.

Memory is intentionally separate from transcripts:

- **Transcripts** preserve what happened in a session.
- **Memory** preserves what is worth reusing later.

## Memory scopes

`agentd_context` exposes two memory files when available:

| Scope | Use for |
|---|---|
| Global memory | Cross-project preferences, standing workflows, and durable operating conventions. |
| Project memory | Project-specific architecture, workflows, decisions, commands, glossary, and pitfalls. |

Use the narrowest scope that will be useful later. A repo-specific workflow
belongs in project memory; a preference the user repeats across repositories
belongs in global memory.
