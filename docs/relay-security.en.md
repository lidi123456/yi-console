# Relay authentication and end-to-end encryption

中文：[中继鉴权与端到端加密方案](relay-security.md)

This document explains how Yi Console authenticates and encrypts connections that go through a relay. For deployment and usage steps, see [Remote access and relay](remote.en.md).

## 1. Design goals

The relay lets a phone or another computer reach a computer behind NAT. The relay server may be run by someone else, or may be compromised, so the design doesn't trust it:

1. **The relay can't see content**: data is end-to-end encrypted; the relay only forwards ciphertext.
2. **The relay can't impersonate anyone**: devices and clients are identified by public keys. The relay stores only public keys, never a secret it could use directly.
3. **Forward secrecy**: even if a long-term key leaks later, previously recorded traffic can't be decrypted.
4. **Each client can be revoked on its own**: every client has its own keys. Pairing QR codes contain no long-term secret, so they're harmless once used.
5. **No secrets in URLs**: nothing ends up in a reverse proxy's access log.
6. **Abuse resistance**: before connecting the two ends, the relay checks that the client is authorized by that device, and only computers invited by the operator can register with the relay.

## 2. Overview

### 2.1 Roles and keys

| Who | Key | Purpose | Stored in |
|---|---|---|---|
| yi-agentd (device) | Ed25519 signing key `agent_sign` | Proves to the relay that it is this device | `~/.yi-console/relay.json` (0600) |
| yi-agentd | X25519 static key `agent_noise` | End-to-end Noise handshake; clients recognize the device by its public key | same |
| Each client | Ed25519 signing key `client_sign` | Proves to the relay that it is an authorized client | the client's host config (`HostConfig.pairing`, hidden in the UI) |
| Each client | X25519 static key `client_noise` | End-to-end Noise handshake; yi-agentd recognizes the client by it | same |
| Relay operator | single-use invite code `invite` | Lets a new computer register its public key with the relay; void once used or expired. **Clients don't need it** | the relay stores only a hash (`relay-invites.json`); yi-agentd clears it after registering |
| Relay | list of registered devices (device public keys) | Decides which yi-agentd instances may attach to this relay; each can be revoked | `relay-devices.json` (public keys only) |
| Relay | TLS certificate | Transport encryption; with a self-signed certificate, clients pin its fingerprint | relay data directory |
| Ephemeral | single-use pairing code `code` (32 bytes) | Used when a new client pairs for the first time; valid for 10 minutes, void once used | only in yi-agentd's memory and in the QR code |

Signing uses Ed25519 and encryption uses X25519. By convention they are two separate key pairs; neither is converted into the other. In code, one signing pair plus one Noise pair is an `Identity` (`crates/link/src/lib.rs`).

**Device ids are derived from public keys**: `device = hex(SHA-256(agent_sign public key)[0..16])`. Given the public key, the relay can recompute the id to confirm whose it is, so no first-come-first-served registry is needed. Client ids work the same way: `client_id = hex(SHA-256(client_sign public key)[0..8])`.

### 2.2 What each layer is responsible for

```
 client ──TLS──▶ relay ◀──TLS── yi-agentd
   │   ① relay-level auth: challenge-signature, │
   │     checked against the allow list the    │
   │     device pushes                         │
   │                                           │
   └────── ② end-to-end Noise IK ──────────────┘
          the relay only forwards ciphertext
```

- **① Relay level**: decides "may you pass through the relay". The relay verifies signatures and checks the allow list; it **can't see** message content.
- **② End to end**: decides "may you talk to this device", and handles encryption. Even if the relay is compromised and the allow list tampered with, nobody without the right Noise keys can reach yi-agentd.

The two layers are independent. ① mainly prevents abuse and resource exhaustion; ② provides security.

## 3. Relay-level authentication

### 3.1 Challenge and signature

The URLs of the three endpoints `/v1/agent`, `/v1/client` and `/v1/stream` take no parameters. After the WebSocket upgrade, **the first messages are authentication**:

