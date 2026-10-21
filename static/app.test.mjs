// Tests for the page logic in app.js. Each test loads app.js unchanged into
// its own vm context, with a fake DOM holding only what the page uses and a
// fake dashboard server, then drives the page as a person would: pressing
// buttons and reading the table. Run with:
//
//     node --test static/

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { inspect } from "node:util";
import vm from "node:vm";

const APP_JS = readFileSync(new URL("app.js", import.meta.url), "utf8");
const INDEX_HTML = readFileSync(new URL("index.html", import.meta.url), "utf8");
/** The page's own names the tests use besides the DOM. */
const PAGE_NAMES = "ui, loadStatus, applyCommitment, fmtDate, stale, sevPri, idleSort, sameFilter, filterKeys, filterLabel";

/** The part of a DOM node the page uses. */
class FakeNode {
  parentNode = null;
  childNodes = [];

  append(...nodes) {
    for (const node of nodes) {
      const child = node instanceof FakeNode ? node : new FakeText(String(node));
      child.remove();
      child.parentNode = this;
      this.childNodes.push(child);
    }
  }

  replaceChildren(...nodes) {
    for (const child of this.childNodes) child.parentNode = null;
    this.childNodes = [];
    this.append(...nodes);
  }

  remove() {
    const siblings = this.parentNode?.childNodes;
    if (siblings) siblings.splice(siblings.indexOf(this), 1);
    this.parentNode = null;
  }

  get textContent() {
    return this.childNodes.map((child) => child.textContent).join("");
  }

  set textContent(text) {
    this.replaceChildren(String(text));
  }

  get isConnected() {
    let node = this;
    while (node.parentNode) node = node.parentNode;
    return node instanceof FakeDocument;
  }

  /** How a failed assertion shows a node: briefly, not with the whole document it links to,
   * which takes node:assert minutes to print. */
  [inspect.custom]() {
    return `${this.constructor.name} ${JSON.stringify(this.textContent.slice(0, 80))}`;
  }

  /** Descendant elements matching `selector`: compound selectors joined by spaces. */
  querySelectorAll(selector) {
    const chain = selector.split(" ").map(compound);
    const found = [];
    const walk = (node) => {
      for (const child of node.childNodes) {
        if (!(child instanceof FakeElement)) continue;
        if (matches(child, chain)) found.push(child);
        walk(child);
      }
    };
    walk(this);
    return found;
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }
}

class FakeText extends FakeNode {
  constructor(data) {
    super();
    this.data = data;
  }

  get textContent() {
    return this.data;
  }

  set textContent(text) {
    this.data = String(text);
  }
}

class FakeElement extends FakeNode {
  #typed = null;

  constructor(document, tagName) {
    super();
    this.ownerDocument = document;
    this.tagName = tagName.toUpperCase();
    this.attributes = new Map();
    this.listeners = new Map();
  }

  get className() {
    return this.getAttribute("class") ?? "";
  }

  set className(name) {
    this.setAttribute("class", name);
  }

  /** What a form field holds: what was typed into it, else its value attribute. */
  get value() {
    return this.#typed ?? this.getAttribute("value") ?? "";
  }

  set value(text) {
    this.#typed = String(text);
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null;
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  addEventListener(type, listener) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  /** Runs the listeners of event `type`, as the browser does when a person acts. */
  dispatch(type) {
    for (const listener of this.listeners.get(type) ?? []) listener({ type, target: this });
  }

  click() {
    this.dispatch("click");
  }

  focus() {
    this.ownerDocument.activeElement = this;
  }

  scrollIntoView() {}

  [inspect.custom]() {
    const attributes = [...this.attributes].map(([name, value]) => ` ${name}="${value}"`).join("");
    return `<${this.tagName.toLowerCase()}${attributes}> ${JSON.stringify(this.textContent.slice(0, 80))}`;
  }
}

/** The page's document: the elements of index.html that have an id, which is how app.js finds them. */
class FakeDocument extends FakeNode {
  activeElement = null;

  constructor() {
    super();
    for (const [, tag, id] of INDEX_HTML.matchAll(/<(\w+) id="([\w-]+)"/g)) {
      const element = this.createElement(tag);
      element.setAttribute("id", id);
      this.append(element);
    }
  }

  createElement(tagName) {
    return new FakeElement(this, tagName);
  }

  createTextNode(data) {
    return new FakeText(data);
  }

