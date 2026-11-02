# Relay authentication and end-to-end encryption, v2

Status: implemented　|　Date: 2026-11-02　|　Not compatible with v1; all devices must re-pair after upgrading

## 1. Why the change

How v1 worked (`crates/link/src/lib.rs`, `crates/relay/src/main.rs`):

| Layer | v1 credential | Problem |
|---|---|---|
| Connecting to the relay | One `--token` shared by everyone | Written into every pairing link, so a single device can't be revoked; without it configured, anyone could use the relay |
| yi-agentd's identity on the relay | Device id + `device_token`, recorded on first connection (TOFU) | The token is sent to the relay in the clear on every connection |
| Client's identity on the relay | None (just the shared token + device id) | Anyone with the token and device id could keep requesting streams and use up the device's 32 slots |
| End to end | All clients share one pairing key K; HMAC mutual challenge, then derived ChaCha20-Poly1305 keys | No forward secrecy: once K leaks, all recorded traffic can be decrypted; individual clients can't be revoked |
| Transport | `token`, `auth` and `stream` in the URL query | Ends up in reverse proxy access logs |

Goals for v2:

1. No secrets in URLs (row 5 above).
2. Before splicing two ends together, the relay confirms the client was authorized by this device (row 3).
3. Devices and clients are identified by public keys; the relay stores only public keys and nothing that could be used directly as a secret (row 2).
4. Each client has its own keys and can be revoked individually; pairing QR codes no longer carry long-term secrets; the end-to-end handshake provides forward secrecy (rows 1 and 4).

## 2. Overall design

### 2.1 Roles and keys

| Who | Key | Purpose | Stored in |
|---|---|---|---|
| yi-agentd (device) | Ed25519 signing key `agent_sign` | Proves to the relay that it is this device | `relay.json` (0600) |
| yi-agentd | X25519 static key `agent_noise` | End-to-end Noise handshake; clients recognize the device by its public key | `relay.json` |
| Each client | Ed25519 signing key `client_sign` | Proves to the relay that it is an authorized client | The client's host config (`HostConfig.pairing`, hidden in the UI) |
| Each client | X25519 static key `client_noise` | End-to-end Noise handshake; yi-agentd recognizes the client by it | Same as above |
| Relay operator | One-time invite code `invite` (replaces the fixed `--token`) | Lets a new computer register its public key with the relay; void once used or expired. **Clients don't need it** | The relay stores only a hash (`relay-invites.json`); yi-agentd discards it after use |
| Relay | List of registered devices (device public keys) | Decides which yi-agentd instances may attach to this relay; individually revocable | `relay-devices.json` (public keys only) |
| Relay | TLS certificate | Transport encryption; clients pin it by fingerprint when self-signed | Relay data directory |
| Ephemeral | One-time pairing code `code` (32 bytes) | Used for a new client's first pairing; valid for 10 minutes, void once used | Only in yi-agentd's memory and the QR code |

Ed25519 for signing and X25519 for encryption, kept as two separate key pairs by convention, without converting between them.

**The device id is derived from the public key**: `device = hex(SHA-256(agent_sign public key)[0..16])`. Given the public key, the relay can recompute the id to confirm whose it is, so v1's "first to register owns it" (TOFU) token registry is no longer needed. `relay-devices.json` is kept, but now holds the public keys of devices registered through invites (see 3.3). Client ids work the same way: `client_id = hex(SHA-256(client_sign public key)[0..8])`.

### 2.2 What each of the two authentication layers covers

```
 client ──TLS──▶ relay ◀──TLS── yi-agentd
   │   ① relay layer: challenge-signature;   │
   │     the relay checks the device's       │
   │     pushed allow list                   │
   │                                         │
   └────── ② end-to-end Noise IK ────────────┘
          the relay only forwards ciphertext
```

- **① Relay layer**: decides "may you pass through the relay". The relay verifies signatures and checks allow lists, and **cannot see** message contents.
- **② End to end**: decides "may you talk to this device", and handles encryption. Even if the relay is compromised and its allow lists tampered with, nobody without the right Noise keys can reach yi-agentd.

The two layers are independent. ① mainly prevents abuse and resource exhaustion; ② provides security.

## 3. Relay-layer authentication (goals 1, 2, 3)

### 3.1 No secrets in URLs

The URLs of the three endpoints `/v1/agent`, `/v1/client` and `/v1/stream` take no parameters. After the WebSocket upgrade, **the first messages are authentication**:

