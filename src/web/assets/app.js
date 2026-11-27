'use strict';
// swarmy web: four views over the JSON the server shapes from the same code as the commands.
// Everything is drawn with text nodes: names, projects and paths come from transcripts and are
// not trusted. No library, no network beyond this server.

const POLL_MS = { live: 3000 }; // the limits view every fifth tick, 15 s
const TABS = [
  ['live', 'Live'],
  ['limits', 'Limits'],
  ['spend', 'Spend'],
  ['help', 'Help'],
];
const STATES = {
  working: ['●', 'working', 'working'],
  thinking: ['◌', 'thinking', 'thinking'],
  waiting: ['◐', 'waiting for agents', 'waiting for agents'],
  needs_you: ['▲', 'needs you', 'needs you'],
  stalled: ['✖', 'stalled', 'stalled'],
  idle: ['○', 'idle', 'idle'],
  done: ['✓', 'done', 'idle'],
  gone: ['·', 'exited', 'idle'],
};

const app = {
  tab: 'live',
  status: null,
  gloss: new Map(), // term name -> {short, long, aliases, see}
  expanded: new Set(), // agent keys whose detail row is open
  spend: { by: 'project', period: 'week' },
  helpFilter: '',
  timer: null,
  seq: 0, // a late answer for a tab the user has left is dropped
};

// ---- DOM -------------------------------------------------------------------------------------

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v === null || v === undefined) continue;
    if (k === 'class') el.className = v;
    else if (k === 'style') Object.assign(el.style, v); // CSSOM, which the CSP allows
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid === null || kid === undefined || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}
const view = () => document.getElementById('view');
const show = (...nodes) => view().replaceChildren(...nodes.flat(Infinity).filter(Boolean));

// ---- formats (the same shapes the terminal prints) -------------------------------------------

function money(x) {
  if (x === null || x === undefined || !Number.isFinite(x)) return '-';
  const a = Math.abs(x);
  const sign = x < 0 ? '-' : '';
  if (a >= 1 || a === 0) return sign + '$' + a.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  return sign + '$' + a.toFixed(a >= 0.01 ? 2 : 4);
}
function duration(sec) {
  sec = Math.max(0, Math.round(sec));
  const d = Math.floor(sec / 86400);
  const hr = Math.floor((sec % 86400) / 3600);
  const m = Math.floor((sec % 3600) / 60);
  if (d) return `${d}d ${hr}h`;
  if (hr) return `${hr}h ${m}m`;
  if (m) return `${m}m`;
  return `${sec}s`;
}
const ago = (iso) => duration((Date.now() - Date.parse(iso)) / 1000);
const until = (iso) => duration((Date.parse(iso) - Date.now()) / 1000);
const when = (iso) => new Date(iso).toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' });
const pct = (x) => (Number.isFinite(x) ? x.toFixed(x < 10 ? 1 : 0) + '%' : '-');
function tokens(n) {
  if (n >= 1e6) return (n / 1e6).toFixed(2) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'k';
  return String(n);
}
const count = (n) => Number(n).toLocaleString('en-US');

// A term's one-line meaning, shown as a tooltip on column heads and chips.
const tip = (term) => (app.gloss.get(term) || {}).short;

// ---- data ------------------------------------------------------------------------------------

async function api(path, params) {
  const q = params ? '?' + new URLSearchParams(params) : '';
  const res = await fetch('/api/' + path + q, { headers: { Accept: 'application/json' } });
  let body = null;
  try { body = await res.json(); } catch { /* an error page without JSON */ }
  if (!res.ok) throw new Error((body && body.error) || `${path}: ${res.status}`);
  return body;
}

function fail(err) {
  const box = h('div', { class: 'error', role: 'alert' }, 'Could not read the data: ', err.message || String(err));
  const first = view().firstChild;
  if (first && first.classList && first.classList.contains('error')) first.replaceWith(box);
  else view().prepend(box);
}

// ---- header ----------------------------------------------------------------------------------