```
relay → peer   {"type":"challenge","v":2,"nonce":"<32 random bytes, b64>"}
peer  → relay  {"type":"auth", ...see table... , "sig":"<Ed25519 signature, b64>"}
relay → peer   {"type":"ok"}             or the connection is closed (see close codes below)
```

What gets signed (fields are length-prefixed to avoid ambiguity):

```
"yi-console relay auth v2" ‖ role ‖ device ‖ stream ‖ nonce
```

| role | Endpoint | Fields in the auth message | How the relay decides |
|---|---|---|---|
| `agent` | `/v1/agent` | `device`, `pub` (agent_sign public key), `sig`; plus `invite` on first registration | device id equals hash(pub); the signature is valid; pub is in the registered device list, or `invite` is valid (registration then completes, see 3.3) |
| `stream` | `/v1/stream` | `device`, `pub`, `stream` (stream id issued by the relay), `sig` | the device is registered, the signature is valid, and `stream` is a stream this device is expected to open |
| `client` | `/v1/client` | `device`, `pub` (client_sign public key), `sig`; plus `ticket` on first pairing | the signature is valid **and** pub is in the device's client allow list, or `ticket` is among the device's valid pairing tickets |

Close codes (`crates/link/src/auth.rs`): 4401 unauthorized, 4403 forbidden, 4004 device offline, 4008 peer not responding, 4029 too many connections, 4410 revoked.

Why messages after the upgrade rather than URL parameters or HTTP headers:

- They never appear in URLs, Referer headers or reverse-proxy access logs (most proxies log only the request line by default, but many people log headers too).
- The relay sends a fresh nonce on every connection, so a signature is valid only for that connection; intercepting it doesn't allow a replay.
- Browser and Android WebSocket APIs differ in their support for custom headers; messages work the same everywhere.

Authentication has its own 10-second limit. Failures fall into two groups:

- Bad signature, malformed data, timeout, or a wrong invite code or pairing ticket: **counted against the IP** (`Limits::record_fail`), which is banned once it reaches the threshold.
- Identity proven but not permitted (a removed device, an unregistered computer, the device offline): **not counted**. Removed devices keep reconnecting automatically and often share a NAT with other devices; counting them would get every device behind that IP banned. Clients that hit this kind of refusal also stop reconnecting automatically.

### 3.2 Client allow list (ACL) on the relay

Over its authenticated control connection, yi-agentd pushes the device's allow list to the relay:

```json
{"type":"acl",
 "clients":["<client_sign public key b64>", "..."],
 "tickets":[{"hash":"<b64>","exp":1762070400}]}
```

- The full list is pushed every time the control connection is established, and again on pairing, revocation, and pairing-code expiry.
- The relay keeps it **in memory only**, never on disk. While the device is offline clients can't connect anyway; after a relay restart, yi-agentd reconnects and pushes it again.
- When a new ACL arrives, the relay **immediately disconnects** streams of clients no longer on the list (it remembers which client public key each spliced pair belongs to).

All the relay sees are public keys, which aren't secret.

### 3.3 Who can attach a computer to the relay: invite registration

Clients (phones) can't connect to the relay at will: the relay only lets through client public keys on the device's allow list (3.2), or clients holding a valid pairing ticket, and both can only be issued by a registered yi-agentd. So the only admission question the relay itself has to answer is: **which yi-agentd instances may register with this relay**.

This follows the same model as Tailscale auth keys and NetBird setup keys: **a single-use invite code is only for registration; after that the device relies on its own key**. There is no long-lived token shared by all devices.

**Invite codes**

- Created by the operator on the relay server: `yi-relay invite [--ttl 24h] [--uses 1] [--note <note>]`. By default single-use, expiring after 24 hours.
- Prints an access link `yiconsole://relay?v=2&url=<relay address>&fp=<cert fingerprint>&invite=<code>` along with a QR code. The address in the link comes from the relay's `--public-url` option; without it only the invite code is printed.
- The relay stores only the SHA-256 of the invite code (`relay-invites.json`) and compares hashes in constant time.
- The first time the relay starts with no devices and no invites, it creates a single-use invite valid for 24 hours and prints it in the startup log, much like RustDesk prints its public key or Jenkins prints its initial admin password on first start.

