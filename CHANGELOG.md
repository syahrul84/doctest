# Changelog

## Unreleased (0.1.0)

Not released yet: being tested on real projects first.

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
