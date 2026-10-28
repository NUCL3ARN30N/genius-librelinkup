# bllu-server – caching server for Better LibreLinkUp

An always-on companion (Docker, LXC, any Linux box) that polls LibreLinkUp around the clock and
keeps every reading. Paired desktop apps use it to

- **fill gaps** – readings from while the computer was off are fetched on the next start (and
  every 15 minutes), so reports and HbA1c estimates stay complete;
- optionally **take live values from the server** instead of logging in to LibreLinkUp on every
  computer (fewer logins on your account, no LibreLinkUp password on the desktop at all).

## Security

### Connection

| Layer | |
|---|---|
| Transport | **TLS 1.3 only** (rustls, aws-lc-rs), **mutual TLS**: the server does not finish a handshake without a client certificate. HTTP redirects are refused by the client. |
| Server identity | On first start the server creates an ECDSA P-256 certificate in `<data>/tls/`. The **pairing code** carries its SHA-256 fingerprint; the app pins it and refuses any other certificate before a single byte of data is sent (no MITM, even on a hostile network). |
| Client identity | Every app creates its **own ECDSA P-256 key pair** at pairing time and keeps it in the OS keychain (Windows Credential Manager / Secret Service). The server binds the client to that certificate's fingerprint. |
| Authorisation | A random **256-bit bearer token** that is only accepted *together with* the bound client certificate. A stolen token is useless without the private key and vice versa. The server stores only SHA-256 hashes and compares in constant time. |
| Token rotation | The app replaces its token every 30 days (`POST /api/v1/rotate`); the old token stops working immediately. |
| Pairing | `bllu-server pair` creates a **one-time secret** (192 bit, default 10 min, max 60, at most 5 pending). The pairing endpoint is closed while no code is pending. Re-pairing a key replaces its old binding. |
| Revocation | `bllu-server clients` shows every computer with key fingerprint, last use and IP; `revoke <id>` / `revoke --all` take effect in the running server immediately. `close-pairing` discards unused codes. |

### Server hardening

- **Brute force:** 10 failed authentication or pairing attempts from one IP within 10 minutes
  block that IP for 15 minutes (HTTP 429).
- **Network allowlist:** `BLLU_ALLOW_CIDRS` (e.g. LAN + VPN ranges); everything else gets 403.
  The port should not be exposed to the internet. For remote access use a VPN (WireGuard,
  Tailscale).
- **Limits:** 8 KiB request bodies, 30 s per request, 10 s TLS handshake, 64 concurrent
  requests, at most 400 days of data per request.
- **Responses:** `Cache-Control: no-store`, `nosniff`, `X-Frame-Options: DENY`, HSTS.
- **Files:** data directory `0700`; secrets, tokens, the TLS key and the audit log `0600`.
  Readings are encrypted at rest with `BLLU_DATA_PASSPHRASE` (XChaCha20-Poly1305 / Argon2id).
- **Audit log:** `<data>/audit.log` (JSON lines) records pairing, rotation, revocation, bans
  and every rejected attempt with its IP. It never contains secrets.
- **Least privilege:** the Docker image runs as UID 10001 with a read-only root filesystem, all
  capabilities dropped and `no-new-privileges`. The systemd unit runs as `bllu` with
  `ProtectSystem=strict`, no capabilities, a syscall filter and only network address families.
- **Client side:** every reading received from the server is range-checked (time, value,
  trend) before it touches the local log, and responses are capped at 32 MiB.

### Threat model in short

| Attacker | Result |
|---|---|
| on the network (MITM, rogue Wi-Fi, DNS spoofing) | handshake fails: certificate pinning |
| scanning the port | needs a client certificate to finish TLS, then a valid token bound to that certificate, and is banned after 10 attempts |
| has an old pairing code | single use, expires after ≤ 60 min |
| copied the settings file | it holds no secrets; token and key are in the OS keychain |
| stole the token (e.g. from memory) | useless without the private key; rotated every 30 days; revoke the client |
| got the server's disk | readings encrypted with `BLLU_DATA_PASSPHRASE`; token hashes only |

