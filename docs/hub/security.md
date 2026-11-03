# Hub security

Trust model, auth, remote access, TLS, machines, share links. Front door: [../hub.md](../hub.md).

## Trust model

- **The hub is a shell over HTTP.** The token, a login cookie, and a peer credential each mean a shell as you; protect the state dir as you would `~/.ssh` (full-disk encryption against a lost disk; keep it out of unencrypted backups).
- **Agents and plugins run as you, unsandboxed.** Both can read every state-dir file (token, `machines.json`). Run untrusted agents under another user or in a container.
- **Not a sandbox**: the hub's token is never handed to agents (argv or env), but agents run as your user and can read `<state-dir>/token` like any of your files. Run untrusted agents under another user or in a container.
- **Channel**: hooks report over `<state-dir>/report.sock` (unix socket, 0600, never on the network whatever `--bind` says; a state dir too long for a socket path uses `$TMPDIR/flywheel-hub-<uid>/<digest>.sock`, dir 0700) with a per-spawn random secret that authorizes status for that one session only. Reports are advisory UI state: they cannot start, stop, or change a session.
- **Share links** are read-only but show everything that terminal shows ([below](#share-links)).
- **Held-out graders** are kept out of what the hub tells agents, not out of their reach ([workflows](workflows.md#fan-outs-and-flywheel-tasks)).

## Auth

- Token: 256 random bits, hex, `<state-dir>/token` (0600). Rotate with `fw hub token --rotate`.
- One-time codes: 20 Crockford base32 characters (100 bits), single use, 5 minutes, at most 64 outstanding (a 65th drops the oldest, so a QR still on screen can stop working), digests only in memory, dropped on rotation. Minted by `serve`, `url`, or `POST /api/login-codes`. Case, `-`/spaces and `i l o` look-alikes are forgiven when typed.
- Browsers: `/login?code=...` or `/login?token=...` (or the form at `/`, whose one field takes either) sets an `HttpOnly; SameSite=Strict` cookie named `fw_hub_<digest of the token>` (hubs on one host do not clobber each other's login) (`Secure` over the hub's own TLS, behind an `X-Forwarded-Proto: https` trusted proxy, or under an https `--public-url` / tailscale serve host). Its value is a fresh 256-bit per-login secret, never the token: digests in `<state-dir>/logins.json` (0600, survive restarts, 30 days, at most 256). `POST /logout` ends that login and closes the sockets it opened (code 4002; also on expiry or eviction); rotation ends all. The signed-in page replaces itself with `/` (keeping `#session=`), so Back never revisits a spent code URL.
- Scripts: `Authorization: Bearer <token>` (codes are not bearer credentials).
- Failure limit: 10 wrong credentials (token, code, cookie, or bearer) per client address in a sliding 10 minutes, then everything that client presents gets 429 + `Retry-After` until the oldest failure ages out -- except a live login cookie, so an attacker sharing a NAT/proxy address cannot lock signed-in users out. IPv6 clients are keyed by /64. Direct connections from this machine (loopback or the host's own address, no `X-Forwarded-For`: the CLI, a local browser) are exempt, so a remote attacker cannot lock out `fw hub url`. Caveat: a local tunnel that adds no `X-Forwarded-For` (`ssh -L`, socat) makes its traffic look direct, hence exempt.
- Share links (`fwview_...`) reach only their own `/share/` path; anywhere else 403.
- Cross-origin `Origin` headers are refused on WebSocket upgrades and on every non-GET request.
- Connection bounds (`serve` raises its soft `RLIMIT_NOFILE` to the hard limit first):
  - **Authenticated** (the latest request carried the token, a login cookie, or a peer credential): at most a quarter of the descriptor limit (16-1024) at once (an upgraded WebSocket no longer counts); each request head within 10 s (also the keep-alive idle limit).
  - **Unauthenticated** (every connection until a request head on it carries a credential -- nothing is known before the head, so loopback CLI connections start here too, for microseconds): a separate pool of a quarter of that (8-128). The first head within 3 s, that request done within 10 s (public bodies: 4 KiB in 5 s), then `Connection: close`. A stop or `upgrade` drops them at once, never waits on them.
  - Clients: the TCP peer (IPv6: /64); behind loopback, a `--trusted-proxy`, or the host's own address, the `X-Forwarded-For` client once the head arrives (none: each connection its own). Nothing is refused at accept, so a flood sharing your address never locks out your token or cookie. A client past 8 connections loses its own oldest that has sent no head; past 8 requests without a credential, gets 429.
  - A full pool evicts from the client holding the most, silent connections before ones mid-request, oldest first; one evicted just as its head arrives may still authenticate. Limit: a flood from more addresses than the pool has slots can still reset strangers' logins (retry), never an authenticated request.

## Remote access

Recommended -- Tailscale (HTTPS on the tailnet, which phones need for the clipboard API):

```bash
fw hub serve                                     # loopback only
tailscale serve --bg http://127.0.0.1:7420       # https://<machine>.<tailnet>.ts.net -> the hub
fw hub url --qr                                  # scan with the phone
```

- `serve` and `url` run `tailscale status --json` / `tailscale serve status --json` (3 s timeout; `tailscale` on `PATH` or the macOS app): a `serve` whose `/` proxies to the hub's port is listed and, when found at startup, accepted as an origin. Tailnet up but nothing served: they print the `tailscale serve` command.
- Behind any reverse proxy on this machine, `X-Forwarded-For` (last entry), `X-Forwarded-Host` (counts as same-origin), and `X-Forwarded-Proto` are believed only from loopback peers (IPv4-mapped ones under `--bind ::` included) and `--trusted-proxy` addresses. A proxy elsewhere: pass `--public-url` and `--trusted-proxy`. `tailscale serve` keeps `Host` and sets the forwarded headers, so starting it after `fw hub serve` works (it is just not listed as an origin until the next start).
- LAN without Tailscale: `fw hub serve --bind 0.0.0.0 --tls auto`, then `fw hub url --qr` ([below](#https-without-tailscale)). Plain `--bind 0.0.0.0` warns: plain HTTP, so phones get no installable app, notifications, or clipboard.

## HTTPS without Tailscale

**The local CA is name-constrained.** `--tls auto` makes a CA whose certificate carries a critical X.509 `nameConstraints` extension permitting only this hub's names and addresses (as they were when it was made). X.509 reads a DNS constraint as that name **and every name under it**, so a bare single-label host name is never a constraint (a host called `dev` would cover all of `.dev`): the CA gets `<host>.local` instead, and `tls_names` refuses single labels. A device that installs it trusts it for those names (and their subdomains) only: even a stolen CA key cannot impersonate your bank or any other site to that device. A CA made before this (constrained to a bare label) is flagged by `serve` and `doctor`: `fw hub tls rotate`. rustls (tested) and current Chrome and Firefox enforce it; still treat it like any root you install: only on devices you use with this hub, after checking the fingerprint against `fw hub tls ca`.

```bash
fw hub serve --bind 0.0.0.0 --tls auto     # or hub.toml [server] tls = "auto"
fw hub url --qr                            # two QRs: install the CA (first time, http /ca) / log in (https)
fw hub tls ca                              # fingerprint, constraints, file (--out FILE [--der] for a copy)
fw hub tls rotate                          # new CA (old key deleted); every device re-installs
```

- **One port, both schemes**: each connection is https or http by its first byte. With TLS on, plain http from the network reaches only `/ca` (plus its fonts/icon); everything else gets `308` to the same URL over https (`Cache-Control: no-store`; no HSTS: it would pin the whole host name). This machine (loopback, or its own address as the source) and `--trusted-proxy` peers keep plain http (already a secure context / the proxy terminates TLS), so `tailscale serve`, a local browser, and the CLI work unchanged.
- **Files** (`<state-dir>/tls/`, dir 0700): `ca-key.pem` (0600, never leaves it), `ca-cert.pem`. ECDSA P-256, CA valid 10 years. Leaf: every address `url` prints plus `localhost`, the host name when dotted (else `<host>.local`), the tailnet name/IPs, `tls_names`; valid 14 days, re-issued 5 days before expiry and whenever the names change (checked every 60 s). Served chain: leaf, CA.
- **A name outside the constraints** (a new DHCP lease, a WSL port-forward's Windows IP): left out of the leaf with a log warning, the rest keeps working. Cover future leases up front with `tls_names = ["192.168.0.0/16"]` (a network, at most /8), then `fw hub tls rotate`.
- **`/ca`** (public, read-only): the fingerprint to compare out of band, downloads (`/ca/hub-ca.crt` DER, `.pem`, `.mobileconfig` iOS profile), steps per OS (iOS: Settings > Profile Downloaded > Install, then General > About > Certificate Trust Settings; Android: Settings > Encryption & credentials > Install a certificate > CA certificate; Windows/WSL browsers: Trusted Root Certification Authorities, current user; macOS: login keychain, Always Trust; Linux: NSS `certutil` / `update-ca-certificates`). Its script probes `https://<same host>/ca/probe` and says whether this device trusts the CA; it never navigates by itself (a trusting device can still compare the fingerprint) and never sees a credential.
- **QR with TLS**: two codes. `http://<lan>:<port>/ca` (no credential: the page is plain http, which an active LAN attacker could rewrite, so check the fingerprint) for a device's first time, then `https://<lan>:<port>/login?code=<one-time code>`. The login code only ever goes to the https origin.
- **Rotation is revocation**: `tls rotate` deletes the old key (nothing new is ever signed by it) and a running hub switches at once (`POST /api/tls/reload`): it closes every TLS connection made under the old CA and resumes no sessions (the hub and its clients disable TLS session resumption), so a device or machine still trusting only the old CA fails its next request. Remove the old CA from devices; machines pinned to it must re-pin (the error names the new CA fingerprint).
- **Cookies/origins**: logins over the hub's TLS get `Secure`; over https the hub's origin is `https://...` (an `http://` Origin on the same host is refused for state changes).
- **CLI and other hubs**: the local CLI uses plain http to loopback or the specific `--bind` address (traffic that never leaves this machine), so it works whatever names the certificate carries and across rotation. `--url https://...` (attach, ls, ...) trusts the system roots plus this state dir's CA, or `--fingerprint <CA SHA-256>` from the hub's `fw hub tls ca`. `machine add --fingerprint` takes the same CA fingerprint and survives leaf renewals (the chain is verified against that CA: names, expiry, constraints).
- **`--tls CERT,KEY`**: PEM chain + key you supply (absolute paths in hub.toml), re-read when they change; no local CA, `/ca` answers 404.
- `GET /api/tls` (signed in; peers refused): see [api](api.md#settings-config-plugins-notify-tls).

## Machines and peers

One browser UI over several hubs (herdr's "connecting machines"). This hub (A) registers other hubs (B) and relays the browser's API calls and WebSockets to them under `/m/<machine>/api/...`, so the browser talks to A's origin only (one `tailscale serve` covers the phone) and never holds a remote credential.

```bash
# on B: a peer credential for A (printed once, alone on stdout)
fw hub peer issue --name laptop > /tmp/cred
# on A: register B (never pass secrets on argv)
fw hub machine add desktop https://desktop.tailnet.ts.net < /tmp/cred
FW_HUB_MACHINE_SECRET=$(ssh desktop fw hub token) fw hub machine add desktop URL --mint   # or: B's token, used once to mint one; not stored
fw hub ls --machine desktop; fw hub attach --machine desktop SESSION
```

- **Peer credential** (`fwpeer_<id>_<secret>`, 256 bits; digests in B's `peers.json`, 0600): API and WebSockets only -- never a browser login. **Equivalent to B's token**: it starts sessions running any command as B's user, so it can read B's token file. Allowed: what drives sessions, search and transcripts, templates, and usage included. Refused (403, defense in depth, not a boundary): `/api/token/rotate`, `/api/login-codes`, `/api/peers*`, `/api/machines*`, `/api/workspaces/import`, `/api/code-workspaces`, `/api/plugins*`, `/api/settings*`, `/api/config`, `/api/shares*`, `/api/recordings*`, `/api/sessions/{id}/record/*`, `/m/*`, and every non-`/api` page. Survives B's token rotation; `fw hub peer revoke` ends its requests at once and closes its sockets (4003). `peer issue/ls/revoke` go through B's running hub, else edit the file (a running hub re-reads it on change).
- **A's side**: `machines.json` (0600, state dir 0700) holds name, URL, peer credential, optional pin. Plaintext on purpose: an OS keyring (Secret Service, Keychain) hands secrets to any process of the same user once unlocked -- exactly who can already read the file (and the hub token beside it, and every session) -- is absent on headless boxes and WSL where hubs usually run, and its Linux client (`keyring` over Secret Service) would add a D-Bus client stack to the static musl release builds for no gain against that threat. Protect the state dir as you would `~/.ssh`: full-disk encryption against a lost disk; keep it out of unencrypted backups. `machine ls` names the store (`credentials: <path> (plaintext file, mode 0600)`; `--json`: `credential_store`). `add` checks the credential with `GET /api/whoami` there and refuses a hub token, this hub itself, and a hub already registered (same URL, hub id, or peer; a credential it minted for a refused add is revoked). `add --replace` (UI: Re-pair) swaps a machine's credential, URL, or pin keeping its name, and revokes the old credential. `rm` asks B to revoke the credential (`DELETE /api/peer`; else it says how). Names: `[a-z0-9-]`, 1-32, not `local`.
- **Proxy**: only `/api/...` on a registered machine's base URL (SSRF guard; redirects, cookies refused/stripped). Request bodies up to 8 MiB, answers up to 32 MiB, 120 s per call; answers are served as JSON, plain text, or opaque bytes only, with `Content-Disposition: attachment` and CSP `sandbox` (a machine's answer never runs as this hub's page). A machine's 401 becomes 502 `{"error","machine","machine_state":"refused"}` (never signs the browser out); unreachable is 502 `offline`. After a refusal A answers for 90 s without asking (B's failure limit counts A's address). WebSockets relay frame by frame with backpressure; B's 4001-4003 reach the browser as 4010, a removed machine closes with 4011, a lost link 1011.
- **TLS**: https machines are verified against the system roots, or by `--fingerprint`: the SHA-256 of a hub's local CA (`fw hub tls ca` on it; the chain is verified against it, names and expiry included) or of a self-signed leaf (`openssl x509 -noout -fingerprint -sha256`; names and expiry not checked, like an SSH host key). An untrusted certificate's refusal names the fingerprint to pin (the top of the chain presented).
- **UI**: the sidebar groups workspaces by machine (this hub first), each head with its link state (online / connecting / offline with retry countdown / refused) and Reconnect; each machine has its own events socket with independent backoff (0.5 s doubling to 30 s), and keeps its last lists while unreachable. Needs you, notifications, tab counts, attach, layout, commands, review, worktrees, launcher (templates live on, and launch on, the machine; cost chips come from its `/api/usage`), search ([internals](internals.md#search)), and the Flywheel panel all work for its sessions; a machine badge marks them. The sidebar's spend strip is this hub's only and is labelled with its host once machines show. New workspaces pick a machine (VS Code import: this hub only). **Machines** (sidebar footer, palette) adds, tests, and removes them.

API: [api](api.md#auth-machines-peers).

## Share links

**A viewer sees everything that terminal shows** -- output, typed secrets echoed back, the scrollback in the snapshot. Nothing is redacted; share a session you would screen-share.

```bash
fw hub share agent --expires 2h --name pairing   # URLs (every origin, remote first) to /share/fwview_<id>_<secret>; --qr for a phone
fw hub share ls; fw hub share revoke pairing     # or the id; --all [--session S]
```

- Credential: `fwview_<id>_<secret>` (256 bits) in the link's path; digests only in `<state-dir>/shares.json` (0600, survives restarts and upgrades). Expiry 60 s to `shares.max_expiry` (30 d unless set; default `shares.default_expiry`, 1 h unless set; `GET /api/shares` has `policy` `{"default_expires_in","max_expires_in"}` and the UI's picker follows it); labels unique among live links; at most 128 live. Minted by the token or a login (`POST /api/shares`; a peer credential gets 403; [api](api.md#shares-and-recordings)).
- Reaches only: `/share/<cred>` (viewer page: terminal, title, status, expiry; no sidebar, no input), `/share/<cred>/info` (`{"share":{"name","expires","now"},"session":{"name","workspace","kind","state","code","signal","title","agent","live","cols","rows"}|null}`), `/share/<cred>/ws` (the terminal socket, read-only), and its own assets (`viewer.js`, `term-view.js`, `viewer.css`, xterm). Any other path under it, and the credential as a bearer anywhere (`/api/...`, `/`, `/assets/...`): 403.
- Read-only socket: the snapshot, output, `size`, `exit`, `reset` as on `/api/sessions/{id}/ws`; every `input`/`resize`/`theme`/binary message is refused (`{"type":"error"}`), nothing reaches the PTY. It never sizes the PTY, never counts as viewing (seen/notify) or in `live.clients` (it is `live.link_viewers`). Cross-origin upgrades refused.
- Revoke or expiry closes open views at once (code **4004**); the link then answers 404 (revoked) / 410 (expired). Removing a session revokes its links (`share.revoked` each). A live handoff closes them with 1012 and the viewer reconnects. Token rotation leaves links alone (revoke them).
- Admission: a request whose path carries a live link's credential holds a **share-link slot** (its own pool, sized like the unauthenticated one, 8 per client; kept alive; full: 429 at once), never a main slot, so a link holder cannot lock the operator out. At most 32 open views per link and 256 in all (429). A wrong credential is a failure for the failure limit.
- Viewer: `fit` / `readable` toggle (fit the whole screen to the window, or a readable font that scrolls; auto picks readable when fitting would go under 8 px; remembered per browser). After revoke/expiry it says the screen is no longer live.
- UI: the `share` header pill (count = link viewers now), palette "Share <session>", sidebar `sharing` chip: mint (label, expiry; copied), list with viewers, revoke. This hub's sessions only (a machine's: share there).
