# Changelog

## Unreleased (0.1.0)

Not released yet: being tested on real projects first.

Fixes from the first long run on a real project:

- Text first, screenshots after: guides are written without screenshots (a `<!-- shot -->` line marks each place);
  `shoot_pages` takes every page's opening screenshot in parallel without the AI and puts it in every language's
  guide; `shots:<page>` items ask for the action screenshots once the writing is done. A guide short of
  screenshots is a warning, not a refusal, and screenshots do not make translations stale.
- Screenshots no longer hang for 15 minutes: Chrome calls time out after 60 s, a shot after 90 s, page dialogs are
  answered, and route containers (`<router-view/>` only) are not pages. `shoot` returns the image only with
  `show: true`; crops below the fold of a layout's scroll box work, and a blank image is refused.

- A shot that lands on the sign-in page no longer says "session expired" when it is not: it names the request the
  server refused (401/419) or safe mode refused, and the worker's browser starts again from the saved session, so
  later shots keep working (before, every later shot of that worker failed).
- Calls to another portal's API reached through shared code (an admin layout reaching selfcare endpoints) are not
  the page's actions.

- A page's actions come only from its own components; actions reached through shared screens, layouts or API
  modules are listed as possible and are not required unless the page shows the button.
- Fewer required screenshots: the page, one form shot for Add and Edit, one per other action; `<!-- no-data -->`
  for pages with no records.
- Safe mode for screenshots (on by default): saves are refused while shooting; `allow_saves` and `[screenshots]
  allow` to opt in.
- Chrome: restarted when its connection drops; Chromes left behind are stopped; `doctor` loads a real page.
- Masks: `hide`, shadow roots, re-applied on re-render, regex `(?i)`, a note for masks that matched nothing.
  Steps: `wait_text`, `scroll`; `fill` clears the field. `shoot` accepts a flat spec and says when it retook a
  stored one.
- Work queue: `queue` (guides or tests) and `portals` filters on `claim`, `status` and `plan`, applied when an item
  is handed out; page facts cached between runs, so a repeat `status` reads only changed pages.
- A page's fingerprint covers only what its guide depends on, so upgrading doctest no longer marks every guide stale
  (guides checked with the old format carry over).
- Content checks: bold labels may hold ">" (a menu path is split only at " > "); form fields count by their name
  (Name, Code); column headers and dialog texts set in scripts count as screen text; messages given as i18n keys are
  shown as their text; only sentence-like success messages are expected in the guide.
- System document: module facts list the endpoints the module's pages call (controller, permissions, fields), the
  controllers, the tables of their models, the jobs they dispatch and the menu permissions; the server block is only
  in the start page, configuration and architecture facts.
- Translations reuse the primary language's screenshots (`[screenshots] per_language = true` retakes them per
  language from the stored specs in one call); screenshots are saved as lossless WebP (same pixels, about 70%
  smaller than PNG on a real project) and `doctest shrink` converts existing PNGs and updates the guides that
  reference them.

- MCP server and CLI for Laravel + Vue 2/3 projects, built on codemap.
- User guide per page and per language, one guide per portal, checked against the real screen (labels, buttons,
  messages), with a section per action and screenshots; published alone from `document/guide` (e.g. at `/docs`).
- System document: start page with server setup from scratch, architecture, configuration, API, database, jobs,
  modules.
- PHPUnit tests with data providers, one row per case; cases already covered by existing tests are never written
  again; obsolete rows flagged; failing rows removed or kept as known bugs.
- Code points to fix: N+1 queries, missing indexes, unoptimised queries and Vue code, failing tests.
- `verify` validates a change; scopes `changes`, `branch`, `all`; progress percentages; parallel AI workers via
  `claim`.
- Problem reports (`doctest reports`, MCP `report_problem`) filed as GitHub issues only after confirmation;
  `doctest update` from GitHub releases; `doctest doctor`.
