# 0095-first-party-named-tunnels

Status: accepted
Date: 2026-07-14
Area: architecture
Scope: Construct's first-party tunnel names, identity, authorization, and runtime state

## Decision

Construct's first-party tunnel provider uses a user-selected DNS label and a deterministic, provider-scoped user identifier to form `<label>.<user-id>.tunnel.zarvis.ai`.

The tunnel owner authenticates before registering a name. Registration produces a short-lived capability limited to one runtime reverse endpoint. The hosted service keeps active routes only in memory and may materialize generated restriction data only on ephemeral storage; it does not persist users, tunnels, names, or access-control lists.

The hosted service is deployed independently on Oracle Cloud infrastructure. It is not part of the `zarvis.ai` web deployment. DNS delegates `tunnel.zarvis.ai` and `*.tunnel.zarvis.ai` to the tunnel service's reserved public address.

The same social identity that owns the tunnel is the initial authorization boundary for browser access. A visitor authenticates with GitHub or Google, and the service derives their user identifier from the provider plus immutable provider subject. Access is allowed only when that identifier equals the hostname's user-id. Sharing and persistent ACLs are non-goals until they have an explicit product design.

User identifiers are an HMAC of the provider and immutable provider subject under a server secret, encoded as a DNS-safe label. Display names, usernames, and email addresses are not identity keys. Changing providers intentionally changes the user-id.

Requested labels use lowercase ASCII letters, digits, and interior hyphens, with the DNS 63-byte limit. Because the user-id namespace separates owners, two users may choose the same label. One owner cannot run two live tunnels at the same label; a second registration replaces or rejects the first atomically.

## Reason

Provider subjects are stable and do not require an identity database. HMAC prevents public provider identifiers from being recoverable from hostnames. Owner-equals-visitor authorization gives social login a precise stateless meaning without inventing an invitation system.

Runtime allocation avoids deterministic TCP-port collisions. Short-lived, narrowly scoped registration capabilities keep `wstunnel` from opening arbitrary reverse endpoints. Losing runtime state on restart is safe because supervised clients register again.

## Consequences

- The client needs a social-login owner token and a selected label before starting the provider.
- The service must validate the capability on the `wstunnel` upgrade and restrict its reverse bind to the allocated endpoint.
- A public hostname is not reported ready until the gateway can reach its reverse endpoint.
- Service restarts may briefly interrupt tunnels, but no database restore is required; clients reconnect and register again.
- OAuth client secrets, the HMAC identity key, and the session-signing key are operational secrets, not persisted user or tunnel data.
- The tunnel service has its own deployment lifecycle; changing the `zarvis.ai` web application does not deploy or configure it.

## Non-Goals

- Cross-account sharing, teams, invitations, and durable ACLs.
- Reserving a label while its owner is offline.
- Embedding `wstunnel` into the Construct executable; the initial client uses the separately installed binary.