function renderSummary(d) {
  const c = d.counts;
  const chips = [
    h('span', { class: 'chip', title: tip('api-equivalent cost') }, `today ${money(d.today_cost)}`),
    h('span', { class: 'chip', title: tip('burn') }, `burn ${money(d.burn_per_hour)}/h`),
    h('span', { class: 'chip' }, `${c.agents} agents`),
    h('span', { class: 'chip', title: tip('working') }, `● ${c.working} working`),
    h('span', { class: 'chip', title: tip('thinking') }, `◌ ${c.thinking} thinking`),
  ];
  if (c.needs_you) chips.push(h('span', { class: 'chip attn', title: tip('needs you') }, `▲ ${c.needs_you} need you`));
  if (c.stalled) chips.push(h('span', { class: 'chip bad', title: tip('stalled') }, `✖ ${c.stalled} stalled`));
  document.getElementById('summary').replaceChildren(...chips);
  document.getElementById('updated').textContent = 'updated ' + new Date().toLocaleTimeString();
  document.title = c.needs_you ? `(${c.needs_you}) swarmy: needs you` : 'swarmy';
}

// ---- live ------------------------------------------------------------------------------------

function stateCell(a) {
  const [glyph, label, term] = STATES[a.state] || ['?', a.state, a.state];
  return h('span', { class: `state s-${a.state}`, title: tip(term) }, `${glyph} ${label}`);
}

// What the agent is doing right now, in words: the tool and what it was asked, the question
// it waits on, or who it waits for.
function activityText(a) {
  if (a.state === 'needs_you' || a.state === 'working') return a.detail;
  if (a.state === 'waiting') return `waiting for ${a.detail}`;
  return '';
}

function facts(a) {
  const rows = [];
  const add = (k, v) => v !== null && v !== undefined && v !== '' && rows.push(h('dt', {}, k), h('dd', {}, v));
  add('project', a.project);
  add('checkout', a.checkout ? a.checkout + (a.branch ? ` · ⎇ ${a.branch}` : '') : null);
  add('seat', `${a.seat} (${a.harness})`);
  add('type', a.agent_type || (a.main ? 'main thread' : 'subagent'));
  add('model', a.model ? `${a.model}${a.effort ? ' · effort ' + a.effort : ''}` : null);
  add('in this state', `${ago(a.since)} (since ${new Date(a.since).toLocaleTimeString()})`);
  add('last activity', `${ago(a.last_activity)} ago`);
  add('requests', a.unpriced ? `${count(a.requests)} (unpriced model)` : count(a.requests));
  add('cost', money(a.cost));
  if (a.main) add('session total Σ', `${money(a.session_cost)} · ${money(a.session_burn)}/h over the last hour`);
  add('context', `${tokens(a.context_tokens)} of ${tokens(a.context_window)} (${pct(a.context_pct)})${a.context_pct >= 80 ? ' · filling up' : ''}`);
  return h('dl', { class: 'facts' }, rows);
}

function agentRows(a) {
  const open = app.expanded.has(a.key);
  const toggle = () => {
    if (app.expanded.has(a.key)) app.expanded.delete(a.key);
    else app.expanded.add(a.key);
    paint(); // from the last answer: opening a row needs no new data
  };
  const indent = a.depth ? h('span', { class: 'tree' }, '  '.repeat(a.depth - 1) + '└ ') : null;
  const lead = h('button', { class: 'row', 'aria-expanded': String(open), 'aria-label': `${a.name}: ${open ? 'hide' : 'show'} details`, onclick: toggle },
    indent, h('span', { class: 'name' }, a.name || '(unnamed)'));
  const rows = [
    h('tr', { class: a.main ? 'main' : 'sub' },
      h('td', {}, stateCell(a)),
      h('td', { class: 'clip' }, lead),
      h('td', { class: 'clip' }, a.project),
      h('td', {}, a.model ? `${a.model}${a.effort ? ' ' + a.effort : ''}` : '-'),
      h('td', { class: `clip mono${a.state === 'needs_you' ? ' warn' : ''}` }, activityText(a)),
      h('td', { class: 'num' }, ago(a.since)),
      h('td', { class: 'num' }, pct(a.context_pct)),
      h('td', { class: 'num' }, a.unpriced ? '-' : (a.main ? 'Σ' : '') + money(a.main ? a.session_cost : a.cost))),
  ];
  if (open) {
    rows.push(h('tr', { class: 'detail' }, h('td', { colspan: 8 }, a.hint ? h('p', { class: 'hint' }, a.hint) : null, facts(a))));
  }
  return rows;
}

