"use strict";

const VERDICT = {
  on_track: ["On track", "ok"],
  at_risk: ["At risk", "warn"],
  off_track: ["Off track", "bad"],
  no_date: ["No date", "neutral"],
  not_started: ["Not started", "neutral"],
};
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const BLOCKERS_SHOWN = 10;
/** The id of the "All items" section, which the date filters scroll to. */
const ITEMS_ID = "items";
const CARD_BLOCKERS_SHOWN = 5;
const POLL_REFRESHING_MS = 3000;
const POLL_IDLE_MS = 60000;
const PRIORITY_RANK = { blocker: 0, critical: 1, major: 2 };
const FIX_STATE = { delivered: "Delivered", under_way: "Under way", not_started: "Not started" };
const CHANGE_HEADERS = { "X-EXA-Status": "1", "Content-Type": "application/json" };
/** The columns of a blocker list, as its export names them. */
const BLOCKER_COLUMNS = ["Key", "Summary", "Owner", "Reason"];
/** How long a saved export stays readable for the browser's download. */
const EXPORT_URL_MS = 60000;

const ui = {
  snap: null,
  status: null,
  settings: null,
  snapshotAt: null,
  tabId: null,
  viewId: null,
  pollTimer: null,
  // The open date editor: { key, date, note } as typed so far.
  editing: null,
  // A snapshot arrived while the editor was open; render once it closes.
  stale: false,
  expanded: new Set(),
  // Per view: text filter, sort, and `only` ({ date, atRisk }), which limits
  // the table to one date group, or to the tickets putting it at risk.
  tables: {},
};

const prefs = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* private window: nothing to remember */ } },
};

function el(tag, attrs = {}, ...kids) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") node.className = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) {
    if (kid === null || kid === undefined || kid === false) continue;
    node.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return node;
}

const $ = (id) => document.getElementById(id);
const fmtDate = (iso) => {
  if (!iso) return "none";
  const [y, m, d] = iso.slice(0, 10).split("-").map(Number);
  return `${d} ${MONTHS[m - 1]} ${y}`;
};
const fmtTime = (iso) => (iso ? new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "");
const sameDay = (a, b) => a.toDateString() === b.toDateString();
/** "18:04" today, "Fri 23 Oct 18:04" on another day. */
const fmtWhen = (iso) => {
  if (!iso) return "";
  const t = new Date(iso);
  return sameDay(t, new Date()) ? fmtTime(iso) : `${t.toLocaleDateString([], { weekday: "short", day: "numeric", month: "short" })} ${fmtTime(iso)}`;
};
const verdictOf = (v) => VERDICT[v] || ["None", "neutral"];
const chip = (v) => el("span", { class: `chip ${verdictOf(v)[1]}` }, verdictOf(v)[0]);
const toneChip = (tone, text) => el("span", { class: `chip ${tone === "danger" ? "bad" : "warn"}` }, text);
const dot = (v) => el("span", { class: `dot ${verdictOf(v)[1]}`, title: verdictOf(v)[0], role: "img", "aria-label": verdictOf(v)[0] });
const link = (url, text) => el("a", { href: url, target: "_blank", rel: "noopener noreferrer" }, text);
const branchName = (branch) => branch.replace(/^b_/, "").replace(/_0$/, "");

async function getJson(url) {
  const r = await fetch(url, { cache: "no-store" });
  let data = null;
  try { data = await r.json(); } catch { /* not JSON */ }
  if (!r.ok) throw new Error((data && data.error) || `${url}: HTTP ${r.status}`);
  return data;
}

async function postJson(url, body) {
  let r;
  try {
    r = await fetch(url, { method: "POST", headers: CHANGE_HEADERS, body: JSON.stringify(body ?? {}) });
  } catch (e) {
    return { ok: false, data: { error: `The dashboard did not answer (${e.message}); nothing was saved.` } };
  }
  let data = {};
  try { data = await r.json(); } catch { /* empty reply */ }
  return { ok: r.ok, data };
}

function say(text) {
  const f = $("flash");
  if (f) f.textContent = text;
}

async function loadStatus() {
  try {
    ui.status = await getJson("/api/status");
    if (ui.status.snapshot_at && ui.status.snapshot_at !== ui.snapshotAt) {
      const s = await getJson("/api/snapshot");
      ui.snapshotAt = ui.status.snapshot_at;
      ui.snap = s.empty ? null : s;
      if (ui.editing) {
        ui.stale = true;
        renderMeta();
      } else {
        render();
      }
    } else {
      renderMeta();
      if (!ui.snap) render();
    }
  } catch (e) {
    say(`Not up to date: ${e.message}`);
  }
  clearTimeout(ui.pollTimer);
  ui.pollTimer = setTimeout(loadStatus, ui.status && ui.status.refreshing ? POLL_REFRESHING_MS : POLL_IDLE_MS);
}