  getElementById(id) {
    return this.querySelector(`#${id}`);
  }
}

/** A compound selector (a tag, `#id`, `.class`, `[attr]`, `[attr="value"]`) as a test of
 * one element. Anything else throws, so a test never passes on a selector it misread. */
function compound(text) {
  const parts = text.match(/^[a-z][a-z0-9]*|#[\w-]+|\.[\w-]+|\[[\w-]+(?:="[^"]*")?\]/g) ?? [];
  if (parts.join("") !== text) throw new Error(`the fake DOM does not handle the selector "${text}"`);
  return (element) => parts.every((part) => {
    if (part.startsWith("#")) return element.getAttribute("id") === part.slice(1);
    if (part.startsWith(".")) return element.className.split(/\s+/).includes(part.slice(1));
    if (part.startsWith("[")) {
      const [, name, value] = part.match(/^\[([\w-]+)(?:="(.*)")?\]$/);
      return value === undefined ? element.getAttribute(name) !== null : element.getAttribute(name) === value;
    }
    return element.tagName === part.toUpperCase();
  });
}

/** The element matches the last compound, and ancestors of it the others, in order. */
function matches(element, chain) {
  let i = chain.length - 1;
  if (!chain[i](element)) return false;
  for (let up = element.parentNode; up && i > 0; up = up.parentNode) {
    if (up instanceof FakeElement && chain[i - 1](up)) i -= 1;
  }
  return i === 0;
}

/** The dashboard server as the page sees it. A test changes `snapshot` and
 * `status` to make a newer snapshot arrive (see `publish`). */
class FakeServer {
  constructor(snapshot) {
    this.snapshot = snapshot;
    this.status = {
      refreshing: false,
      queued: false,
      finished: null,
      failed: false,
      snapshot_at: snapshot.generated_at,
      next_refresh: null,
      interval_minutes: 30,
      refresh: null,
    };
    this.settings = { refresh_minutes: 30, choices: [0, 15, 30, 60], max_note_chars: 500 };
    /** The changes the page sent, oldest first. */
    this.posted = [];
  }

  async fetch(url, init = {}) {
    if (init.method !== "POST") {
      return reply({ "/api/status": this.status, "/api/snapshot": this.snapshot, "/api/settings": this.settings }[url]);
    }
    const body = JSON.parse(init.body);
    this.posted.push({ url, body });
    if (url !== "/api/commitments") return reply(undefined);
    // The entry as the server stores it: the note trimmed, none when empty.
    const committed = body.date ? { date: body.date, note: body.note?.trim() || null, key: body.key } : null;
    return reply({ saved: true, committed, refresh: "started" });
  }
}

/** A response with `data` as its JSON body, or a 404 for a path the fake server lacks. */
function reply(data) {
  if (data === undefined) return { ok: false, status: 404, json: async () => ({ error: "no such path" }) };
  const body = JSON.stringify(data);
  return { ok: true, status: 200, json: async () => JSON.parse(body) };
}

/** A ticket with the fields the page reads, as the server writes them. */
function ticket(key, fields) {
  return {
    key,
    url: `https://jira.example/browse/${key}`,
    summary: `${key} summary`,
    status: "In Progress",
    priority: "Major",
    severity: null,
    assignee: null,
    issue_type: "Bug",
    due: null,
    fix_versions: [],
    last_comment: null,
    idle_days: 1,
    also: [],
    source: null,
    fix: null,
    fix_state: "no_fix",
    committed: null,
    patches: [],
    ...fields,
  };
}

/** A date promised on `key`. */
const promise = (key, date, note = null) => ({ date, note, key });

/** A view holding nothing but `fields`. */
function view(id, kind, fields) {
  return {
    id,
    label: id,
    kind,
    verdict: "on_track",
    summary: "",
    kpis: [],
    tracks: [],
    date_groups: [],
    undated: 0,
    blockers: [],
    items: [],
    notes: [],
    pm: null,
    health: null,
    initiatives: [],
    editable_commitments: false,
    ...fields,
  };
}

/** NVIDIA's tickets and the EXA7 GA release on 21 Oct 2026; a new copy on every call. */
function sampleSnapshot() {
  const nvidia = view("customers-nvidia", "customers", {
    label: "NVIDIA",
    verdict: "at_risk",
    editable_commitments: true,
    undated: 1,
    items: [
      // A mirrored ticket: NP-131 is NCP-163's copy on Jira cloud, and the promise is filed there.
      ticket("NCP-163", {
        also: ["NP-131"],
        idle_days: 3,
        severity: { level: 1, from_priority: "Critical", label: "S1 (Critical)" },
        committed: promise("NP-131", "2026-11-01", "per QBR"),
      }),
      ticket("NVDCSE-250", {
        idle_days: 40,
        fix: { key: "EX-15181", url: "https://jira.example/browse/EX-15181", version: "6.3.10", date: "2026-11-01", status: "In Progress" },
        fix_state: "under_way",
      }),
      ticket("NVDCSE-248", { status: "Open", idle_days: 2, fix_state: "not_started", committed: promise("NVDCSE-248", "2026-11-01") }),
      ticket("NVDCSE-216", { idle_days: 10, committed: promise("NVDCSE-216", "2026-12-15") }),
      ticket("NCP-9", { idle_days: 5, committed: promise("NCP-9", "2026-10-15") }),
      // Its comments could not be read.
      ticket("DDN-7162", { idle_days: null }),
    ],
    date_groups: [
      {
        date: "2026-10-15",
        days_left: -6,
        keys: ["NCP-9"],
        by_status: [{ status: "In Progress", count: 1 }],
        verdict: "off_track",
        reason: "Date passed",
        stalled: [],
      },
      {
        date: "2026-11-01",
        days_left: 11,
        keys: ["NCP-163", "NVDCSE-250", "NVDCSE-248"],
        by_status: [{ status: "In Progress", count: 2 }, { status: "Open", count: 1 }],
        verdict: "at_risk",
        reason: "2 not started or idle",
        stalled: ["NVDCSE-250", "NVDCSE-248"],
      },
      {
        date: "2026-12-15",
        days_left: 55,
        keys: ["NVDCSE-216"],
        by_status: [{ status: "In Progress", count: 1 }],
        verdict: "on_track",
        reason: null,
        stalled: [],
      },
    ],
  });
  const exa7 = view("dev-exa7-ga", "release", {
    label: "EXA7 GA",
    items: [
      ticket("EX-15181", { fix_versions: ["7.0.0"], idle_days: 4 }),
      // No rule needed its idle time, so it was not read.
      ticket("EX-16104", { fix_versions: ["7.0.0"], idle_days: null }),
      ticket("EX-15711", { fix_versions: ["7.0.0"], idle_days: 0 }),
    ],
  });
  return {
    generated_at: new Date().toISOString(),
    sources: [{ name: "Jira", error: null }],
    tabs: [
      { id: "customers", label: "Customers", verdict: "at_risk", views: [nvidia] },
      { id: "development", label: "Development", verdict: "on_track", views: [exa7] },
    ],
  };
}

const nvidiaOf = (snapshot) => snapshot.tabs[0].views[0];
const exa7Of = (snapshot) => snapshot.tabs[1].views[0];

/** Waits for what the page set off: the fake server answers at once, so one
 * turn of the event loop runs every pending request to its end. */
const settled = () => new Promise((resolve) => setImmediate(resolve));

/** Loads app.js as index.html does, in a fresh context, and waits for its first render. */
async function openPage(server = new FakeServer(sampleSnapshot())) {
  const document = new FakeDocument();
  const context = vm.createContext({
    document,
    Node: FakeNode,
    fetch: (url, init) => server.fetch(url, init),
    // The page polls the server on a timer; the tests poll by hand (`publish`).
    setTimeout: () => 0,
    clearTimeout: () => {},
  });
  vm.runInContext(APP_JS, context, { filename: "static/app.js" });
  await settled();
  return { ...vm.runInContext(`({ ${PAGE_NAMES} })`, context), document, server };
}

/** Makes `snapshot` the server's newer one and has the page poll for it. */
async function publish(page, snapshot) {
  snapshot.generated_at = new Date(Date.parse(page.server.status.snapshot_at) + 60_000).toISOString();
  page.server.snapshot = snapshot;
  page.server.status.snapshot_at = snapshot.generated_at;
  await page.loadStatus();
}

/** The keys of the rows in the "All items" table, top to bottom. */
const rows = (page) => page.document.querySelectorAll("#items tbody tr").map((row) => row.querySelector("a").textContent);

/** An element's name as a screen reader reads it: its aria-label, else its text. */
const nameOf = (element) => element.getAttribute("aria-label") ?? element.textContent;

function button(page, name) {
  const found = page.document.querySelectorAll("button").filter((b) => nameOf(b) === name);
  assert.equal(found.length, 1, `one button is named "${name}"`);
  return found[0];
}

/** The name of the focused element; null when it left the page, where the browser
 * would have moved the focus to the body. */
function focused(page) {
  const element = page.document.activeElement;
  return element?.isConnected ? nameOf(element) : null;
}

/** How many date editors the table shows. */
const editors = (page) => page.document.querySelectorAll('#items input[type="date"]').length;

async function press(page, name) {
  button(page, name).click();
  await settled();
}

const heading = (page, label) => page.document.querySelectorAll("#items th").find((th) => th.textContent === label);

/** The cell of ticket `key` in the column headed `label`. */
function cell(page, key, label) {
  const column = page.document.querySelectorAll("#items th").indexOf(heading(page, label));
  const row = page.document.querySelectorAll("#items tbody tr").find((tr) => tr.querySelector("a").textContent === key);
  return row.childNodes[column];
}

const flash = (page) => page.document.getElementById("flash").textContent;
const meta = (page) => page.document.getElementById("meta").textContent;

/** The date filter the chip above the table names; null when there is none. */
function filterChip(page) {
  const chip = page.document.querySelector("#items .filter-chip");
  return chip && chip.childNodes[0].textContent;
}

const NOV_1 = "3 tickets, due 1 Nov 2026";
const NOV_1_AT_RISK = "2 not started or idle, due 1 Nov 2026";
const ALL_ROWS = ["NCP-163", "NVDCSE-250", "NVDCSE-248", "NVDCSE-216", "NCP-9", "DDN-7162"];

test("fmtDate shows day, month and year, and none for no date", async () => {
  const { fmtDate } = await openPage();

  assert.equal(fmtDate("2026-11-01"), "1 Nov 2026");
  assert.equal(fmtDate("2026-01-09T23:30:00+03:00"), "9 Jan 2026", "the date as written, not converted");
  assert.equal(fmtDate(null), "none");
  assert.equal(fmtDate(""), "none");
});

for (const [name, minutes, ageMinutes, expected] of [
  ["within two intervals", 30, 59, false],
  ["older than two intervals", 30, 61, true],
  ["by hand, within a day", 0, 23 * 60, false],
  ["by hand, older than a day", 0, 25 * 60, true],
]) {
  test(`stale: a snapshot ${name}`, async () => {
    const { stale } = await openPage();
    const generatedAt = new Date(Date.now() - ageMinutes * 60_000).toISOString();

    assert.equal(stale(generatedAt, minutes), expected);
  });
}

test("sevPri shows the severity, else the priority", async () => {
  const { sevPri } = await openPage();

  assert.equal(sevPri({ severity: { level: 1, label: "S1 (Critical)" }, priority: "Critical" }), "S1 (Critical)");
  assert.equal(sevPri({ severity: null, priority: "Major" }), "Major");
  assert.equal(sevPri({ severity: null, priority: null }), "");
});

test("sameFilter needs the same date and the same subset", async () => {
  const { sameFilter } = await openPage();
  const due = { date: "2026-11-01", atRisk: false };

  assert.equal(sameFilter(due, { date: "2026-11-01", atRisk: false }), true);
  assert.equal(sameFilter(due, { date: "2026-11-01", atRisk: true }), false);
  assert.equal(sameFilter(due, { date: "2026-12-15", atRisk: false }), false);
  assert.equal(sameFilter(null, due), false);
  assert.equal(sameFilter(due, null), false);
});

test("filterKeys and filterLabel read the group as the snapshot has it", async () => {
  const { filterKeys, filterLabel } = await openPage();
  const nvidia = nvidiaOf(sampleSnapshot());
  const due = { date: "2026-11-01", atRisk: false };
  const atRisk = { date: "2026-11-01", atRisk: true };
  const gone = { date: "2026-11-08", atRisk: true };

  assert.deepEqual([...filterKeys(nvidia, due)], ["NCP-163", "NVDCSE-250", "NVDCSE-248"]);
  assert.deepEqual([...filterKeys(nvidia, atRisk)], ["NVDCSE-250", "NVDCSE-248"]);
  assert.deepEqual([...filterKeys(nvidia, gone)], []);
  assert.equal(filterLabel(nvidia, due), "Due 1 Nov 2026");
  assert.equal(filterLabel(nvidia, atRisk), "Due 1 Nov 2026, 2 not started or idle");
  assert.equal(filterLabel(nvidia, gone), "Due 8 Nov 2026");
});

test("idleSort ranks unknown idle time as the most idle in customer views, and below zero days elsewhere", async () => {
  const { idleSort } = await openPage();
  const unknown = { idle_days: null };

  assert.equal(idleSort(true)(unknown), Infinity);
  assert.equal(idleSort(false)(unknown), -1);
  assert.equal(idleSort(true)({ idle_days: 0 }), 0);
});

test("a group's ticket count limits the table to its tickets, and pressing it again lifts the limit", async () => {
  const page = await openPage();
  assert.deepEqual(rows(page), ALL_ROWS);

  await press(page, NOV_1);

  assert.deepEqual(rows(page), ["NCP-163", "NVDCSE-250", "NVDCSE-248"]);
  assert.equal(button(page, NOV_1).getAttribute("aria-pressed"), "true");
  assert.equal(filterChip(page), "Due 1 Nov 2026");
  assert.equal(flash(page), "Due 1 Nov 2026: 3 of 6 tickets shown.");
  assert.equal(focused(page), "Clear date filter");

  await press(page, NOV_1);

  assert.deepEqual(rows(page), ALL_ROWS);
  assert.equal(button(page, NOV_1).getAttribute("aria-pressed"), "false");
  assert.equal(filterChip(page), null);
  assert.equal(flash(page), "Showing every date.");
  assert.equal(focused(page), NOV_1);
});

test("the at-risk count limits the table to the tickets putting the group at risk", async () => {
  const page = await openPage();

  await press(page, NOV_1_AT_RISK);

  assert.deepEqual(rows(page), ["NVDCSE-250", "NVDCSE-248"]);
  assert.equal(filterChip(page), "Due 1 Nov 2026, 2 not started or idle");

  await press(page, NOV_1);

  assert.deepEqual(rows(page), ["NCP-163", "NVDCSE-250", "NVDCSE-248"], "the group's other toggle replaces the filter");

  await press(page, "Clear date filter");

  assert.deepEqual(rows(page), ALL_ROWS);
  assert.equal(filterChip(page), null);
  assert.equal(focused(page), "Filter items");
});

test("a group with no ticket putting it at risk offers only its ticket count", async () => {
  const page = await openPage();

  const passed = page.document.querySelectorAll("#view .group").find((g) => g.textContent.startsWith("15 Oct 2026"));

  assert.deepEqual(passed.querySelectorAll("button").map((b) => b.textContent), ["1 ticket"]);
  assert.match(passed.textContent, /\. Date passed/);
});

for (const [name, toggle, change] of [
  ["its tickets were re-dated", NOV_1, (group) => { group.date = "2026-11-08"; }],
  ["the group is no longer at risk", NOV_1_AT_RISK, (group) => Object.assign(group, { verdict: "on_track", reason: null, stalled: [] })],
]) {
  test(`a date filter is dropped when a newer snapshot says ${name}`, async () => {
    const page = await openPage();
    await press(page, toggle);
    const newer = sampleSnapshot();
    change(nvidiaOf(newer).date_groups[1]);

    await publish(page, newer);

    assert.deepEqual(rows(page), ALL_ROWS);
    assert.equal(filterChip(page), null);
    assert.equal(flash(page), "No ticket matches the 1 Nov 2026 date filter any more; showing every date.");
  });
}

test("a date filter stays while a newer snapshot still has tickets for it", async () => {
  const page = await openPage();
  await press(page, NOV_1_AT_RISK);
  const newer = sampleSnapshot();
  Object.assign(nvidiaOf(newer).date_groups[1], { reason: "1 not started or idle", stalled: ["NVDCSE-250"] });

  await publish(page, newer);

  assert.deepEqual(rows(page), ["NVDCSE-250"]);
  assert.equal(filterChip(page), "Due 1 Nov 2026, 1 not started or idle");
});

for (const [name, filterFirst, control, shown] of [
  ["a date toggle", false, NOV_1, ["NCP-163", "NVDCSE-250", "NVDCSE-248", "NVDCSE-255"]],
  ["the chip's clear button", true, "Clear date filter", [...ALL_ROWS, "NVDCSE-255"]],
]) {
  test(`pressing ${name} closes the date editor and shows the snapshot that waited for it`, async () => {
    const page = await openPage();
    if (filterFirst) await press(page, NOV_1);
    cell(page, "NCP-163", "Date").querySelector("button").click();
    const newer = sampleSnapshot();
    nvidiaOf(newer).items.push(ticket("NVDCSE-255", { committed: promise("NVDCSE-255", "2026-11-01") }));
    nvidiaOf(newer).date_groups[1].keys.push("NVDCSE-255");
    await publish(page, newer);
    assert.equal(editors(page), 1, "the editor stays open while a snapshot arrives");
    assert.match(meta(page), /Newer data waits until the date editor closes/);

    await press(page, control);

    assert.equal(page.ui.editing, null);
    assert.equal(editors(page), 0);
    assert.deepEqual(rows(page), shown);
    assert.doesNotMatch(meta(page), /Newer data waits/);
  });
}

test("unknown idle time shows as unknown and sorts as the most idle in customer views", async () => {
  const page = await openPage();
  assert.equal(cell(page, "DDN-7162", "Idle").textContent, "unknown");

  heading(page, "Idle").click();

  assert.deepEqual(rows(page), ["NVDCSE-248", "NCP-163", "NCP-9", "NVDCSE-216", "NVDCSE-250", "DDN-7162"]);
  assert.equal(heading(page, "Idle").getAttribute("aria-sort"), "ascending");

  heading(page, "Idle").click();

  assert.deepEqual(rows(page), ["DDN-7162", "NVDCSE-250", "NVDCSE-216", "NCP-9", "NCP-163", "NVDCSE-248"]);
  assert.equal(heading(page, "Idle").getAttribute("aria-sort"), "descending");
});

test("unknown idle time is blank in release views and sorts below zero days", async () => {
  const page = await openPage();
  await press(page, "Development");
  assert.equal(cell(page, "EX-16104", "Idle").textContent, "");

  heading(page, "Idle").click();

  assert.deepEqual(rows(page), ["EX-16104", "EX-15711", "EX-15181"]);
});

test("text columns sort by the numbers in them", async () => {
  const page = await openPage();

  heading(page, "Key").click();

  assert.deepEqual(rows(page), ["DDN-7162", "NCP-9", "NCP-163", "NVDCSE-216", "NVDCSE-248", "NVDCSE-250"]);
});

test("applyCommitment updates the customer rows whose promise is filed under the key", async () => {
  const page = await openPage();
  const item = (key) => nvidiaOf(page.ui.snap).items.find((t) => t.key === key);

  page.applyCommitment("NP-131", promise("NP-131", "2026-11-20"));
  assert.deepEqual({ ...item("NCP-163").committed }, promise("NP-131", "2026-11-20"), "the NCP row of a promise filed on its NP copy");
  assert.equal(item("NVDCSE-248").committed.date, "2026-11-01", "other rows keep theirs");

  page.applyCommitment("DDN-7162", promise("DDN-7162", "2026-12-01"));
  assert.equal(item("DDN-7162").committed.date, "2026-12-01", "a row without a promise, by its own key");

  page.applyCommitment("NP-131", null);
  assert.equal(item("NCP-163").committed, null, "a cleared promise");

  page.applyCommitment("EX-16104", promise("EX-16104", "2026-12-01"));
  assert.equal(exa7Of(page.ui.snap).items.find((t) => t.key === "EX-16104").committed, null, "release items take no promise");

  page.ui.snap = null;
  assert.doesNotThrow(() => page.applyCommitment("NP-131", promise("NP-131", "2026-11-20")), "before the first snapshot");
});

test("a date saved in the editor is filed under the promise's key and shown at once", async () => {
  const page = await openPage();
  cell(page, "NCP-163", "Date").querySelector("button").click();
  const date = cell(page, "NCP-163", "Date").querySelector('input[type="date"]');
  assert.equal(date.value, "2026-11-01");
  date.value = "2026-11-20";
  date.dispatch("input");

  await press(page, "Save");

  assert.deepEqual(page.server.posted, [{ url: "/api/commitments", body: { key: "NP-131", date: "2026-11-20", note: "per QBR" } }]);
  assert.equal(cell(page, "NCP-163", "Date").textContent, "20 Nov 2026 committed on NP-131 Edit");
  assert.equal(flash(page), "Saved NP-131: 20 Nov 2026. Refreshing dates and verdicts.");
});