function renderLive(d) {
  const waiting = d.agents.filter((a) => a.state === 'needs_you');
  const head = (text, term) => h('th', { title: tip(term) }, text);
  const headNum = (text, term) => h('th', { class: 'num', title: tip(term) }, text);
  const table = d.agents.length
    ? h('table', {},
        h('thead', {}, h('tr', {},
          head('State', 'working'), head('Agent'), head('Project', 'project'), head('Model', 'effort'),
          head('Activity'), headNum('In state'), headNum('Context', 'context'), headNum('Cost', 'session total'))),
        h('tbody', {}, d.agents.map(agentRows)))
    : h('p', { class: 'empty' }, 'No agents are running or were active recently. Start Claude Code in a terminal and they appear here.');
  const done = d.recent_done.slice(0, 8);
  show(
    waiting.length
      ? h('div', { class: 'banner', role: 'status' },
          h('strong', {}, `▲ ${waiting.length} waiting for you: `),
          waiting.map((a, i) => [i ? ' · ' : '', `${a.name} (${a.detail || 'input needed'})`]),
          h('div', { class: 'dim' }, 'Switch to that terminal. The terminal dashboard can jump there with J.'))
      : null,
    table,
    d.conflicts.length
      ? [h('h3', {}, 'Same file edited by several agents (30 min)'),
         h('p', {}, d.conflicts.map((c, i) => [i ? ', ' : '', h('span', { class: 'mono warn' }, c.file), ` (${c.agents} agents)`]))]
      : null,
    done.length
      ? [h('h3', {}, 'Recently finished'),
         h('table', {}, h('tbody', {}, done.map((r) => h('tr', {},
           h('td', { class: 'clip' }, r.name), h('td', {}, r.project), h('td', {}, r.model || '-'),
           h('td', { class: 'num dim' }, `${ago(r.finished)} ago`), h('td', { class: 'num' }, money(r.cost))))))]
      : null);
}

// ---- limits ----------------------------------------------------------------------------------

function runsOut(w) {
  if (w.eta_seconds === 0) return h('span', { class: 'bad' }, 'LIMIT REACHED');
  if (w.runs_out_at) return h('span', { class: 'warn' }, `runs out ${when(w.runs_out_at)} (in ${until(w.runs_out_at)})`);
  if (w.points_per_hour !== null && w.burn_usd_per_hour > 0) return 'lasts until reset';
  if (w.burn_usd_per_hour > 0) return 'pace unknown';
  return null;
}

function windowCard(w) {
  const lvl = String(w.level).toLowerCase();
  const shown = w.estimate_now;
  const maxShare = w.shares.length ? w.shares[0].share : 0;
  const reading = w.reset_passed
    ? `new window since ${when(w.reading.resets_at)}, no reading yet`
    : `reading ${pct(w.reading.utilization)}, ${ago(w.reading.fetched_at)} old`;
  const lines = [
    h('p', { class: 'dim' }, `resets ${when(w.resets_at)} (in ${until(w.resets_at)}) · ${reading}`),
    h('p', {}, `burn ${money(w.burn_usd_per_hour)}/h`, w.points_per_hour !== null ? ` ≈ ${w.points_per_hour.toFixed(1)}%/h` : '', ' · ', runsOut(w)),
  ];
  if (w.usd_per_point && w.window_usd) {
    lines.push(h('p', { class: 'dim', title: tip('1% ≈ $') }, `1% ≈ ${money(w.usd_per_point)} · a full window ≈ ${money(w.window_usd)} API-equivalent${w.reference ? ` (reference: ${w.reference}, same plan)` : ''}`));
  }
  if (w.outside_points !== null && w.outside_points >= 2) {
    lines.push(h('p', { class: 'warn', title: tip('elsewhere') }, `on this machine ≈${w.visible_points.toFixed(1)}% · elsewhere (claude.ai, desktop, other computers) ≈${w.outside_points.toFixed(1)}%`));
  }
  if (w.manual_reset) lines.push(h('p', { class: 'dim' }, `limit reset by hand ≈${when(w.manual_reset)}; counting from there`));
  const shares = w.shares.slice(0, 8);
  const table = shares.length
    ? h('table', {},
        h('thead', {}, h('tr', {}, h('th', {}, 'Project'), h('th', { class: 'num' }, 'Cost'), h('th', {}, 'Share'),
          h('th', { class: 'num', title: tip('% of week') }, '≈ Limit'))),
        h('tbody', {}, shares.map((s) => h('tr', {},
          h('td', { class: 'clip' }, s.name), h('td', { class: 'num' }, money(s.cost)),
          h('td', {}, h('span', { class: 'bar', style: { width: `${maxShare > 0 ? (s.share / maxShare) * 8 : 0}rem` } }), ` ${pct(s.share * 100)}`),
          h('td', { class: 'num' }, `≈${s.limit_points.toFixed(1)}%`)))))
    : null;
  return h('div', { class: 'window' },
    h('div', {},
      h('div', { class: 'dim', title: tip('weekly limit') }, w.label),
      h('div', { class: `big lvl-${lvl}`, title: tip('estimate') }, `≈${pct(shown)}`),
      h('div', { class: `gauge lvl-${lvl}`, role: 'img', 'aria-label': `${w.label} limit ${pct(shown)} used` }, h('div', { style: { width: `${Math.min(100, Math.max(0, shown))}%` } }),)),
    h('div', {}, lines, table));
}