async function loadSettings() {
  try {
    ui.settings = await getJson("/api/settings");
    renderMeta();
  } catch (e) {
    say(`Settings not read: ${e.message}`);
  }
}

async function refreshNow() {
  const r = await postJson("/api/refresh");
  if (!r.ok) say(r.data.error || "The refresh was refused");
  else if (r.data.refresh === "queued") say("A refresh is running; another one follows it.");
  loadStatus();
}

async function setRefreshInterval(minutes) {
  const r = await postJson("/api/settings", { refresh_minutes: minutes });
  if (r.ok) {
    ui.settings.refresh_minutes = minutes;
    say(minutes === 0 ? "Refreshing by hand only" : `Refreshing every ${minutes} minutes`);
    loadStatus();
  } else {
    say(r.data.error || "The interval was not saved");
    renderMeta();
  }
}

function renderMeta() {
  const meta = $("meta");
  meta.replaceChildren();
  if (ui.snap) {
    for (const s of ui.snap.sources) {
      meta.append(el("span", { title: s.error || "answering" }, s.name, el("span", { class: `dot ${s.error ? "bad" : "ok"}` })));
    }
  }
  const st = ui.status;
  let when = "No data yet";
  if (st && st.refreshing) when = st.queued ? "Refreshing now, one more queued" : "Refreshing now";
  else if (ui.snap) when = `As of ${fmtWhen(ui.snap.generated_at)}${st && st.next_refresh ? `, next ${fmtWhen(st.next_refresh)}` : ""}`;
  meta.append(el("span", {}, when));
  if (ui.snap && st && stale(ui.snap.generated_at, st.interval_minutes)) {
    meta.append(el("span", { class: "err", title: "No refresh has finished for two intervals" }, "Out of date"));
  }
  if (ui.stale) meta.append(el("span", { class: "muted" }, "Newer data waits until the date editor closes"));
  if (st && st.failed) meta.append(el("span", { class: "err", title: "See data/refresh.log" }, "Last refresh failed"));
  if (ui.settings) {
    const select = el("select", { "aria-label": "Refresh interval", onchange: (e) => setRefreshInterval(Number(e.target.value)) },
      ui.settings.choices.map((m) => el("option", { value: String(m), selected: m === ui.settings.refresh_minutes }, m === 0 ? "By hand" : `Every ${m} min`)));
    meta.append(select);
  }
  meta.append(el("button", { type: "button", onclick: refreshNow }, st && st.refreshing ? "Refreshing" : "Refresh now"));
}

/** The snapshot is more than two refresh intervals old (a day when refreshing by hand). */
function stale(generatedAt, minutes) {
  const limit = (minutes > 0 ? 2 * minutes : 24 * 60) * 60 * 1000;
  return Date.now() - new Date(generatedAt).getTime() > limit;
}

function select(t, v) {
  ui.tabId = t;
  prefs.set("tab", t);
  ui.viewId = v || prefs.get(`view:${t}`);
  if (v) prefs.set(`view:${t}`, v);
  closeEditor();
}

/** Closes the date editor and shows anything that arrived while it was open. */
function closeEditor() {
  ui.editing = null;
  ui.stale = false;
  render();
}

function render() {
  renderMeta();
  const tabs = $("tabs");
  const sub = $("subtabs");
  const view = $("view");
  tabs.replaceChildren();
  sub.replaceChildren();
  view.replaceChildren();
  if (!ui.snap) {
    view.append(el("p", { class: "empty" }, ui.status && ui.status.refreshing
      ? "The first refresh is running; it takes a few minutes."
      : "No snapshot yet. Press Refresh now."));
    return;
  }
  const tab = ui.snap.tabs.find((t) => t.id === ui.tabId) || ui.snap.tabs[0];
  ui.tabId = tab.id;
  for (const t of ui.snap.tabs) {
    const active = t.id === tab.id;
    tabs.append(el("button", { type: "button", class: `tab${active ? " active" : ""}`, role: "tab", "aria-selected": active ? "true" : "false", onclick: () => select(t.id, null) }, t.label, dot(t.verdict)));
  }
  const v = tab.views.find((x) => x.id === ui.viewId) || tab.views[0];
  ui.viewId = v.id;
  if (tab.views.length > 1) {
    for (const x of tab.views) {
      sub.append(el("button", { type: "button", class: `subtab${x.id === v.id ? " active" : ""}`, onclick: () => select(tab.id, x.id) }, x.label, dot(x.verdict)));
    }
  }
  view.append(renderView(v));
}

