# EXA status

Local dashboard that answers, for each customer group, each EXAScaler release
and each Future work initiative: where we are, what is blocking, and whether
we are on track for the proposed dates. Read-only toward Jira, Confluence,
Gerrit and EXA Reporter.

## Run

```bash
cargo build --release
./target/release/exa-status serve          # https://localhost:9836
./target/release/exa-status refresh-once --print
```

Run both from this directory: `data/`, `certs/`, `commitments.json` and
`config.local.json` are resolved relative to it. Only one refresh runs at a
time (`data/refresh.lock`); a second one stops with an error.
`refresh-once --out FILE` writes the snapshot elsewhere and adds no history
line; it still updates the caches.

`serve` refreshes when the snapshot is older than `refresh_minutes` (default
480, every 8 hours; 0 means by hand only) and on "Refresh now"; a failed refresh is retried
after one interval. A request for a refresh while one runs (including saving
a committed date) queues one more. Each refresh runs as a separate
`refresh-once` process on the server's own data directory and commitments
file, stopped after 15 minutes, so the server stays at a few MB. The server
rereads `refresh_minutes` from the config file every 30 seconds; other config
changes reach the next refresh, except `data_dir`, `commitments_file`,
`cert_dir`, `host` and `port`, which need a restart (the log says so). After
`cargo build`, restart the server: a server older than the snapshot layout
refuses to serve it.

As a background service (launchd, starts at login):

```bash
scripts/exa_status_daemon.sh start
```

`start` and `restart` install or update the agent; `status`, `logs`, `stop`
and `uninstall` do what they say. `EXA_STATUS_CONFIG` names the config file
the service is started with and `EXA_STATUS_PORT` its port; left unset, both
keep what the installed agent has, and an empty value drops one. The script
takes the port and certificate directory from `exa-status settings`.

## HTTPS

The server listens on the loopback address only (`host`: 127.0.0.1, or ::1)
and serves HTTPS with a self-signed certificate it creates in `certs/` on
first start (valid 820 days). It is replaced a month before its own expiry
date, or when the key does not match it; both are checked at start and
daily, and a replacement needs no restart. To stop browser warnings, trust it
once:

```bash
security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db certs/cert.pem
```

Trust it again whenever the certificate is replaced.

## Credentials

None stored here; each comes from the tool that owns it and is used only in
HTTP headers, never logged:

- Jira (cloud and Whamcloud) and Confluence: the `cloud` and `lu` entries of
  the llm_jira tool's `~/.jira-tool.json`.
- Gerrit: `GERRIT_URL`, `GERRIT_USER`, `GERRIT_PASS` in the gerrit-cli tool's
  `~/.config/gerrit-cli/.env`.
- EXA Reporter: no credentials; its certificate
  (`~/work/src/lustre_reporter/certs/cert.pem`) is pinned, and no other is accepted.

## Files

Everything the dashboard writes is created readable by the owner only.

| Path | What |
|---|---|
| `data/latest.json` | Last good snapshot; the page renders from it |
| `data/history.jsonl` | One line per refresh: open items and blockers per view, and whether the refresh was partial (a source failed, or data a verdict depends on was missing); kept for trend charts later |
| `data/comments.json` | Newest human comment per ticket, read again when the ticket or the bot list changes |
| `data/tags.json` | First ddn tag of each merged Gerrit revision |
| `data/refresh.log` | What the last refresh wrote to stderr |
| `data/refresh.lock` | Held by the running refresh |
| `certs/cert.pem`, `certs/key.pem` | The dashboard's certificate and key |
| `commitments.json` | Dates promised to NVIDIA and GCP; edited on the page |
| `config.local.json` | Optional overrides of any key in `src/config.rs`; the page saves the refresh interval here |

`data/`, `certs/`, `commitments.json` and `config.local.json` are git-ignored.

`scripts/seed_commitments.py` adds the committed dates of an NVIDIA tracker
workbook to `commitments.json`; stop the dashboard while it runs. Its tests:

```bash
/usr/bin/python3 -m unittest discover -s scripts -p 'test_*.py'
```

## Page tests

`static/app.test.mjs` loads `static/app.js` unchanged into a fake DOM, with
a fake server behind it, and drives the page as a person would: date
filters, the date editor, sorting, saved dates. It needs Node and nothing
installed:

```bash
node --test static/
```

## Rules

Defined in `src/rules.rs` and tuned in `config.local.json`:

- Groups: NVIDIA (NVDCSE, NCP, NP, and DDN/SFAFE tickets naming NVIDIA), GCP
  (GCP on Whamcloud, and DDN/SFAFE tickets naming GCP or Google), all others.
  Each `customer_sources` entry lists its projects. `[TEST]` tickets are left
  out. A ticket and its copy in a mirror project (`mirror_projects`: NCP and
  NP) are one row, with the more severe rating of the two. The All view shows
  the three groups together, judged by the same rules; its committed dates
  are set in the GCP and NVIDIA views.
- Idle time: days since the newest comment by a person (bots in
  `bot_authors`), or since creation when no person ever commented. It is read
  for every customer ticket and for the release items a rule needs it for,
  and blank elsewhere. When it could not be read, a rule treats it as idle:
  an S1/S2 ticket with unknown idle time puts its view at risk, and customer
  tables show it as "unknown" and sort it as the most idle.
- S1/S2 idle limits use DDN's Severity field. GCP, NVDCSE and NCP have no
  severity field, so their priority stands in (`priority_severity`):
  Blocker and Critical as S1, Major as S2, shown as "S1 (Critical)". NP, NCP's copy on
  Jira cloud, often carries DDN's Severity; an NP ticket without one is
  rated the same way.
