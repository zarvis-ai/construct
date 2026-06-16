# 0029-smith-os-sandbox-pluggable

Status: proposed
Date: 2026-06-14
Area: adapter-smith
Scope: OS-level sandbox enforcement for smith's tool execution, behind a pluggable backend — with `edit_file` writes guarded too.

## Decision

Smith gains a real OS-enforced sandbox (Codex-equivalent), behind a **pluggable `Sandbox` trait** so the enforcement backend is swappable per host and extensible:

- A `SandboxPolicy` (capability data) is **derived at runtime** from session context (no config file required): writable roots = the session worktree + widgets dir + tmp; network denied by default.
- Two consumers apply it: **subprocess tools** (`shell`/`proc`) run wrapped by the backend; **`edit_file`** (in-process today) routes its *write* through a **sandboxed writer subprocess** so the kernel enforces it too.
- The approval gate is reframed from "the tool is Risky" to "the action would **leave** the sandbox": in-sandbox actions run silently; only boundary crossings hit the existing Manual/AutoReview gate, and an approved crossing **re-runs with a relaxed policy**.
- Backends — **Seatbelt** (macOS), **Bubblewrap**/**Landlock** (Linux), a **Custom** bring-your-own-command, and **Noop** (unsupported/disabled, cooperative-gate-only) — are selected at startup (auto-detect or configured). Adding a backend = implement the trait; the consumers never change.

Persistence (user/project policy files) is explicitly **out of scope here** (see 0029's non-goals) — the mechanism works with derived defaults; a file is a later config layer feeding the same `SandboxPolicy`.

## The data: `SandboxPolicy`

```rust
pub struct SandboxPolicy {
    pub mode: SandboxMode,            // ReadOnly | WorkspaceWrite | FullAccess  (↔ codex modes)
    pub writable_roots: Vec<PathBuf>, // absolute, canonicalized
    pub readable: ReadScope,          // All | Roots(Vec<PathBuf>)
    pub network: NetworkPolicy,       // Denied | Allowed
}

impl SandboxPolicy {
    /// Runtime default — NO file. Everything from session context.
    fn workspace_default(ctx: &ToolCtx, widgets_dir: &Path) -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: canon_all(&[&ctx.cwd, widgets_dir, &std::env::temp_dir()]),
            readable: ReadScope::All,
            network: NetworkPolicy::Denied,   // ← biggest single win: smith has zero net containment today
        }
    }
    fn escalated(&self) -> Self { Self { mode: FullAccess, network: Allowed, ..self.clone() } }
    fn allows_write(&self, p: &Path) -> bool { /* FullAccess || within writable_roots */ }
}
```

## The pluggable backend: `Sandbox` trait

```rust
pub trait Sandbox: Send + Sync {
    fn name(&self) -> &'static str;          // "seatbelt" | "bwrap" | "landlock" | "custom" | "noop"
    fn available(&self) -> bool;             // binary present / kernel supports / etc.
    fn enforces(&self) -> bool;              // false for Noop — used to surface "no backstop"

    /// Rewrite a command to run under `policy`. Returns program+args to spawn.
    fn wrap_command(&self, p: &SandboxPolicy, program: &str, args: &[String]) -> (String, Vec<String>);

