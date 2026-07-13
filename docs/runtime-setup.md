# Setting up a project for `knit run`

`knit run up` turns a bundle into a running app: every bundle repo with a
docker-compose file is lifted into an isolated per-bundle stack, built from the
bundle's worktrees — "docker compose up in each repo, with the bundle's code."
This guide walks from zero configuration to a fully prepared project, in the
order you should actually do it.

Requirements: Docker running, and at least one bundle repo with a compose file.
That's it — knit has no runtime dependencies beyond `docker compose`.

## Step 0 — try it with no configuration

If a repo already has a `docker-compose.yml` that developers use on `main`,
you're done:

```sh
knit bundle "my feature" --repo api --cd
knit run up        # lift the stack: worktree code, stable ports, isolated volumes
knit run status    # live service states, ports, URLs
knit run down      # stop and remove
```

What knit does to the compose shape (transform mode):

- paths that resolve into tracked repos (build contexts, dockerfiles,
  bind mounts) are remapped to the bundle's worktrees — repos *not* in the
  bundle keep building from their source checkouts on `main`
- every published host port moves to a free bundle port; repeated `up` calls
  reuse that bundle's recorded ports, and textual references inside
  `environment:`/build args are rewritten to match
- the stack runs as compose project `knit-run-<bundle>` with its own networks
  and named volumes, so bundles never collide with your dev stack or each other

Several compose-bearing repos in the bundle? Each becomes its own stack
(`knit-run-<bundle>--<repo>`), and references from one stack's environment to a
sibling stack's published port are rewired to the sibling's bundle port —
stacks find each other's bundle instances automatically.

## Step 1 — follow the one convention: ports live in compose `environment:`

Knit can only rewrite what it can see. The single most important preparation:

> Every cross-service URL, origin, or port your app needs must appear as a
> full `host:port` string in the compose file's `environment:` (or build
> args), and the app must read it from env.

Good (knit rewires all three per bundle):

```yaml
services:
  api:
    environment:
      - SELF_URL=http://localhost:8001
      - FRONTEND_ORIGIN=http://localhost:3000          # follows the frontend stack
      - PRICING_URL=http://host.docker.internal:8088   # follows the pricing stack
```

Bad (invisible to knit): ports hardcoded in `settings.py` / `config.ts`, or
origin allowlists maintained inside the app. Move the *values* to compose env;
keep the *reading* in app config.

For anything origin-checked (CORS, CSRF), also accept any localhost port in dev
so no allocation can ever block you — e.g. Django:

```python
CSRF_TRUSTED_ORIGINS += env.list('EXTRA_TRUSTED_ORIGINS', default=[])
CORS_ALLOWED_ORIGINS += env.list('EXTRA_CORS_ORIGINS', default=[])
if DEBUG:
    CORS_ALLOWED_ORIGIN_REGEXES += [r"^https?://(localhost|127\.0\.0\.1)(:\d+)?$"]
```

## Step 2 — commit the runtime block (`knit.project.json`)

The durable, machine-readable spec lives in the stack repo as
`knit.project.json` and is pulled into the workspace with
`knit project pull --repo <stack-repo>`:

```json
{
  "schemaVersion": "0.1",
  "kind": "KnitProject",
  "id": "my-project",
  "runtime": {
    "kind": "docker-compose",
    "database": {
      "mode": "shared",
      "service": "db",
      "host": "host.docker.internal",
      "port": 5435,
      "name": "app_dev",
      "startCommand": ["docker", "compose", "up", "-d", "db"]
    }
  }
}
```

Everything in `runtime` is optional. The fields you'll actually reach for:

| Field | What it does |
| --- | --- |
| `database.mode: "shared"` + `database.service` | Test against real dev data: the named compose service is stripped from every lifted stack and references to it are rewired to the shared dev database on `host`/`port` (URLs like `@db:5432`, split `*_HOST`/`*_PORT` vars). Reachability is checked before anything starts; `startCommand` can boot it. **Tradeoff:** bundle code — including its migrations — runs against the shared database. Omit the block to keep the default: an isolated, empty database per bundle. |
| `stacks: ["api", "frontend"]` | Narrow which repos lift (default: every bundle repo with a compose file). |
| `stackRepo` | Legacy single-stack pin; prefer `stacks`. |
| `composeFile` | Non-default compose filename in the configured stack repo. |
| `ports` | Contract-mode port pools (see below). |
| `profilePath` | Path opened on the frontend port by `knit run status`. |

## Step 3 — when the lift gets your stack wrong: `knit run eject`

The transform is a heuristic rewrite of your compose file, and some stacks are
beyond it — multi-database services, unusual build graphs, config the rewriter
can't see. The exit is one command:

```sh
knit run eject
```

It writes the lift as `docker-compose.knit.yml` into the stack repo checkout:
your compose shape, with repo paths, published ports, and the database wiring
replaced by `${KNIT_*}` interpolations (the file's header documents them all).
That file is ordinary docker compose — fix whatever the lift got wrong, run
`knit run up`, and commit it with the bundle. From then on the file is run
**as-is** with the environment contract injected (contract mode), and the
automatic transform no longer applies to that repo:

- `KNIT_ROOT`, `KNIT_BUNDLE`
- `KNIT_CHECKOUT_<REPO>` / `KNIT_SRC_<REPO>` / `KNIT_REV_<REPO>` per repo
- `KNIT_PORT_<SERVICE>` per published service — pools come from the
  `${KNIT_PORT_X:-default}` defaults in the file itself (or `runtime.ports`
  config, which wins)
- `KNIT_DB_HOST` / `KNIT_DB_PORT` / `KNIT_DB_NAME` / `KNIT_DB_HOST_PORT` / `KNIT_DB_MODE`

You can also write a contract file from scratch (any compose file named
`docker-compose.knit.yml` or referencing `${KNIT_*}` opts in), but ejecting a
working lift and editing it is almost always faster. Most projects never need
either — the point is that when the transform fails, the fix is a file in your
repo, not a knit change.

## Checklist for a new project

1. Each runnable repo has a working `docker-compose.yml` (the one devs already
   use). Nothing knit-specific in it.
2. Cross-service URLs/origins are `host:port` strings in compose
   `environment:`; the app reads them from env; dev origin checks accept any
   localhost port.
3. `knit.project.json` committed in the stack repo with the `runtime` block
   (shared database if you want real data); `knit project pull --repo <repo>`.
4. `knit bundle "smoke" && knit run up` — verify ports print, stacks start,
   and (shared mode) the app sees dev data.

## Troubleshooting

- **"No compose file found"** — the bundle repo has no
  `docker-compose.yml`/`compose.yaml`; add one or narrow `runtime.stacks`.
- **A project command named `up`/`down`/`status` exists** — it shadows the
  runtime verbs by design; rename it or run the runtime from another verb.
- **Stack count changed (repo added/removed)** — `knit run down` before the
  next `up`: compose project names change shape between single- and
  multi-stack runs.
- **App talks to the wrong port** — the reference isn't in compose
  `environment:`; move it there (step 1).
- **`knit run up` fails, or the lifted stack is just wrong** — `knit run
  eject`, then edit the generated `docker-compose.knit.yml` (step 3). Don't
  fight the transform.
- **First build is slow** — real image builds; layer cache makes subsequent
  runs fast. `knit run down` never deletes named volumes.
