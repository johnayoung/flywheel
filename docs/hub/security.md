# Hub security

Front door: [hub](../hub.md).

## Trust model

- **The hub is a shell over HTTP.** The token, a login cookie, and a peer credential each mean a shell as you.
- **Agents and plugins run as you, unsandboxed.** Agents never get the token but can read every state-dir file (`token`, `machines.json`): run untrusted ones as another user or containerized. Protect the state dir like `~/.ssh`; secrets 0600, the dir 0700.
- **Status reports are advisory**: hooks report with a per-spawn secret for one session, over `<state-dir>/report.sock` (unix socket) or as a POST to the hub from this machine (`/api/telemetry/hook`); they cannot start, stop, or change a session.
- **Telemetry** (`telemetry.db`, 0600) keeps one scrubbed line of what each tool call acts on (a path, the start of a command; never its output) and the start of each prompt, which the activity journal never does. It goes to no plugin, digest, or share link; the token, a login, or a peer reads it, and `[telemetry] upstream` sends it to that machine.
- The folder picker (`/api/fs/*`) lists directory names on the hub's machine (never files or contents) to whoever can create a workspace: token, login, peer; never a share link.
- Share links show everything the terminal shows. Plugins are disabled until enabled. Held-out graders: kept out of what the hub tells agents, not out of their reach ([hub-workflows](workflows.md#fan-outs-and-flywheel-tasks)).

## Auth

- Token: 256 random bits in `<state-dir>/token`. `fw hub token --rotate` ends every cookie, code, and socket of the old one (4001); agent hooks are unaffected.
- One-time codes: 100 bits, single use, 5 minutes, 64 outstanding at most; typing forgives case, `-`, spaces, `i l o` look-alikes.
- `/login?code=` (or `?token=`) sets an `HttpOnly; SameSite=Strict` cookie holding a fresh per-login secret, never the token (`Secure` under https); it lasts `server.login_days` (30) after its last use (sliding, written at most hourly), 256 max; `POST /logout` ends one.
- Scripts: `Authorization: Bearer <token>`. Cross-origin `Origin` is refused on WebSocket upgrades and every non-GET.
- 10 wrong credentials per client address (IPv6 /64) in 10 minutes -> 429, except with a live login cookie; direct local connections (and local tunnels such as `ssh -L`) are exempt.
- Unauthenticated connections get their own pool and 3 s for a request head: a flood never locks you out. A WebSocket that stops reading is dropped after 60 s.

## Remote access

Recommended: Tailscale (https: phones need it for clipboard, notifications, install): the hub on loopback, `tailscale serve --bg http://127.0.0.1:7420` (`https://<machine>.<tailnet>.ts.net`), `fw hub url --qr` to scan.

- `serve` and `url` detect a `tailscale serve` of the hub's port, else print the command.
- `X-Forwarded-*` is believed from loopback and `--trusted-proxy` peers only; a proxy elsewhere: `--public-url` + `--trusted-proxy`.
- LAN without Tailscale: `--bind 0.0.0.0 --tls auto` (below); plain `--bind 0.0.0.0` warns.

## HTTPS without Tailscale

```bash
fw hub serve --bind 0.0.0.0 --tls auto  # or hub.toml [server] tls = "auto"
fw hub url --qr  # QR 1: install the CA (http /ca), QR 2: log in (https)
fw hub tls ca  # fingerprint, constraints, file
fw hub tls rotate  # new CA, old key deleted; every device re-installs
```

- **The local CA is name-constrained** (critical `nameConstraints`: this hub's names and addresses only): a stolen key cannot impersonate other sites. Bare hosts become `<host>.local`. Check the fingerprint (`fw hub tls ca`) before installing.
- `--tls-name`: private names (`.local` `.lan` `.home.arpa` `.internal` `.localhost` `<tailnet>.ts.net`) and addresses only; one you own: `--tls-allow-public-name NAME`, warned.
- One port, both schemes: with TLS on, plain http from the network reaches only `/ca` (else 308 to https); local and `--trusted-proxy` peers keep http.
- `<state-dir>/tls/`: `ca-key.pem` (never leaves it), `ca-cert.pem`; ECDSA P-256; CA 10 years; leaf 14 days, re-issued 5 days before expiry or when names change. A name outside the constraints (new DHCP lease) is left out, warned: add a network to `tls_names`, then `tls rotate`.
- `/ca` (public, plain http): the CA files, per-OS steps, a trust probe; the login code only goes to https.
- **Rotation is revocation**: the old key is deleted, its connections close, session resumption is off, so old-CA devices fail their next request.
- CLI: local verbs use http to loopback; `--url https://...` trusts system roots plus this state dir's CA, or `--fingerprint <CA SHA-256>`. `--tls CERT,KEY`: your PEM chain + key, re-read on change; no `/ca`.

## Machines and peers

One UI over several hubs: this hub (A) registers hub B and proxies the browser's API calls and sockets under `/m/<machine>/api/...`; the browser never holds B's credential.

```bash
fw hub peer issue --name laptop > /tmp/cred  # on B: printed once
fw hub machine add desktop https://desktop.tailnet.ts.net < /tmp/cred  # on A: never on argv
FW_HUB_MACHINE_SECRET=$(ssh desktop fw hub token) fw hub machine add desktop URL --mint  # or mint one with B's token (not stored)
fw hub ls --machine desktop; fw hub attach --machine desktop SESSION
```

- **A peer credential is equivalent to B's token** (its sessions can read B's token file); API and sockets only, never a browser login. `fw hub peer revoke` ends its requests and sockets (4003); it survives B's token rotation.
- Peers get 403 (defense in depth, not a boundary) on token rotation, login codes, peers, machines, import, plugins, settings, config, shares, recordings, permissions, autopilot control, steering, task writes, `/m/*`, and every page.
- `machines.json` on A holds credentials in plaintext (0600) on purpose: a keyring hands them to the same user, and headless and WSL hosts lack one.
- `machine add` verifies the credential (`/api/whoami`), refusing a hub token, this hub, or a duplicate; `--replace` swaps credential / URL / pin; `rm` asks B to revoke.
- Proxy: `/api/...` of registered URLs only (SSRF guard), 8 MiB requests, 32 MiB answers, 120 s, as attachments with CSP `sandbox`; B's 401 becomes 502.
- https machines verify against system roots or `--fingerprint` (a CA, or a self-signed leaf); a refusal names the fingerprint to pin.

## Share links

**A viewer sees everything that terminal shows**, typed secrets included. Share only what you would screen-share.

```bash
fw hub share agent --expires 2h --name pairing  # /share/fwview_<id>_<secret> on every origin; --qr for a phone
fw hub share ls; fw hub share revoke pairing  # or the id; --all [--session S]
```

- Credential (256 bits) in the URL path; only digests stored. Expiry 60 s to `shares.max_expiry`, default `shares.default_expiry`; 128 live at most.
- Reaches only `/share/<cred>`, its `info`, read-only `ws`, and assets; minted by the token or a login. Input and resize refused; never sizes the PTY.
- Revoke, expiry, or removing the session closes views at once (4004); token rotation does not. Own pool (32 views per link, 256 total): link holders cannot lock out the operator.
- UI: the `share` pill (viewers now), palette "Share <session>"; this hub's sessions only.