function section(title, body, id) {
  return el("section", { id }, el("h2", {}, title), body);
}

function renderView(v) {
  dropVanishedFilter(v);
  const root = el("div", {});
  root.append(el("div", { class: "headline" }, chip(v.verdict), el("span", {}, v.summary)));
  for (const n of v.notes) root.append(el("p", { class: "note" }, n));
  root.append(section("Where we are", el("div", { class: "kpis" }, kpis(v), v.health ? health(v.health) : null)));
  if (v.kind === "customers") root.append(section("On track for the dates?", customerDates(v)));
  if (v.kind === "release") root.append(section("On track for the proposed dates?", tracks(v.tracks)));
  if (v.kind === "initiatives") root.append(section("On track for the proposed dates?", initiativeCards(v.initiatives)));
  if (v.kind !== "initiatives") root.append(section("Immediate blockers", blockerList(v.id, v.blockers, BLOCKERS_SHOWN, `${v.label} blockers`)));
  root.append(section(`All ${v.items.length} items`, itemTable(v), ITEMS_ID));
  const tests = v.tests?.length;
  if (tests) root.append(section(`${tests} Test item${tests > 1 ? "s" : ""}`, itemTable(v, { items: v.tests, stateId: `${v.id}:tests`, name: "Test items" })));
  return root;
}

/** The tiles; a tile counting tickets by source offers each count as a table filter. */
function kpis(v) {
  const sourceToggle = (s) => {
    const text = `${s.source} ${s.count}`;
    return filterToggle(v, { source: s.source }, text, `${text} ticket${s.count > 1 ? "s" : ""}`);
  };
  return v.kpis.map((k) => el("div", { class: "kpi" },
    el("div", { class: "kpi-label" }, k.label),
    el("div", { class: `kpi-value ${k.tone || ""}` }, k.value),
    el("div", { class: "kpi-note" }, k.by_source?.length ? k.by_source.flatMap((s, i) => [i ? ", " : "", sourceToggle(s)]) : k.note)));
}

function health(h) {
  const rate = (r) => (r === null || r === undefined ? "none" : `${r.toFixed(1)}%`);
  const parts = [
    [h.es6, "es6 sessions clean", "es6: no test sessions"],
    [h.es7, "es7 sessions clean", "es7: no test sessions"],
    [h.emf, "EMF nightly passing", "EMF: no finished nightly runs"],
  ].filter(([part]) => part);
  const rows = parts.map(([part, label, none]) => {
    const name = label.split(" ")[0];
    if (part.error) return el("div", { class: "err", title: part.error }, `${name}: unavailable`);
    const left = part.excluded ? ` (${part.excluded} cancelled, skipped, timed out or still running)` : "";
    if (!part.count) return el("div", {}, `${none} in ${h.days} days${left}`);
    const of = part.capped ? `the newest ${part.count}, EXA Reporter's limit` : String(part.count);
    return el("div", {}, `${label}: `, el("strong", {}, rate(part.rate)), ` of ${of}${left}`);
  });
  return el("div", { class: "kpi health" },
    el("div", { class: "kpi-label" }, `Build health, last ${h.days} days`),
    rows,
    el("div", { class: "kpi-note" }, link(h.url, "Open EXA Reporter")));
}

function dateCell(label, date, note, url) {
  return el("div", {},
    el("div", { class: "kpi-label" }, label),
    el("div", { class: "date-value" }, url && date ? link(url, fmtDate(date)) : fmtDate(date)),
    note ? el("div", { class: "kpi-note" }, note) : null);
}

function trackCard(t, extra) {
  const pmNote = t.pm
    ? `${t.pm.status || "no status"}, target "${t.pm.target || "none"}", page updated ${fmtDate(t.pm.updated)}`
    : t.pm_unread ? "the PM page could not be read" : "no PM page for this one";
  const projNote = t.per_item ? "for reference; judged per item" : (t.projected ? `${t.closed_per_week} closed per week` : "nothing closed in the window");
  return el("div", { class: "track" },
    el("div", { class: "track-head" },
      el("strong", {}, t.name),
      el("span", { class: "muted" }, `${t.open} open, ${t.weighted} weighted`),
      chip(t.verdict)),
    el("div", { class: "dates" },
      t.ref_source === "agreed" ? dateCell("Release date", t.ref_date, "agreed; the verdict uses it") : null,
      dateCell("PM status page", t.pm_date, pmNote, t.pm ? t.pm.url : null),
      dateCell("Jira", t.jira_date, t.start_date ? `starts ${fmtDate(t.start_date)}` : ""),
      dateCell("Projected at current rate", t.projected, projNote)),
    el("p", { class: `reason${t.gap_flagged ? " flag" : ""}` }, t.reason),
    extra || null);
}