```
relay → peer   {"type":"challenge","v":2,"nonce":"<32 random bytes, b64>"}
peer → relay   {"type":"auth", ...see table below... , "sig":"<Ed25519 signature, b64>"}
relay → peer   {"type":"ok"}             or close (4401 unauthorized / 4403 refused / 4004 device offline)
```

What is signed (fields concatenated with length prefixes to avoid ambiguity):

```
"yi-console relay auth v2" ‖ role ‖ device ‖ stream ‖ nonce
```

| role | Endpoint | Fields in the auth message | How the relay decides |
|---|---|---|---|
| `agent` | `/v1/agent` | `device`, `pub` (agent_sign public key), `sig`; plus `invite` on first registration | device id equals hash(pub); the signature is valid; pub is in the registered device list, or `invite` is valid (registration then completes, see 3.3) |
| `stream` | `/v1/stream` | `device`, `pub`, `stream` (stream id issued by the relay), `sig` | the device is registered, the signature is valid, and `stream` is a stream this device is expected to open |
| `client` | `/v1/client` | `device`, `pub` (client_sign public key), `sig`; plus `ticket` on first pairing | the signature is valid **and** pub is in the device's client allow list, or `ticket` is among the device's valid pairing tickets |

Why messages after the handshake rather than HTTP headers:

- They never appear in URLs, Referers or reverse proxy access logs (most proxies only log the request line by default, but many people log headers too).
- The relay sends a fresh nonce for every connection, so a signature is only valid for that connection and can't be replayed if intercepted.
- Browser and Android WebSocket APIs differ in their support for custom headers; messages work the same everywhere.

Authentication has its own 10-second limit. Failures come in two kinds: bad signatures, malformed messages, timeouts, and wrong guesses of invite codes or pairing tickets **count toward the IP's failures** (`Limits::record_fail`), and repeated failures get the IP banned; a proven identity without permission (a removed device, an unregistered computer, a device offline) **doesn't count**. A removed device keeps auto-reconnecting and often sits behind the same NAT as other devices; counting it would ban every device on that IP (this actually happened in end-to-end testing). Clients that hit such a refusal also stop auto-reconnecting.

### 3.2 Client allow list (ACL) on the relay

Over its authenticated control connection, yi-agentd pushes this device's allow list to the relay:

```json
{"type":"acl",
 "clients":["<client_sign public key b64>", "..."],
 "tickets":[{"hash":"<b64>","exp":1762070400}]}
```

- The full list is pushed every time the control connection is established, and again on pairing, revocation and pairing code expiry.
- The relay keeps it **in memory only**, never on disk. When the device is offline, clients can't connect anyway; after a relay restart, yi-agentd reconnects and pushes again.
- On receiving a new ACL, the relay **immediately drops** streams of clients no longer on the list (it remembers which client public key each splice belongs to).

All the relay sees are public keys, which aren't secrets.

### 3.3 Who may attach a computer to the relay: invite registration instead of a fixed `--token`

First, note that **clients (phones) can no longer connect to the relay at will**. The relay only admits client public keys on the device's allow list (3.2), or clients holding a valid pairing ticket, and only a registered yi-agentd can issue either. So the only admission question left for the relay itself is: **which yi-agentd instances may register with this relay**.

v1 handled this with a fixed `--token`, which was valid forever, shared by all devices (a leak meant replacing it everywhere and reconfiguring every computer), and couldn't revoke a single device. v2 follows the same model as Tailscale auth keys and NetBird setup keys: **a one-time invite code is only for registration; after that the device relies on its own key**.

**Invite codes**

- Generated by the operator on the relay server: `yi-relay invite [--ttl 24h] [--uses 1] [--note <note>]`. By default single-use, expiring after 24 hours.
- Outputs an access link `yiconsole://relay?v=2&url=<relay address>&fp=<cert fingerprint>&invite=<invite code>` and prints a QR code. The relay's new `--public-url` option provides the address in the link; without it, only the invite code is printed.
- The relay stores only the SHA-256 of invite codes (`relay-invites.json`) and compares hashes in constant time.
- On the relay's first start, with no devices and no invites yet, it automatically generates a single-use invite valid for 24 hours and prints it in the startup log, much like RustDesk printing its public key or Jenkins printing the initial admin password on first start.

**Registration flow**