- Projection: open items weighted by status (`status_weights`) divided by items
  closed per week over `closed_window_days`. An epic in a release counts as
  one item: 12 of the 23 open EXA7 GA epics had no child in a GA version on
  21 Oct 2026, so the epic is the only record of that work.
- Scope filters (`scope_filters`): a release whose work a saved Jira filter
  defines counts the filter's open issues, and its closures in the window,
  instead of every item in the fixVersion. ES6.3.10 uses filter 49208
  ("EX 6.3.10 scope"). Its view lists only those items for that release and
  says how many it leaves out; if the filter cannot be read, the release
  counts every item and its view reads as incomplete data.
- Verdict: measured against the agreed release date (`release_dates`, by
  release: an EXA6 fixVersion, EXA7 Beta, EXA7 GA, an EXA SD phase or an
  initiative; ES6.3.10: 27 Jan 2027, EXA7 GA: 31 Mar 2027), else the PM
  status page date, else the Jira fixVersion date. Up to `late_tolerance_days` late
  is at risk, later is off track. PM and Jira dates more than `date_gap_days`
  apart are flagged and, unless a date is agreed, turn on track into at
  risk. Releases and initiatives with `small_release_max` or fewer open items
  are judged per item.
- Incomplete data: when data a view's verdict depends on was not read, that
  view (and only that view) reads at best at risk and says so. That covers a
  customer source (in the views its projects feed), issues of a list that
  could not be read, an initiative, a configured fixVersion Jira lacks (its
  release reads "no date"), a dated PM page (the Jira date stands in, and the
  track says "PM page not read"), linked fixes, the commitments file
  (GCP and NVIDIA), and Gerrit in the views whose delivery patches decide.
  A refresh writes its snapshot before it reads Gerrit, so those views read
  as incomplete until Gerrit is added a minute or two later.
- "Added" in the last-days number: items created, or moved into one of the
  view's own fixVersions, in the window; a move into another view's version
  does not count here.
- Fixes: tickets in `fix_projects` (EX, EXR) linked by any relation except
  `non_fix_relations` (escalation bookkeeping); DDN links most fixes with "is
  related to". A fix's date is the release date of its EX fixVersion; EXR
  fixVersions are planning quarters ("2025Q4"), so an EXR fix gives no date.
  The Fix complete tile counts tickets whose every linked fix, EX or EXR,
  was read and is done, and complete tickets (see Patches);
  once a fix is under way, an open customer ticket no longer counts as "not
  started" in its date group. Where patches decide a ticket's delivery, they
  stand in for its linked fixes.
- Patches (`tag_families`): every ticket a view shows is looked up in
  Gerrit, on b_es6_0 and b_es7_0 only (master is not searched): changes
  whose commit message names the ticket or its mirror copy. Commit messages
  never name a customer ticket, so for one the EX and LU tickets linked as
  its fixes (see Fixes) count too; any other ticket counts only its own
  changes, since a linked ticket's changes are often other work (EX-1917,
  a To Do item, would otherwise read as delivered by EX-2010's merged scsi
  changes). Abandoned changes do not count. Where a branch's `views` name
  the view, its patches decide delivery in place of Jira: es6 in every
  customer group and EXA6, es7 in GCP (which runs both) and EXA7 Beta and
  GA. A ticket with patches on two such branches is complete once both are
  merged.
  - Complete: every such patch is merged, in a ddn tag or due in the next
    one. A complete ticket is almost done, never stalled: it leaves the
    S1/S2 idle limits, the idle tile, its date group's "not started or
    idle", the blocker lists (a customer ticket stays only when its
    committed date passed, chipped Complete) and the release risk tiles
    (Blocker priority, Critical, Past due date, which say how many they
    leave out). It needs no date. It counts as complete instead: in the Fix
    complete and Dated tiles, its date group ("2 complete") and the open
    scope of EXA6 and EXA7; the tables show a Complete pill on a green row.
  - Customer tickets: a complete one reads as fix delivered; one with a
    patch in review reads as under way and is dated by the release
    expected to carry it (`release`: ES6.3.10 for es6, EXA7 Beta for es7;
    of two, the later). A committed date still comes first. The Fix column
    shows the patches: "es6 ddn262", "es6 merged (next tag)", "es7 review,
    for EXA7 Beta".
  - Release items: a complete item counts as done in the projection and
    one with a patch in review weighs as In Review and counts as in review,
    whatever the Jira status; judged per item, a complete item is never at
    risk and one in review has started.
- Test cases (issue type Test) are not counted as release blockers or in
  EXA SD's open scope. A release view lists them in a table of their own,
  below the other items.
- Awaiting verification: a ticket whose status is in
  `verification_statuses` (Awaiting Verification) or that carries a label in
  `verification_labels` (awaiting_validation, which NVDCSE tickets use) is
  highlighted in the item tables, with that status or label as a pill; as
  an immediate blocker it is marked "Awaiting verification".
- Table filters: each count on the Where we are tiles limits the item
  table below to the tickets it counts. Customers: a source's (Open), S1,
  S2 and those past the idle limit, idle over 7 or 30 days or unknown, fix
  complete and due in the next tag, and those with no date (Dated); so do a
  date group's counts. Releases: open scope, complete, in review or
  verification, Blocker priority, blocked, Critical, S1, S2, past due and
  with a date. Future work: the initiatives' items, those of the ones at
  risk or off track, and the blockers. Closed and added items, which no
  table shows, open their Jira search instead. Pressing a count again, or
  the chip, lifts the filter; a newer snapshot keeps it with that count's
  tickets then, and drops it when it counts none.
- Release scope: a view that judges several releases (EXA6, EXA SD) names
  each on its card as a button that limits the view's blockers and item
  tables to that release's open items; the tiles and the verdict stay the
  view's. Pressing it again, or Show all, or the view's tab or sub-tab,
  shows the whole view again.