function tracks(list) {
  if (!list.length) return el("p", { class: "muted" }, "No dated release.");
  return el("div", { class: "tracks" }, list.map((t) => trackCard(t)));
}

function initiativeCards(cards) {
  if (!cards.length) return el("p", { class: "muted" }, "No initiatives configured.");
  return el("div", { class: "tracks" }, cards.map((c, i) => trackCard(c.track, el("div", { class: "card-extra" },
    c.epics.length ? el("div", { class: "epics" }, el("span", { class: "kpi-label" }, "Epics: "),
      c.epics.map((e, j) => el("span", { title: `${e.summary} (${e.status}${e.owner ? `, ${e.owner}` : ""})` }, j ? ", " : "", link(e.url, e.key)))) : null,
    c.blockers.length ? blockerList(`card-${i}`, c.blockers, CARD_BLOCKERS_SHOWN, `${c.track.name} blockers`) : el("p", { class: "kpi-note" }, "No blockers.")))));
}

function customerDates(v) {
  const box = el("div", { class: "track" });
  if (!v.date_groups.length) box.append(el("p", { class: "muted" }, "No ticket in this view has a date yet."));
  for (const g of v.date_groups) {
    const left = g.days_left >= 0 ? `${g.days_left} days left` : `${-g.days_left} days ago`;
    const statuses = g.by_status.map((s) => `${s.count} ${s.status}`).join(", ");
    const n = g.keys.length;
    // The ticket count, and the count of tickets putting the group at risk,
    // limit the table to them.
    const groupToggle = ({ atRisk, text }) => filterToggle(v, { date: g.date, atRisk }, text, `${text}, due ${fmtDate(g.date)}`);
    const reason = g.reason && g.stalled.length ? groupToggle({ atRisk: true, text: g.reason }) : g.reason;
    box.append(el("div", { class: "group" },
      el("span", {},
        el("strong", {}, `${fmtDate(g.date)}, ${left}: `),
        groupToggle({ atRisk: false, text: `${n} ticket${n > 1 ? "s" : ""}` }),
        `, ${statuses}`,
        g.complete ? `, ${g.complete} complete` : null,
        reason ? [". ", reason] : null),
      chip(g.verdict)));
  }
  const setIn = v.editable_commitments ? "the Date column" : datesSetIn(v);
  const source = setIn
    ? `Committed dates come from the local commitments file; set them in ${setIn}. Other dates are the release date of the linked EX fix; EXR fixes give no date.`
    : "Dates are the release date of the linked EX fix; EXR fixes give no date.";
  box.append(el("p", { class: "kpi-note" }, `${v.undated} tickets have no date. ${source}`));
  if (v.pm) {
    box.append(el("p", { class: "kpi-note" }, "PM status page ", link(v.pm.url, v.pm.title),
      `: ${v.pm.status || "no status"}, target ${v.pm.target || "none"}, updated ${fmtDate(v.pm.updated)}`));
  }
  return box;
}

/** Where the committed dates of a view without the date editor are set: "the
 * GCP and NVIDIA views" for All, named from the views that edit dates and
 * share its tickets; "" when none does. */
function datesSetIn(v) {
  const keys = new Set(v.items.map((t) => t.key));
  const labels = ui.snap.tabs.flatMap((t) => t.views)
    .filter((x) => x.editable_commitments && x.items.some((t) => keys.has(t.key)))
    .map((x) => x.label);
  return labels.length ? `the ${labels.join(" and ")} view${labels.length > 1 ? "s" : ""}` : "";
}

/** The top `shown` blockers, or all when asked for, and the exports of those
 * shown; `name` names the list in the export. */
function blockerList(id, list, shown, name) {
  if (!list.length) return el("p", { class: "muted" }, "None.");
  const all = ui.expanded.has(id);
  const visible = all ? list : list.slice(0, shown);
  const rows = visible.map((b) => el("div", { class: "brow" },
    link(b.url, b.key),
    el("span", { class: "sum", title: b.summary }, b.summary),
    el("span", { class: "muted" }, b.owner || "Unassigned"),
    el("span", { class: "reasons" },
      toneChip(b.tone, b.reason),
      b.verification ? el("span", { class: "chip verify", title: b.verification }, "Awaiting verification") : null,
      b.complete ? el("span", { class: "chip ok", title: "Its patches are merged" }, "Complete") : null)));
  const toggle = () => { if (all) ui.expanded.delete(id); else ui.expanded.add(id); render(); };
  return el("div", { class: "blockers" }, rows,
    el("p", { class: "more" },
      list.length > shown ? el("button", { type: "button", onclick: toggle }, all ? `Show the top ${shown}` : `Show all ${list.length}`) : null,
      exportTools(name, {
        header: BLOCKER_COLUMNS,
        rows: () => rows.map((row) => [...row.children].map(shownText)),
        keys: () => visible.map((b) => b.key),
      })));
}