1. Paste the access link on the computer (or run `yi-agentd relay setup <link>`).
2. yi-agentd connects to the relay and, after receiving the challenge, includes `invite` and its signature in the `auth` message.
3. The relay verifies the signature, confirms the invite is valid, writes `{device, pub, note, registration time, source invite}` to `relay-devices.json`, and decrements the invite's remaining uses.
4. yi-agentd discards the invite code. Every later connection uses only its signature, with no shared secret.

**Device-issued invites (enabling the relay for another of your computers)**

- A registered yi-agentd can ask the relay over its control connection for a single-use invite valid for 10 minutes: `{"type":"invite"}` → `{"type":"invite","code":"..."}`.
- "One-click relay reuse" (`relay.reuse`) uses this (see section 8).
- **Off by default**; the operator enables it with `--device-invites`. When off, invites can only be issued on the server, and the UI's "one-click relay reuse" tells the user to run `yi-relay invite` on the relay server.
- The relay records which device issued each invite.

**Admin commands** (run on the relay server; they read and write the JSON files in the data directory directly; a running relay checks the files for changes every 5 seconds and also rereads them on registration)

- `yi-relay devices`: list registered devices (device id, note, registration time, source, last seen)
- `yi-relay revoke <device id> [--cascade]`: revoke a device. If online it's disconnected immediately (control connection and all streams) and can never connect again. With `--cascade`, devices registered through invites it issued are revoked too
- `yi-relay invites [--revoke <id>]`: list or void invite codes

**Open mode**

- `--open`: anyone can register directly without an invite. Meant for publicly open relays like Syncthing's public relays; the startup log shows a warning.
- Off by default. v1's behavior of "silently open to everyone when `--token` isn't configured" is gone.

**`--token` and `HUB_RELAY_TOKEN` are removed.** yi-agentd's `relay.json` no longer stores any relay token either.

**What still can't be avoided**: anyone can complete TCP, TLS and the WebSocket upgrade and receive a challenge. The defenses: authentication must complete within 10 seconds; failures count toward the IP and get it banned past a threshold; there are global and per-IP connection caps. All of these apply to unregistered connections.

**Alternatives considered and not adopted**

| Approach | Example | Why not |
|---|---|---|
| Fixed shared token | frp `auth.token`, v1's `--token` | Its problems are exactly what this change fixes |
| Time-limited HMAC credentials: the relay and an issuer share a key, credential = HMAC(key, expiry:user) | TURN REST API (coturn `use-auth-secret`) | Needs an online issuing service, and the issuing key is itself another fixed shared secret |
| Signed credential chains: the relay trusts only the operator's public key, and the operator signs device credentials offline | NATS operator / JWT mode | The relay needn't store a device list, but revocation needs a separate blocklist, and credentials and signing tools would have to be built; too heavy for a self-hosted personal relay |
| External admission callback or OIDC | Tailscale `--verify-client-url`, frp OIDC | Requires deploying an extra identity service; could be added later as an extension |

### 3.4 Resource limits

- At most 32 concurrent streams per device (unchanged), and **at most 8 per client public key**, so a single client can't use up a whole device's slots.
- At most 2 concurrent connections per device that came in with a pairing ticket.
- The existing global connection cap, per-IP cap and banning stay as they were.

### 3.5 Behind Nginx / Caddy or another reverse proxy

To the outside, the relay is plain HTTP/1.1 WebSocket (`/v1/agent`, `/v1/client`, `/v1/stream`), so it can sit behind a reverse proxy that handles TLS with a real certificate, with the relay listening on `127.0.0.1` without TLS. All v2 authentication happens inside WebSocket messages, so no secrets appear in the proxy's access logs.

**The problem that must be solved: the real client IP.** The relay applies the per-IP connection cap and failure bans by `peer.ip()`. Behind a proxy, every connection appears to come from `127.0.0.1`, so:

- the per-IP cap (default 32) becomes a global cap;
- one person's repeated authentication failures get **everyone** banned.

The fix: a new `--trusted-proxy <IP or subnet, repeatable>` option (e.g. `127.0.0.1`, `::1`). Only when the TCP peer is on this list does the relay read the `X-Forwarded-For` request header and take the rightmost address not on the trusted list as the client IP; otherwise the header is ignored, so direct connections can't forge their IP. Without `--trusted-proxy`, behavior is unchanged.

As a consequence, rate limiting and banning only know the real IP after the WebSocket upgrade, once the request headers are available:
- at TCP connect, only the global count by TCP peer applies;
- after the upgrade, the per-IP count and ban check use the real IP.

