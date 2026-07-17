# Local CI on the NAS: git push → Dockhand builds & redeploys

A fully local pipeline — no GitHub Actions, no image registry, no CI runner.
The pieces are a lightweight git server on the NAS and Dockhand's own git-stack
machinery:

```
laptop ── git push nas ──▶ Gitea (a Dockhand stack on fnOS)
                              │  push webhook
                              ▼
                          Dockhand git stack "shion"
                              │  clone repo → docker compose up -d --build
                              ▼
                          shion container (image built natively on the NAS)
```

The NAS is amd64, so the image builds natively — no buildx cross-compiling
from a laptop. The Dockerfile's BuildKit cache mounts (cargo registry +
target dir) persist on the NAS daemon, so only the first build is slow;
incremental pushes recompile just what changed.

## 1. Git server (Gitea)

Deploy Gitea as an ordinary Dockhand stack (Forgejo works identically —
Dockhand's webhook support names both; Gitea's Docker Hub image is the
practical pick where codeberg.org pulls are slow). Pin the minor version and
bump deliberately, not via `latest`:

```yaml
# Set GITEA_HOST to the NAS's LAN IP (or hostname) in the stack's
# environment — it only affects the clone/webhook URLs Gitea displays.
services:
  gitea:
    image: gitea/gitea:1.27
    container_name: gitea
    restart: unless-stopped
    environment:
      USER_UID: "1000"
      USER_GID: "1000"
      TZ: ${TZ:-Asia/Shanghai}
      # Render correct clone URLs for the non-default ports.
      GITEA__server__ROOT_URL: http://${GITEA_HOST:-nas.local}:3000/
      GITEA__server__SSH_DOMAIN: ${GITEA_HOST:-nas.local}
      GITEA__server__SSH_PORT: "2222"
      # LAN git server for one person — no open signups. The first-run
      # installer still creates the admin account.
      GITEA__service__DISABLE_REGISTRATION: "true"
    volumes:
      - ${GITEA_DATA_DIR:-/vol1/docker/gitea}:/data
    ports:
      - "3000:3000"   # web + http git
      - "2222:22"     # ssh git (22 belongs to the NAS's own sshd)
    networks:
      - common

# Shared pre-existing network, so other stacks (Dockhand itself, a reverse
# proxy) reach Gitea by container name instead of the published ports.
networks:
  common:
    external: true
```

Open `http://<nas>:3000`, finish the installer (SQLite is fine; create the
admin account there), create the `shion` repo, then from the dev machine:

```bash
git remote add nas ssh://git@<nas-ip>:2222/<user>/shion.git
git push nas main
```

## 2. Dockhand git stack

Create a new stack in Dockhand with type **Git**:

- **Repository**: the Gitea URL above (credentials under Settings → Git)
- **Compose file**: `docker-compose.yml` (repo root — Dockhand clones the
  whole directory, so the `build: .` context includes the full source tree)
- **Build images on deploy**: **on** (adds `--build` to `docker compose up`)
- **Stack environment variables**:

  | var | value | why |
  |---|---|---|
  | `SHION_PULL_POLICY` | `build` | build locally, never pull from ghcr |
  | `SHION_TAG` | `local` | local image tag never collides with a pulled `:latest` |
  | `SHION_DATA_DIR` | `/vol1/docker/shion/data` | absolute host path for `/data` |
  | `SHION_WORKSPACE_DIR` | `/vol1/docker/shion/workspace` | absolute host path for `/workspace` |

  Credentials go in either `<SHION_DATA_DIR>/.env` or as further stack
  variables (the compose file passes `DEEPSEEK_API_KEY` etc. through).

## 3. Webhook: push → redeploy

Copy the stack's webhook URL from Dockhand
(`POST /api/stacks/{id}/webhook?token=<secret>`) and add it in the Gitea
repo under Settings → Webhooks → Gitea, triggering on push. Dockhand
redeploys only when the git commit actually changed, so duplicate webhook
deliveries are harmless.

That's the whole loop: `git push nas` rebuilds and redeploys shion.

## Notes

- The ghcr path is untouched: without `SHION_PULL_POLICY` the compose file
  still defaults to `pull_policy: always` against
  `ghcr.io/solren7/shion:latest`, and the `build:` section is inert unless
  `--build` is passed.
- First NAS build of a Rust release binary takes a while on an N-series CPU
  (expect tens of minutes); later builds reuse the BuildKit cache mounts.
  If that ever becomes the bottleneck, the upgrade path is a Gitea Actions
  runner pushing to a local registry — but don't add that infrastructure
  until the simple loop actually hurts.
- No tests run in this loop by design; it is a deploy pipeline. Run
  `cargo test` before pushing, or add a Gitea Actions workflow later if
  gating is wanted.