**Registration**

1. Paste the access link on the computer (or run `yi-agentd relay setup <link>`).
2. yi-agentd connects to the relay and, after the challenge, includes `invite` and its signature in the `auth` message.
3. The relay verifies the signature and the invite, writes `{device, pub, note, registration time, source invite}` to `relay-devices.json`, and decrements the invite's remaining uses.
4. yi-agentd clears the invite code. From then on every connection uses only signatures, with no shared secret.

**Device-issued invites (enabling the relay on another computer of yours)**

- A registered yi-agentd can ask the relay, over the control connection, for a single-use invite valid for 10 minutes: `{"type":"invite"}` → `{"type":"invite","code":"..."}`.
- "Use <relay>" in the app (`relay.reuse`) relies on this; see section 8.
- **Off by default**; the operator turns it on with `--device-invites`. When it's off, invites can only be created on the relay server, and the UI tells the user to run `yi-relay invite` there.
- The relay records which device issued each invite.

**Management commands** (run on the relay server; they read and write the JSON files in the data directory directly. A running relay checks the files for changes every 5 seconds and also rereads them on registration.)

- `yi-relay devices`: list registered devices (device id, note, registration time, source, last seen).
- `yi-relay revoke <device id> [--cascade]`: revoke a device. If it's online it's disconnected immediately (control connection and all streams) and can never connect again. With `--cascade`, devices registered through invites it issued are revoked too.
- `yi-relay invites [--revoke <id>]`: list or revoke invite codes.

**Open mode**

- `--open`: anyone can register without an invite. Meant for publicly open relays like Syncthing's public relays; the startup log shows a warning.
- Off by default.

**What can't be avoided**: anyone can complete TCP, TLS and the WebSocket upgrade and receive a challenge. The defenses: authentication must finish within 10 seconds; failures count against the IP, which is banned at a threshold; there are global and per-IP connection limits. All of these apply to unregistered connections.

**Alternatives considered and rejected**

| Approach | Example | Why not |
|---|---|---|
| Fixed shared token | frp `auth.token` | Valid forever and shared by every device: a leak means replacing it everywhere, and a single device can't be revoked |
| Time-limited HMAC credentials: the relay and an issuer share a secret; credential = HMAC(secret, expiry:user) | TURN REST API (coturn `use-auth-secret`) | Needs an online issuing service, and the issuing secret is itself a fixed shared secret |
| Signed credential chain: the relay trusts only the operator's public key, and the operator signs device credentials offline | NATS operator / JWT mode | The relay doesn't need a device list, but revocation needs a separate deny list, and the credentials and signing tools would have to be built; too heavy for a self-hosted personal relay |
| External admission callback or OIDC | Tailscale `--verify-client-url`, frp OIDC | Requires deploying an identity service; could be added later as an extension |

### 3.4 Resource limits

- At most 32 concurrent streams per device, and **at most 8 per client public key**, so one client can't use up a device's slots.
- At most 2 concurrent connections per device that were opened with a pairing ticket.
- A global connection limit (`--max-conns`, default 1000) and a per-IP limit (`--max-per-ip`, default 32); an IP with 10 failures within 10 minutes is banned for 10 minutes.

### 3.5 Behind a reverse proxy such as Nginx or Caddy

To the outside, the relay is plain HTTP/1.1 WebSocket (`/v1/agent`, `/v1/client`, `/v1/stream`), so it can sit behind a reverse proxy that handles TLS with a real certificate, with the relay listening on `127.0.0.1` without TLS. All authentication happens inside WebSocket messages, so no secrets appear in the proxy's access logs.

**The real client IP**: the relay applies per-IP connection limits and failure bans by client IP. Behind a proxy every connection appears to come from `127.0.0.1`, which turns the per-IP limit into a global limit and means one person's repeated auth failures get **everyone** banned.

