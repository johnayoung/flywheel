# Hub security

Trust model, auth, remote access, TLS, machines, share links. Front door: [hub](../hub.md).

## Trust model

- **The hub is a shell over HTTP.** The token, a login cookie, and a peer credential each mean a shell as you.
- **Agents and plugins run as you, unsandboxed.** The token is never passed to agents, but they can read every state-dir file (`token`, `machines.json`). Run untrusted agents under another user or in a container.
- Protect the state dir like `~/.ssh`: full-disk encryption, no unencrypted backups. Every secret file is 0600, the dir 0700.
- **Status reports are advisory**: hooks report over `<state-dir>/report.sock` (unix socket, never on the network) with a per-spawn secret valid for one session; they cannot start, stop, or change a session.
- Share links are read-only but show everything the terminal shows ([below](#share-links)).
- Plugins (`<state-dir>/plugins/`) are your executables, run as you; disabled until enabled.
- Held-out graders: kept out of what the hub tells agents, not out of their reach ([hub-workflows](workflows.md#fan-outs-and-flywheel-tasks)).

## Auth

- Token: 256 random bits in `<state-dir>/token`. `fw hub token --rotate` ends every cookie, code, and socket of the old one (close 4001); agent hooks are unaffected.
- One-time codes: 100 bits, single use, 5 minutes, at most 64 outstanding. Minted by `serve`, `url`, `open`, `POST /api/login-codes`. Typing forgives case, `-`, spaces, and `i l o` look-alikes.
- Login: `/login?code=` (or `?token=`) sets an `HttpOnly; SameSite=Strict` cookie holding a fresh per-login secret, never the token. `Secure` under https. Logins last 30 days (max 256); `POST /logout` ends one.
- Scripts: `Authorization: Bearer <token>`. Codes are not bearer credentials.
- Cross-origin `Origin` is refused on WebSocket upgrades and every non-GET request.
- Failure limit: 10 wrong credentials per client address (IPv6 /64) in 10 minutes -> 429, except for a live login cookie. Direct local connections are exempt (caveat: a local tunnel such as `ssh -L` looks direct).
- Connection bounds (`serve` raises `RLIMIT_NOFILE` first): authenticated and unauthenticated connections have separate pools; unauthenticated ones get 3 s for a request head and are evicted first, so a flood never locks out your token or cookie.

## Remote access

Recommended: Tailscale (https on the tailnet, which phones need for clipboard, notifications, install).

```bash
fw hub serve                                     # loopback only
tailscale serve --bg http://127.0.0.1:7420       # https://<machine>.<tailnet>.ts.net -> the hub
fw hub url --qr                                  # scan with the phone
```

- `serve` and `url` detect a `tailscale serve` for the hub's port (accepted as an origin) and print the command when the tailnet is up but nothing is served.
- `X-Forwarded-*` headers are believed only from loopback and `--trusted-proxy` peers. A proxy on another machine: `--public-url` + `--trusted-proxy`.
- LAN without Tailscale: `--bind 0.0.0.0 --tls auto` ([below](#https-without-tailscale)). Plain `--bind 0.0.0.0` warns: plain HTTP.

## HTTPS without Tailscale

```bash
fw hub serve --bind 0.0.0.0 --tls auto     # or hub.toml [server] tls = "auto"
fw hub url --qr                            # QR 1: install the CA (http /ca), QR 2: log in (https)
fw hub tls ca                              # fingerprint, constraints, file (--out FILE [--der])
fw hub tls rotate                          # new CA, old key deleted; every device re-installs
```

- **The local CA is name-constrained**: a critical `nameConstraints` extension permits only this hub's names and addresses, so even a stolen CA key cannot impersonate other sites. Bare single-label hosts become `<host>.local` (a label constraint would cover a whole TLD). Install it only after checking the fingerprint against `fw hub tls ca`.
- One port, both schemes. With TLS on, plain http from the network reaches only `/ca`; everything else 308s to https. Local and `--trusted-proxy` peers keep plain http.
- Files `<state-dir>/tls/`: `ca-key.pem` (never leaves it), `ca-cert.pem`. ECDSA P-256; CA 10 years; leaf 14 days, re-issued 5 days before expiry or when names change.
- A name outside the constraints (new DHCP lease) is left out of the leaf with a warning. Cover leases with `tls_names = ["192.168.0.0/16"]` (at most /8), then `tls rotate`.
- `/ca` (public): fingerprint, `.crt` / `.pem` / iOS `.mobileconfig` downloads, per-OS install steps, and a probe saying whether this device trusts the CA. It is plain http, so compare the fingerprint. The login code only ever goes to https.
- **Rotation is revocation**: the old key is deleted, the running hub closes connections made under the old CA, and session resumption is off, so old-CA devices fail their next request.
- CLI: local verbs use plain http to loopback (works across rotation). `--url https://...` trusts system roots plus this state dir's CA, or `--fingerprint <CA SHA-256>`.
- `--tls CERT,KEY`: your own PEM chain + key (absolute paths in hub.toml), re-read on change; no `/ca`.

## Machines and peers

One UI over several hubs. This hub (A) registers hub B and proxies the browser's API calls and sockets under `/m/<machine>/api/...`; the browser never holds B's credential.

```bash
fw hub peer issue --name laptop > /tmp/cred                          # on B: printed once
fw hub machine add desktop https://desktop.tailnet.ts.net < /tmp/cred   # on A: never on argv
FW_HUB_MACHINE_SECRET=$(ssh desktop fw hub token) fw hub machine add desktop URL --mint   # or mint one with B's token (not stored)
fw hub ls --machine desktop; fw hub attach --machine desktop SESSION
```

- **A peer credential is equivalent to B's token**: it can start sessions that read B's token file. API and sockets only, never a browser login.
- Peers get 403 (defense in depth, not a boundary) on token rotation, login codes, peers, machines, import, plugins, settings, config, shares, recordings, `/m/*`, and every page.
- `fw hub peer revoke` ends its requests and closes its sockets (4003). Peer credentials survive B's token rotation.
- `machines.json` on A holds credentials in plaintext (0600) on purpose: an OS keyring gives them to the same user who can already read the file, and is absent on headless and WSL hosts.
- `machine add` verifies the credential (`/api/whoami`) and refuses a hub token, this hub, or a duplicate. `--replace` swaps credential / URL / pin; `rm` asks B to revoke.
- Proxy: `/api/...` of registered URLs only (SSRF guard), 8 MiB requests, 32 MiB answers, 120 s. Answers are served as attachments with CSP `sandbox`. B's 401 becomes 502 `refused` (never signs you out).
- TLS: https machines verify against system roots or `--fingerprint` (a hub's CA, or a self-signed leaf). A refusal names the fingerprint to pin.
- UI: the sidebar groups workspaces by machine with link state. Attach, layout, commands, review, launcher, search, and the Flywheel panel work for machine sessions.

## Share links

**A viewer sees everything that terminal shows**, typed secrets included. Share only what you would screen-share.

```bash
fw hub share agent --expires 2h --name pairing   # /share/fwview_<id>_<secret> on every origin; --qr for a phone
fw hub share ls; fw hub share revoke pairing     # or the id; --all [--session S]
```

- Credential (256 bits) in the URL path; only digests stored (`shares.json`). Expiry 60 s up to `shares.max_expiry` (30 d), default `shares.default_expiry` (1 h). Max 128 live.
- Reaches only `/share/<cred>`, its `info`, read-only `ws`, and its assets; anywhere else 403. Minted by the token or a login (peers 403).
- Read-only: input and resize are refused; it never sizes the PTY or counts as viewing.
- Revoke or expiry closes views at once (4004). Removing a session revokes its links. Token rotation does not.
- Own connection pool, 32 views per link, 256 total, so link holders cannot lock out the operator.
- UI: the `share` header pill (count = viewers now), palette "Share <session>". This hub's sessions only.
