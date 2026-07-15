# 0095-first-party-named-tunnels

Status: accepted
Date: 2026-07-15
Area: architecture
Scope: Construct's first-party tunnel names, identity, authorization, and persistence

## Decision

Selecting `tunnel.zarvis.ai` in `/remote-connect` opens a name-entry step before browser authorization. It proposes a human-friendly random name that the user may accept or replace. Names contain 1–32 lowercase ASCII letters, digits, and interior hyphens.

After a registration succeeds, Construct remembers the chosen name locally and preselects it the next time the dialog opens. A first-time user still receives the random friendly suggestion. Stopping a first-party tunnel also opens the service logout endpoint, which clears the shared browser session cookie; merely closing the dialog or returning to the chooser does not log out.

The public hostname is a single DNS label shaped as `<chosen-name>-<stable-user-id>.tunnel.zarvis.ai`. The service assigns each OAuth identity a persistent, human-readable, privacy-preserving stable ID. The ID is mapped from the OAuth provider plus its immutable provider subject; the raw subject, username, display name, and email are never placed in the URL.

The service persists identities and tunnel reservations in SQLite on the VM's durable disk. A reservation maps the authenticated identity and chosen name to one stable Construct installation ID. The same installation may reconnect to its reservation. A different installation owned by the same identity receives a name-conflict error. Different identities may choose the same name because their stable user suffixes differ.

Active reverse endpoints, short-lived capabilities, upstream credentials, authorization requests, and browser sessions remain runtime state. A service restart retains identities and reservations but drops active routes; the client must authenticate and register again to reactivate its reserved hostname.

Owner authentication is an interactive browser handoff initiated by the running Construct daemon. Construct opens the browser capability and retains the polling capability only in process memory. After social login, the polling capability returns an owner credential exactly once. The credential is consumed directly for registration and is never displayed, copied, configured through an environment variable, or written to disk.

The public tunnel ready screen shows the public URL and QR but not the remote listener's Basic username or password. Those are gateway-to-upstream credentials for this provider; public visitors authenticate socially and never enter them. LAN and providers without a credential-injecting gateway continue to display the Basic credentials.

The same social identity that owns the reservation is the authorization boundary for browser access. A visitor authenticates with GitHub or Google, and access is allowed only when the identity in the signed login session equals the reservation owner. Sharing and persistent multi-user ACLs are non-goals until explicitly designed.

Construct links the `wstunnel` Rust library and runs the client inside the daemon's supervised async task. No external `wstunnel` executable or PATH configuration is required.

## Reason

The chosen name makes the URL recognizable, while the stable per-identity suffix allows independent users to choose familiar names without global contention. SQLite preserves names and identity across deployments and VM restarts. Binding each reservation to a stable local installation prevents a second Construct instance on the same account from silently taking over an existing tunnel.

Keeping active routing and credentials ephemeral limits the durable database to identity and ownership metadata. OAuth identity remains the visitor access boundary without exposing provider identifiers in public hostnames.

## Consequences

- The client validates the chosen name before opening OAuth; the service validates it again at registration.
- A failed registration does not replace the remembered name.
- A 409 name-conflict response is shown in the `/remote-connect` error view.
- Construct persists a random installation ID in its data directory. It is not an authentication secret.
- Reconnecting the same installation and identity with the same name preserves the public URL.
- Deleting Construct's installation ID makes prior reservations appear owned by another installation; reclaim or transfer requires a future explicit workflow.
- The SQLite database must live on the VM's persistent filesystem and be excluded from deployment synchronization and image replacement.
- The service must validate the short-lived capability on the `wstunnel` upgrade and restrict its reverse bind to the allocated endpoint.
- A public hostname is not reported ready until the gateway can reach its reverse endpoint.
- OAuth client secrets and the session-signing key remain operational secrets outside the SQLite identity database.

## Non-Goals

- Cross-account sharing, teams, invitations, durable multi-user ACLs, reservation transfer, and name reclamation.
- Encoding raw OAuth provider subjects, usernames, emails, or display names in URLs.