With your own certificate from a public CA (`BLLU_TLS_CERT`/`BLLU_TLS_KEY`), pairing codes
contain no fingerprint and normal certificate validation is used. TLS must end at the server
itself, so reverse proxies have to use TLS passthrough: mutual TLS cannot work through a proxy
that terminates TLS.

## Docker

```bash
cd server
printf '%s' 'your-librelinkup-password' > llu_password.txt
head -c 32 /dev/urandom | base64 > data_passphrase.txt
chmod 600 llu_password.txt data_passphrase.txt
# edit LLU_EMAIL, BLLU_PUBLIC_URL, BLLU_TLS_SANS and BLLU_ALLOW_CIDRS in docker-compose.yml
docker compose up -d
docker exec bllu bllu-server pair
```

Paste the printed `bllu1:…` code into the desktop app: **Settings → Caching server → Pair**, or on
the sign-in screen **Use a caching server instead**.

Build the image yourself from the repository root: `docker build -f server/Dockerfile -t bllu-server .`
Tagged releases publish `ghcr.io/nucl3arn30n/bllu-server`.

## LXC / plain Linux (systemd)

In a Debian/Ubuntu container (e.g. Proxmox LXC):

```bash
git clone https://github.com/NUCL3ARN30N/better-librelinkup && cd better-librelinkup/server/packaging
sudo sh install-lxc.sh            # downloads the latest bllu-server release and verifies its checksum
sudo nano /etc/bllu-server.env    # LibreLinkUp login, public URL
sudo systemctl restart bllu-server
sudo bllu-pair                    # prints a pairing code
```

The service runs as the unprivileged user `bllu` with a hardened systemd unit; data lives in
`/var/lib/bllu-server`.

## Configuration

| Variable | Default | |
|---|---|---|
| `LLU_EMAIL`, `LLU_PASSWORD` / `LLU_PASSWORD_FILE` | – | LibreLinkUp follower login (required) |
| `LLU_SERVER` | automatic | force a LibreLinkUp API host |
| `BLLU_PUBLIC_URL` | `https://<first SAN>:<port>` | address put into pairing codes |
| `BLLU_TLS_SANS` | hostname, localhost, 127.0.0.1 | names/IPs in the self-signed certificate |
| `BLLU_TLS_CERT`, `BLLU_TLS_KEY` | – | use your own certificate (PEM) |
| `BLLU_LISTEN` | `0.0.0.0:8443` | |
| `BLLU_DATA_DIR` | `./data` (`/data` in Docker) | |
| `BLLU_DATA_PASSPHRASE` / `_FILE` | – | encrypt stored readings (recommended) |
| `BLLU_ALLOW_CIDRS` | everyone | networks allowed to connect, e.g. `192.168.0.0/16,100.64.0.0/10` |
| `BLLU_POLL_SECONDS` | `60` | 30–900 |
| `BLLU_NAME` | `bllu-server on <host>` | shown in the app |
| `BLLU_DEMO=1` | – | synthetic data, no LibreLinkUp (try it out) |

Commands: `serve` (default), `pair [--ttl MIN] [--url URL]`, `clients`, `revoke ID|--all`,
`close-pairing`, `fingerprint`.
A pairing code is also printed to the log on the first start while no client is paired.

## API (v1)

Every connection needs TLS 1.3 with a client certificate. All endpoints except `health` and
`pair` also need `Authorization: Bearer <token>` matching that certificate.

| | |
|---|---|
| `GET /api/v1/health` | name, version |
| `POST /api/v1/pair` | `{secret, client_name}` → `{client_id, token, server_name}` |
| `POST /api/v1/rotate` | → `{token}` (replaces the current token) |
| `GET /api/v1/patients` | followed patients incl. current value, sensor, last poll |
| `GET /api/v1/readings?patient=&since=&until=` | `[[unix, mg/dL, trend], …]` (max. 400 days per request) |