Requirements on the proxy side (the README gives Nginx and Caddy examples):
- forward the `Upgrade` / `Connection` headers; Nginx needs `proxy_http_version 1.1`;
- read/write timeouts longer than 90 seconds (the control connection's heartbeat timeout); 1 hour is recommended;
- disable response buffering;
- set `X-Forwarded-For`. Caddy's `reverse_proxy` does this by default.

With a real CA certificate, `fp` in the pairing info is left empty and clients verify against the system CAs. The relay address may include a path prefix, such as `wss://example.com/hub`; the proxy strips the prefix before forwarding to the relay, and clients append `/v1/...` to the address.

## 4. End-to-end encryption: Noise IK (goal 4 and forward secrecy)

Switch to `Noise_IK_25519_ChaChaPoly_SHA256` from the [Noise Protocol Framework](https://noiseprotocol.org/noise.html), implemented with the `snow` crate, instead of a home-grown handshake.

- **IK** means the client knows the device's static public key in advance (I = the client's static key is sent in the first message, K = the peer's static key is known). The handshake completes in one round trip.
- The prologue is `"yi-console e2e v2" ‖ device`, binding the handshake to this device.

```
client                                                    yi-agentd
  │ -> e, es, s, ss   [payload: client name, client_sign public key] │
  │ ───────────────────────────────────────────────────────────▶ │  check that the client static key is in the paired list
  │ <- e, ee, se                                                 │
  │ ◀─────────────────────────────────────────────────────────── │
  │        afterwards each WebSocket binary frame = one Noise transport message      │
```

Resulting security properties:

- **Mutual authentication**: the client confirms the peer is this device using the `agent_noise` public key from the QR code; yi-agentd confirms the client using its paired list.
- **Forward secrecy**: both sides generate ephemeral keys `e` for every connection, and the session keys depend on the `ee` DH result. Even if long-term keys leak later, previously recorded traffic can't be decrypted.
- **Client identity hidden from the relay**: the client's Noise static public key is already encrypted in the first message.
- **Ordering and integrity**: Noise transport messages carry incrementing nonces; tampering, replay or reordering all fail to decrypt, and the connection is dropped.

A Noise message is at most 65535 bytes, so `pump()` chunks at 60 KiB instead of 64 KiB.

yi-agentd also checks that the `client_sign` public key in the payload and the public key the relay reported in the `open` message belong to the same paired client, so the relay's allow list and the end-to-end identity agree.

## 5. Pairing flow

v1's QR code contained the long-term key K. v2's QR code contains **only the device public key and a one-time pairing code**:

```
yiconsole://pair?v=2&relay=<wss://...>&fp=<cert fingerprint>&device=<device id>
               &apk=<agent_noise public key b64>&code=<one-time pairing code b64>&name=<device name>
```

```mermaid
sequenceDiagram
    participant U as User (on the computer)
    participant A as yi-agentd
    participant R as Relay
    participant C as Phone
    U->>A: Click "Add device"
    A->>A: Generate code (valid 10 minutes)
    A->>R: acl push: tickets += SHA256("ticket"‖code)
    A-->>U: Show QR code
    C->>C: Scan; generate its own client_sign / client_noise
    C->>R: /v1/client, auth (role=client, pub, ticket, sig)
    R->>R: ticket hash matches and not expired → admit
    R->>A: {"type":"open","stream":sid,"client":pub,"pairing":true}
    A->>R: /v1/stream, auth
    R-->>C: Splice both ends
    C->>A: Noise IKpsk1 (psk = HKDF(code, "psk"))
    A->>A: Handshake succeeds = proof of holding code → record client, void code
    A->>R: acl push: clients += pub, tickets -= this ticket
    Note over C,A: From then on Noise IK only; code no longer needed
```

Key points:

- **The relay only knows `SHA256("ticket"‖code)`**, from which the Noise psk can't be derived, so the relay can neither impersonate yi-agentd to complete pairing nor pair a client of its own.
- Pairing uses `IKpsk1`, mixing the psk in at the end of the first IK message. psk1 rather than psk2 lets yi-agentd confirm the peer holds the pairing code as soon as it reads the first message, and only then add it to the paired list; psk2 would only verify on the second message, after yi-agentd had already added an unverified client. Without the code the handshake fails; without the `agent_noise` private key nobody can impersonate yi-agentd.
- The code is single-use, expires after 10 minutes, lives only in yi-agentd's memory, and is voided when yi-agentd restarts. **A used QR code is harmless even if leaked.**
- After pairing, the client saves the following as a `yiconsole://paired?v=2&...` credential: relay, fp, device, apk, name, and its own two private keys. The credential contains private keys and stays hidden in the UI (`REDACTED`).

## 6. Revocation and rotation

| Action | How | Impact |
|---|---|---|
| Revoke one phone | yi-agentd removes it from the paired list → pushes a new ACL → the relay drops its streams; yi-agentd also closes its connections on this machine | That phone only |
| Reset all pairings | `yi-agentd relay reset` / "More → Reset all pairings" in the UI: clear the paired list, void unused pairing codes, rotate `agent_noise` | All clients must re-pair; `agent_sign` (the device id) stays, so no re-registration with the relay is needed |
| Relay revokes a computer | The operator runs `yi-relay revoke <device id>`; if online it's disconnected immediately | That computer only (plus computers registered through its invites with `--cascade`) |
| Invite code leaked | `yi-relay invites --revoke <id>`; revoke any devices already registered with it one by one | No other devices affected |
| Relay certificate replaced | As before: set the new fingerprint | The fp in pairing info must be updated |

New RPCs and CLI:

- `relay.pairStart` → `{uri, expiresAt}`; the UI shows the QR code and a countdown
- `relay.clients` → `[{id, name, createdAt, lastSeen, online}]`
- `relay.revoke {id}`, `relay.renameClient {id, name}`
- `relay.reset`
- CLI: `yi-agentd relay pair` (prints a one-time QR code), `yi-agentd relay clients`, `yi-agentd relay revoke <id>`, `yi-agentd relay reset` (replaces `--reset-key`)

## 7. What the relay can see / do

| | v1 | v2 |
|---|---|---|
| Message contents | No | No |
| Long-term secrets | Sees the shared token and device tokens | Public keys only; invite codes stored as hashes and void after use |
| Record traffic and decrypt later with long-term keys | Yes | No (forward secrecy) |
| Impersonate a device or client | No (no K) | No (no private keys) |
| Know which client is connecting | No | Knows the client_sign public key (a random identifier unrelated to real identity) |
| Metadata (IP, timing, traffic volume) | Yes | Yes (out of scope for this design) |
| Disconnect or deny service | Yes | Yes (inherent to any relay) |

## 8. Effects on "forwarding through a computer" and "one-click relay reuse"

- **Forwarding through a computer** (`proxy.setHosts`): the client on the computer hands its v2 credential (with private keys) to the local yi-agentd, which connects to the target device on the client's behalf. Like handing over K in v1, this only happens locally. The target device treats it as that computer's client, and revoking that computer revokes this too.
- **One-click relay reuse** (`relay.reuse`): the client calls the new RPC `relay.invite` on the source host (only authorized clients can call it). The source yi-agentd asks the relay for a single-use invite and returns it as an access link, which the client passes to the target host's `relay.configure`. So the source host must be online and the relay must have `--device-invites` enabled (off by default).
- **`relay.known`**: deduplicates url and fp taken from credentials.
- `relay.pairing` no longer returns `accessUri`, since there's no reusable long-term access link anymore.

## 9. Change list

**`crates/link`**
- Dependencies: add `snow`, `ed25519-dalek`, `subtle`; remove `hmac` (`hkdf` stays for psk derivation).
- Split `Pairing` into `PairOffer` (QR code, `yiconsole://pair?v=2`) and `ClientCredential` (saved after pairing, `yiconsole://paired?v=2`).
- Add `AgentKeys` / `ClientKeys` (generation, serialization) and `device_id_of(pub)`.
- Relay layer: `relay_auth(ws, role, ...)`: receive the challenge, sign, wait for ok; URLs carry no parameters.
- End to end: remove `handshake_client` / `handshake_agent` / `Cipher`, replaced by `snow`-based `noise_initiator` / `noise_responder` (IK and IKpsk1); `pump()` uses the Noise transport state and 60 KiB chunks.
- `client_connect(cred)`, `client_pair(offer, name) -> ClientCredential`, `agent_control`, `agent_accept` (returns the peer's client_sign public key for yi-agentd to check).
- Control connection: `next_control` parses the new `open` message (with `client`, `pairing`); add `send_acl`.

**`crates/relay`**
- Remove URL query authentication and the old `relay-devices.json` registry.
- `accept_ws` only upgrades; afterwards `authenticate()` sends the challenge, verifies the signature and checks by role; failures call `record_fail`.
- `Device` gains `acl: HashSet<public key>`, `tickets: Vec<(hash, exp)>` and per-client active stream counts; `splice` records which client each splice belongs to and drops removed clients on ACL updates.
- The `open` message carries `client` and `pairing`.
- Remove `--token` / `HUB_RELAY_TOKEN`; add `--public-url`, `--open`, `--device-invites`, `--trusted-proxy`.
- Per-IP rate limiting and banning use the real client IP after the WebSocket upgrade (see 3.5).
- Add `relay-devices.json` (device public keys) and `relay-invites.json` (invite hashes, expiry, remaining uses, source); at runtime the files are checked for changes every 5 seconds, and revoked online devices are disconnected immediately.
- Subcommands: `invite`, `invites`, `devices`, `revoke`; an invite is generated automatically on first start.
- Handle `{"type":"invite"}` on the control connection.

**`crates/yi-agentd`**
- `relay.rs`: `RelayConfig` becomes `{enabled, url, relay_fp, name, sign_key, noise_key, clients: [PairedClient]}` (no relay token); `setup` accepts an access link with an invite code, which is only kept in memory for the first registration; add `relay.invite`; keep pending pairing codes in memory; push the ACL after the control connection is established and whenever the lists change; after `agent_accept`, handle IK or IKpsk1 and write successful pairings to `clients`; remember the client_id of each connection and close the matching connections on `relay.revoke`; update `lastSeen`.
- `server.rs`: add the RPCs listed in section 6.
- `main.rs`: adjust CLI subcommands per section 6.
- `proxy.rs`: read `ClientCredential` instead of `Pairing`.

**`crates/client`**
- `api.rs`: adding a relay host becomes "scan / paste pairing link → `client_pair` → save credential"; `relay.parseLink` recognizes v2; `relay.known` and `relay.reuse` change per section 8; `client_connect` uses the credential.
- `hosts.rs`: the `pairing` field stores the `ClientCredential` link (same field name, still hidden).

**`app`**
- `HostsView.tsx`: the relay area gains "Add device" (one-time QR code + countdown), a "Paired devices" list (name, last seen, revoke, rename) and "Reset device identity"; the old "Reset key" is removed.
- When adding a relay host, note that "the pairing code is valid for 10 minutes and can only be used once".

**Docs**: update the relay entries in `AGENTS.md` and the relay deployment instructions in the README (`--token` removed, replaced by invite registration).

## 10. Test plan

- `link` unit tests: round-trip parsing of credentials and pairing links; device id derived from the public key; IK and IKpsk1 handshakes succeed; failures with a wrong psk, wrong device public key or unauthorized client; tampered and replayed frames fail to decrypt.
- `relay` integration tests (in-process relay + simulated yi-agentd + simulated client):
  - the full flow works with no parameters in any URL;
  - unauthorized client public keys are refused, count as failures, and get banned past the threshold;
  - expired or wrong tickets are refused;
  - after a new ACL is pushed, revoked clients' connections are dropped;
  - yi-agentd can't register without an invite; invites are void once used up or expired; after registering, it connects without an invite;
  - after `yi-relay revoke`, the online device is disconnected and can't connect again; `--cascade` works;
  - device-issued invites are refused by default and work with `--device-invites`; `--open` needs no invite;
  - the per-client stream cap works;
  - with `--trusted-proxy`, rate limiting and banning follow `X-Forwarded-For`, and connections not on the trusted list can't forge it;
  - the full flow works behind Nginx (connections stay up after more than 2 minutes idle).
- End to end (done on Windows as usual): enable the relay on the desktop → pair a phone by scanning → chat → revoke → the phone can't connect; pair another one; forwarding through a computer; port forwarding; automatic recovery after restarting yi-agentd and the relay separately.

## 11. Compatibility notes

- The relay, yi-agentd and clients must be upgraded together; v1 and v2 can't talk to each other.
- After upgrading, yi-agentd generates a new device identity and the device id changes; every relay host must be paired again by scanning.
- The `device_id`, `device_token`, `key` and `relay_token` fields in the old `relay.json` are dropped; an old-format `relay-devices.json` must be deleted (the new format uses the same name with different content).
- The relay drops `--token`; after upgrading, each computer must re-register with an invite generated by `yi-relay invite`.
- The relay deployment instructions in the README must be rewritten for the new registration method.
