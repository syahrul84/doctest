# doctest

An MCP server (and CLI) in Rust for Laravel + Vue 2/3 projects. It keeps three things current, with Claude doing
the writing:

- **User guide**: one page per screen, in each language you configure, with screenshots: one guide per portal
  (admin, tenant, vendor…) in static HTML under `document/guide/`, a folder that stands alone so the server can
  publish it at `/docs`.
- **System document**: `document/index.html` (installing on a real server from scratch) plus architecture,
  configuration, API, database, jobs and one page per module.
- **PHPUnit tests**: one parameterised test per route or method, with a data provider row for each case
  (`ok`, `guest`, `forbidden`, `not_found`, `invalid:<field>:<rule>`, …). A case is never tested twice: doctest
  reads your hand-written tests and laravel-parity's too.

It also flags **code to fix** as `file:line — problem: reason. Fix: …` (N+1 queries, missing indexes, filtering in
PHP, unpaginated lists, mail sent during the request, `env()` outside config, Vue `v-for` without `:key`, API calls
in loops, whole-library imports, failing tests…). And it **validates a change** (`verify`): it runs the affected
tests and doctest's tests, then lists problems on the changed lines.

doctest never calls an LLM. It works out what is missing or stale, gives Claude the facts (from
[codemap](https://github.com/syahrul84/codemap)), checks what Claude writes against the real page, takes the
screenshots, runs the tests and builds the HTML.

## Install

```bash
cargo build --release && ./install.sh          # copies target/release/doctest to ~/.local/bin
doctest setup                                   # registers the MCP server and adds a rule block to ~/.claude/CLAUDE.md
```

`doctest setup --scope project` does it for one project instead (`.mcp.json`, the project's `CLAUDE.md`, and
`.doctest.toml` when the Laravel repo has none). `--check` reports, `--dry-run` shows what would change, `--remove`
undoes it.

Needs the codemap release binary (`codemap` on the PATH or `~/.local/bin/codemap`, or `CODEMAP_BIN`), git and, for
screenshots, Google Chrome (or `CHROME_PATH`).

## Set up a project

In the Laravel repo (the backend; frontends can be sibling repos):

```bash
doctest init      # writes .doctest.toml; fill in the TODO lines
```

```toml
[project]
name = "shop"
frontends = ["../shop-web"]          # Vue apps, relative to this file
languages = ["en", "id"]              # first = primary; guides are written in it first, then translated
out = "document"

[app]                                 # the running app for screenshots
base_url = "http://localhost:8196"
login_url = "/session/admin/login"
login_steps = [
  { fill = "input[name=username]", value = "$user" },
  { fill = "input[name=password]", value = "$password" },
  { click = "button[type=submit]" },
  { wait = ".admin-layout" },
]
locale_steps = [ { goto = "/admin?lang=$lang" } ]   # how to switch the UI language, if you have several

# Screenshots sign in through `doctest login` (below). Only for unattended runs, name env vars with a test login:
# [app.users.admin]
# user_env = "DOCTEST_USER"
# password_env = "DOCTEST_PASSWORD"

[screenshots]
max_width = 1600
mask = [                              # applied before every capture, inside shadow roots, again when the page re-renders
  { selector = ".tenant-name", text = "Demo Tenant" },                       # replace text or input values
  { regex = "\\b[A-Z]{2}-\\d{2}-\\d{2}\\b", text = "D-01-01" },             # JavaScript regex; (?i) ignores case
  { hide = ".user-avatar" },                                                 # hide elements
]
# safe = true                         # default: the browser refuses POST/PUT/PATCH/DELETE while shooting
allow = ["POST /api/*/search"]        # requests that only read, let through in safe mode
# per_language = true                 # retake screenshots in every language (default: translations reuse the primary's)

[tests]
run = "docker exec shop-backend sh -c 'cd /var/www/api && php artisan test {filter}'"

[guide]
modules_from = "../shop-web/src/config/adminMenu.ts"   # menu file: chapters and menu paths
allow_bold = ["Note"]                 # bold words allowed that are not on any screen
include = ["admin.*"]                 # only these pages (route name or path; * at the end); empty = all
exclude = []
require_actions = true                # a section per action (Add, Edit, Delete, Approve…) with what you should see
# min_screenshots = 4                 # default: the page, one form shot for Add and Edit, one per other action

publish_path = "/docs"                # where the server publishes document/guide

# Portals: written by `doctest init` from the app's login pages; pages matching no portal are public.
[guide.portals.admin]
title = "Admin guide"
pages = ["admin.*", "/admin/*"]
login_url = "/session/admin/login"
[guide.portals.selfcare]
title = "Selfcare guide"
pages = ["selfcare.*", "/selfcare/*"]
login_url = "/session/selfcare/login"

[work]
workers = 4                           # AI workers claiming items, tool calls at the same time
# parallel_reads = true               # also read pages with several codemap clients (off: slower on big apps)
```

## Screenshots

A screenshot is a spec: `path` (with `params` for `:id`), `lang`, `steps`, `crop` (a CSS selector), `mask`. Steps run
in order: `{ fill = "css", value = "text" }` (clears the field first), `{ select = "css", value = "v" }`,
`{ click = "css" }`, `{ wait = "css" }`, `{ wait_text = "Saved" }`, `{ scroll = "css" }` (or `"top"`, `"bottom"`),
`{ goto = "/path" }`. doctest waits for the page's data to finish loading before masking and capturing, and tells
you which masks matched nothing.

**Safe mode** (on by default): while a screenshot is taken, the browser refuses every request that could change
data (POST, PUT, PATCH, DELETE), so a step that clicks Save or Delete changes nothing; the reply lists what it
blocked. A spec sets `allow_saves` to show a real save on test data (retaking it saves again, and doctest warns);
`[screenshots] allow` lets through requests that only read, such as a search sent as POST.

Each worker has its own Chrome. If Chrome crashes or its connection drops, doctest starts a new one and tries again.
Every Chrome doctest starts is tagged with its doctest, so Chromes left behind by a doctest that was killed are
stopped within a minute, and a doctest stops its own when the client closes. `doctest doctor` opens the app in
headless Chrome to check it loads.

Translations reuse the primary language's screenshots. With `per_language = true` the AI retakes a page's stored
screenshots in another language with one call (`shoot {retake_page, lang}`). Screenshots are written as compact PNGs;
`doctest shrink` re-encodes existing ones (about 30% smaller, same pixels).

A guide needs the page, one form shot for Add and Edit, and one shot per other action; more only where they help.
If a page has no records to show, put `<!-- no-data: why -->` in the guide and only what works without records is
required.

## Portals and signing in

doctest reads every screen of the app, whatever it is built with: Vue 2 / Vue 3 / Nuxt pages (a separate repo or
inside the Laravel repo), Filament pages and Blade pages. It finds the **portals** — the app's sign-in areas — from
its login pages: `/session/admin/login` and `/session/selfcare/login` make an *admin* and a *selfcare* portal, a
Filament panel at `/admin` makes *admin*, `/portal/login` makes *portal*, a single `/login` makes one portal for the
whole app, and an app with no login page has none. Each screen belongs to the portal named in its URL or route name;
screens outside every portal are public. Each portal gets its own guide.

```bash
doctest portals               # what doctest found: portals, their sign-in page, screens, signed in or not
doctest login                 # signs in to every portal not signed in yet, one Chrome window each (OTP is fine)
doctest login selfcare        # just one portal
```

doctest keeps each portal's session in its own Chrome profile (under `~/.doctest/chrome/`, separate from your
browser); screenshots of a page reuse its portal's session, public pages need none. No password is stored. When a
session expires doctest says so; run `doctest login` again (or the AI calls the `login` tool, which opens the same
window). `doctest init` writes the portals it found into `.doctest.toml`, so you can rename them or move pages.

Examples: an app with admin, selfcare and contractor logins gets three portals; a Laravel site with a Filament
admin and a Blade member area gets two, plus its public pages.

## Use

In Claude Code, type one of doctest's commands:

| Command | Does |
| --- | --- |
| `/mcp__doctest__all` | Document and test the whole application: everything missing or out of date |
| `/mcp__doctest__all redo` | The same, rewriting every guide and retaking its screenshots |
| `/mcp__doctest__branch` | Only what this git branch changed (compared with main/master); `/mcp__doctest__branch develop` compares with develop |
| `/mcp__doctest__changes` | Only what changed since the last finished run |

One worker does the work unless you add a number: `/mcp__doctest__all 4` runs four in parallel. At the end doctest
reports how long the run took (also on the changes page) and what it wrote.

The MCP tools behind these commands:

| Tool | Does |
| --- | --- |
| `status`, `plan` | What is missing or stale, with overall progress % (`scope`: `changes` since the last `finish`, `branch` since the branch left `base` (main/master), or `all`) |
| `claim` | For parallel AI workers: each worker gets its own next items (claims expire after 30 min); `queue: "guides"` or `"tests"` keeps guide and test workers apart, `portals: ["admin"]` keeps a worker to some portals (also on `status` and `plan`) |
| `facts` | Everything for one work item: real labels, texts, endpoints, rules, messages, menu path, skeleton |
| `write_doc` | Store a guide/translation/system section; refused if it names UI the page lacks |
| `shoot` | Take or retake a screenshot from a spec; returns the image |
| `cases`, `write_test` | Expected cases and who covers them; add data-provider rows, run them, keep what passes |
| `run_tests`, `check`, `build` | Run doctest's tests; check everything (CI); rebuild the HTML |
| `issues` | Code to fix, as points |
| `verify_changes` | Validate the current change: pass/fail with points |
| `finish` | Record the commits so the next `changes` scope starts here |

`status`, `plan` and `claim` keep each page's facts between runs (under `~/.doctest/cache/`) and read again only
the pages whose code changed: on a 900-screen app a repeat `status` takes about 20 seconds instead of 3 minutes.

Long calls report progress (`[ 37%] reading pages 170/460`) as MCP progress notifications and on stderr.

Parallel: `doctest serve --workers 4` (the default) runs 4 tool calls at the same time; start 4 subagents that each
`claim` their own items — writing guides and tests is where the time goes. Reading the code stays on one codemap
client by default: on a large app (900+ screens), four clients were 2.5× slower than one (codemap is CPU-bound per
call), while the single-client planner covers 165 screens in about 45 seconds. Test runs stay one at a
time per project, because they share the test database.

CLI: `doctest doctor`, `doctest shrink`, `doctest reports [--send <id>]`, `doctest update [--check]`, `doctest status [--scope changes|branch|all] [--base master]`, `doctest check` (exit 1 on errors), `doctest verify` (exit 1 on failures),
`doctest build`, `doctest serve`.

## Publishing the guide

Only `document/guide/` is published. On the server, point the web server at it, e.g. nginx:

```nginx
location ^~ /docs/ { alias /opt/your-app/api/document/guide/; index index.html; }
```

The start page and system document name your env keys and server layout: keep them internal.

## Problems, updates, health

- **Problem reports.** When doctest itself fails (a panic, codemap crashing or hanging, test output it cannot read,
  Chrome failing) it records a problem report on this machine, scrubbed of paths, names and hosts. The AI can also
  report a wrong or confusing answer with the MCP tool `report_problem`. Nothing is sent anywhere until you run
  `doctest reports --send <id>`, see exactly what would be filed, and say yes: it becomes a GitHub issue on this
  repository (or a comment on the open issue for the same problem). `doctest reports` lists them.
- **Updates.** `doctest update` installs the latest release after checking it against the release's SHA256SUMS;
  `doctest update --check` only asks. The MCP server checks once a day and mentions a newer version once.
- **Health.** `doctest doctor` (and the MCP tool `doctor`) checks codemap, git, curl, gh, Chrome, the settings, the
  apps, languages, menu, test login, test command and state, and says what to fix.

## Files

```
document/
  index.html, system/…                   internal: server setup, architecture, API, database (do not publish)
  guide/                                 the published user guide: index.html, <lang>/<portal>/…, assets/, img/<lang>/
  assets/                                css, js and search index of the internal pages
  _source/                               the Markdown sources (edit these)
  .doctest/state.json                    commits per repo, page fingerprints, screenshot specs, coverage
```

Generated tests live in a `// <doctest>` … `// </doctest>` block at the end of each test class. Rows for found bugs
go to a `bugs_<method>` provider run by `<method>_known_bugs` in group `doctest-bug`, so CI can exclude them with
`--exclude-group doctest-bug`.
