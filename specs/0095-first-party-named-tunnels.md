# 0095-first-party-named-tunnels

Status: accepted
Date: 2026-07-14
Area: architecture
Scope: Construct's first-party tunnel names, identity, authorization, and runtime state

## Decision

Construct's first-party tunnel provider asks the hosted service to assign a human-friendly random DNS label and forms `<label>.tunnel.zarvis.ai`. Provider identity is never encoded in the public URL.

The tunnel owner authenticates before registering a name. Registration produces a short-lived capability limited to one runtime reverse endpoint. The hosted service keeps active routes only in memory and may materialize generated restriction data only on ephemeral storage; it does not persist users, tunnels, names, or access-control lists.

Owner authentication is an interactive browser handoff initiated by the running Construct daemon. The service creates a short-lived authorization request with separate high-entropy browser and polling capabilities. Construct opens the browser capability and retains the polling capability only in process memory. After social login, the polling capability returns an owner credential exactly once. The credential is consumed directly for registration and is never displayed, copied, configured through an environment variable, or written to disk.

The public tunnel ready screen shows the public URL and QR but not the remote listener's Basic username or password. Those are gateway-to-upstream credentials for this provider; public visitors authenticate socially and never enter them. LAN and providers without a credential-injecting gateway continue to display the Basic credentials.

The hosted service is deployed independently on Oracle Cloud infrastructure. It is not part of the `zarvis.ai` web deployment. DNS delegates `tunnel.zarvis.ai` and `*.tunnel.zarvis.ai` to the tunnel service's reserved public address.

The same social identity that owns the tunnel is the authorization boundary for browser access. The service records the owner's OAuth provider plus immutable provider subject on the active in-memory route. A visitor authenticates with GitHub or Google, and access is allowed only when the identity in the signed login session equals the route owner. Sharing and persistent ACLs are non-goals until they have an explicit product design.

Display names, usernames, and email addresses are not identity keys. The provider and immutable provider subject are stored only in signed capabilities, signed browser sessions, and active in-memory route mappings. Changing providers intentionally produces a different identity.

Assigned labels use lowercase ASCII letters, digits, and interior hyphens, with the DNS 63-byte limit. The service generates names during registration and retries atomically on active-name collisions, so every active hostname is globally unique. Construct supplies an ephemeral instance identifier that remains stable for one supervised tunnel run; reconnecting that instance atomically replaces its reverse endpoint and capability while retaining its assigned name.

The interactive client does not ask for a name. Selecting the provider begins browser authorization, and the service returns the assigned name with the completed registration.

Construct links the `wstunnel` Rust library at a pinned upstream revision and runs the client inside the daemon's supervised async task. No external `wstunnel` executable, PATH entry, environment override, or subprocess is part of the first-party provider.

## Reason

Provider subjects are stable and do not require an identity database. Keeping identity out of hostnames makes URLs shorter and prevents the public URL from carrying even a pseudonymous account identifier. Owner-equals-visitor authorization gives social login a precise meaning without inventing an invitation system.

Runtime allocation avoids deterministic TCP-port collisions. Short-lived, narrowly scoped registration capabilities keep `wstunnel` from opening arbitrary reverse endpoints. Losing runtime state on restart is safe because supervised clients register again.

## Consequences

- The client starts browser authorization immediately after provider selection and accepts the service-assigned name without exposing the owner token to the user.
- Pending authorization requests are memory-only, single-use, and expire after ten minutes. Losing service state requires starting the login flow again and grants no durable access.
- Stopping or restarting the daemon cancels the in-process tunnel. Because authorization capabilities are not persisted, reconnecting after a restart is an explicit `/remote-connect` plus browser authorization rather than an automatic background login.
- The service must validate the capability on the `wstunnel` upgrade and restrict its reverse bind to the allocated endpoint.
- A public hostname is not reported ready until the gateway can reach its reverse endpoint.
- Service restarts may briefly interrupt tunnels, but no database restore is required; clients reconnect and register again.
- OAuth client secrets and the session-signing key are operational secrets, not persisted user or tunnel data.
- The tunnel service has its own deployment lifecycle; changing the `zarvis.ai` web application does not deploy or configure it.

## Non-Goals

- Cross-account sharing, teams, invitations, and durable ACLs.
- Reserving a label while its owner is offline.