/** Today as YYYYMMDD, which every export's file name ends with. */
function stamp() {
  const d = new Date();
  return `${d.getFullYear()}${String(d.getMonth() + 1).padStart(2, "0")}${String(d.getDate()).padStart(2, "0")}`;
}

/** "EXA SD Test items" as "exa-sd-test-items". */
const slug = (text) => text.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");

/** Has the browser save `blob` as the file `name`. */
function save(name, blob) {
  const url = URL.createObjectURL(blob);
  const a = el("a", { href: url, download: name });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), EXPORT_URL_MS);
}

/** A cell's text as shown, without its buttons ("Edit", "Set"); pieces drawn
 * apart, such as two patch chips, are kept apart by a space. */
function shownText(node) {
  const text = (n) => (!n.tagName ? n.textContent : n.tagName === "BUTTON" ? "" : [...n.childNodes].map(text).join(" "));
  return text(node).replace(/\s+/g, " ").trim();
}

/** The two exports of a table named `label`, of the rows it shows at the
 * click: every column (`header`, `rows()`) as .xlsx, or the ticket keys
 * (`keys()`) as a Markdown list. */
function exportTools(label, { header, rows, keys }) {
  const file = (ext) => `${slug(label)}-${stamp()}.${ext}`;
  const toXlsx = () => save(file("xlsx"), xlsxBlob(label, [header, ...rows()]));
  const toMd = () => save(file("md"), new Blob([keys().map((k) => `- ${k}\n`).join("")], { type: "text/markdown" }));
  return el("span", { class: "exports" },
    el("button", { type: "button", "aria-label": `Export xlsx: ${label}`, onclick: toXlsx }, "Export xlsx"),
    el("button", { type: "button", "aria-label": `Export keys (md): ${label}`, onclick: toMd }, "Export keys (md)"));
}

/** "es6 ddn262", "es6 merged" before its first tag, or "es7 review". */
const patchText = (branch, merged, tag) => `${branchName(branch)} ${merged ? (tag ? tag.replace(/^.*-/, "") : "merged") : "review"}`;

function patchCell(t) {
  const shown = (t.patches || []).filter((p) => p.status !== "abandoned");
  if (!shown.length) return "";
  return el("span", { class: "patches" }, shown.map((p) => {
    const text = patchText(p.branch, p.status === "merged", p.first_tag);
    return el("a", { class: `chip ${p.status === "merged" ? "ok" : "neutral"}`, href: p.url, target: "_blank", rel: "noopener noreferrer", title: `Change ${p.number}` }, text);
  }));
}

/** Where the patches that decide a ticket's delivery are, branch by branch
 * ("es6 merged (next tag)" until the next ddn tag has them), and the release
 * expected to carry those in review. */
function deliveryText(d) {
  const where = d.branches
    .map((b) => (b.merged && !b.tag ? `${branchName(b.branch)} merged (next tag)` : patchText(b.branch, b.merged, b.tag)))
    .join(", ");
  return d.release ? `${where}, for ${d.release}` : where;
}

/** The date a customer ticket is judged by, as the rules pick it: the
 * committed date, else its patches', else its linked fix's. */
const targetDate = (t) => (t.committed ? t.committed.date : t.delivery ? t.delivery.date : t.fix ? t.fix.date : null);

/** Shows a saved or cleared date on its rows at once, so the editor never
 * reopens on the old one; the next snapshot brings the rest (groups, verdicts). */
function applyCommitment(key, committed) {
  if (!ui.snap) return;
  for (const tab of ui.snap.tabs) {
    for (const v of tab.views) {
      for (const t of v.kind === "customers" ? v.items : []) {
        if ((t.committed ? t.committed.key : t.key) === key) t.committed = committed;
      }
    }
  }
}

async function saveCommitment(key, date, note) {
  const r = await postJson("/api/commitments", { key, date, note });
  if (r.ok) {
    applyCommitment(key, r.data.committed ?? null);
    const when = r.data.refresh === "queued" ? "Dates and verdicts update after the next refresh." : "Refreshing dates and verdicts.";
    say(date ? `Saved ${key}: ${fmtDate(date)}. ${when}` : `Cleared the date of ${key}. ${when}`);
    closeEditor();
    loadStatus();
  } else {
    say(r.data.error || `Not saved (${key})`);
  }
}