function renderLimits(d) {
  if (!d.accounts.length) {
    show(h('p', { class: 'empty' }, 'No limit readings yet. Claude Code caches them after it shows usage: run /usage in Claude Code once, or install the status line with `swarmy setup` for exact numbers.'));
    return;
  }
  show(d.accounts.map((acc) => h('section', { class: 'card' },
    h('h2', {}, acc.name, acc.tier ? h('span', { class: 'dim' }, ` · ${acc.tier}`) : null),
    acc.windows.map(windowCard))),
    h('p', { class: 'dim', title: tip('estimate') }, '≈ marks an estimate: between readings, limit usage is assumed proportional to API-equivalent cost.'));
}

// ---- spend -----------------------------------------------------------------------------------

const PERIODS = [['today', 'today'], ['yesterday', 'yesterday'], ['24h', 'last 24 hours'], ['week', 'this limit week'], ['7d', 'last 7 days'], ['month', 'this month'], ['all', 'all time']];

function select(id, options, value, onchange) {
  return h('select', { id, onchange }, options.map(([v, text]) => h('option', { value: v, selected: v === value }, text)));
}

function renderSpend(d) {
  const r = d.report;
  const max = r.rows.reduce((m, x) => Math.max(m, x.cost), 0);
  const groupings = (app.status ? app.status.groupings : [{ id: app.spend.by, title: app.spend.by }]).map((g) => [g.id, g.title.toLowerCase()]);
  const controls = h('div', { class: 'controls' },
    h('label', {}, 'Group by ', select('by', groupings, app.spend.by, (e) => { app.spend.by = e.target.value; refresh(); })),
    h('label', {}, 'Period ', select('period', PERIODS, app.spend.period, (e) => { app.spend.period = e.target.value; refresh(); })),
    h('span', { class: 'dim' }, `${r.period.label}: ${when(r.period.start)} to ${when(r.period.end)}`));
  const body = r.rows.length
    ? h('table', {},
        h('thead', {}, h('tr', {}, h('th', {}, d.title), h('th', { class: 'num' }, 'Requests'), h('th', { class: 'num', title: tip('api-equivalent cost') }, 'Cost'), h('th', {}, 'Share'))),
        h('tbody', {}, r.rows.map((x) => {
          const unpriced = x.requests > 0 && x.unpriced === x.requests && x.cost === 0;
          const share = r.total.cost > 0 ? x.cost / r.total.cost : 0;
          return h('tr', {},
            h('td', { class: 'clip' }, x.label),
            h('td', { class: 'num' }, count(x.requests)),
            h('td', { class: 'num', title: unpriced ? tip('unpriced') : null }, unpriced ? '-' : money(x.cost)),
            h('td', {}, h('span', { class: 'bar', style: { width: `${max > 0 ? (x.cost / max) * 10 : 0}rem` } }), ` ${pct(share * 100)}`));
        })),
        h('tfoot', {}, h('tr', {}, h('td', {}, 'Total'), h('td', { class: 'num' }, count(r.total.requests)), h('td', { class: 'num' }, money(r.total.cost)), h('td', {}))))
    : h('p', { class: 'empty' }, 'No requests in this period.');
  show(controls, body);
}

