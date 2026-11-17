# SwingMusix

Rust rewrite of [Swing Music](https://github.com/swingmx/swingmusic) v2.1.14 — same functionality, different architecture (Rust/axum vs Python/Flask) and new UI.

**License: AGPL-3.0-or-later** (upstream derivative work — source must stay public).

## Status — F1 (auth core) ✅

- Global JWT middleware (cookies + headers, exempt list, expiring-cookie refresh) — parity `app_builder.py`
- All 13 auth routes — parity `api/auth.py`
- PBKDF2-SHA256 password hashing (100k, salt=`serverId`) with legacy `serverId` fallback
- HS256 JWT, claims byte-compatible with flask-jwt-extended (flask-jwt-extended 4.6.x)
- `/docs` + `/docs/openapi.json` + `/openapi.json`
- Static client serve + `/onboarding-data`
- **E2E: 26/26 parity tests pass** (`test-parity.sh`)

Not yet (later phases): library scan, streaming, playlists, SSE, plugins, image proxy — see PRD §13+.

## Build

```bash
cargo build --release
```

Binary: `target/release/swingmusix`.

## Run

```bash
SWINGMUSIC_CONFIG_DIR=/your/config SWINGMUSIC_PORT=1970 ./target/release/swingmusix
```

| Env var | Default | Purpose |
|---|---|---|
| `SWINGMUSIC_CONFIG_DIR` | `$XDG_CONFIG_HOME/swingmusic` or `~/.config/swingmusic` | config dir (holds `settings.json`, `swingmusic.db`, `jwt.key`) |
| `SWINGMUSIC_CLIENT_DIR` | `./client` | Vue SPA static files |
| `SWINGMUSIC_PORT` | `1970` | listen port |

`jwt.key` auto-created (32 random bytes hex, mode 0600) on first run.

## Verify

```bash
bash test-parity.sh   # requires the release binary at ./target/release/swingmusix
```

Runs an isolated instance on port 1971 with a scratch config dir (`/tmp/smxtest`); never touches CT101 data.

## DB compatibility

Drop-in with Swing Music v2.1.14 SQLite schema (`user`, `track`, `playlist`, …). Migration 1:1: copy `swingmusic.db` from an existing install.

## Deliverables

- PRD: `Projects/Personal/swingmusix.md` (Obsidian vault)
- Infra: CT104 `192.168.1.113`, Tailscale `swingmusix` / `100.70.165.50`
- Deploy target: `/opt/swingmusix` (CT104)
