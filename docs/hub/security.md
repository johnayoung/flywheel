# Hub security

Trust model, auth, remote access, TLS, machines, share links. Front door: [hub](../hub.md).

## Trust model

- **The hub is a shell over HTTP.** The token, a login cookie, and a peer credential each mean a shell as you.
- **Agents and plugins run as you, unsandboxed.** Agents never get the token but can read every state-dir file (`token`, `machines.json`): run untrusted ones under another user or in a container.
- Protect the state dir like `~/.ssh` (full-disk encryption, no unencrypted backups); secrets are 0600, the dir 0700.
- **Status reports are advisory**: hooks report over `<state-dir>/report.sock` (unix socket, never on the network) with a per-spawn secret for one session; they cannot start, stop, or change a session.
- Share links show everything the terminal shows ([below](#share-links)). Plugins are your executables, disabled until enabled.
- Held-out graders: kept out of what the hub tells agents, not out of their reach ([hub-workflows](workflows.md#fan-outs-and-flywheel-tasks)).

## Auth

- Token: 256 random bits in `<state-dir>/token`. `fw hub token --rotate` ends every cookie, code, and socket of the old one (close 4001); agent hooks are unaffected.
- One-time codes: 100 bits, single use, 5 minutes, at most 64 outstanding; typing forgives case, `-`, spaces, `i l o` look-alikes.
- `/login?code=` (or `?token=`) sets an `HttpOnly; SameSite=Strict` cookie holding a fresh per-login secret, never the token (`Secure` under https). 30 days, max 256; `POST /logout` ends one.
- Scripts: `Authorization: Bearer <token>`; codes are not bearer credentials. Cross-origin `Origin` is refused on WebSocket upgrades and every non-GET request.
- 10 wrong credentials per client address (IPv6 /64) in 10 minutes -> 429, except with a live login cookie; direct local connections are exempt (a local tunnel such as `ssh -L` looks direct).
- Authenticated and unauthenticated connections have separate pools; unauthenticated ones get 3 s for a request head, so a flood never locks you out. A WebSocket that stops reading (terminal, share, events, `/m/` relay either way) is dropped after 60 s, nothing more written to it.

## Remote access

Recommended: Tailscale (https on the tailnet, which phones need for clipboard, notifications, install).

```bash
fw hub serve  # loopback only
tailscale serve --bg http://127.0.0.1:7420  # https://<machine>.<tailnet>.ts.net -> the hub
fw hub url --qr  # scan with the phone
```

- `serve` and `url` detect a `tailscale serve` of the hub's port (accepted as an origin), else print the command.
- `X-Forwarded-*` is believed only from loopback and `--trusted-proxy` peers; a proxy elsewhere: `--public-url` + `--trusted-proxy`.
- LAN without Tailscale: `--bind 0.0.0.0 --tls auto` ([below](#https-without-tailscale)); plain `--bind 0.0.0.0` warns (plain HTTP).

## HTTPS without Tailscale

```bash
fw hub serve --bind 0.0.0.0 --tls auto  # or hub.toml [server] tls = "auto"
fw hub url --qr  # QR 1: install the CA (http /ca), QR 2: log in (https)
fw hub tls ca  # fingerprint, constraints, file (--out FILE [--der])
fw hub tls rotate  # new CA, old key deleted; every device re-installs
```

- **The local CA is name-constrained** (critical `nameConstraints`: this hub's names and addresses only): a stolen CA key cannot impersonate other sites. Bare hosts become `<host>.local`. Check the fingerprint against `fw hub tls ca` before installing.
- `--tls-name` takes private names only (`.local` `.lan` `.home.arpa` `.internal` `.localhost` `<tailnet>.ts.net`; private, loopback, link-local, CGNAT, ULA addresses). One you own: `--tls-allow-public-name NAME`, warned.
- One port, both schemes: with TLS on, plain http from the network reaches only `/ca` (the rest 308s to https); local and `--trusted-proxy` peers keep http.
- `<state-dir>/tls/`: `ca-key.pem` (never leaves it), `ca-cert.pem`. ECDSA P-256; CA 10 years; leaf 14 days, re-issued 5 days before expiry or when names change.
- A name outside the constraints (a new DHCP lease) is left out of the leaf with a warning; cover leases with `tls_names = ["192.168.0.0/16"]` (at most /8), then `tls rotate`.
- `/ca` (public, plain http: compare the fingerprint): `.crt` / `.pem` / iOS `.mobileconfig`, per-OS steps, a trust probe. The login code only goes to https.
- **Rotation is revocation**: the old key is deleted, connections made under it close, and session resumption is off, so old-CA devices fail their next request.
- CLI: local verbs use http to loopback (works across rotation); `--url https://...` trusts system roots plus this state dir's CA, or `--fingerprint <CA SHA-256>`.
- `--tls CERT,KEY`: your own PEM chain + key (absolute paths in hub.toml), re-read on change; no `/ca`.

## Machines and peers

One UI over several hubs: this hub (A) registers hub B and proxies the browser's API calls and sockets under `/m/<machine>/api/...`; the browser never holds B's credential.

```bash
fw hub peer issue --name laptop > /tmp/cred  # on B: printed once
fw hub machine add desktop https://desktop.tailnet.ts.net < /tmp/cred  # on A: never on argv
FW_HUB_MACHINE_SECRET=$(ssh desktop fw hub token) fw hub machine add desktop URL --mint  # or mint one with B's token (not stored)
fw hub ls --machine desktop; fw hub attach --machine desktop SESSION
```

- **A peer credential is equivalent to B's token** (it can start sessions that read B's token file); API and sockets only, never a browser login.
- Peers get 403 (defense in depth, not a boundary) on token rotation, login codes, peers, machines, import, plugins, settings, config, shares, recordings, permissions, `/m/*`, and every page.
- `fw hub peer revoke` ends its requests and sockets (4003); peer credentials survive B's token rotation.
- `machines.json` on A holds credentials in plaintext (0600) on purpose: a keyring hands them to the same user, and is absent on headless and WSL hosts.
- `machine add` verifies the credential (`/api/whoami`) and refuses a hub token, this hub, or a duplicate; `--replace` swaps credential / URL / pin; `rm` asks B to revoke.
- Proxy: `/api/...` of registered URLs only (SSRF guard), 8 MiB requests, 32 MiB answers, 120 s; answers as attachments with CSP `sandbox`; B's 401 becomes 502 (never signs you out). Sockets relay frames and close codes as they come (protocol 2 too).
- https machines verify against system roots or `--fingerprint` (a hub's CA, or a self-signed leaf); a refusal names the fingerprint to pin.
- The sidebar groups workspaces by machine; attach, layout, review, search, and Flywheel work for their sessions.

## Share links

**A viewer sees everything that terminal shows**, typed secrets included. Share only what you would screen-share.

```bash
fw hub share agent --expires 2h --name pairing  # /share/fwview_<id>_<secret> on every origin; --qr for a phone
fw hub share ls; fw hub share revoke pairing  # or the id; --all [--session S]
```

- Credential (256 bits) in the URL path; only digests stored (`shares.json`). Expiry 60 s to `shares.max_expiry` (30 d), default `shares.default_expiry` (1 h); 128 live at most.
- Reaches only `/share/<cred>`, its `info`, read-only `ws` (protocol 1), and assets; minted by the token or a login (peers 403). Input and resize refused; never sizes the PTY.
- Revoke or expiry closes views at once (4004); removing a session revokes its links; token rotation does not.
- Own connection pool (32 views per link, 256 total), so link holders cannot lock out the operator.
- UI: the `share` pill (viewers now), palette "Share <session>"; this hub's sessions only.