function dateEditor(t) {
  const draft = ui.editing;
  const date = el("input", { type: "date", value: draft.date, "aria-label": `Committed date for ${t.key}`, oninput: (e) => { draft.date = e.target.value; } });
  const note = el("input", { type: "text", value: draft.note, placeholder: "Note", maxlength: ui.settings ? String(ui.settings.max_note_chars) : null, "aria-label": `Note for ${t.key}`, oninput: (e) => { draft.note = e.target.value; } });
  const save = () => {
    if (!draft.date) { say("Pick a date, or use Clear"); return; }
    saveCommitment(draft.key, draft.date, draft.note);
  };
  return el("span", { class: "editor" }, date, note,
    el("button", { type: "button", onclick: save }, "Save"),
    t.committed ? el("button", { type: "button", onclick: () => saveCommitment(draft.key, null, null) }, "Clear") : null,
    el("button", { type: "button", onclick: closeEditor }, "Cancel"));
}

function openEditor(t) {
  // A promise filed under the mirror copy (NP for an NCP row) is edited there.
  const key = t.committed ? t.committed.key : t.key;
  ui.editing = { row: t.key, key, date: t.committed ? t.committed.date : "", note: t.committed && t.committed.note ? t.committed.note : "" };
  render();
}

function customerDateCell(t, editable) {
  if (editable && ui.editing && ui.editing.row === t.key) return dateEditor(t);
  const edit = editable ? el("button", { type: "button", class: "link-button", onclick: () => openEditor(t) }, t.committed ? "Edit" : "Set") : null;
  if (t.committed) {
    const filed = t.committed.key === t.key ? "committed" : `committed on ${t.committed.key}`;
    return el("span", { title: t.committed.note || "" }, fmtDate(t.committed.date), " ", el("span", { class: "badge" }, filed), " ", edit);
  }
  if (t.delivery && t.delivery.date) {
    const why = `The date of ${t.delivery.release}, expected to carry its patches in review`;
    return el("span", { title: why }, fmtDate(t.delivery.date), " ", el("span", { class: "badge" }, "patch"), " ", edit);
  }
  if (t.fix) return el("span", {}, fmtDate(t.fix.date), " ", el("span", { class: "badge" }, "fix"), " ", edit);
  return edit || "";
}

const sevPri = (t) => (t.severity ? t.severity.label : t.priority || "");
/** Every patch that decides the ticket's delivery is merged: it is in a ddn
 * tag, or due in the next one. */
const isComplete = (t) => Boolean(t.delivery && t.delivery.merged);
/** The status, with pills for what it may not say: the fix awaits
 * verification (that status or a label), or the ticket is complete. */
function statusCell(t) {
  const pills = [
    t.verification ? el("span", { class: "badge verify", title: "Awaiting verification" }, t.verification) : null,
    isComplete(t) ? el("span", { class: "badge complete", title: "Its patches are merged: in a ddn tag, or due in the next one" }, "Complete") : null,
  ].filter(Boolean);
  if (!pills.length) return t.status;
  const parts = [...(t.verification === t.status ? [] : [t.status]), ...pills];
  return el("span", {}, parts.flatMap((part, i) => (i ? [" ", part] : [part])));
}
/** Highlights a row whose fix awaits verification, or that is complete. */
const rowClass = (t) => [t.verification ? "awaiting" : null, isComplete(t) ? "complete" : null].filter(Boolean).join(" ") || null;
const unknownIdle = (t) => t.idle_days === null || t.idle_days === undefined;
/** Idle time. Customer tickets are all read, so a missing one could not be
 * read and shows as unknown; release items are read only where a rule needs it. */
const idleCell = (customers) => (t) => {
  if (unknownIdle(t)) return customers ? el("span", { title: "The newest comment could not be read" }, "unknown") : "";
  const who = t.last_comment
    ? `Last comment by ${t.last_comment.author || "an unnamed author"} on ${fmtDate(t.last_comment.date)}`
    : "No comment by a person; counted from creation";
  return el("span", { title: who }, `${t.idle_days}d`);
};
/** Unknown idle time sorts as the most idle in customer views, as the rules treat it. */
const idleSort = (customers) => (t) => (unknownIdle(t) ? (customers ? Infinity : -1) : t.idle_days);
const fixCell = (t) => {
  if (t.delivery) {
    const why = t.delivery.merged ? "Complete: every patch is merged" : "A patch is still in review";
    return el("span", { title: why }, deliveryText(t.delivery));
  }
  if (t.fix) return el("span", { title: `${t.fix.status}` }, link(t.fix.url, `${t.fix.key} ${t.fix.version}`));
  return FIX_STATE[t.fix_state] || "";
};

function keyCell(t) {
  const span = el("span", {}, link(t.url, t.key));
  if (t.also && t.also.length) span.append(el("span", { class: "also" }, ` also ${t.also.join(", ")}`));
  return span;
}

