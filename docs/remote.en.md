# Remote access and relay

Yi Console connects to three kinds of hosts: this machine, SSH servers, and computers reached through a relay server. For using the local machine, see the [user guide](guide.en.md).

中文：[远程连接与中继](remote.md)

## SSH hosts

Add SSH hosts in "Hosts" (via "Add / Manage Hosts" at the bottom left), or import them from `~/.ssh/config`. Authentication can use ssh-agent, a key file, a pasted private key or a password, and jump hosts are supported.

On first connect you're shown the host's fingerprint to confirm. After that, the client detects the remote OS and architecture, uploads the matching yi-agentd, and talks to it through `yi-agentd bridge`. The yi-agentd on the remote machine listens on no ports.

## Reaching a computer through a relay (from a phone or another computer)

When a computer is behind NAT and your phone or another computer can't reach it directly, use a relay. Below, the computer being connected to is called the "target computer". For the security design, see [Relay security design](relay-security.en.md).

1. Deploy `yi-relay` on a server with a public address (see "Deploying a relay" below). The relay operator runs `yi-relay invite` to create a **relay access link** for each target computer (single-use, valid for 24 hours by default).
2. In the desktop app on the target computer, open "Hosts" and click "Make this computer reachable" at the top right, then follow the card:
   - **Step 1, connect to a relay**:
     - Paste the relay access link (`yiconsole://relay?...&invite=...`) and click "Turn on". The computer uses the invite code in the link to register its public key with the relay; from then on it connects with its own key only, and the invite code is used up.
     - If this app already has a device connected through a relay, and that relay was started with `--device-invites`, you can just click "Use <relay>": that computer asks the relay for a single-use invite on your behalf.
     - Without a link, click "No link? Enter the relay address and invite code" and fill in the relay address, invite code and certificate fingerprint.

     If it can't connect, the card tells you why (not registered, invite expired, removed by the relay, fingerprint mismatch, or address/network unreachable).
   - **Step 2, add this computer on another device**: click "Generate pairing QR code" to get a **single-use** QR code and pairing link. It's valid for 10 minutes and pairs exactly one device.
   - **Paired devices**: lists the phones and computers that have paired. You can rename them, or remove one at a time (it disconnects immediately and must pair again).
3. On the device that will connect, open "Hosts" → "Pair device":
   - Phone: tap "Scan" and scan the QR code on the target computer.
   - Another computer: send yourself the copied pairing link. When you open "Pair device" with a pairing link on the clipboard, it's filled in automatically and the name of the device to be added is shown; click "Add and connect".

   While adding, this device generates its own key and pairs with the target computer using the pairing code. What gets saved is this device's own credentials; the pairing link is useless once used. The target computer then shows up in the host list marked "Via relay", and sessions, history, files and terminals all work. The target computer needs yi-agentd running; to make it reachable right after boot, register the startup service on it (see the [user guide](guide.en.md#the-background-service-on-each-machine)).

On a computer without a GUI, use the command line:
```bash
yi-agentd relay setup 'yiconsole://relay?...'                         # register and turn on with an access link
yi-agentd relay setup wss://relay.example.com --invite <code> [--fp <cert fingerprint>]
yi-agentd relay pair                                                 # print a single-use pairing QR code and link in the terminal
yi-agentd relay clients                                              # list paired devices
yi-agentd relay revoke <device id>                                   # remove one
yi-agentd relay reset                                                # clear all pairings
```

### Proxies

If the computer's network can only reach the internet through a proxy (common on corporate networks), yi-agentd uses the proxy automatically. It looks in this order: `HTTPS_PROXY` (for `wss://` addresses) or `HTTP_PROXY` (for `ws://`), then `ALL_PROXY`, and on Windows finally the system proxy settings. Hosts listed in `NO_PROXY` are reached directly, as are private addresses (10.x, 172.16–31.x, 192.168.x and so on). Only HTTP proxies are supported.

The desktop and mobile clients also use the proxy by default when connecting to SSH hosts and relays, with the same rules. You can switch to a manual proxy or no proxy in the "Network Proxy" card under "Preferences"; the setting is stored in `client-settings.json` in the client data directory. If tunnelling SSH through the proxy fails (corporate proxies often only allow port 443), it falls back to a direct connection.

### Security in brief