    /// Sandboxed file write (the edit_file path). Default: wrap_command + a writer.
    fn write_file(&self, p: &SandboxPolicy, path: &Path, content: &[u8]) -> io::Result<()> {
        let (prog, args) = self.wrap_command(p, WRITER_PROGRAM, &writer_args(path));
        spawn_and_pipe(prog, args, content)   // EPERM if path ∉ writable_roots
    }
}
```

Selection at startup (config `backend = "auto"` by default):

```
auto → first available of [macos: Seatbelt] [linux: Bubblewrap, Landlock] else Noop
none → Noop ;  custom → CustomCmd(template) ;  seatbelt|bwrap|landlock → forced
```

If the chosen backend `!enforces()` (Noop) **smith logs and surfaces a one-time notice** that the OS sandbox is off and only the cooperative gate applies — so "no backstop" is never silent.

### Backends

- **Seatbelt (macOS):** generate an SBPL profile from the policy, front commands with `sandbox-exec -p <profile>`.
  ```scheme
  (version 1) (deny default)
  (allow process-exec process-fork)
  (allow file-read*)                  ; ReadScope::All
  (deny network*)                     ; NetworkPolicy::Denied
  (allow file-write*
    (subpath "/abs/worktree") (subpath "/abs/widgets") (subpath "<canon TMPDIR>"))
  ```
- **Bubblewrap (Linux):** `bwrap --ro-bind / / --bind <root> <root> … --unshare-net --chdir <cwd> -- <cmd>` (writable roots as `--bind`, the rest `--ro-bind`; omit `--unshare-net` when network is Allowed).
- **Landlock (Linux, no bwrap):** the writer/child self-applies a Landlock ruleset (allow writes under roots) + seccomp to drop network. In-process; no helper binary needed for the dedicated writer.
- **Custom (bring-your-own):** a config command template with placeholders — `["firejail", "--whitelist={root}", "--net=none", "--", "{cmd}"]` — so users plug firejail/nsjail/containers without code.
- **Noop:** identity wrap; `write_file` = `tokio::fs::write`. Unsupported platforms / `backend=none` / dev. Cooperative gate is then the only layer.

## Consumer A — subprocess tools (`shell`/`proc`)

One-line change at the spawn site (`tools/shell.rs`, `tools/proc.rs`): before building the `Command`, `let (prog, args) = sandbox.wrap_command(&policy, "bash", &["-lc", cmd])`. **Network containment lives here, not on the adapter process** — the adapter itself needs network for LLM calls, so net-deny must be per-model-subprocess.

## Consumer B — `edit_file` (the OS-guarded write)

`edit_file` already computes the final content in-adapter (reads are broad). Change only the final step:

```rust
// was: tokio::fs::write(&p.path, &p.new_text).await
sandbox.write_file(&policy, &p.path, p.new_text.as_bytes())   // kernel EPERM if outside writable_roots
```

The writer is a sandboxed subprocess (`construct-adapter-smith --apply-write` self-exec reading `{path}` + content from stdin; or `tee` for the zero-binary path). `EPERM` from the writer maps to "blocked by sandbox → needs approval".

## The gate reframe (escalation)

```rust
let policy = SandboxPolicy::workspace_default(&ctx, &widgets);   // or session mode
match plan(&tool, &input, &policy) {              // pre-flight: would this leave the sandbox?
    InSandbox          => run_wrapped(&tool, &input, &policy),    // ← no prompt; sandbox makes it safe
    Escalate { reason } => match approval_mode {
        UnsafeAuto      => run_wrapped(&tool, &input, &policy.escalated()),
        Manual          => ask_user(reason)?,                     // existing prompt
        AutoReview      => match auto_review(...).await {         // existing reviewer
            Approve     => run_wrapped(&tool, &input, &policy.escalated()),
            Deny|AskUser=> ask_user(reason)?,                     // smith still defers, not denies
        },
    },
}
```

`plan()` returns `Escalate` for: a write outside `writable_roots`, a network-needing command, or an exec that leaves the roots; otherwise `InSandbox`. Result: routine in-worktree edits and `cargo test` stop prompting (the sandbox makes them safe); the gate is reserved for genuine boundary crossings — Codex's model, and the prompt-fatigue fix for free.

`SandboxMode` (capability floor) becomes **orthogonal** to `ApprovalMode` (who approves a crossing) — the two Codex dimensions, vs today's single conflated `ApprovalMode`.

## Config (pluggability surface; no policy persistence)

```toml
[adapters.smith.sandbox]
backend = "auto"          # auto | seatbelt | bwrap | landlock | custom | none
# custom_command = ["firejail", "--whitelist={root}", "--net=none", "--", "{cmd}"]
```

That's the only config this spec adds. Derived defaults need nothing.

## Reason

Smith's approval today is purely cooperative — risk-tag a tool, then prompt — with **no enforced backstop**, so an un-asked or mis-tagged action (or a `read_only`-mistagged shell) can do anything, and network is entirely uncontained. OS sandboxing (Seatbelt/Landlock/bwrap) gives the real boundary Codex has; a pluggable backend keeps it portable and lets hosts opt into their own enforcement. Guarding `edit_file` (not just subprocesses) closes the obvious in-process write hole. Reframing the gate to "leaving the sandbox" both matches Codex and removes prompt fatigue.

## Consequences

Routine work runs silently inside the sandbox; prompts shrink to real boundary crossings; network is denied by default (per-subprocess). The backend is auto-selected and degrades to Noop (cooperative-gate-only) with a visible notice, so behavior is safe-by-default and transparent where unenforced. `edit_file` costs a subprocess per write (cheap). The policy struct is the single source of truth a future config/persistence layer can feed without touching consumers.

## Non-Goals

- **Persisted policy files / decision memory** — derived defaults suffice; a config layer comes later (feeds the same `SandboxPolicy`).
- **Whole-adapter confinement (Option A)** — would require daemon-brokered escalation and whitelisting the adapter's own writes + keeping its network; deferred. This spec confines the *actions*, not the adapter process.
- **seccomp user-notification "trap-and-ask"** — out of scope; the ask stays a cooperative pre-flight, with the sandbox as the hard floor.
- **Windows enforcement** — Noop (cooperative gate) there.
