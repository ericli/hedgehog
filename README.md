# 🦔 hedgehog

**Drop a repo on your Mac, get a private URL.**

hedgehog turns a Mac (say, a Mac mini at home) into a tiny Railway for personal software. Put a project folder in `~/Hedgehog/apps`, or run `hedgehog deploy` from your laptop, and hedgehog:

- **builds it**, from its Dockerfile or, if it has none, with [Railpack](https://railpack.com) (Railway's open-source builder),
- **runs it** in Docker, restarting it when it crashes and after reboots,
- **gives it a Postgres database** from one shared instance (`$DATABASE_URL`, one database and role per app) and a persistent `/data` folder,
- **publishes it** at `https://<app>.<id>.opentunnel.xyz` through [OpenTunnel](https://github.com/anomalyco/opentunnel), with no domain, port forwarding, or certificates to manage,
- **keeps it private**: only devices you've paired can open your apps.

```
your phone ──https──▶ OpenTunnel relay ──(still encrypted)──▶ ┌─ your Mac ───────────────────────────┐
                                                              │ hedgehog daemon                       │
                                                              │  ├─ TLS ends here (OpenTunnel client) │
                                                              │  ├─ gateway: is this a paired device? │
                                                              │  ├─ budget  (container) ─┐            │
                                                              │  ├─ notes   (container) ─┼─ Postgres  │
                                                              │  └─ control API ◀── `hedgehog` CLI    │
                                                              └───────────────────────────────────────┘
```

## Quick start

On the Mac that will host your apps (it needs [Docker Desktop](https://www.docker.com/products/docker-desktop/) or [OrbStack](https://orbstack.dev) running):

```sh
cargo install --path crates/hedgehog            # from a clone of this repo
curl -sSL https://railpack.com/install.sh | sh   # for apps without a Dockerfile
hedgehog setup
```

`setup` checks your tools, starts the daemon at login, connects the tunnel, and shows a QR code. Scan it with your phone to pair it. Your phone can then open your apps and manage them from the [dashboard](#the-dashboard).

Then either drop a folder in:

```sh
git clone https://github.com/you/budget ~/Hedgehog/apps/budget
```

or deploy from inside a project:

```sh
cd ~/code/budget
hedgehog deploy
# ==> building budget with railpack
# ==> preparing the database
# ==> starting
# ✓ Live at https://budget.k3x9m2q8a1bc.opentunnel.xyz
```

## Deploying from your laptop

The hedgehog Mac's control API listens on its [Tailscale](https://tailscale.com) address, so you can deploy from anywhere on your tailnet.

```sh
# on the hedgehog Mac
hedgehog token
# prints a token and the exact command to run on your laptop, e.g.:

# on your laptop
hedgehog remote add home http://mac-mini.tail1234.ts.net:7700 --token <token>
cd ~/code/budget && hedgehog deploy
```

`hedgehog deploy` uploads the folder as it is on disk (uncommitted changes included), skipping anything `.gitignore`d plus `.git`, `node_modules`, and `.venv`. The new version starts next to the old one, and traffic switches over only once it answers HTTP. A broken build or a crashing app never takes down the running version.

Folders that are git repos also redeploy on their own: `git pull` inside `~/Hedgehog/apps/<app>` and the new commit goes live a few seconds later.

## Who can open your apps

OpenTunnel URLs are on the public internet, so hedgehog puts a gateway in front of every app.

- **Pair a device:** run `hedgehog pair` and open the link (or scan the QR code) on that device. Pairing sets a long-lived cookie for `<id>.opentunnel.xyz`, so one pairing covers every app.
- **Admin devices:** `hedgehog pair --admin` also lets the device manage hedgehog from the dashboard. Plain pairing only opens apps, which keeps it safe to give to other people later.
- **Unpaired visitors** see a "this is a private app" page.
- **Make an app public:** `hedgehog public <app> on` lets anyone with its URL in, e.g. for webhooks or something you want to share.
- **Manage devices:** `hedgehog devices` lists paired devices and what each can do; `hedgehog devices revoke <id>` removes one.

Your apps never see hedgehog's cookie, and they receive `X-Forwarded-Proto: https` and `X-Forwarded-Host`.

## The dashboard

On an admin device, `https://<id>.opentunnel.xyz` (the address `hedgehog status` shows as "home") is a dashboard for your hedgehog. It works on a phone. From it you can:

- see every app's status, URL, last deploy, and commit, and open it,
- **redeploy**, **restart**, **stop**, or **start** an app, with build output streamed live,
- **pull & redeploy** an app that's a git clone: hedgehog runs `git pull` in its folder on the Mac and deploys the new commit. This is how you ship from your phone after pushing to GitHub,
- follow an app's logs and read its last build log,
- add, change, or remove env vars (values stay hidden until you tap Show); saving restarts the app,
- make an app public or private, or remove it,
- pair new devices with a QR code, and revoke old ones.

The dashboard uses the same control API as the CLI, but logs in with your device cookie instead of the CLI token. Requests must come from the dashboard page itself, so a page on one of your apps (they share the `opentunnel.xyz` site) can't drive it with your cookie. For private repos, "pull" needs this Mac to fetch without a password prompt, e.g. with an SSH key.

## What your app gets

| Variable | Value |
|---|---|
| `PORT` | `8080` (or `port` from `hedgehog.toml`). Listen on it. |
| `HOST` | `0.0.0.0` |
| `DATABASE_URL` | `postgres://<app>:…@hedgehog-postgres:5432/<app>` |
| `DATA_DIR` | `/data`, backed by `~/Hedgehog/data/<app>`, which survives redeploys |
| `HEDGEHOG_APP`, `HEDGEHOG_URL` | the app's name and public URL |

Env is layered, last wins: the app folder's `.env` → the variables above → `[env]` in `hedgehog.toml` → `hedgehog env <app> set`. Changing variables restarts the app.

Databases are dumped nightly to `~/Library/Application Support/hedgehog/backups/<app>/`, keeping the last 7.

## A dev database on your laptop

To work on an app locally against the same Postgres it gets when deployed, you don't need a daemon, only Docker:

```sh
hedgehog db up        # dev Postgres on localhost:5432; it starts whenever Docker does
cd ~/code/budget
hedgehog db init      # creates the `budget` database and adds DATABASE_URL to .env
bun dev               # or whatever runs your app; it reads .env as usual
```

- **Same setup as deployed:** it's the same Postgres image as the hedgehog Mac, with one database and role per app, named the way `hedgehog deploy` would name the app.
- **Your `.env` is safe:** `db init` won't overwrite a `DATABASE_URL` that points somewhere else unless you pass `--force`.
- **No leaks into deploys:** a deployed app always gets its own `DATABASE_URL` from hedgehog, so the local one never applies there.
- **Other commands:** `hedgehog db url` prints the URL, and `hedgehog db reset` empties the database but keeps its URL.
- **Port and data:** `hedgehog db up --port 5433` moves it off a busy port, and `db init` updates `.env` to match. `hedgehog db down` stops it and keeps the data.
- **psql:** `hedgehog db psql --dev` opens psql on the project's dev database.

## `hedgehog.toml` (optional)

```toml
name = "budget"             # app name for `hedgehog deploy` (default: folder name)
port = 3000                 # if the app ignores $PORT
public = false              # skip the device check
postgres = true             # set false to skip the database
start = "bun run start"     # start command, if Railpack can't work it out
build = "railpack"          # or "dockerfile" (default: Dockerfile if present)
dockerfile = "Dockerfile"

[env]
NODE_ENV = "production"
```

## Commands

```
hedgehog setup                    set up this Mac (daemon at login, tunnel, pair your phone)
hedgehog deploy [path]            upload a folder and deploy it
hedgehog redeploy <app>           rebuild from the folder already on the hedgehog Mac
hedgehog ls                       apps, status, URLs
hedgehog logs <app> [-f] [--build]
hedgehog env <app> [set K=V… | unset K… | import .env]
hedgehog restart|start|stop <app>
hedgehog rm <app> [--purge]       folder goes to the Trash; --purge also drops data and database
hedgehog open <app>
hedgehog public <app> on|off
hedgehog pair [--admin]           pair a device (--admin: it can use the dashboard too)
hedgehog devices [revoke <id>]
hedgehog db up|down [--port N]    dev Postgres on localhost (no daemon needed)
hedgehog db init|url|reset        a project's dev database and its DATABASE_URL in .env
hedgehog db psql <app> | --dev    psql into a deployed app's database, or the dev one
hedgehog status / doctor
hedgehog token / remote add|ls|use|rm
hedgehog daemon run|install|uninstall|logs
```

`--remote <name>` (or `HEDGEHOG_REMOTE`) picks which hedgehog a command talks to.

## Running it on a Mac mini

hedgehog runs as a per-user LaunchAgent, because Docker Desktop and OrbStack run per user too. For a Mac that should come back on its own after a power cut:

- System Settings → Users & Groups → **automatically log in** as your user. This requires turning FileVault off; with FileVault on, the Mac waits at the login screen after a reboot.
- System Settings → Energy → **start up automatically after a power failure** and **prevent automatic sleeping**. From a terminal: `sudo pmset -a sleep 0 autorestart 1`.
- Set Docker Desktop or OrbStack to **start at login**.
- Keep Tailscale running, so `hedgehog deploy` works from your laptop.

## Good to know

- **Tunnel hostnames.** Each hedgehog gets one random OpenTunnel hostname (`<id>.opentunnel.xyz`). It stays the same as long as `~/Library/Application Support/hedgehog/opentunnel` exists. Custom domains aren't supported by OpenTunnel. OpenTunnel closes connections that are idle for 15 minutes, so long-lived WebSockets should send keepalives.
- **Docker credentials.** Background services can't answer macOS keychain prompts, so hedgehog's `docker` commands use a copy of your Docker config without the credential helper. Public base images work as usual. If you build from private registries, set `HEDGEHOG_DOCKER_CREDENTIALS=1` in the daemon's environment.
- **HTTP only.** hedgehog proxies HTTP(S) and WebSockets. It doesn't handle raw TCP.

## Development

```sh
cargo test                                   # unit tests, including a gateway proxy/auth test
HEDGEHOG_HOME=/tmp/hh cargo run -- daemon run --no-tunnel   # a throwaway daemon on 127.0.0.1
HEDGEHOG_HOME=/tmp/hh cargo run -- deploy examples/hello-bun
```

`HEDGEHOG_HOME` keeps apps, data, and state in one scratch folder instead of `~/Hedgehog` and `~/Library/Application Support/hedgehog`.

- `crates/hedgehog` is the CLI.
- `crates/hedgehog-daemon` holds the daemon: gateway, deploy pipeline, Docker, Postgres, tunnel, and control API.
- `crates/hedgehog-core` holds the types they share.
- `examples/` has a Railpack-built Bun app that uses Postgres and WebSockets, and a Dockerfile-built Python app that uses `/data`.

## License

MIT
