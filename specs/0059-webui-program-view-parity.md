# 0059-webui-program-view-parity

Status: accepted
Date: 2026-06-28
Area: webui
Scope: The web UI exposes the per-session Program surface with functional parity to the TUI, built on the same daemon contract.

## Decision

Every session's Program is reachable and fully usable from the web UI, not only the TUI. The web Program surface is a third per-session view mode, peer to Terminal and Chat, available for any session (every session owns a Program). It must support, at functional parity with the TUI:

- **View** the live Markdown document and its version.
- **Edit** the document as plain Markdown text.
- **Save** with optimistic-version conflict handling: a save carries the base version; when the document advanced underneath the editor, the client performs a three-way merge against the common ancestor and writes the result, surfacing standard conflict markers when the merge cannot reconcile overlapping edits.
- **Run** the whole document, or the current text selection, dispatching the program to the owning session.
- **Run shimmer**: while a run is in flight, the still-pending blocks shimmer with a travelling-wave highlight, and a per-block run-status tooltip is reachable.
- **Desktop hover affordances**: on pointer-hover devices, hovering a shimmering block shows its run-status tooltip, and hovering a `@{session:…}` smart-clip shows a compact terminal-tail preview for the referenced session with a status caption. If the target session cannot be previewed, the clip hover degrades to the caption text.
- **Templates**: an empty Program offers the available non-blank templates as one-click starting points.
- **Smart-clip autocomplete**: typing the clip trigger offers matching sessions and harnesses and inserts the corresponding clip reference.
- **Find** within the document.
- **Live updates**: concurrent edits and run-state changes from agents or other clients are adopted live when the local buffer has no unsaved edits, and preserved (not clobbered) when it does.

Block identity, the shimmer pending set, three-way merge, and clip-instance-id normalization are computed from the **same shared rules the daemon and TUI use**. Clients must not invent a divergent block-id scheme; equal content must yield equal block ids across every client and the daemon.

## Reason

Program is a primary collaboration surface (a shared space the user and agents edit and run together), but it had been authored only through the TUI. Remote-control and mobile users reach agentd through the web UI and could see neither the document nor its run state. Parity removes that gap. Reusing the existing daemon RPC and broadcast surface — which already serves web clients — means no new server contract is required; the gap was purely a missing client renderer.

Anchoring block identity and merge on the shared rules is what lets shimmer, settle-on-edit, and concurrent merge behave identically no matter which client issued the edit. A client-local block-id scheme would make a block shimmer in one client and not another for the same content.

## Consequences

- The web Program surface rides the existing program get/update/edit/execute/list-templates calls and the program-state broadcast. Changes to those contracts must keep the web client working, not just the TUI.
- The shared block-identity and block-span rules are now a cross-client contract with at least two independent implementations (daemon/TUI in one language, web client in another). Changing the hashing or block-splitting rules requires updating every implementation together, or block ids silently diverge and shimmer breaks across clients.
- The optimistic-version + three-way-merge save protocol is likewise replicated per client; the conflict-marker shape is user-visible and should stay consistent across clients.
- Idiomatic per-platform input is expected and acceptable: the web surface uses native text-editing affordances (caret, selection, clipboard, undo, IME) rather than reproducing the TUI's exact keybindings. Parity is defined by capability and by the shared daemon contract, not by keystroke-for-keystroke equivalence.

## Non-Goals

- Pixel-identical rendering or identical keybindings between TUI and web.
- A rich rendered-Markdown editor: the Program surface is a plain-text Markdown editor with overlays (clips, shimmer, find), matching the TUI's plain-text model, not a WYSIWYG view.
- Pixel-identical hover cards between TUI and web. The required parity is semantic: shimmer hover surfaces status text, and session-clip hover previews the referenced session when possible.