function columns(v) {
  const summary = { label: "Summary", sort: (t) => t.summary, cell: (t) => el("span", { title: t.summary }, t.summary), cls: "cell-sum" };
  const patches = { label: "Patches", sort: (t) => (t.patches || []).length, cell: patchCell };
  if (v.kind === "customers") {
    return [
      { label: "Key", sort: (t) => t.key, cell: keyCell },
      summary,
      { label: "Source", sort: (t) => t.source || "", cell: (t) => t.source || "" },
      { label: "Status", sort: (t) => t.status, cell: statusCell },
      { label: "Sev / Pri", sort: (t) => (t.severity ? t.severity.level : 9) * 10, cell: sevPri },
      { label: "Owner", sort: (t) => t.assignee || "", cell: (t) => t.assignee || "Unassigned" },
      { label: "Idle", sort: idleSort(true), cell: idleCell(true), cls: "num" },
      { label: "Date", sort: (t) => targetDate(t) || "9999", cell: (t) => customerDateCell(t, v.editable_commitments) },
      { label: "Fix", sort: (t) => (t.delivery ? deliveryText(t.delivery) : t.fix ? t.fix.key : FIX_STATE[t.fix_state] || ""), cell: fixCell },
      patches,
    ];
  }
  return [
    { label: "Key", sort: (t) => t.key, cell: keyCell },
    summary,
    { label: "Version", sort: (t) => t.fix_versions.join(", "), cell: (t) => t.fix_versions.join(", ") },
    { label: "Type", sort: (t) => t.issue_type, cell: (t) => t.issue_type },
    { label: "Status", sort: (t) => t.status, cell: statusCell },
    { label: "Priority", sort: (t) => PRIORITY_RANK[(t.priority || "").toLowerCase()] ?? 3, cell: (t) => t.priority || "" },
    { label: "Owner", sort: (t) => t.assignee || "", cell: (t) => t.assignee || "Unassigned" },
    { label: "Due", sort: (t) => t.due || "9999", cell: (t) => (t.due ? fmtDate(t.due) : "") },
    { label: "Idle", sort: idleSort(false), cell: idleCell(false), cls: "num" },
    patches,
  ];
}

/** A view's table state, made on first use. */
function tableState(id) {
  return ui.tables[id] || (ui.tables[id] = { q: "", col: -1, asc: true, only: null });
}

/** Both filters name the same date and subset, or the same source. A filter
 * (`only`) is { date, atRisk } or { source }. */
const sameFilter = (a, b) => Boolean(a && b && a.date === b.date && a.atRisk === b.atRisk && a.source === b.source);
/** "source" or "date": what a filter limits the table by. */
const filterKind = (only) => (only.source ? "source" : "date");
/** Finds a filter's toggle again after a render. */
const filterId = (only) => (only.source ? `source:${only.source}` : `${only.date}:${only.atRisk}`);
/** The date group a filter names, if the snapshot still has it. */
const groupOf = (v, only) => v.date_groups.find((g) => g.date === only.date);

/** The keys a filter shows: a source's tickets, a date group's, or those putting it at risk. */
function filterKeys(v, only) {
  if (only.source) return new Set(v.items.filter((t) => t.source === only.source).map((t) => t.key));
  const g = groupOf(v, only);
  return new Set(g ? (only.atRisk ? g.stalled : g.keys) : []);
}

/** "From DDN", "Due 1 Nov 2026", or "Due 1 Nov 2026, 13 not started or idle", from the view as it is now. */
function filterLabel(v, only) {
  if (only.source) return `From ${only.source}`;
  const g = groupOf(v, only);
  return `Due ${fmtDate(only.date)}${only.atRisk && g && g.reason ? `, ${g.reason}` : ""}`;
}

/** A count that limits the table to its tickets (`only`); pressing it again lifts the limit. */
function filterToggle(v, only, text, label) {
  const on = sameFilter(tableState(v.id).only, only);
  return el("button", {
    type: "button",
    class: "link-button",
    "aria-pressed": on ? "true" : "false",
    "aria-label": label,
    "data-filter": filterId(only),
    title: on ? `Remove this ${filterKind(only)} filter` : "Show only these tickets in the table below",
    onclick: () => toggleGroup(v, only),
  }, text);
}

/** Lifts a filter that the latest snapshot leaves empty (its date passed,
 * its tickets were re-dated, the group is no longer at risk, or no ticket
 * comes from its source any more), so the table never reads as empty when
 * the tickets still exist. */
