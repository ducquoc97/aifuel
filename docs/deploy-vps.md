# Deploy aifuel on a server

Run `aifuel` on a VPS and it becomes your own AI endpoint wrapper: your apps
point an OpenAI- or Anthropic-compatible SDK at it, aifuel executes through the
Provider Integrations you configure (API-key integrations, OAuth-backed wire
integrations, or provider CLIs installed on the host), and downstream callers
authenticate with gateway keys you issue.

## What listens where

One listener serves everything (default port 8787):

| Surface | Path | Authenticates with |
|---|---|---|
| Client API | `/v1/*` (`/v1/chat/completions`, `/v1/responses`, `/v1/messages`, `/v1/embeddings`, `/v1/models`) | `aifuel-gw-*` gateway keys |
| Dashboard + admin API | `/`, `/credentials`, `/gateway/*`, `/api/*` | Admin Session (sign-in page) |
| Health | `GET /healthz` | none |

Binding a non-loopback address (`--host 0.0.0.0` or a LAN/public IP) switches
the server into remote mode:

- **Startup requires an Admin Credential.** With neither `admin.json` nor
  `AIFUEL_ADMIN_PASSWORD` present, `aifuel --host 0.0.0.0` refuses to bind.
- **Dashboard and `/api/*` require sign-in.** `POST /api/login` with the admin
  password issues an `aifuel_session` cookie (HttpOnly, SameSite=Lax, 30-day
  sliding expiry, `Secure` behind TLS). Sessions are in-memory: a restart
  signs everyone out.
- **`/v1` fails closed.** While the gateway key store holds no usable key,
  `/v1` answers 503 instead of the loopback anonymous posture - a public
  socket never serves unauthenticated requests.

`--host 127.0.0.1` (the default) is unchanged: no sign-in is needed unless an
admin password is configured, and `/v1` keeps accepting local callers.

## Provision

### Option A: binary + systemd

```bash
# on the VPS
./scripts/install.sh                       # installs `aifuel` into ~/.local/bin
aifuel auth set-admin --stdin              # set the dashboard admin password
aifuel auth set-key openai:api-key --stdin # store a provider key (repeat per integration)
```

`/etc/systemd/system/aifuel.service`:

```ini
[Unit]
Description=aifuel gateway
After=network-online.target

[Service]
ExecStart=%h/.local/bin/aifuel --host 127.0.0.1 --port 8787 --no-browser
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now aifuel
```

### Option B: Docker

```bash
docker build -t aifuel .
docker run -d --name aifuel \
  -e AIFUEL_ADMIN_PASSWORD='change-me-now' \
  -v aifuel-data:/data \
  -p 127.0.0.1:8787:8787 \
  aifuel
```

All state lives in `/data` (`credentials.json`, `admin.json`,
`gateway-keys.json`, `gateway.json`, run history). The `AIFUEL_ADMIN_PASSWORD`
env var is a bootstrap credential - the first `aifuel auth set-admin`
supersedes it:

```bash
docker exec -it aifuel aifuel auth set-admin --stdin
docker exec -it aifuel aifuel auth set-key openai:api-key --stdin
```

Inside the container only `*:api-key` and configured HTTP integrations work -
there are no provider CLIs. For CLI-backed providers (claude, codex, copilot,
antigravity, devin, ...) install those CLIs in a derived image and sign in, or
prefer the binary/systemd path on the host where they already exist.

## TLS: put a reverse proxy in front

aifuel speaks HTTP only. For a public endpoint, terminate TLS at a proxy so
gateway keys and the admin password never travel in the clear.

Caddyfile:

```caddy
ai.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Important: a public domain means requests arrive with `Host: ai.example.com`,
which the loopback guards reject by design. Run aifuel in remote mode -
`--host 0.0.0.0` (the Docker CMD does this) or bound to a private interface
the proxy can reach - and let the firewall be the network boundary so only
the proxy can touch port 8787. The Admin Credential requirement is what makes
that bind safe.

```bash
sudo ufw allow from 127.0.0.1 to any port 8787
sudo ufw deny 8787
```

For purely private use you can skip the proxy entirely: keep the default
`--host 127.0.0.1` and reach the dashboard through
`ssh -L 8787:127.0.0.1:8787 vps`.

## Issue gateway keys

Sign in to the dashboard (`https://ai.example.com` or via
`ssh -L 8787:127.0.0.1:8787 vps` for the loopback shape) and open
**Gateway - API Keys**: create one key per downstream app, optionally scoped
to specific `model` selectors. Keys are shown once; the store keeps only
SHA-256 digests.

Headless alternative after signing in once:

```bash
curl -fsS -b aifuel_session=<token> \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-app","models":["auto"]}' \
  https://ai.example.com/api/gateway/keys
```

## Point apps at it

```python
from openai import OpenAI
client = OpenAI(base_url="https://ai.example.com/v1", api_key="aifuel-gw-...")
client.chat.completions.create(model="auto", messages=[...])
```

`model` accepts the gateway addressing forms: `auto` (quota-headroom routing),
`<integration>/<model>` (pinned), a bare integration id, or a name defined in
`gateway.json` aliases/combos (editable under **Gateway - Routes**).

## Security notes

- Never expose the port without TLS and an admin password. Remote mode
  refuses to start unauthenticated, but TLS is on you: gateway keys and the
  admin password are bearer-equivalent secrets.
- Firewall 8787 to the proxy only (`ufw deny 8787` plus a loopback rule, or
  bind the proxy-facing interface).
- `admin.json` and `gateway-keys.json` are written owner-only (0600); keep
  the volume private.
- The dashboard's **Quit** button posts `/api/shutdown` - it stops the
  process, so treat sign-in sessions as operator-level.