- Every computer and every client has its own key, and private keys never leave their machine. The relay only knows public keys: it can't see the content and can't impersonate anyone.
- End-to-end encryption uses a Noise IK handshake (X25519 + ChaCha20-Poly1305) with forward secrecy: even if a key leaks later, previously recorded traffic can't be decrypted.
- A pairing QR code contains only the computer's public key and a single-use pairing code, so it's harmless once used or expired. If you think a device is lost, remove it under "Paired devices"; to revoke everything, use "More → Reset all pairings".

## Deploying a relay

If you have a domain, put the relay behind Caddy or Nginx and let them handle TLS:

```bash
yi-relay --listen 127.0.0.1:7878 --data /var/lib/yi-relay \
          --public-url wss://relay.example.com --trusted-proxy 127.0.0.1
```

Caddy:

```
relay.example.com {
    reverse_proxy 127.0.0.1:7878
}
```

Nginx:

```nginx
location /v1/ {
    proxy_pass http://127.0.0.1:7878;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;
    proxy_buffering off;
}
```

Set `--trusted-proxy` to the reverse proxy's address (repeatable; CIDR ranges are accepted). The relay reads `X-Forwarded-For` for the real client IP only on connections from these addresses. Without it, every connection through the proxy counts as the proxy's IP, so per-IP rate limits and bans hit everyone. The relay address may include a path prefix (such as `wss://example.com/hub`); have the proxy strip the prefix before passing requests to the relay.

Without a domain, use `--self-signed`:

```bash
yi-relay --listen 0.0.0.0:7878 --data /var/lib/yi-relay --self-signed --public-url wss://<IP>:7878
```

On first start it generates a self-signed certificate in the data directory and prints its SHA-256 fingerprint in the log. The fingerprint is written into invite links and pairing QR codes automatically, and clients accept only that certificate. As long as you keep the certificate files, the fingerprint doesn't change.

A self-signed certificate only works when clients connect to the relay directly, or through a proxy that passes TCP straight through without terminating TLS. If the proxy terminates TLS, clients see the proxy's certificate and the fingerprint will never match: give the proxy a proper certificate and don't use `--self-signed` on the relay. When both `--self-signed` and `--trusted-proxy` are set, the relay warns at startup and leaves the fingerprint out of links. A TCP pass-through proxy can't add `X-Forwarded-For`, so all connections count as the proxy's IP; raise `--max-per-ip` if many people use it.

yi-relay can also load a regular certificate itself: `--tls-cert cert.pem --tls-key key.pem`.

Managing computers (run on the relay server; these read and write the data directory directly, and a running relay picks up changes within 5 seconds):

```bash
yi-relay invite  --data /var/lib/yi-relay [--ttl 24h] [--uses 1] [--note <note>]   # create an access link
yi-relay invites --data /var/lib/yi-relay [--revoke <invite id>]                   # list / revoke invites
yi-relay devices --data /var/lib/yi-relay                                          # registered computers
yi-relay revoke  <device id> [--cascade] --data /var/lib/yi-relay                  # remove a computer; disconnects it if online
```

The first time the relay starts with no computers registered, it creates an invite valid for 24 hours and prints it in the startup log.

Security-related options:

- Admission: a computer must register with an invite code (single-use, expiring, stored only as a hash on the relay) and afterwards proves its identity by signing with its own Ed25519 key. Phones and other clients need no relay credentials at all; the relay only lets through clients the target computer has authorized. No secrets ever appear in URLs.
- `--device-invites`: lets registered computers issue single-use invites valid for 10 minutes (the one-click "Use <relay>" in the app). Off by default; keep it off if the relay is shared by several people. Issued invites are visible in `yi-relay devices`, and `revoke --cascade` also removes the computers a device invited.
- `--open`: anyone can register a computer without an invite. Only suitable for public relays. Off by default.
- Connection exhaustion protection: the upgrade and authentication must each finish within 10 seconds; by default there are at most 1000 connections overall (`--max-conns`) and 32 per IP (`--max-per-ip`). An IP with 10 suspicious failures (bad signature, malformed data, timeouts, guessing invite or pairing codes) within 10 minutes is banned for 10 minutes. Removed devices being refused while auto-reconnecting don't count, so other devices behind the same NAT aren't caught.
