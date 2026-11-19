# Changelog

## Unreleased (0.1.0)

Not released yet: being tested on real projects first.

Fixes from the first long run on a real project:

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
  language from the stored specs in one call); screenshots are saved as compact RGB PNGs and `doctest shrink`
  re-encodes existing ones (about 30% smaller on a real project).

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