// ---- help ------------------------------------------------------------------------------------

function renderHelp(d) {
  const matches = (t) => {
    const q = app.helpFilter.trim().toLowerCase();
    return !q || [t.name, t.short, t.long, ...t.aliases].some((x) => x.toLowerCase().includes(q));
  };
  const list = h('div', {});
  const draw = () => {
    const groups = d.groups.map((g) => {
      const terms = g.terms.filter(matches);
      if (!terms.length) return null;
      return [h('h3', {}, g.title), terms.map((t) => h('details', { open: app.helpFilter ? true : null },
        h('summary', {}, h('span', { class: 'term' }, t.name), h('span', { class: 'short' }, t.short)),
        h('p', {}, t.long),
        t.aliases.length ? h('p', { class: 'dim' }, 'Also called: ', t.aliases.join(', ')) : null,
        t.see.length ? h('p', { class: 'dim' }, 'See also: ', t.see.join(', ')) : null))];
    });
    list.replaceChildren(...(groups.some(Boolean) ? groups.flat(Infinity).filter(Boolean) : [h('p', { class: 'empty' }, 'No term matches that.')]));
  };
  const search = h('input', { type: 'search', placeholder: 'Find a word or glyph: stalled, ≈, seat…', 'aria-label': 'Find a term', value: app.helpFilter,
    oninput: (e) => { app.helpFilter = e.target.value; draw(); } });
  draw();
  show(h('div', { class: 'controls' }, search, h('span', { class: 'dim' }, 'The same text `swarmy explain` prints.')), list);
}

// ---- loop ------------------------------------------------------------------------------------

const RENDER = { live: renderLive, limits: renderLimits, spend: renderSpend, help: renderHelp };

// The last answer per view: repainting (opening a row) and the header need no new request.
app.data = {};

async function fetchTab(tab) {
  let data;
  if (tab === 'live') data = await api('live');
  else if (tab === 'limits') data = await api('limits');
  else if (tab === 'spend') data = await api('spend', app.spend);
  else data = await api('glossary');
  app.data[tab] = data;
  if (tab === 'live') renderSummary(data); // the header shows the live numbers on every tab
  return data;
}

function paint() {
  const d = app.data[app.tab];
  if (d) RENDER[app.tab](d);
}

async function refresh() {
  const tab = app.tab;
  const mine = ++app.seq;
  try {
    await fetchTab(tab);
    if (mine === app.seq && tab === app.tab) paint(); // a late answer for a view left is dropped
  } catch (err) {
    if (mine === app.seq) fail(err);
  }
}

// One timer. The live view every tick; the limits every fifth; the header's numbers on any other tab.
function schedule() {
  clearInterval(app.timer);
  let n = 0;
  app.timer = setInterval(() => {
    if (document.hidden) return;
    n += 1;
    if (app.tab === 'live' || (app.tab === 'limits' && n % 5 === 0)) refresh();
    else if (app.tab !== 'limits') fetchTab('live').catch(() => {});
  }, POLL_MS.live);
}

function go(tab) {
  app.tab = TABS.some(([id]) => id === tab) ? tab : 'live';
  if (location.hash !== '#' + app.tab) history.replaceState(null, '', '#' + app.tab);
  for (const b of document.querySelectorAll('#tabs button')) b.setAttribute('aria-selected', String(b.dataset.tab === app.tab));
  schedule();
  refresh();
  // The header shows the live numbers on every tab, also on the first paint of another one.
  if (app.tab !== 'live' && !app.data.live) fetchTab('live').catch(() => {});
}

async function start() {
  document.getElementById('tabs').replaceChildren(...TABS.map(([id, text]) =>
    h('button', { role: 'tab', 'data-tab': id, 'aria-selected': 'false', onclick: () => go(id) }, text)));
  // The glossary feeds every tooltip; the page works without it.
  try {
    const g = await api('glossary');
    for (const grp of g.groups) for (const t of grp.terms) app.gloss.set(t.name, t);
  } catch { /* tooltips are an extra */ }
  try { app.status = await api('status'); } catch { /* the grouping list falls back to the current one */ }
  window.addEventListener('hashchange', () => go(location.hash.slice(1)));
  document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
  go(location.hash.slice(1));
}

start();