Hence `--trusted-proxy <IP or CIDR, repeatable>` (for example `127.0.0.1`, `::1`). Only when the TCP peer is in this list does the relay read `X-Forwarded-For`, taking the rightmost address that isn't a trusted proxy as the client IP. Otherwise the header is ignored, so direct connections can't spoof their IP.

So rate limiting and banning happen in two steps:
- at TCP connect, only the global count, by TCP peer;
- after the WebSocket upgrade, once headers are available, per-IP counting and ban checks by real IP.

Requirements on the proxy side (see [Remote access and relay](remote.en.md#deploying-a-relay) for Nginx and Caddy examples):
- pass through the `Upgrade` / `Connection` headers; Nginx needs `proxy_http_version 1.1`;
- read/write timeouts longer than 90 seconds (the control connection's heartbeat timeout); 1 hour is recommended;
- disable response buffering;
- set `X-Forwarded-For`. Caddy's `reverse_proxy` does this by default.

With a real CA certificate, `fp` in the pairing info is left empty and clients verify against the system CAs. The relay address may include a path prefix, such as `wss://example.com/hub`; the proxy strips the prefix before forwarding to the relay, and clients append `/v1/...` to the address.

## 4. End-to-end encryption: Noise IK

Uses `Noise_IK_25519_ChaChaPoly_SHA256` from the [Noise Protocol Framework](https://noiseprotocol.org/noise.html), implemented with the `snow` crate, instead of a home-grown handshake.

- **IK** means the client knows the device's static public key in advance (I = the client's static key is sent in the first message, K = the peer's static key is known). The handshake takes one round trip.
- The prologue is `"yi-console e2e v2" ‖ device`, binding the handshake to this device.

```
client                                                 yi-agentd
  │ -> e, es, s, ss   [payload: client name, client_sign public key] │
  │ ──────────────────────────────────────────────────────────────▶ │  checks the client's static key is in the paired list
  │ <- e, ee, se                                                    │
  │ ◀────────────────────────────────────────────────────────────── │
  │       after that, each WebSocket binary frame = one Noise transport message  │
```

Security properties:

- **Mutual authentication**: the client confirms it's talking to this device using the `agent_noise` public key from the QR code; yi-agentd confirms the client against its paired list.
- **Forward secrecy**: both sides generate an ephemeral key `e` per connection, and the session keys depend on the `ee` DH result. Even if long-term keys leak later, previously recorded traffic can't be decrypted.
- **Client identity hidden from the relay**: the client's Noise static key is already encrypted in the first message.
- **Ordering and integrity**: Noise transport messages carry an incrementing nonce; tampered, replayed or reordered frames fail to decrypt and the connection is closed.

A Noise message is at most 65535 bytes, so data is sent in 60 KiB chunks.

yi-agentd also checks that the `client_sign` public key in the payload and the key the relay reports in its `open` message belong to the same paired client, so the relay's allow list and the end-to-end identity agree.

## 5. Pairing

The pairing QR code contains **only the device public key and a single-use pairing code**, no long-term secret:

```
yiconsole://pair?v=2&relay=<wss://...>&fp=<cert fingerprint>&device=<device id>
               &apk=<agent_noise public key b64>&code=<single-use pairing code b64>&name=<device name>
```

```mermaid
sequenceDiagram
    participant U as User (on the computer)
    participant A as yi-agentd
    participant R as Relay
    participant C as Phone
    U->>A: click "Generate pairing QR code"
    A->>A: generate code (valid for 10 minutes)
    A->>R: acl push: tickets += SHA256("ticket"‖code)
    A-->>U: show QR code
    C->>C: scan, generate its own client_sign / client_noise
    C->>R: /v1/client, auth (role=client, pub, ticket, sig)
    R->>R: ticket hash matches and hasn't expired → let through
    R->>A: {"type":"open","stream":sid,"client":pub,"pairing":true}
    A->>R: /v1/stream, auth
    R-->>C: both ends spliced
    C->>A: Noise IKpsk1 (psk = HKDF(code, "psk"))
    A->>A: handshake succeeds = proof of holding code → record client, void code
    A->>R: acl push: clients += pub, tickets -= this ticket
    Note over C,A: from then on plain Noise IK; code no longer needed
```

Key points:

- **The relay only knows `SHA256("ticket"‖code)`**, from which the Noise psk can't be derived, so the relay can't impersonate yi-agentd during pairing or pair a client of its own.
- Pairing uses `IKpsk1`, which mixes the psk in at the end of IK's first message. psk1 rather than psk2 lets yi-agentd confirm the peer holds the pairing code when it reads the first message, and only then add it to the paired list; with psk2 the check happens in the second message, so yi-agentd would add an unverified client first. Without the code the handshake fails, and without the `agent_noise` private key nobody can impersonate yi-agentd.
- The code is single-use, expires after 10 minutes, lives only in yi-agentd's memory, and is void if yi-agentd restarts. **A used QR code is harmless even if it leaks.**
- After pairing, the client saves a `yiconsole://paired?v=2&...` credential containing relay, fp, device, apk and name, plus its own two private keys. Because it contains private keys, it's hidden in the UI (`REDACTED`).

## 6. Revocation and rotation

| Action | How | Effect |
|---|---|---|
| Revoke one phone | yi-agentd removes it from the paired list → pushes a new ACL → the relay disconnects its streams; yi-agentd also closes its local connections | affects only that phone |
| Reset all pairings | `yi-agentd relay reset` / "More → Reset all pairings" in the UI: clears the paired list, voids unused pairing codes, and replaces `agent_noise` | every client must pair again; `agent_sign` (the device id) is unchanged, so no need to re-register with the relay |
| Relay revokes a computer | the operator runs `yi-relay revoke <device id>`; if online it's disconnected immediately | affects only that computer (plus computers it invited, with `--cascade`) |
| Invite code leaked | `yi-relay invites --revoke <id>`; revoke each device already registered with it | other devices unaffected |
| Relay certificate changed | set the new fingerprint | the fp in pairing info must be updated |

Related RPCs and CLI commands on yi-agentd:

- `relay.pairStart` → `{uri, expiresAt}`; the UI shows the QR code and a countdown
- `relay.clients` → `[{id, name, createdAt, lastSeen, online}]`
- `relay.revoke {id}`, `relay.renameClient {id, name}`
- `relay.reset`
- `relay.invite`: ask the relay for a device-issued invite (see 3.3)
- CLI: `yi-agentd relay pair` (prints a single-use QR code), `yi-agentd relay clients`, `yi-agentd relay revoke <id>`, `yi-agentd relay reset`

## 7. What the relay can see and do

| | |
|---|---|
| Message content | can't see it |
| Long-term secrets | public keys only; invite codes stored as hashes, void once used |
| Record traffic and decrypt it after obtaining long-term keys | no (forward secrecy) |
| Impersonate a device or client | no (no private keys) |
| Know which client is connecting | knows the client_sign public key (a random identifier unrelated to real identity) |
| Metadata (IP, timing, traffic volume) | can see it (out of scope for this design) |
| Disconnect or deny service | yes (inherent to any relay) |

## 8. Forwarding through a computer and "Use <relay>"

- **Forwarding through a computer** (`proxy.setHosts`): the client on the computer hands its credential (with private keys) to the local yi-agentd, which connects to the target device on the client's behalf. The credential only travels locally. The target device treats it as that computer's client, and revoking that computer revokes this too.
- **Use <relay>** (`relay.reuse`): the client calls the RPC `relay.invite` on the source host (only authorized clients can). The source yi-agentd asks the relay for a single-use invite, builds an access link and returns it, and the client passes it to `relay.configure` on the target host. So the source host must be online, and the relay must have `--device-invites` enabled (off by default).
- **`relay.known`**: lists the relay address and fingerprint from saved credentials, deduplicated, for choosing in "Use <relay>".
- There is no reusable long-term access link: every computer registers with its own invite.

## 9. Where the code lives

**`crates/link`** (protocol shared by both ends)
- `lib.rs`: `Identity` (signing + Noise key pairs), `device_id_of` / `client_id_of`, and the three links: `RelayAccess` (`yiconsole://relay`), `PairOffer` (`yiconsole://pair`), `Credential` (`yiconsole://paired`).
- `auth.rs`: relay-level auth: the challenge message, `signed_payload`, `verify`, `ticket_of` / `psk_of` / `invite_hash`, close codes.
- `e2e.rs`: Noise IK / IKpsk1 handshakes and encrypted forwarding: `client_connect`, `client_pair`, `agent_accept` (returns the peer's client_sign public key for yi-agentd to check).
- `wire.rs`: WebSocket connections (certificate pinning, proxies), the yi-agentd control connection `agent_control`, control messages `next_control` / `send_control`.

**`crates/relay`** (`yi-relay`)
- `hub.rs`: post-upgrade auth, per-role checks, ACLs and pairing tickets, stream splicing, per-device / per-client stream limits, handling of `acl` and `invite` control messages.
- `store.rs`: `relay-devices.json` (device public keys), `relay-invites.json` (invite hashes, expiry, remaining uses, source), `relay-meta.json`; checks for file changes every 5 seconds and disconnects revoked online devices immediately.
- `limits.rs`: global / per-IP connection limits, failure counting and bans, real-IP resolution for `--trusted-proxy`.
- `main.rs`: options and the `invite`, `invites`, `devices` and `revoke` subcommands; creates an invite on first start.

**`crates/yi-agentd`**
- `relay.rs`: `RelayConfig` (`relay.json`: enabled, url, relay_fp, name, identity, clients, and an unused invite); registration, the control connection, ACL pushes, pairing codes, IK or IKpsk1 on accept, closing connections on revoke, last-seen updates.
- `server.rs`: the RPCs in section 6; `main.rs`: the `relay` subcommands; `proxy.rs`: reads client credentials when forwarding through a computer.

**`crates/client`**
- `api.rs`: adding a relay host (scan / paste pairing link → `client_pair` → save credential), `relay.parseLink`, `relay.known`, `relay.reuse`.
- `hosts.rs`: the `pairing` field holds the `Credential` link and is hidden before being sent to the frontend.

**`app`**
- `HostsView.tsx`: the "Make this computer reachable" card (connect to a relay, generate a pairing QR code, paired devices) and "Pair device".

## 10. What to test

After changing relay code, cover at least the following:

- `link` unit tests: round-trip parsing of credentials and pairing links; device ids derived from public keys; successful IK and IKpsk1 handshakes; failure with a wrong psk, a wrong device public key, or an unauthorized client; tampered and replayed frames fail to decrypt.
- `relay` integration tests (in-process relay + simulated yi-agentd + simulated client):
  - the full flow works with no URL parameters at all;
  - unauthorized client public keys are refused, counted as failures, and banned at the threshold;
  - expired or wrong tickets are refused;
  - after a new ACL push, connections of revoked clients are dropped;
  - a yi-agentd without an invite can't register; invites are void once used up or expired; after registering, it connects without an invite;
  - after `yi-relay revoke`, an online device is disconnected and can't reconnect; `--cascade` works;
  - device-issued invites are refused by default and work with `--device-invites`; `--open` mode needs no invite;
  - the per-client stream limit is enforced;
  - with `--trusted-proxy`, rate limiting and bans use `X-Forwarded-For`, and connections from untrusted addresses can't spoof it;
  - the full flow works behind Nginx (connections stay up after more than 2 minutes idle).
- End to end: enable the relay on a desktop → pair a phone by QR code → chat → revoke → the phone can't connect; pair another one; forwarding through a computer; port forwarding; automatic recovery after restarting yi-agentd and the relay separately.

## 11. Protocol version

`v=2` in links, `"v":2` in the challenge, and `"yi-console relay auth v2"` / `"yi-console e2e v2"` in signatures and the handshake are the protocol version. Changes to the handshake or message format must be made on all three sides (relay, yi-agentd, client), and the version must be bumped when it breaks compatibility with older releases.
