// Tests for the page logic in app.js. Each test loads the page's scripts
// unchanged, in index.html's order, into its own vm context, with a fake DOM
// holding only what the page uses and a fake dashboard server, then drives
// the page as a person would: pressing buttons and reading the table and the
// files it saves. Run with:
//
//     node --test static/

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { inspect } from "node:util";
import vm from "node:vm";

const INDEX_HTML = readFileSync(new URL("index.html", import.meta.url), "utf8");
/** The scripts index.html loads, in its order, as [file, source]. */
const SCRIPTS = [...INDEX_HTML.matchAll(/<script src="\/([\w.]+)"><\/script>/g)]
  .map(([, file]) => [file, readFileSync(new URL(file, import.meta.url), "utf8")]);
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

  get children() {
    return this.childNodes.filter((child) => child instanceof FakeElement);
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

  /** As in a browser, a link with `download` saves its target instead of opening it. */
  click() {
    if (this.tagName === "A" && this.getAttribute("download") !== null) {
      this.ownerDocument.downloads.push({ name: this.getAttribute("download"), url: this.getAttribute("href") });
    }
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

/** The page's document: a body with the elements of index.html that have an
 * id, which is how app.js finds them. */
class FakeDocument extends FakeNode {
  activeElement = null;
  /** The files links saved, oldest first: { name, url }. */
  downloads = [];

  constructor() {
    super();
    this.body = this.createElement("body");
    this.append(this.body);
    for (const [, tag, id] of INDEX_HTML.matchAll(/<(\w+) id="([\w-]+)"/g)) {
      const element = this.createElement(tag);
      element.setAttribute("id", id);
      this.body.append(element);
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
    labels: [],
    verification: null,
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
    tests: [],
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
    kpis: [
      {
        label: "Open",
        value: "6",
        note: "DDN 1, Whamcloud 5",
        tone: null,
        by_source: [{ source: "DDN", count: 1 }, { source: "Whamcloud", count: 5 }],
      },
      { label: "S1 / S2", value: "1 / 0", note: "0 past the idle limit", tone: null, by_source: [] },
    ],
    items: [
      // A mirrored ticket: NP-131 is NCP-163's copy on Jira cloud, and the promise is filed there.
      ticket("NCP-163", {
        source: "Whamcloud",
        also: ["NP-131"],
        idle_days: 3,
        severity: { level: 1, from_priority: "Critical", label: "S1 (Critical)" },
        committed: promise("NP-131", "2026-11-01", "per QBR"),
      }),
      ticket("NVDCSE-250", {
        source: "Whamcloud",
        idle_days: 40,
        fix: { key: "EX-15181", url: "https://jira.example/browse/EX-15181", version: "6.3.10", date: "2026-11-01", status: "In Progress" },
        fix_state: "under_way",
      }),
      ticket("NVDCSE-248", { source: "Whamcloud", status: "Open", idle_days: 2, fix_state: "not_started", committed: promise("NVDCSE-248", "2026-11-01") }),
      ticket("NVDCSE-216", { source: "Whamcloud", idle_days: 10, committed: promise("NVDCSE-216", "2026-12-15") }),
      ticket("NCP-9", { source: "Whamcloud", idle_days: 5, committed: promise("NCP-9", "2026-10-15") }),
      // Its comments could not be read.
      ticket("DDN-7162", { source: "DDN", idle_days: null }),
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

/** Loads the page's scripts as index.html does, in a fresh context, and waits for its first render. */
async function openPage(server = new FakeServer(sampleSnapshot())) {
  const document = new FakeDocument();
  const blobs = new Map();
  const context = vm.createContext({
    document,
    Node: FakeNode,
    fetch: (url, init) => server.fetch(url, init),
    // The page polls the server on a timer; the tests poll by hand (`publish`).
    setTimeout: () => 0,
    clearTimeout: () => {},
    Blob,
    TextEncoder,
    URL: {
      createObjectURL: (blob) => {
        const url = `blob:${blobs.size}`;
        blobs.set(url, blob);
        return url;
      },
      revokeObjectURL: () => {},
    },
  });
  for (const [file, source] of SCRIPTS) vm.runInContext(source, context, { filename: `static/${file}` });
  await settled();
  /** The files the page saved, oldest first: { name, blob }. */
  const saved = () => document.downloads.map(({ name, url }) => ({ name, blob: blobs.get(url) }));
  return { ...vm.runInContext(`({ ${PAGE_NAMES} })`, context), document, server, saved };
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

/** The note under the date groups that says where the dates come from. */
const dateNote = (page) => page.document.querySelectorAll("#view p.kpi-note").map((p) => p.textContent).find((t) => t.includes("have no date"));

/** `snapshot` with the Customers tab as the server builds it: All, then each
 * group. GCP holds `gcp`, its tickets; NVIDIA is the sample's. */
function withAllView(snapshot, gcp) {
  const nvidia = nvidiaOf(snapshot);
  const others = view("customers-others", "customers", { label: "All others", undated: 2, items: [ticket("DDN-3571"), ticket("DDN-6000")] });
  const gcpView = view("customers-gcp", "customers", { label: "GCP", editable_commitments: true, undated: gcp.length, items: gcp });
  const all = {
    ...structuredClone(nvidia),
    id: "customers-all",
    label: "All",
    editable_commitments: false,
    undated: nvidia.undated + others.undated + gcpView.undated,
    items: structuredClone([...nvidia.items, ...others.items, ...gcp]),
  };
  snapshot.tabs[0].views = [all, others, gcpView, nvidia];
  return snapshot;
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

for (const [name, gcp, views] of [
  ["GCP and NVIDIA", [ticket("GCP-315")], "the GCP and NVIDIA views"],
  ["NVIDIA alone", [], "the NVIDIA view"],
]) {
  test(`All, with ${name} tickets, has no date editor and says where its committed dates are set`, async () => {
    const page = await openPage(new FakeServer(withAllView(sampleSnapshot(), gcp)));

    assert.equal(
      dateNote(page),
      `${3 + gcp.length} tickets have no date. Committed dates come from the local commitments file; set them in ${views}. Other dates are the release date of the linked EX fix; EXR fixes give no date.`,
    );
    assert.equal(cell(page, "NCP-163", "Date").querySelectorAll("button").length, 0);
  });
}

test("the group views keep their date notes and editors beside All", async () => {
  const page = await openPage(new FakeServer(withAllView(sampleSnapshot(), [ticket("GCP-315")])));

  await press(page, "NVIDIA");

  assert.match(dateNote(page), / have no date\. Committed dates come from the local commitments file; set them in the Date column\. Other dates/);
  assert.equal(cell(page, "NCP-163", "Date").querySelectorAll("button").length, 1);

  await press(page, "All others");

  assert.equal(dateNote(page), "2 tickets have no date. Dates are the release date of the linked EX fix; EXR fixes give no date.");
});

test("a date saved in a group view shows at once in All", async () => {
  const page = await openPage(new FakeServer(withAllView(sampleSnapshot(), [])));
  await press(page, "NVIDIA");
  cell(page, "NCP-163", "Date").querySelector("button").click();
  const date = cell(page, "NCP-163", "Date").querySelector('input[type="date"]');
  date.value = "2026-11-20";
  date.dispatch("input");
  await press(page, "Save");

  await press(page, "All");

  assert.equal(cell(page, "NCP-163", "Date").textContent.trim(), "20 Nov 2026 committed on NP-131");
});

const DDN = "DDN 1 ticket";
const WHAMCLOUD = "Whamcloud 5 tickets";

/** The tile labelled `label`. */
const tile = (page, label) => page.document.querySelectorAll("#view .kpi").find((k) => k.querySelector(".kpi-label").textContent === label);

test("the Open tile reads as before, with each source's count a button; other tiles keep their note as text", async () => {
  const page = await openPage();

  assert.equal(tile(page, "Open").querySelector(".kpi-note").textContent, "DDN 1, Whamcloud 5");
  assert.deepEqual(tile(page, "Open").querySelectorAll("button").map((b) => b.textContent), ["DDN 1", "Whamcloud 5"]);
  assert.equal(tile(page, "S1 / S2").querySelector(".kpi-note").textContent, "0 past the idle limit");
  assert.equal(tile(page, "S1 / S2").querySelectorAll("button").length, 0);
});

test("a source's count limits the table to its tickets, and pressing it again lifts the limit", async () => {
  const page = await openPage();

  await press(page, WHAMCLOUD);

  assert.deepEqual(rows(page), ["NCP-163", "NVDCSE-250", "NVDCSE-248", "NVDCSE-216", "NCP-9"]);
  assert.equal(button(page, WHAMCLOUD).getAttribute("aria-pressed"), "true");
  assert.equal(filterChip(page), "From Whamcloud");
  assert.equal(flash(page), "From Whamcloud: 5 of 6 tickets shown.");
  assert.equal(focused(page), "Clear source filter");

  await press(page, WHAMCLOUD);

  assert.deepEqual(rows(page), ALL_ROWS);
  assert.equal(button(page, WHAMCLOUD).getAttribute("aria-pressed"), "false");
  assert.equal(filterChip(page), null);
  assert.equal(flash(page), "Showing every source.");
  assert.equal(focused(page), WHAMCLOUD);
});

test("source and date filters replace each other", async () => {
  const page = await openPage();

  await press(page, NOV_1);
  await press(page, DDN);

  assert.deepEqual(rows(page), ["DDN-7162"]);
  assert.equal(button(page, NOV_1).getAttribute("aria-pressed"), "false");

  await press(page, WHAMCLOUD);

  assert.deepEqual(rows(page), ["NCP-163", "NVDCSE-250", "NVDCSE-248", "NVDCSE-216", "NCP-9"], "another source replaces it too");

  await press(page, NOV_1);

  assert.deepEqual(rows(page), ["NCP-163", "NVDCSE-250", "NVDCSE-248"]);
  assert.equal(button(page, WHAMCLOUD).getAttribute("aria-pressed"), "false");
});

test("the chip lifts a source filter", async () => {
  const page = await openPage();
  await press(page, DDN);

  await press(page, "Clear source filter");

  assert.deepEqual(rows(page), ALL_ROWS);
  assert.equal(filterChip(page), null);
  assert.equal(flash(page), "Showing every source.");
  assert.equal(focused(page), "Filter items");
});

test("a source filter is dropped when a newer snapshot has no ticket from that source", async () => {
  const page = await openPage();
  await press(page, DDN);
  const newer = sampleSnapshot();
  nvidiaOf(newer).items = nvidiaOf(newer).items.filter((t) => t.source !== "DDN");

  await publish(page, newer);

  assert.deepEqual(rows(page), ALL_ROWS.filter((key) => key !== "DDN-7162"));
  assert.equal(filterChip(page), null);
  assert.equal(flash(page), "No ticket comes from DDN any more; showing every source.");
});

/** The section titles of the view shown. */
const sections = (page) => page.document.querySelectorAll("#view section h2").map((h) => h.textContent);

/** The keys of the rows in the section titled `title`, top to bottom. */
function sectionRows(page, title) {
  const found = page.document.querySelectorAll("#view section").find((s) => s.querySelector("h2").textContent === title);
  return found.querySelectorAll("tbody tr").map((row) => row.querySelector("a").textContent);
}

test("a release view lists its Test items in a table of their own, below the other items", async () => {
  const snap = sampleSnapshot();
  exa7Of(snap).tests = ["EX-17001", "EX-17000"].map((key) => ticket(key, { issue_type: "Test", fix_versions: ["7.0.0"] }));
  const page = await openPage(new FakeServer(snap));
  assert.ok(!sections(page).some((title) => title.includes("Test item")), "a view without Test items has no such table");

  await press(page, "Development");

  assert.deepEqual(sections(page).slice(-2), ["All 3 items", "2 Test items"]);
  assert.deepEqual(rows(page), ["EX-15181", "EX-16104", "EX-15711"]);
  assert.deepEqual(sectionRows(page, "2 Test items"), ["EX-17001", "EX-17000"]);

  const search = page.document.querySelectorAll('input[type="search"]').find((i) => i.getAttribute("aria-label") === "Filter Test items");
  search.value = "17000";
  search.dispatch("input");

  assert.deepEqual(sectionRows(page, "2 Test items"), ["EX-17000"]);
  assert.deepEqual(rows(page), ["EX-15181", "EX-16104", "EX-15711"], "each table searches on its own");

  await press(page, "Export keys (md): EXA7 GA Test items");

  const [md] = page.saved();
  assert.match(md.name, /^exa7-ga-test-items-\d{8}\.md$/);
  assert.equal(await md.blob.text(), "- EX-17000\n", "each table exports its own rows");
});

/** A blocker as the server writes it. */
const blockerOf = (key, reason, verification = null) => ({
  key,
  url: `https://jira.example/browse/${key}`,
  summary: `${key} summary`,
  owner: null,
  reason,
  tone: "danger",
  verification,
});

/** The row of ticket `key` in the "All items" table. */
const rowOf = (page, key) => page.document.querySelectorAll("#items tbody tr").find((tr) => tr.querySelector("a").textContent === key);

/** Each blocker's key with the text of its chips. */
const blockerChips = (page) => Object.fromEntries(page.document.querySelectorAll("#view .brow")
  .map((row) => [row.querySelector("a").textContent, row.querySelectorAll(".reasons .chip").map((c) => c.textContent)]));

test("a ticket whose fix awaits verification is highlighted with what says so, in the table and among the blockers", async () => {
  const snap = sampleSnapshot();
  const nvidia = nvidiaOf(snap);
  Object.assign(nvidia.items.find((t) => t.key === "NVDCSE-250"), { labels: ["awaiting_validation"], verification: "awaiting_validation" });
  nvidia.blockers = [blockerOf("NVDCSE-250", "S2 (Major), idle 40d", "awaiting_validation"), blockerOf("DDN-7162", "S1, idle unknown")];
  Object.assign(exa7Of(snap).items.find((t) => t.key === "EX-15181"), { status: "Awaiting Verification", verification: "Awaiting Verification" });
  const page = await openPage(new FakeServer(snap));

  assert.equal(rowOf(page, "NVDCSE-250").getAttribute("class"), "awaiting");
  assert.equal(rowOf(page, "NCP-163").getAttribute("class"), null);
  assert.equal(cell(page, "NVDCSE-250", "Status").textContent, "In Progress awaiting_validation", "a label shows beside the status");
  assert.deepEqual(blockerChips(page), {
    "NVDCSE-250": ["S2 (Major), idle 40d", "Awaiting verification"],
    "DDN-7162": ["S1, idle unknown"],
  });

  await press(page, "Development");

  assert.equal(rowOf(page, "EX-15181").getAttribute("class"), "awaiting");
  assert.equal(cell(page, "EX-15181", "Status").textContent, "Awaiting Verification", "a status that says so shows once");
  assert.equal(cell(page, "EX-15181", "Status").querySelectorAll(".verify").length, 1);
});

/** The files of an uncompressed zip, by name, as text. */
function unzip(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const text = (from, length) => new TextDecoder().decode(bytes.subarray(from, from + length));
  const files = {};
  for (let at = 0; view.getUint32(at, true) === 0x04034b50;) {
    const size = view.getUint32(at + 18, true);
    const nameLength = view.getUint16(at + 26, true);
    const start = at + 30 + nameLength + view.getUint16(at + 28, true);
    files[text(at + 30, nameLength)] = text(start, size);
    at = start + size;
  }
  return files;
}

/** The sheet name and the rows of cell text of a saved .xlsx. */
async function workbook(blob) {
  const files = unzip(new Uint8Array(await blob.arrayBuffer()));
  const unescape = (s) => s.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, '"').replace(/&amp;/g, "&");
  const rows = [...files["xl/worksheets/sheet1.xml"].matchAll(/<row [^>]*>(.*?)<\/row>/g)]
    .map(([, row]) => [...row.matchAll(/<t xml:space="preserve">(.*?)<\/t>/g)].map(([, text]) => unescape(text)));
  return { sheet: unescape(files["xl/workbook.xml"].match(/<sheet name="([^"]*)"/)[1]), rows };
}

test("a table exports the rows it shows: every column as xlsx, the keys as md", async () => {
  const page = await openPage();
  await press(page, WHAMCLOUD);

  await press(page, "Export xlsx: NVIDIA items");
  await press(page, "Export keys (md): NVIDIA items");

  const [xlsx, md] = page.saved();
  const { sheet, rows: sheetRows } = await workbook(xlsx.blob);
  assert.match(xlsx.name, /^nvidia-items-\d{8}\.xlsx$/);
  assert.equal(sheet, "NVIDIA items");
  assert.deepEqual(sheetRows[0], ["Key", "Summary", "Source", "Status", "Sev / Pri", "Owner", "Idle", "Date", "Fix", "Patches"]);
  assert.deepEqual(sheetRows.slice(1).map((row) => row[0]), ["NCP-163 also NP-131", "NVDCSE-250", "NVDCSE-248", "NVDCSE-216", "NCP-9"]);
  assert.deepEqual(sheetRows[1].slice(6, 8), ["3d", "1 Nov 2026 committed on NP-131"], "as shown, without the Edit button");
  assert.match(md.name, /^nvidia-items-\d{8}\.md$/);
  assert.equal(await md.blob.text(), "- NCP-163\n- NVDCSE-250\n- NVDCSE-248\n- NVDCSE-216\n- NCP-9\n");
});

test("a blocker list exports the blockers it shows", async () => {
  const snap = sampleSnapshot();
  nvidiaOf(snap).blockers = Array.from({ length: 12 }, (_, i) =>
    blockerOf(`DDN-${8000 + i}`, "Blocker", i ? null : "Awaiting Verification"));
  const page = await openPage(new FakeServer(snap));

  await press(page, "Export xlsx: NVIDIA blockers");
  await press(page, "Export keys (md): NVIDIA blockers");
  await press(page, "Show all 12");
  await press(page, "Export keys (md): NVIDIA blockers");

  const [xlsx, top, all] = page.saved();
  const { rows: sheetRows } = await workbook(xlsx.blob);
  const keys = async (file) => (await file.blob.text()).split("\n").filter(Boolean).length;
  assert.match(xlsx.name, /^nvidia-blockers-\d{8}\.xlsx$/);
  assert.deepEqual(sheetRows[0], ["Key", "Summary", "Owner", "Reason"]);
  assert.deepEqual(sheetRows[1], ["DDN-8000", "DDN-8000 summary", "Unassigned", "Blocker Awaiting verification"]);
  assert.equal(sheetRows.length, 11, "the header and the top 10 shown");
  assert.equal(await keys(top), 10);
  assert.equal(await keys(all), 12, "all 12 once all are shown");
});