function dropVanishedFilter(v) {
  const st = tableState(v.id);
  if (st.only && !filterKeys(v, st.only).size) {
    say(st.only.source
      ? `No ticket comes from ${st.only.source} any more; showing every source.`
      : `No ticket matches the ${fmtDate(st.only.date)} date filter any more; showing every date.`);
    st.only = null;
  }
}

/** Limits the table to one date group, the tickets putting it at risk, or
 * one source's tickets, or lifts the limit when the same filter is pressed
 * again. An open date editor closes, as on a view switch. Focus moves to
 * what changed. */
function toggleGroup(v, only) {
  const st = tableState(v.id);
  st.only = sameFilter(st.only, only) ? null : only;
  closeEditor();
  if (st.only) {
    $(ITEMS_ID)?.scrollIntoView({ block: "start" });
    document.querySelector(`#${ITEMS_ID} .filter-chip button`)?.focus({ preventScroll: true });
    const shown = document.querySelectorAll(`#${ITEMS_ID} tbody tr`).length;
    say(`${filterLabel(v, st.only)}: ${shown} of ${v.items.length} tickets shown.`);
  } else {
    document.querySelector(`[data-filter="${filterId(only)}"]`)?.focus();
    say(`Showing every ${filterKind(only)}.`);
  }
}

/** Lifts the filter from its chip; focus goes to the search box beside it. */
function clearFilter(v) {
  const st = tableState(v.id);
  const kind = filterKind(st.only);
  st.only = null;
  closeEditor();
  document.querySelector(`#${ITEMS_ID} input[type="search"]`)?.focus();
  say(`Showing every ${kind}.`);
}

/** A table of `items` (the view's own unless given) called `name`, with the
 * search, sort and filter kept under `stateId`, and the exports of the rows
 * it shows. */
function itemTable(v, { items = v.items, stateId = v.id, name = "items" } = {}) {
  const cols = columns(v);
  const st = tableState(stateId);
  const tbody = el("tbody", {});
  const head = el("tr", {});
  const count = el("span", { class: "muted" });
  let shown = items;
  const draw = () => {
    const q = st.q.trim().toLowerCase();
    let rows = items;
    if (st.only) {
      const keys = filterKeys(v, st.only);
      rows = rows.filter((t) => keys.has(t.key));
    }
    if (q) rows = rows.filter((t) => [t.key, t.summary, t.status, t.assignee || "", t.source || "", ...(t.also || []), ...(t.fix_versions || [])].join(" ").toLowerCase().includes(q));
    if (st.col >= 0) {
      const key = cols[st.col].sort;
      rows = [...rows].sort((a, b) => {
        const x = key(a);
        const y = key(b);
        const c = typeof x === "number" && typeof y === "number" ? x - y : String(x).localeCompare(String(y), undefined, { numeric: true });
        return st.asc ? c : -c;
      });
    }
    shown = rows;
    tbody.replaceChildren(...rows.map((t) => el("tr", { class: rowClass(t) }, cols.map((c) => el("td", { class: c.cls || null }, c.cell(t))))));
    head.querySelectorAll("th").forEach((th, i) => {
      th.className = i === st.col ? `sorted${st.asc ? " asc" : ""}` : "";
      th.setAttribute("aria-sort", i === st.col ? (st.asc ? "ascending" : "descending") : "none");
    });
    count.textContent = q || st.only ? `${rows.length} of ${items.length} shown` : "";
  };
  cols.forEach((c, i) => head.append(el("th", { scope: "col", tabindex: "0", onclick: () => { st.asc = st.col === i ? !st.asc : true; st.col = i; draw(); }, onkeydown: (e) => { if (e.key === "Enter") e.target.click(); } }, c.label)));
  const input = el("input", { type: "search", placeholder: "Filter by key, words, status, owner", value: st.q, "aria-label": `Filter ${name}`, oninput: (e) => { st.q = e.target.value; draw(); } });
  const filterChip = st.only
    ? el("span", { class: "filter-chip" },
      filterLabel(v, st.only),
      el("button", { type: "button", onclick: () => clearFilter(v) }, `Clear ${filterKind(st.only)} filter`))
    : null;
  const exports = exportTools(`${v.label} ${name}`, {
    header: cols.map((c) => c.label),
    rows: () => [...tbody.querySelectorAll("tr")].map((tr) => [...tr.querySelectorAll("td")].map(shownText)),
    keys: () => shown.map((t) => t.key),
  });
  draw();
  return el("div", {},
    el("div", { class: "table-tools" }, input, filterChip, count, exports),
    el("div", { class: "table-wrap" }, el("table", {}, el("thead", {}, head), tbody)));
}

ui.tabId = prefs.get("tab");
ui.viewId = ui.tabId ? prefs.get(`view:${ui.tabId}`) : null;
loadSettings();
loadStatus();
