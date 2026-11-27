// End-to-end check of `swarmy web` in a real browser. Not part of `cargo test`.
//   npm i puppeteer-core            (in any scratch dir; no install in the repo)
//   SWARMY_ISOLATED=1 HOME=$(mktemp -d) ./target/release/swarmy web --demo --port 17878 &
//   CHROME=/path/to/chrome node tools/web_e2e.mjs
// Checks every tab, row expansion, search, text escaping, narrow-width layout, and that the
// page makes no external requests and logs no console or CSP errors.
import puppeteer from 'puppeteer-core';
const base = 'http://127.0.0.1:17878';
const browser = await puppeteer.launch({ executablePath: process.env.CHROME || '/usr/bin/google-chrome', headless: 'new', args: ['--no-sandbox', '--disable-gpu'] });
const page = await browser.newPage();
await page.setViewport({ width: 1400, height: 900 });
const problems = [];
page.on('pageerror', (e) => problems.push('pageerror: ' + e.message));
page.on('console', (m) => { if (['error', 'warning'].includes(m.type())) problems.push(`console.${m.type()}: ${m.text()}`); });
page.on('requestfailed', (r) => problems.push('requestfailed: ' + r.url()));
const external = [];
page.on('request', (r) => { if (!r.url().startsWith(base) && !r.url().startsWith('data:')) external.push(r.url()); });
const ok = (name, cond, extra = '') => { console.log((cond ? 'PASS' : 'FAIL') + '  ' + name + (extra ? '  ' + extra : '')); if (!cond) process.exitCode = 1; };

await page.goto(base + '/#limits', { waitUntil: 'networkidle0' });
await page.waitForSelector('.window');
ok('header chips shown on first paint of a non-live tab', (await page.$$('#summary .chip')).length >= 4);
ok('limits: at least one account card', (await page.$$('section.card')).length >= 1);
ok('limits: gauge has a level class', await page.$eval('.gauge', (e) => /lvl-(ok|warn|bad|reached)/.test(e.className)));

await page.click('#tabs button[data-tab="live"]');
await page.waitForSelector('table tbody tr');
const rows = await page.$$eval('table tbody tr', (r) => r.length);
ok('live: agent rows rendered', rows > 5, `rows=${rows}`);
ok('live: URL hash follows the tab', page.url().endsWith('#live'));
const first = await page.$('button.row');
await first.click();
await page.waitForSelector('tr.detail');
const detail = await page.$eval('tr.detail dl.facts', (e) => e.textContent);
ok('live: row expands to a detail grid', /project/.test(detail) && /context/.test(detail), detail.slice(0, 80));
ok('live: aria-expanded is true when open', (await page.$eval('button.row', (e) => e.getAttribute('aria-expanded'))) === 'true');
await page.waitForTimeout?.(100);
await new Promise((r) => setTimeout(r, 3500)); // a poll happens; the row stays open
ok('live: an open row survives the next poll', (await page.$$('tr.detail')).length === 1);
await first.click().catch(() => {});

await page.setViewport({ width: 780, height: 900 });
await new Promise((r) => setTimeout(r, 300));
const narrow = await page.evaluate(() => {
  const t = document.querySelector('table'); const box = t.parentElement;
  const visible = [...t.querySelectorAll('thead th')].filter((e) => e.offsetParent !== null).map((e) => e.textContent);
  const modelRows = [...t.querySelectorAll('tbody tr.main td:nth-child(4)')].slice(0, 5).map((e) => e.getBoundingClientRect().height);
  return { fits: box.scrollWidth <= box.clientWidth + 1, visible, maxModelH: Math.max(...modelRows) };
});
ok('narrow (780px): table fits without clipping', narrow.fits, JSON.stringify(narrow.visible));
ok('narrow: Project and Activity are hidden, Cost stays', !narrow.visible.includes('Project') && !narrow.visible.includes('Activity') && narrow.visible.includes('Cost'));
ok('narrow: model name stays on one line', narrow.maxModelH < 40, `h=${narrow.maxModelH}`);
await page.setViewport({ width: 1400, height: 900 });
await page.click('#tabs button[data-tab="spend"]');
await page.waitForSelector('select#by');
const before = await page.$$eval('tbody tr', (r) => r.length);
await page.select('select#by', 'model');
await page.waitForFunction(() => document.querySelector('thead th')?.textContent === 'MODEL');
ok('spend: changing the grouping reloads the table', true, `before=${before}`);
await page.select('select#period', 'today');
await new Promise((r) => setTimeout(r, 800));
ok('spend: total row present', (await page.$$('tfoot tr')).length === 1 || (await page.$$('.empty')).length === 1);

await page.click('#tabs button[data-tab="help"]');
await page.waitForSelector('details');
const all = (await page.$$('details')).length;
await page.type('input[type="search"]', 'stalled');
await new Promise((r) => setTimeout(r, 200));
const filtered = (await page.$$('details')).length;
ok('help: search narrows the list and keeps the input focused', filtered >= 1 && filtered < all && (await page.evaluate(() => document.activeElement.type)) === 'search', `${all} -> ${filtered}`);
await page.type('input[type="search"]', 'zzzzqq');
await new Promise((r) => setTimeout(r, 200));
ok('help: no match says so', (await page.$eval('.empty', (e) => e.textContent)).includes('No term'));

// Hostile text from a transcript must stay text.
await page.evaluate(() => { document.getElementById('view').replaceChildren(); });
const injected = await page.evaluate(() => {
  const el = h('td', {}, '<img src=x onerror=window.__pwned=1>');
  document.body.append(el);
  return { html: el.innerHTML, pwned: window.__pwned === 1 };
});
ok('untrusted text is rendered as text, not markup', !injected.pwned && injected.html.includes('&lt;img'));

ok('no requests left the server', external.length === 0, external.join(','));
ok('no page errors, console errors or CSP violations', problems.length === 0, problems.join(' | '));
await browser.close();
