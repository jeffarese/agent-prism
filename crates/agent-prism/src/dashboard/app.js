// agent-prism dashboard. Vanilla JS, no dependencies. All text from the
// server goes into the DOM as text nodes, never as HTML.
'use strict';
(() => {
  const NS = 'http://www.w3.org/2000/svg';
  const $ = (q, r = document) => r.querySelector(q);

  // ---------- DOM helpers ----------
  function put(n, kids) {
    for (const k of kids.flat(Infinity)) {
      if (k == null || k === false) continue;
      n.append(k instanceof Node ? k : document.createTextNode(String(k)));
    }
    return n;
  }
  function attrs(n, a) {
    for (const [k, v] of Object.entries(a || {})) {
      if (v == null || v === false) continue;
      if (k.startsWith('on') && typeof v === 'function') n.addEventListener(k.slice(2), v);
      else if (k === 'text') n.textContent = v;
      // CSSOM, not the style attribute: the page's CSP has no 'unsafe-inline'.
      else if (k === 'style') n.style.cssText = v;
      else n.setAttribute(k, v === true ? '' : v);
    }
    return n;
  }
  const el = (t, a, ...k) => put(attrs(document.createElement(t), a), k);
  const sv = (t, a, ...k) => put(attrs(document.createElementNS(NS, t), a), k);

  const ICON = {
    alert: 'M8 2.2 14.6 13.6H1.4Z M8 6.4v3.2 M8 11.7v.05',
    swap: 'M2.5 5.5h10 M10 3l2.5 2.5L10 8 M13.5 10.5h-10 M6 8l-2.5 2.5L6 13',
    clock: 'M8 1.9a6.1 6.1 0 1 0 0 12.2 6.1 6.1 0 1 0 0-12.2 M8 4.6V8l2.4 1.5',
    play: 'M5.2 3.2v9.6L13 8Z',
    stop: 'M4.5 4.5h7v7h-7Z',
    check: 'M3.2 8.6 6.4 11.6 12.8 4.6',
    arrow: 'M2.5 8h11 M9.5 4l4 4-4 4',
    key: 'M10.5 1.9a3.6 3.6 0 1 0 0 7.2 3.6 3.6 0 1 0 0-7.2 M7.9 8.1 2 14 M4 12l1.6 1.6 M5.6 10.4l1.4 1.4',
    minus: 'M8 1.9a6.1 6.1 0 1 0 0 12.2 6.1 6.1 0 1 0 0-12.2 M5 8h6',
    dash: 'M3 8h10',
    list: 'M2.5 4h11 M2.5 8h11 M2.5 12h7',
    spark: 'M8 1.5v3 M8 11.5v3 M1.5 8h3 M11.5 8h3 M3.4 3.4l2 2 M10.6 10.6l2 2 M3.4 12.6l2-2 M10.6 5.4l2-2',
  };
  function icon(name, size = 14) {
    return sv('svg', { width: size, height: size, viewBox: '0 0 16 16', fill: 'none', stroke: 'currentColor',
      'stroke-width': 1.6, 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true' },
      sv('path', { d: ICON[name] }));
  }

  // ---------- State ----------
  const st = {
    range: '24h', tool: 'all', theme: 'auto',
    state: null, hist: null, act: null,
    hidden: new Set(), tables: new Set(), feedAll: false,
    sort: { k: 'start', dir: -1 }, runsAll: false,
    skew: 0, lastOk: 0, failed: false, slowBusy: false,
  };
  try {
    const s = JSON.parse(localStorage.getItem('agp-dashboard') || '{}');
    if (s.range === '24h' || s.range === '7d') st.range = s.range;
    if (['all', 'claude', 'codex'].includes(s.tool)) st.tool = s.tool;
    if (['auto', 'light', 'dark'].includes(s.theme)) st.theme = s.theme;
    if (Array.isArray(s.hidden)) st.hidden = new Set(s.hidden);
  } catch { /* storage unavailable: defaults */ }
  function save() {
    try {
      localStorage.setItem('agp-dashboard', JSON.stringify({ range: st.range, tool: st.tool, theme: st.theme, hidden: [...st.hidden] }));
    } catch { /* ignore */ }
  }
  const nowS = () => Date.now() / 1000 + st.skew;

  // ---------- Formatting ----------
  function compact(n) {
    n = Number(n) || 0;
    const u = [[1e12, 'T'], [1e9, 'B'], [1e6, 'M'], [1e3, 'K']];
    for (const [v, s] of u) if (Math.abs(n) >= v) {
      const x = n / v;
      return (x >= 100 ? Math.round(x) : x.toFixed(1).replace(/\.0$/, '')) + s;
    }
    return String(Math.round(n));
  }
  function dur(s) {
    s = Math.max(0, Math.round(s));
    const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600), m = Math.floor(s % 3600 / 60);
    if (d > 0) return h ? `${d}d ${h}h` : `${d}d`;
    if (h > 0) return m ? `${h}h ${m}m` : `${h}h`;
    if (m > 0) return `${m}m`;
    return s > 0 ? '<1m' : '0m';
  }
  function ago(ts) {
    const s = nowS() - ts;
    if (s < 10) return 'just now';
    if (s < 60) return `${Math.round(s)}s ago`;
    return `${dur(s)} ago`;
  }
  const pct = (u) => `${Math.round(u)}%`;
  const fTime = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
  const fDay = new Intl.DateTimeFormat(undefined, { weekday: 'short', day: 'numeric' });
  const fDayLong = new Intl.DateTimeFormat(undefined, { weekday: 'long', day: 'numeric', month: 'short' });
  const fWd = new Intl.DateTimeFormat(undefined, { weekday: 'short' });
  function sameDay(a, b) { return a.toDateString() === b.toDateString(); }
  function when(ts) {
    const d = new Date(ts * 1000), n = new Date(nowS() * 1000);
    return sameDay(d, n) ? fTime.format(d) : `${fDay.format(d)}, ${fTime.format(d)}`;
  }
  function dayLabel(ts) {
    const d = new Date(ts * 1000), n = new Date(nowS() * 1000);
    if (sameDay(d, n)) return 'Today';
    const y = new Date(n); y.setDate(n.getDate() - 1);
    if (sameDay(d, y)) return 'Yesterday';
    return fDayLong.format(d);
  }
  const MODE = {
    tui: ['Interactive', 'agp sessions with the dock'],
    exec: ['Scripted', 'agp -p and agp codex exec, replaced by the tool'],
    print: ['Supervised print', 'agp -p with --on-limit; agent-prism handles limits'],
  };
  const LIMIT = { '5h': '5-hour', '7d': 'weekly', scoped: 'model' };

  // ---------- Accounts ----------
  let accMap = new Map();
  function acct(id) { return accMap.get(id) || { id, label: id, slot: null, tool: null, unknown: true }; }
  function color(id) { const a = acct(id); return a.slot == null ? 'var(--other)' : `var(--s${a.slot + 1})`; }
  function toolOk(a) { return st.tool === 'all' || !a.tool || a.tool === st.tool; }
  function who(id) { return el('span', { class: 'who' }, el('span', { class: 'dot', style: `--acct:${color(id)}` }), acct(id).label); }
  function planLabel(a) {
    const tier = (a.tier || '').toLowerCase(), plan = (a.plan || '').toLowerCase();
    if (tier.includes('20x')) return 'Max 20x';
    if (tier.includes('5x')) return plan === 'team' ? 'Team' : 'Max 5x';
    if (!plan) return null;
    return plan.charAt(0).toUpperCase() + plan.slice(1);
  }

  // ---------- Tooltip ----------
  const tip = $('#tip');
  function placeTip(x, y) {
    const r = tip.getBoundingClientRect(), pad = 12;
    let left = x + 14, top = y + 14;
    if (left + r.width > innerWidth - pad) left = x - r.width - 14;
    if (top + r.height > innerHeight - pad) top = y - r.height - 14;
    tip.style.left = `${Math.max(pad, left)}px`;
    tip.style.top = `${Math.max(pad, top)}px`;
  }
  // A tip opened by keyboard focus stays when the page scrolls (focus
  // scrolls the element into view); a pointer tip goes.
  let tipByKey = false;
  function showTip(content, x, y, byKey = false) {
    tipByKey = byKey;
    tip.replaceChildren(...[content].flat(Infinity).filter((k) => k != null && k !== false));
    tip.hidden = false;
    placeTip(x, y);
  }
  function hideTip() { tip.hidden = true; }
  function bindTip(node, fn) {
    node.addEventListener('pointermove', (e) => showTip(fn(), e.clientX, e.clientY));
    node.addEventListener('pointerleave', hideTip);
    node.addEventListener('focus', () => {
      const r = node.getBoundingClientRect();
      showTip(fn(), r.left + r.width / 2, r.top + r.height / 2, true);
    });
    node.addEventListener('blur', hideTip);
  }
  const tipH = (t) => el('div', { class: 'tip-h' }, t);
  const tipRow = (k, value, label) => el('div', { class: 'tip-row' }, k, el('b', {}, value), el('span', {}, label));
  const lineKey = (c) => el('span', { class: 'key line', style: `--k:${c}` });
  const rectKey = (c) => el('span', { class: 'key rect', style: `--k:${c}` });

  // ---------- Fetch ----------
  async function getJSON(u) {
    const r = await fetch(u, { cache: 'no-store', headers: { Accept: 'application/json' } });
    if (!r.ok) throw new Error(`${u}: HTTP ${r.status}`);
    return r.json();
  }
  async function loadState() {
    try {
      const s = await getJSON('/api/state');
      st.skew = s.now - Date.now() / 1000;
      const first = !st.state;
      st.state = s;
      accMap = new Map(s.accounts.map((a) => [a.id, a]));
      st.lastOk = Date.now();
      st.failed = false;
      renderNow();
      if (first && st.act) renderSlow();
    } catch {
      st.failed = true;
    }
    renderLive();
  }
  async function loadSlow() {
    if (st.slowBusy) return;
    st.slowBusy = true;
    const busy = ['#history-card', '#timeline-card', '#dist', '#feed-card', '#runs-card'].map((q) => $(q));
    if (st.act) busy.forEach((n) => n.classList.add('loading'));
    try {
      const q = `range=${st.range}&tool=${st.tool}`;
      const [h, a] = await Promise.all([getJSON(`/api/history?${q}`), getJSON(`/api/activity?${q}`)]);
      st.hist = h;
      st.act = a;
      if (st.state) { renderSlow(); renderKpis(); }
    } catch {
      st.failed = true;
      renderLive();
    } finally {
      busy.forEach((n) => n.classList.remove('loading'));
      st.slowBusy = false;
    }
  }

  // ---------- Header ----------
  function renderLive() {
    const live = $('#live'), t = $('#live-text');
    live.classList.toggle('ok', !st.failed && st.lastOk > 0);
    live.classList.toggle('bad', st.failed);
    if (st.failed) t.textContent = st.lastOk ? 'Offline · retrying' : 'Cannot reach agp dashboard';
    else if (st.lastOk) {
      const s = Math.round((Date.now() - st.lastOk) / 1000);
      t.textContent = `Live · updated ${s < 2 ? 'now' : `${s}s ago`}`;
    }
  }
  function setPressed(group, attr, val) {
    group.querySelectorAll('button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset[attr] === val)));
  }
  function applyTheme() {
    if (st.theme === 'auto') delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = st.theme;
    setPressed($('#theme'), 'themeSet', st.theme);
  }
  $('#theme').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    st.theme = b.dataset.themeSet; save(); applyTheme();
  });
  $('#range').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b || b.dataset.range === st.range) return;
    st.range = b.dataset.range; save(); setPressed($('#range'), 'range', st.range); loadSlow();
  });
  $('#tool').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b || b.dataset.tool === st.tool) return;
    st.tool = b.dataset.tool; save(); setPressed($('#tool'), 'tool', st.tool); renderNow(); loadSlow();
  });

  // ---------- Now: KPIs, picks, accounts ----------
  function visibleAccounts() { return (st.state ? st.state.accounts : []).filter(toolOk); }
  function liveFor(id) { return (st.state ? st.state.live : []).filter((l) => l.profile === id); }

  function kpi(label, value, foot, extra, cls) {
    return el('div', { class: `kpi ${cls || ''}` }, el('div', { class: 'kpi-label' }, label),
      el('div', { class: 'kpi-value' }, value), foot != null ? el('div', { class: 'kpi-foot' }, foot) : null, extra);
  }
  function spark(values, w = 160, hgt = 26) {
    const max = Math.max(...values, 1);
    if (!values.length) return null;
    const bw = w / values.length;
    return sv('svg', { width: '100%', height: hgt, viewBox: `0 0 ${w} ${hgt}`, preserveAspectRatio: 'none', 'aria-hidden': 'true' },
      values.map((v, i) => {
        const bh = v > 0 ? Math.max(1.5, (v / max) * (hgt - 2)) : 0;
        return sv('rect', { x: i * bw + 0.5, y: hgt - bh, width: Math.max(1, bw - 1.5), height: bh, rx: 1, fill: i === values.length - 1 ? 'var(--seq)' : 'var(--axis)' });
      }));
  }
  function renderKpis() {
    const accs = visibleAccounts();
    const pool = accs.filter((a) => a.in_pool);
    const ready = pool.filter((a) => !a.limited && a.token.state !== 'missing');
    const live = (st.state ? st.state.live : []).filter((l) => toolOk(acct(l.profile)));
    const onAccts = new Set(live.map((l) => l.profile)).size;
    const a = st.act, t = a && a.totals;
    const rangeTxt = st.range === '24h' ? 'last 24 hours' : 'last 7 days';
    let readyFoot = 'can take a new session';
    if (pool.length && !ready.length) {
      const resets = pool.map((x) => x.limited && x.limited.until).filter(Boolean).sort();
      readyFoot = resets.length ? `all limited · first reset in ${dur(resets[0] - nowS())}` : 'all pool accounts are limited';
    }
    const kids = [
      kpi('Ready now', [String(ready.length), el('small', {}, ` of ${pool.length}`)], readyFoot, null, 'hero'),
      kpi('Live sessions', String(live.length), live.length ? `on ${onAccts} account${onAccts === 1 ? '' : 's'}` : 'none running'),
      kpi('Runs', t ? String(t.runs) : '—', t ? `${dur(t.secs)} active · ${rangeTxt}` : rangeTxt),
      kpi('Limit hits', t ? String(t.limits) : '—', t ? `${t.switches} switch${t.switches === 1 ? '' : 'es'} · ${t.waits} wait${t.waits === 1 ? '' : 's'}` : ''),
    ];
    if (t) {
      const series = a.token_series.buckets.map(([, by]) => Object.values(by).reduce((x, y) => x + y, 0));
      const tok = t.tokens, total = tok.input + tok.output + tok.cache_write + tok.cache_read;
      kids.push(kpi('Tokens', total ? compact(total) : '—',
        total ? `${compact(tok.output)} out · ${Math.round(100 * tok.cache_read / total)}% cache reads` : 'no transcript data yet',
        total ? spark(series) : null));
    } else kids.push(kpi('Tokens', '—', ''));
    $('#kpis').replaceChildren(...kids);
  }

  function renderPicks() {
    const s = st.state;
    const picks = s.picks.filter((p) => st.tool === 'all' || p.tool === st.tool)
      .filter((p) => s.accounts.some((a) => a.tool === p.tool && a.in_pool));
    $('#picks').replaceChildren(...picks.map((p) => {
      if (p.chosen) {
        return el('div', { class: 'pick' },
          el('div', { class: 'pick-ico', style: `color:${color(p.chosen)}` }, icon('arrow', 18)),
          el('div', { class: 'pick-main' },
            el('div', { class: 'pick-t' }, `Next ${p.name} session goes to`),
            el('div', { class: 'pick-who' }, el('span', { class: 'dot', style: `--acct:${color(p.chosen)}` }), p.chosen_label || p.chosen),
            el('div', { class: 'pick-why', title: p.reason }, p.reason)));
      }
      const er = p.earliest_reset;
      return el('div', { class: 'pick none' },
        el('div', { class: 'pick-ico', style: 'color:var(--crit-ink)' }, icon('alert', 18)),
        el('div', { class: 'pick-main' },
          el('div', { class: 'pick-t' }, `Next ${p.name} session`),
          el('div', { class: 'pick-who' }, 'No account available'),
          el('div', { class: 'pick-why' }, er ? [`${er.label} resets in `, el('span', { 'data-until': er.at }, dur(er.at - nowS()))] : p.reason)));
    }));
  }

  function meter(name, w, opts = {}) {
    if (!w) {
      return el('div', { class: 'meter-row' }, el('div', { class: 'meter-name' }, name),
        el('div', { class: 'meter none', 'aria-label': `${name}: no data` }), el('div', { class: 'meter-val inferred' }, '—'));
    }
    const u = Math.max(0, Math.min(100, w.util));
    const sev = u >= 85 ? 'crit' : u >= 60 ? 'warn' : 'good';
    const m = el('div', { class: `meter${opts.stale ? ' stale' : ''}`, style: `--fill:var(--${sev})`, role: 'meter',
      'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-valuenow': Math.round(u), 'aria-label': `${name} window` },
      el('div', { class: 'fill', style: `width:${u}%` }));
    const p = w.projected;
    if (p != null && p > u + 0.5) {
      const pv = Math.min(100, p);
      m.append(el('div', { class: 'ghost', style: `left:${u}%;width:${pv - u}%` }), el('div', { class: 'tick', style: `left:calc(${pv}% - 1px)` }));
    }
    if (opts.soft) m.append(el('div', { class: 'soft', style: `left:${opts.soft}%`, title: `New sessions avoid this account from ${opts.soft}%` }));
    const sub = el('div', { class: 'meter-sub' });
    if (w.resets_at) sub.append(el('span', {}, 'resets in ', el('span', { 'data-until': w.resets_at }, dur(w.resets_at - nowS()))));
    else if (w.inferred) sub.append(el('span', {}, 'window reset, next reset unknown'));
    if (p != null && p > u + 0.5) sub.append(el('span', { class: 'proj' }, '→ ', el('b', {}, pct(Math.min(100, p))), ' at reset'));
    return el('div', { class: 'meter-row' }, el('div', { class: 'meter-name' }, name), m,
      el('div', { class: `meter-val${w.inferred ? ' inferred' : ''}` }, (w.inferred ? '~' : '') + pct(u)), sub.childNodes.length ? sub : null);
  }

  function stateChip(a) {
    if (a.limited) {
      return el('span', { class: 'chip crit' }, icon('alert', 12), `Limited · ${LIMIT[a.limited.kind] || a.limited.kind}`);
    }
    if (a.next_pick) return el('span', { class: 'chip next' }, icon('arrow', 12), 'Next pick');
    if (a.token.state === 'missing') return el('span', { class: 'chip crit' }, icon('key', 12), 'Not logged in');
    if (a.token.state === 'unreadable') return el('span', { class: 'chip warn' }, icon('key', 12), 'Login unreadable');
    if (a.token.state === 'expired') return el('span', { class: 'chip warn' }, icon('key', 12), 'Token expired');
    if (!a.fetched_at) return el('span', { class: 'chip dim' }, icon('dash', 12), 'No usage yet');
    if (a.usage_status.state === 'stale') return el('span', { class: 'chip dim' }, icon('clock', 12), 'Stale');
    if (!a.in_pool) return el('span', { class: 'chip dim' }, icon('minus', 12), 'Out of pool');
    return el('span', { class: 'chip ok' }, icon('check', 12), 'Ready');
  }

  function accountCard(a) {
    const s = st.state.settings;
    const stale = a.usage_status.state !== 'fresh';
    const cls = ['card', 'acct', a.limited ? 'limited' : '', a.in_pool ? '' : 'out', a.next_pick ? 'next' : ''].join(' ');
    const who2 = [a.email || a.name, a.org].filter(Boolean).join(' · ');
    const plan = planLabel(a);
    const head = el('div', { class: 'acct-top' },
      el('span', { class: 'dot' }),
      el('div', { class: 'acct-id' },
        el('div', { class: 'acct-name' }, a.label, plan ? el('span', { class: 'plan' }, plan) : null),
        el('div', { class: 'acct-who', title: who2 }, who2 || a.id)),
      el('div', { class: 'acct-state' }, stateChip(a)));
    const body = [];
    if (a.limited) {
      body.push(el('div', { class: 'acct-note' }, `Hit the ${LIMIT[a.limited.kind] || a.limited.kind} limit`,
        a.limited.until ? [' · resets in ', el('b', { 'data-until': a.limited.until }, dur(a.limited.until - nowS()))] : '',
        '. New sessions skip it until then.'));
    } else if (a.token.state === 'expired' && a.fetched_at) {
      body.push(el('div', { class: 'acct-note' }, 'Login token expired ', el('span', { 'data-ago': a.token.since }, ago(a.token.since)),
        `. The numbers are from the last poll; ${a.tool === 'codex' ? 'Codex' : 'Claude'} refreshes the token the next time it runs on this account.`));
    } else if (!a.fetched_at) {
      const why = a.token.state === 'missing' ? `Log in with agp add ${a.id} to use this account.`
        : a.last_error ? `The last poll failed: ${a.last_error}.`
          : s.polling ? 'Usage appears after the first poll.' : 'Polling is off (AGP_NO_POLL).';
      body.push(el('div', { class: 'acct-note' }, why));
    }
    if (a.fetched_at) {
      const meters = el('div', { class: 'meters' },
        meter('5-hour', a.five_hour, { stale, soft: s.soft_5h }),
        meter('Weekly', a.seven_day, { stale }));
      body.push(meters);
      if (a.scoped && a.scoped.length) {
        body.push(el('div', { class: 'scoped' }, a.scoped.map((x) => el('span', {}, `${x.label} weekly `, el('b', {}, (x.inferred ? '~' : '') + pct(x.util))))));
      }
    }
    if (a.next_pick && a.reason) body.push(el('div', { class: 'acct-reason' }, a.reason));
    const live = liveFor(a.id);
    const foot = el('div', { class: 'acct-foot' });
    if (live.length) {
      const names = [...new Set(live.map((l) => l.project))].join(', ');
      foot.append(el('span', { class: 'live-pill', title: names }, `${live.length} live`), el('span', {}, names));
    } else if (a.last_used) {
      foot.append(el('span', {}, 'last session ', el('span', { 'data-ago': a.last_used }, ago(a.last_used))));
    } else foot.append(el('span', {}, 'no sessions yet'));
    if (a.fetched_at) foot.append(el('span', { class: 'sp' }, 'polled ', el('span', { 'data-ago': a.fetched_at }, ago(a.fetched_at))));
    return el('article', { class: cls, style: `--acct:${color(a.id)}`, 'aria-label': `${a.label} account` }, head, body, foot);
  }

  function renderAccounts() {
    const s = st.state;
    const root = $('#accounts');
    if (!s.accounts.length) {
      root.replaceChildren(el('div', { class: 'card' }, empty('No accounts yet', [
        el('p', {}, 'agent-prism has no accounts in its config. Run ', el('code', {}, 'agp'), ' to set it up, or ', el('code', {}, 'agp add'), ' to add one.')])));
      return;
    }
    const groups = [];
    for (const p of s.picks) {
      if (st.tool !== 'all' && p.tool !== st.tool) continue;
      const accs = s.accounts.filter((a) => a.tool === p.tool);
      if (!accs.length) continue;
      const inPool = accs.filter((a) => a.in_pool).length;
      groups.push(el('div', { class: 'group' },
        el('div', { class: 'group-head' }, el('h3', { class: 'group-h' }, `${p.name} accounts`),
          el('span', { class: 'sub' }, `${accs.length} account${accs.length === 1 ? '' : 's'} · ${inPool} in the pool · ${p.plan_name} plans`)),
        el('div', { class: 'cards' }, accs.map(accountCard))));
    }
    root.replaceChildren(...groups);
  }

  function renderNow() {
    const s = st.state;
    const tools = new Set(s.accounts.map((a) => a.tool));
    $('#tool').hidden = tools.size < 2;
    if (tools.size < 2 && st.tool !== 'all') { st.tool = 'all'; setPressed($('#tool'), 'tool', 'all'); }
    const set = s.settings;
    $('#policy').textContent = `Policy ${set.policy} · new sessions avoid 5h ≥ ${set.soft_5h}% · limited at ${set.hard}%${set.polling ? '' : ' · polling off'}`;
    renderKpis();
    renderPicks();
    renderAccounts();
    renderFoot();
  }
  function renderFoot() {
    const s = st.state;
    $('#foot').replaceChildren(
      el('span', {}, `agent-prism ${s.version}`),
      el('span', {}, 'Journal ', el('code', {}, s.journal.path)),
      el('span', {}, s.settings.polling ? `Usage refreshes every ${dur(s.settings.poll_idle_secs)} while this page's server runs` : 'Usage polling is off'),
      el('span', {}, 'Only 127.0.0.1 can reach this page'));
  }

  // ---------- Shared chart bits ----------
  function empty(title, paras, ico = 'list') {
    return el('div', { class: 'empty' }, el('div', { class: 'ico' }, icon(ico, 20)), el('div', {}, el('h4', {}, title), paras));
  }
  function width(node) { return Math.max(280, Math.floor(node.clientWidth)); }
  function timeTicks(since, now, w) {
    const out = [];
    const span = now - since;
    if (span <= 2 * 86400) {
      const step = w < 520 ? 6 : 3;
      const d = new Date(since * 1000); d.setMinutes(0, 0, 0); d.setHours(d.getHours() + 1);
      for (; d.getTime() / 1000 <= now; d.setHours(d.getHours() + 1)) {
        if (d.getHours() % step === 0) out.push({ t: d.getTime() / 1000, label: d.getHours() === 0 ? fWd.format(d) : fTime.format(d) });
      }
    } else {
      const d = new Date(since * 1000); d.setHours(24, 0, 0, 0);
      for (; d.getTime() / 1000 <= now; d.setDate(d.getDate() + 1)) out.push({ t: d.getTime() / 1000, label: w < 520 ? fWd.format(d) : fDay.format(d) });
    }
    return out;
  }
  function tableToggle(id, onChange) {
    const on = st.tables.has(id);
    return el('button', { type: 'button', class: 'link-btn', 'aria-pressed': String(on), onclick: () => {
      if (st.tables.has(id)) st.tables.delete(id); else st.tables.add(id);
      onChange();
    } }, on ? 'Chart' : 'Table');
  }
  function table(cols, rows) {
    return el('div', { class: 'table-wrap' }, el('table', {},
      el('thead', {}, el('tr', {}, cols.map((c) => el('th', { class: c.num ? 'num' : '', scope: 'col' }, c.h)))),
      el('tbody', {}, rows.map((r) => el('tr', {}, cols.map((c, i) => el('td', { class: c.num ? 'num' : '' }, r[i])))))));
  }
  function legend(ids, kind, onToggle) {
    return el('div', { class: 'legend', role: 'group', 'aria-label': 'Accounts' }, ids.map((id) => {
      const on = !st.hidden.has(id);
      return el('button', { type: 'button', 'aria-pressed': String(on), title: on ? 'Hide (⌥-click: show only this)' : 'Show', 'data-acct': id,
        onclick: (e) => {
          if (e.altKey || e.metaKey) { // isolate
            const others = ids.filter((x) => x !== id);
            const isolated = others.every((x) => st.hidden.has(x)) && !st.hidden.has(id);
            others.forEach((x) => (isolated ? st.hidden.delete(x) : st.hidden.add(x)));
            st.hidden.delete(id);
          } else if (st.hidden.has(id)) st.hidden.delete(id); else st.hidden.add(id);
          save(); onToggle();
        } },
      el('span', { class: `key ${kind}`, style: `--k:${color(id)}` }), acct(id).label);
    }));
  }
  // Hovering or focusing a legend entry fades the other accounts' marks.
  function emphasize(lg, root) {
    const set = (id) => root.querySelectorAll('[data-id]').forEach((m) => m.classList.toggle('dim', id != null && m.dataset.id !== id));
    lg.querySelectorAll('button').forEach((b) => {
      const id = b.dataset.acct;
      b.addEventListener('pointerenter', () => set(id));
      b.addEventListener('focus', () => set(id));
      b.addEventListener('pointerleave', () => set(null));
      b.addEventListener('blur', () => set(null));
    });
  }

  // ---------- History ----------
  function renderHistory() {
    const card = $('#history-card');
    const h = st.hist, a = st.act;
    const series = h.series.filter((s) => accMap.has(s.id) && toolOk(acct(s.id)) && s.points.length);
    const head = el('div', { class: 'card-h' }, el('h3', {}, 'Limit windows over time'),
      el('span', { class: 'sub' }, st.range === '24h' ? 'last 24 hours · 5-minute peaks' : 'last 7 days · 30-minute peaks'),
      el('div', { class: 'tools' }, series.length ? tableToggle('history', renderHistory) : null));
    if (!series.length) {
      card.replaceChildren(head, empty('No usage samples in this range', [
        el('p', {}, 'agent-prism records a sample each time it polls an account: while a session runs, and while this dashboard is open.')], 'clock'));
      return;
    }
    const ids = series.map((s) => s.id);
    if (st.tables.has('history')) {
      const rows = series.map((s) => {
        const p5 = s.points.filter((p) => p[1] != null), p7 = s.points.filter((p) => p[2] != null);
        const last = (arr, k) => (arr.length ? pct(arr[arr.length - 1][k]) : '—');
        const peak = (arr, k) => (arr.length ? pct(Math.max(...arr.map((p) => p[k]))) : '—');
        return [who(s.id), last(p5, 1), peak(p5, 1), last(p7, 2), peak(p7, 2), String(s.points.length)];
      });
      card.replaceChildren(head, table([{ h: 'Account' }, { h: '5h latest', num: 1 }, { h: '5h peak', num: 1 }, { h: 'Weekly latest', num: 1 }, { h: 'Weekly peak', num: 1 }, { h: 'Buckets', num: 1 }], rows));
      return;
    }
    const limits = (a ? a.events : []).filter((e) => e.ev === 'limit');
    const lg = legend(ids, 'line', renderHistory);
    lg.append(el('span', { class: 'sep' }),
      el('span', { class: 'static' }, sv('svg', { class: 'key mk', viewBox: '0 0 10 10', 'aria-hidden': 'true' }, sv('path', { d: 'M1 1.5h8L5 8.5Z', fill: 'var(--crit)' })), 'limit hit'),
      el('span', { class: 'static' }, el('span', { class: 'key line', style: '--k:var(--ink2);height:1px' }), `limit (${st.state.settings.hard}%)`));
    const box = el('div', { class: 'chart' });
    card.replaceChildren(head, lg, box);
    drawHistory(box, series, limits, h);
    emphasize(lg, box);
  }

  function drawHistory(box, series, limits, h) {
    const W = width(box);
    const has7 = series.some((s) => s.points.some((p) => p[2] != null));
    const ML = 34, MR = 10, PH5 = 170, PH7 = 120, GAP = 34, TOP = 18, XB = 24;
    const panels = [{ k: 1, name: '5-hour window', y0: TOP, ph: PH5 }];
    if (has7) panels.push({ k: 2, name: 'Weekly window', y0: TOP + PH5 + GAP, ph: PH7 });
    const H = panels[panels.length - 1].y0 + panels[panels.length - 1].ph + XB;
    const x = (t) => ML + ((t - h.since) / (h.now - h.since)) * (W - ML - MR);
    const svg = sv('svg', { viewBox: `0 0 ${W} ${H}`, height: H, tabindex: 0, role: 'img',
      'aria-label': `Utilization of ${series.length} accounts over the ${st.range === '24h' ? 'last 24 hours' : 'last 7 days'}. Use the arrow keys to read values; the table view lists them.` });
    const gap = Math.max(3 * h.bucket_secs, 1200);
    const vis = series.filter((s) => !st.hidden.has(s.id));
    const ticks = timeTicks(h.since, h.now, W);
    const hard = st.state.settings.hard;
    for (const p of panels) {
      const y = (u) => p.y0 + p.ph - (Math.max(0, Math.min(100, u)) / 100) * p.ph;
      p.y = y;
      svg.append(sv('text', { x: ML, y: p.y0 - 6, class: 'lbl-strong' }, p.name));
      for (const g of [0, 25, 50, 75, 100]) {
        svg.append(sv('line', { x1: ML, x2: W - MR, y1: y(g), y2: y(g), class: g === 0 ? 'base' : 'grid' }),
          sv('text', { x: ML - 6, y: y(g) + 3.5, 'text-anchor': 'end' }, `${g}%`));
      }
      for (const tk of ticks) svg.append(sv('line', { x1: x(tk.t), x2: x(tk.t), y1: p.y0, y2: p.y0 + p.ph, class: 'grid', opacity: 0.6 }));
      svg.append(sv('line', { x1: ML, x2: W - MR, y1: y(hard), y2: y(hard), stroke: 'var(--ink2)', 'stroke-width': 1, opacity: 0.55 }));
      for (const s of vis) {
        const c = color(s.id);
        let d = '', prev = null, run = [];
        const flush = () => {
          if (run.length === 1) svg.append(sv('circle', { cx: run[0][0], cy: run[0][1], r: 2.5, fill: c, 'data-id': s.id }));
          run = [];
        };
        for (const pt of s.points) {
          const v = pt[p.k];
          if (v == null) { prev = null; flush(); continue; }
          const X = x(pt[0]), Y = y(v);
          if (prev == null || pt[0] - prev > gap) { flush(); d += `M${X.toFixed(1)},${Y.toFixed(1)}`; } else d += `L${X.toFixed(1)},${Y.toFixed(1)}`;
          run.push([X, Y]);
          prev = pt[0];
        }
        flush();
        if (d) svg.append(sv('path', { d, class: 'line', stroke: c, 'data-id': s.id }));
      }
      // Limit hits of this window, on the account's line.
      for (const e of limits) {
        const isWeekly = e.limit === '7d' || e.limit === 'scoped';
        if ((p.k === 1) === isWeekly || st.hidden.has(e.profile) || !accMap.has(e.profile) || !toolOk(acct(e.profile))) continue;
        const X = x(e.ts), Y = y(hard) - 3;
        const m = sv('path', { d: `M${X - 6},${Y - 9}h12l-6,9Z`, fill: 'var(--crit)', stroke: 'var(--surface)', 'stroke-width': 2, class: 'mark', tabindex: 0,
          'aria-label': `${acct(e.profile).label} hit the ${LIMIT[e.limit] || ''} limit at ${when(e.ts)}` });
        bindTip(m, () => [tipH(when(e.ts)), el('div', {}, who(e.profile), ` hit the ${LIMIT[e.limit] || 'usage'} limit`), el('div', { class: 'tip-note' }, e.project)]);
        svg.append(m);
      }
    }
    const last = panels[panels.length - 1];
    for (const tk of ticks) svg.append(sv('text', { x: x(tk.t), y: last.y0 + last.ph + 16, 'text-anchor': 'middle' }, tk.label));

    // Crosshair: snaps to the nearest bucket; one readout for every account.
    const cross = sv('g', { visibility: 'hidden', 'pointer-events': 'none' });
    const xl = sv('line', { class: 'xhair', y1: TOP - 4, y2: last.y0 + last.ph });
    cross.append(xl);
    const dots = sv('g');
    cross.append(dots);
    svg.append(cross);
    const times = [...new Set(vis.flatMap((s) => s.points.map((p) => p[0])))].sort((a1, b1) => a1 - b1);
    let cur = -1;
    function valueAt(s, t, k) {
      let best = null;
      for (const p of s.points) if (p[k] != null && Math.abs(p[0] - t) <= h.bucket_secs * 1.5 && (!best || Math.abs(p[0] - t) < Math.abs(best[0] - t))) best = p;
      return best ? best[k] : null;
    }
    function show(i, cx, cy) {
      if (i < 0 || i >= times.length) return;
      cur = i;
      const t = times[i], X = x(t);
      xl.setAttribute('x1', X); xl.setAttribute('x2', X);
      dots.replaceChildren();
      const rows = [];
      for (const s of vis) {
        const v5 = valueAt(s, t, 1), v7 = valueAt(s, t, 2);
        if (v5 == null && v7 == null) continue;
        for (const p of panels) {
          const v = p.k === 1 ? v5 : v7;
          if (v != null) dots.append(sv('circle', { cx: X, cy: p.y(v), r: 4, fill: color(s.id), stroke: 'var(--surface)', 'stroke-width': 2 }));
        }
        rows.push({ id: s.id, v5, v7 });
      }
      rows.sort((r1, r2) => (r2.v5 ?? -1) - (r1.v5 ?? -1));
      cross.setAttribute('visibility', 'visible');
      const content = [tipH(when(t)), rows.length ? rows.map((r) => el('div', { class: 'tip-row' }, lineKey(color(r.id)),
        el('b', {}, r.v5 == null ? '—' : pct(r.v5)), has7 ? el('b', {}, r.v7 == null ? '—' : pct(r.v7)) : null, el('span', {}, acct(r.id).label)))
        : el('div', { class: 'tip-note' }, 'No samples here')];
      if (rows.length && has7) content.splice(1, 0, el('div', { class: 'tip-note', style: 'margin:-2px 0 3px' }, '5h · weekly'));
      const byKey = cx == null;
      if (byKey) { const r = svg.getBoundingClientRect(); cx = r.left + (X / W) * r.width; cy = r.top + 30; }
      showTip(content, cx, cy, byKey);
    }
    function nearest(t) {
      let lo = 0, hi = times.length - 1;
      while (lo < hi) { const m = (lo + hi) >> 1; if (times[m] < t) lo = m + 1; else hi = m; }
      if (lo > 0 && Math.abs(times[lo - 1] - t) < Math.abs(times[lo] - t)) lo -= 1;
      return lo;
    }
    const hit = sv('rect', { x: ML, y: TOP - 4, width: W - ML - MR, height: last.y0 + last.ph - TOP + 4, class: 'hit' });
    svg.insertBefore(hit, cross);
    hit.addEventListener('pointermove', (e) => {
      const r = svg.getBoundingClientRect();
      const px = ((e.clientX - r.left) / r.width) * W;
      const t = h.since + ((px - ML) / (W - ML - MR)) * (h.now - h.since);
      if (times.length) show(nearest(t), e.clientX, e.clientY);
    });
    const off = () => { cross.setAttribute('visibility', 'hidden'); hideTip(); };
    hit.addEventListener('pointerleave', off);
    svg.addEventListener('keydown', (e) => {
      if (!times.length) return;
      if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
        e.preventDefault();
        const step = e.shiftKey ? 12 : 1;
        show(cur < 0 ? times.length - 1 : Math.max(0, Math.min(times.length - 1, cur + (e.key === 'ArrowRight' ? step : -step))));
      } else if (e.key === 'Home') { e.preventDefault(); show(0); } else if (e.key === 'End') { e.preventDefault(); show(times.length - 1); } else if (e.key === 'Escape') off();
    });
    svg.addEventListener('blur', off);
    box.append(svg);
  }

  // ---------- Account timeline ----------
  // `brief`: a one-line note for the sections after the timeline, which
  // already explains it.
  function journalEmpty(brief) {
    const s = st.state;
    if (brief) {
      return el('p', { class: 'sec-sub', style: 'padding:6px 2px' },
        s.journal.exists ? `No runs in the ${st.range === '24h' ? 'last 24 hours' : 'last 7 days'}.` : 'No runs recorded yet; see the account timeline above.');
    }
    if (!s.journal.exists) {
      return empty('No runs recorded yet', [
        el('p', {}, 'agent-prism writes one line per run event (start, limit, switch, wait, end) to ', el('code', {}, s.journal.path), '.'),
        el('p', {}, 'Start a session with ', el('code', {}, 'agp'), ', ', el('code', {}, 'agp codex'), ' or ', el('code', {}, 'agp -p "…"'), ' and it shows up here within seconds.')], 'play');
    }
    return empty(`No runs in the ${st.range === '24h' ? 'last 24 hours' : 'last 7 days'}`, [
      el('p', {}, st.range === '24h' ? 'Switch the range to the last 7 days to see older runs.' : 'Runs appear here as soon as a session starts.')], 'play');
  }
  function runTip(r) {
    const path = [];
    for (const sg of r.segments) if (!sg.waiting && (!path.length || path[path.length - 1] !== sg.profile)) path.push(sg.profile);
    const rows = [tipH(`${when(r.start)} – ${r.live ? 'now' : when(r.end)}`),
      el('div', { style: 'font-weight:600;margin-bottom:3px' }, r.project, el('span', { style: 'color:var(--ink2);font-weight:400' }, ` · ${(MODE[r.mode] || [r.mode])[0]}${r.tool === 'codex' ? ' · Codex' : ''}`)),
      el('div', { class: 'path' }, path.map((p, i) => [i ? el('span', { class: 'arrow' }, '→') : null, who(p)]))];
    const len = r.end - r.start;
    const facts = [r.live ? `running ${dur(len)}` : r.ended || len > 0 ? dur(len) : 'no end time'];
    if (r.tokens && (r.tokens.input + r.tokens.output + r.tokens.cache_read + r.tokens.cache_write) > 0) {
      facts.push(`${compact(r.tokens.input + r.tokens.output + r.tokens.cache_read + r.tokens.cache_write)} tokens`);
    }
    const lim = r.marks.filter((m) => m.ev === 'limit').length;
    if (lim) facts.push(`${lim} limit hit${lim > 1 ? 's' : ''}`);
    if (r.exit != null) facts.push(`exit ${r.exit}`);
    rows.push(el('div', { class: 'tip-note' }, facts.join(' · ')));
    return rows;
  }
  function renderTimeline() {
    const card = $('#timeline-card');
    const a = st.act;
    const runs = a.runs;
    const head = el('div', { class: 'card-h' }, el('h3', {}, 'Runs per account'),
      el('span', { class: 'sub' }, `${runs.length} run${runs.length === 1 ? '' : 's'}`),
      el('div', { class: 'tools' }, runs.length ? tableToggle('timeline', renderTimeline) : null));
    if (!runs.length) { card.replaceChildren(head, journalEmpty()); return; }
    if (st.tables.has('timeline')) {
      const rows = a.by_account.map((s) => [who(s.key), String(s.runs), dur(s.secs), compact(s.tokens.input + s.tokens.output + s.tokens.cache_read + s.tokens.cache_write)]);
      card.replaceChildren(head, table([{ h: 'Account' }, { h: 'Runs', num: 1 }, { h: 'Active', num: 1 }, { h: 'Tokens', num: 1 }], rows));
      return;
    }
    const box = el('div', { class: 'chart' });
    card.replaceChildren(head, box);
    const W = width(box);
    const LW = W < 520 ? 76 : 120, MR = 10, TOP = 6, XB = 22, BAR = 12, PITCH = 16, PAD = 8;
    const since = a.since, now = a.now;
    const x = (t) => LW + ((Math.max(since, Math.min(now, t)) - since) / (now - since)) * (W - LW - MR);
    // Lanes: the accounts in config order, then any others the journal names.
    const laneIds = visibleAccounts().map((x1) => x1.id);
    for (const r of runs) for (const sg of r.segments) if (!laneIds.includes(sg.profile)) laneIds.push(sg.profile);
    const lanes = new Map(laneIds.map((id) => [id, { id, rows: [], items: [] }]));
    // Pack each lane's segments into rows so parallel runs don't overlap.
    const segs = [];
    for (const r of runs) r.segments.forEach((sg, i) => segs.push({ r, sg, i }));
    segs.sort((s1, s2) => s1.sg.from - s2.sg.from);
    for (const it of segs) {
      const ln = lanes.get(it.sg.profile); if (!ln) continue;
      const x0 = x(it.sg.from), x1 = Math.max(x0 + 4, x(it.sg.to));
      let row = ln.rows.findIndex((end) => end + 2 <= x0);
      if (row < 0) { row = ln.rows.length; ln.rows.push(0); }
      ln.rows[row] = x1;
      ln.items.push({ ...it, x0, x1, row });
    }
    let y = TOP;
    for (const ln of lanes.values()) {
      ln.y = y;
      ln.h = PAD * 2 + Math.max(1, ln.rows.length) * PITCH - (PITCH - BAR);
      y += ln.h;
    }
    const H = y + XB;
    const svg = sv('svg', { viewBox: `0 0 ${W} ${H}`, height: H, role: 'group', 'aria-label': 'Runs per account over time' });
    const ticks = timeTicks(since, now, W);
    for (const tk of ticks) {
      svg.append(sv('line', { x1: x(tk.t), x2: x(tk.t), y1: TOP, y2: y, class: 'grid' }),
        sv('text', { x: x(tk.t), y: y + 15, 'text-anchor': 'middle' }, tk.label));
    }
    let zebra = false;
    for (const ln of lanes.values()) {
      if (zebra) svg.append(sv('rect', { x: 0, y: ln.y, width: W, height: ln.h, fill: 'var(--wash)' }));
      zebra = !zebra;
      const lab = acct(ln.id).label;
      const cy = ln.y + ln.h / 2;
      svg.append(sv('circle', { cx: 6, cy, r: 4, fill: color(ln.id) }),
        sv('text', { x: 16, y: cy + 4, class: 'lbl' }, lab.length > (LW < 100 ? 7 : 13) ? `${lab.slice(0, LW < 100 ? 6 : 12)}…` : lab));
      if (!ln.items.length) svg.append(sv('text', { x: LW + 6, y: cy + 4, opacity: 0.8 }, 'idle'));
    }
    svg.append(sv('line', { x1: LW, x2: LW, y1: TOP, y2: y, class: 'base' }), sv('line', { x1: LW, x2: W - MR, y1: y, y2: y, class: 'base' }));
    const barY = (ln, row) => ln.y + PAD + row * PITCH;
    const pos = new Map();
    for (const ln of lanes.values()) {
      for (const it of ln.items) {
        const by = barY(ln, it.row);
        pos.set(`${it.r.id}:${it.i}`, { x0: it.x0, x1: it.x1, y: by + BAR / 2 });
        const c = color(ln.id);
        const rect = sv('rect', { x: it.x0, y: by, width: it.x1 - it.x0, height: BAR, rx: 3,
          fill: c, 'fill-opacity': it.sg.waiting ? 0.28 : 1, stroke: it.sg.waiting ? c : 'none', 'stroke-width': 1,
          class: 'mark', tabindex: it.i === 0 ? 0 : -1,
          'aria-label': `${it.r.project} on ${acct(ln.id).label}, ${when(it.sg.from)} for ${dur(it.sg.to - it.sg.from)}${it.sg.waiting ? ', waiting for reset' : ''}` });
        bindTip(rect, () => (it.sg.waiting ? [tipH(`${when(it.sg.from)} – ${when(it.sg.to)}`), el('div', {}, who(ln.id), ' waiting for its reset'), el('div', { class: 'tip-note' }, it.r.project)] : runTip(it.r)));
        svg.append(rect);
        if (it.r.live && it.i === it.r.segments.length - 1) {
          svg.append(sv('circle', { cx: it.x1, cy: by + BAR / 2, r: 4, fill: 'var(--good)', stroke: 'var(--surface)', 'stroke-width': 2 }));
        }
      }
    }
    // Switch connectors and limit marks.
    for (const r of runs) {
      for (let i = 1; i < r.segments.length; i++) {
        const p0 = pos.get(`${r.id}:${i - 1}`), p1 = pos.get(`${r.id}:${i}`);
        if (!p0 || !p1 || r.segments[i - 1].profile === r.segments[i].profile) continue;
        const X = p1.x0;
        svg.append(sv('path', { d: `M${p0.x1},${p0.y}H${X}V${p1.y}`, fill: 'none', stroke: 'var(--ink2)', 'stroke-width': 1.25, 'stroke-linejoin': 'round', opacity: 0.8 }),
          sv('circle', { cx: X, cy: p1.y, r: 2.5, fill: 'var(--ink2)' }));
      }
      for (const m of r.marks) {
        if (m.ev !== 'limit' || m.ts < since) continue;
        const ln = lanes.get(m.profile); if (!ln) continue;
        const X = x(m.ts), Y = ln.y + 2;
        const mk = sv('path', { d: `M${X - 5},${Y}h10l-5,7.5Z`, fill: 'var(--crit)', stroke: 'var(--surface)', 'stroke-width': 1.5, class: 'mark', tabindex: -1 });
        bindTip(mk, () => [tipH(when(m.ts)), el('div', {}, who(m.profile), ` hit the ${LIMIT[m.limit] || 'usage'} limit`), el('div', { class: 'tip-note' }, r.project)]);
        svg.append(mk);
      }
    }
    svg.append(sv('line', { x1: x(now), x2: x(now), y1: TOP, y2: y, stroke: 'var(--ink2)', 'stroke-width': 1, opacity: 0.5 }));
    box.append(svg);
    // Arrow keys move between runs.
    svg.addEventListener('keydown', (e) => {
      if (!['ArrowRight', 'ArrowLeft', 'ArrowDown', 'ArrowUp'].includes(e.key)) return;
      const marks = [...svg.querySelectorAll('rect.mark')];
      const i = marks.indexOf(document.activeElement);
      if (i < 0) return;
      e.preventDefault();
      const next = marks[(i + (e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : -1) + marks.length) % marks.length];
      next.focus();
    });
  }

  // ---------- Distribution ----------
  const tokTotal = (t) => (t ? t.input + t.output + t.cache_write + t.cache_read : 0);
  function tokBreakdown(t) {
    return el('div', { class: 'tip-note' }, `in ${compact(t.input)} · out ${compact(t.output)} · cache write ${compact(t.cache_write)} · cache read ${compact(t.cache_read)}`);
  }
  function bars(items) {
    const max = Math.max(...items.map((i) => i.v), 1);
    return el('div', { class: 'bars' }, items.map((i) => {
      const row = el('div', { class: 'bar-row', tabindex: 0, 'aria-label': `${i.label}: ${i.aria}` },
        el('div', { class: 'bar-name', title: i.label }, i.key ? el('span', { class: 'dot', style: `--acct:${i.key}` }) : null, el('span', {}, i.label)),
        el('div', { class: 'bar-track' }, el('div', { class: 'bar-fill', style: `width:calc(${(i.v / max) * 100}% - ${(i.v / max) * 124}px);${i.key ? `--k:${i.key}` : ''}` }), el('div', { class: 'bar-val' }, i.val)));
      if (i.tip) bindTip(row, i.tip);
      return row;
    }));
  }
  function distCard(title, sub, body, cls) {
    return el('div', { class: `card ${cls || ''}` }, el('div', { class: 'card-h' }, el('h3', {}, title), sub ? el('span', { class: 'sub' }, sub) : null), body);
  }
  function stackBar(items, total) {
    return el('div', {},
      el('div', { class: 'stack', role: 'img', 'aria-label': items.map((i) => `${i.label} ${Math.round((100 * i.v) / total)}%`).join(', ') },
        items.filter((i) => i.v > 0).map((i) => {
          const d = el('div', { style: `flex:${i.v};background:${i.c}` });
          bindTip(d, () => [tipH(i.label), el('div', {}, el('b', {}, `${Math.round((100 * i.v) / total)}%`), ` · ${i.val}`)]);
          return d;
        })),
      el('div', { class: 'stack-legend' }, items.map((i) => el('div', { class: 'row', title: i.desc || '' }, rectKey(i.c),
        el('span', {}, i.label), el('span', { class: 'v' }, el('b', {}, `${total ? Math.round((100 * i.v) / total) : 0}%`), ` · ${i.val}`)))));
  }
  const NEUTRAL = ['var(--ink2)', 'var(--muted)', 'var(--axis)'];
  function renderDist() {
    const root = $('#dist');
    const a = st.act;
    if (!a.runs.length) { root.replaceChildren(el('div', { class: 'card', style: 'grid-column:1/-1' }, journalEmpty(true))); return; }
    const t = a.totals;
    const hasTok = tokTotal(t.tokens) > 0;
    const accItems = a.by_account.filter((s) => s.secs > 0 || s.runs > 0).map((s) => ({
      label: acct(s.key).label, key: color(s.key), v: s.secs, aria: `${dur(s.secs)}, ${s.runs} runs`,
      val: [el('b', {}, dur(s.secs)), ` · ${s.runs} run${s.runs === 1 ? '' : 's'}`],
      tip: () => [tipH(acct(s.key).label), el('div', {}, el('b', {}, dur(s.secs)), ` active in ${s.runs} run${s.runs === 1 ? '' : 's'}`), tokTotal(s.tokens) ? tokBreakdown(s.tokens) : null],
    }));
    const cards = [distCard('Run time by account', `${dur(t.secs)} in total`, accItems.length ? bars(accItems) : el('p', { class: 'sec-sub' }, 'No active time in this range.'))];

    const tokItems = a.by_account.filter((s) => tokTotal(s.tokens) > 0).sort((s1, s2) => tokTotal(s2.tokens) - tokTotal(s1.tokens)).map((s) => ({
      label: acct(s.key).label, key: color(s.key), v: tokTotal(s.tokens), aria: `${compact(tokTotal(s.tokens))} tokens`,
      val: [el('b', {}, compact(tokTotal(s.tokens))), ` · ${compact(s.tokens.output)} out`],
      tip: () => [tipH(acct(s.key).label), el('div', {}, el('b', {}, compact(tokTotal(s.tokens))), ' tokens'), tokBreakdown(s.tokens)],
    }));
    cards.push(distCard('Tokens by account', hasTok ? `${compact(tokTotal(t.tokens))} in ${t.runs_with_tokens} run${t.runs_with_tokens === 1 ? '' : 's'}` : null,
      tokItems.length ? bars(tokItems) : el('div', { class: 'acct-note' }, 'No token data for these runs. Tokens come from the Claude transcripts and Codex rollout files of the sessions in the journal.')));

    const modes = ['tui', 'exec', 'print'].map((k, i) => {
      const s = a.by_mode.find((x1) => x1.key === k) || { runs: 0, secs: 0 };
      return { label: MODE[k][0], desc: MODE[k][1], c: NEUTRAL[i], v: s.runs, val: `${s.runs} run${s.runs === 1 ? '' : 's'}${s.secs ? `, ${dur(s.secs)}` : ''}` };
    });
    const modeBody = [stackBar(modes, t.runs)];
    if (a.by_tool.length > 1) {
      const tools = a.by_tool.map((s, i) => ({ label: s.key === 'codex' ? 'Codex' : 'Claude', c: NEUTRAL[i % 3], v: s.runs, val: `${s.runs} runs, ${dur(s.secs)}` }));
      modeBody.push(el('div', { class: 'sub-h' }, 'By tool'), stackBar(tools, t.runs));
    }
    cards.push(distCard('Interactive vs scripted', `${t.runs} run${t.runs === 1 ? '' : 's'}`, modeBody));

    // Tokens over time, stacked by account.
    const tsBody = el('div', { class: 'chart' });
    cards.push(distCard('Tokens over time', st.range === '24h' ? 'per hour' : 'per 6 hours', hasTok ? tsBody : el('div', { class: 'acct-note' }, 'No token data in this range.'), 'wide'));

    const modelItems = a.by_model.filter((s) => tokTotal(s.tokens) > 0).sort((s1, s2) => tokTotal(s2.tokens) - tokTotal(s1.tokens)).slice(0, 8).map((s) => ({
      label: s.key, v: tokTotal(s.tokens), aria: `${compact(tokTotal(s.tokens))} tokens`,
      val: [el('b', {}, compact(tokTotal(s.tokens))), ` · ${compact(s.tokens.output)} out`],
      tip: () => [tipH(s.key), el('div', {}, el('b', {}, compact(tokTotal(s.tokens))), ' tokens'), tokBreakdown(s.tokens)],
    }));
    cards.push(distCard('Tokens by model', null, modelItems.length ? bars(modelItems) : el('div', { class: 'acct-note' }, 'No token data in this range.')));

    const heatBody = el('div', { class: 'chart' });
    cards.push(distCard('When agents run', 'active time by weekday and hour, local time', heatBody, 'wide'));

    const projItems = a.by_project.map((s) => ({
      label: s.key, v: s.secs || s.runs, aria: `${dur(s.secs)}, ${s.runs} runs`,
      val: [el('b', {}, dur(s.secs)), ` · ${s.runs} run${s.runs === 1 ? '' : 's'}`],
      tip: () => [tipH(s.key), el('div', {}, el('b', {}, dur(s.secs)), ` active in ${s.runs} run${s.runs === 1 ? '' : 's'}`), tokTotal(s.tokens) ? tokBreakdown(s.tokens) : null],
    }));
    cards.push(distCard('Projects', `${a.by_project.length} work dir${a.by_project.length === 1 ? '' : 's'}`, bars(projItems)));
    root.replaceChildren(...cards);
    if (hasTok) drawTokenSeries(tsBody, a);
    drawHeat(heatBody, a.heat);
  }

  function drawTokenSeries(box, a) {
    const W = width(box), H = 190, ML = 40, MR = 6, TOP = 8, XB = 22;
    const bk = a.token_series.buckets, bs = a.token_series.bucket_secs;
    const ids = visibleAccounts().map((x1) => x1.id);
    for (const [, by] of bk) for (const id of Object.keys(by)) if (!ids.includes(id)) ids.push(id);
    const totals = bk.map(([, by]) => Object.values(by).reduce((s, v) => s + v, 0));
    const max = Math.max(...totals, 1);
    const step = niceStep(max / 4);
    const top = Math.ceil(max / step) * step;
    const ph = H - TOP - XB;
    const y = (v) => TOP + ph - (v / top) * ph;
    const x = (t) => ML + ((t - a.since) / (a.now - a.since)) * (W - ML - MR);
    const svg = sv('svg', { viewBox: `0 0 ${W} ${H}`, height: H, role: 'group', 'aria-label': 'Tokens over time by account' });
    for (let g = 0; g <= top + 1e-9; g += step) {
      svg.append(sv('line', { x1: ML, x2: W - MR, y1: y(g), y2: y(g), class: g === 0 ? 'base' : 'grid' }),
        sv('text', { x: ML - 6, y: y(g) + 3.5, 'text-anchor': 'end' }, compact(g)));
    }
    for (const tk of timeTicks(a.since, a.now, W)) svg.append(sv('text', { x: x(tk.t), y: H - 6, 'text-anchor': 'middle' }, tk.label));
    const slot = ((W - ML - MR) * bs) / (a.now - a.since);
    const bw = Math.max(3, Math.min(24, slot - 4));
    bk.forEach(([t, by], i) => {
      if (!totals[i]) return;
      const cx = x(Math.min(a.now, t + bs / 2)), x0 = Math.max(ML, cx - bw / 2);
      const g = sv('g', { class: 'mark', tabindex: 0, 'aria-label': `${when(t)}: ${compact(totals[i])} tokens` });
      let acc = 0;
      const parts = ids.filter((id) => by[id]);
      parts.forEach((id, k) => {
        const v = by[id];
        const y1 = y(acc + v), y0 = y(acc);
        const hgt = Math.max(1, y0 - y1 - (k < parts.length - 1 ? 2 : 0));
        const isTop = k === parts.length - 1;
        g.append(sv('path', { d: isTop ? roundTop(x0, y1, bw, Math.max(1, y0 - y1), Math.min(4, bw / 2)) : `M${x0},${y1 + (y0 - y1 - hgt)}h${bw}v${hgt}h${-bw}Z`, fill: color(id) }));
        acc += v;
      });
      g.prepend(sv('rect', { x: x0 - 2, y: TOP, width: bw + 4, height: ph, fill: 'transparent' }));
      bindTip(g, () => [tipH(`${when(t)} – ${fTime.format(new Date((t + bs) * 1000))}`),
        parts.slice().sort((p1, p2) => by[p2] - by[p1]).map((id) => tipRow(rectKey(color(id)), compact(by[id]), acct(id).label)),
        el('div', { class: 'tip-note' }, `${compact(totals[i])} in total`)]);
      svg.append(g);
    });
    box.append(svg);
  }
  function roundTop(x0, y0, w, h, r) {
    r = Math.min(r, h);
    return `M${x0},${y0 + h}V${y0 + r}Q${x0},${y0} ${x0 + r},${y0}H${x0 + w - r}Q${x0 + w},${y0} ${x0 + w},${y0 + r}V${y0 + h}Z`;
  }
  function niceStep(raw) {
    const p = 10 ** Math.floor(Math.log10(Math.max(raw, 1)));
    const f = raw / p;
    return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * p;
  }

  function drawHeat(box, heat) {
    const W = width(box), ML = 36, TOP = 18;
    const cw = (W - ML) / 24, ch = Math.max(14, Math.min(24, cw * 0.8)), H = TOP + ch * 7 + 4;
    const max = Math.max(...heat.flat(), 1);
    const days = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
    const base = new Date(2024, 0, 1); // a Monday
    const dn = days.map((_, i) => fWd.format(new Date(base.getFullYear(), 0, 1 + i)));
    const svg = sv('svg', { viewBox: `0 0 ${W} ${H}`, height: H, role: 'group', 'aria-label': 'Active time by weekday and hour' });
    for (let hr = 0; hr < 24; hr += W < 520 ? 6 : 3) svg.append(sv('text', { x: ML + hr * cw + 1, y: 11 }, `${String(hr).padStart(2, '0')}:00`));
    heat.forEach((row, d) => {
      svg.append(sv('text', { x: ML - 8, y: TOP + d * ch + ch / 2 + 4, 'text-anchor': 'end' }, dn[d]));
      row.forEach((v, hr) => {
        const p = v > 0 ? 10 + 90 * Math.pow(v / max, 0.7) : 0;
        const r = sv('rect', { x: ML + hr * cw + 1, y: TOP + d * ch + 1, width: cw - 2, height: ch - 2, rx: 3,
          fill: v > 0 ? `color-mix(in oklab, var(--seq) ${p.toFixed(0)}%, var(--surface))` : 'var(--wash)', class: 'mark',
          tabindex: v > 0 ? 0 : -1, 'aria-label': `${dn[d]} ${hr}:00, ${dur(v)} active` });
        bindTip(r, () => [tipH(`${dn[d]} ${String(hr).padStart(2, '0')}:00–${String((hr + 1) % 24).padStart(2, '0')}:00`), el('div', {}, el('b', {}, dur(v)), ' active')]);
        svg.append(r);
      });
    });
    box.append(svg, el('div', { class: 'heat-legend' }, 'less', el('span', { class: 'ramp' }), 'more',
      el('span', { style: 'margin-left:auto' }, `busiest hour ${dur(max)}`)));
  }

  // ---------- Events ----------
  function feedItem(e) {
    const t2 = [when(e.ts), e.project, e.mode && e.mode !== 'tui' ? MODE[e.mode][0] : null].filter(Boolean).join(' · ');
    let ico, cls = '', t1;
    switch (e.ev) {
      case 'limit': ico = 'alert'; cls = 'crit'; t1 = [who(e.profile), ` hit the ${LIMIT[e.limit] || 'usage'} limit`]; break;
      case 'switch': ico = 'swap'; t1 = [e.from ? who(e.from) : 'Run', el('span', { class: 'arrow', 'aria-label': 'moved to' }, '→'), who(e.profile),
        e.reason ? el('span', { class: 't2' }, e.reason === 'limit' ? '· after a limit' : e.reason === 'manual' ? '· manual switch' : `· ${e.reason}`) : null]; break;
      case 'wait': ico = 'clock'; cls = 'warn'; t1 = [who(e.profile), ' waits for its reset', e.until ? [' until ', el('b', {}, fTime.format(new Date(e.until * 1000)))] : '']; break;
      case 'resume': ico = 'play'; cls = 'ok'; t1 = [who(e.profile), ' reset, the run goes on']; break;
      case 'start': ico = 'play'; t1 = ['Started on ', who(e.profile)]; break;
      case 'end': ico = 'stop'; t1 = ['Ended on ', who(e.profile), e.exit != null && e.exit !== 0 ? el('span', { class: 't2' }, `· exit ${e.exit}`) : null]; break;
      default: ico = 'dash'; t1 = [e.ev, ' ', who(e.profile)];
    }
    const sub = e.ev === 'start' && e.reason ? `${t2} · ${e.reason}` : t2;
    return el('li', {}, el('div', { class: `ico ${cls}`, 'aria-hidden': 'true' }, icon(ico, 14)),
      el('div', {}, el('div', { class: 't1' }, t1), el('div', { class: 't2' }, sub)));
  }
  function renderFeed() {
    const card = $('#feed-card');
    const a = st.act;
    let items = a.events.slice();
    if (st.feedAll) {
      for (const r of a.runs) {
        if (r.start >= a.since) items.push({ ts: r.start, ev: 'start', profile: r.profile, reason: r.reason, project: r.project, mode: r.mode, tool: r.tool });
        if (r.ended && r.end >= a.since) items.push({ ts: r.end, ev: 'end', profile: r.segments.length ? r.segments[r.segments.length - 1].profile : r.profile, exit: r.exit, project: r.project, mode: r.mode, tool: r.tool });
      }
      items.sort((e1, e2) => e2.ts - e1.ts);
    }
    const head = el('div', { class: 'card-h' }, el('h3', {}, 'Events'),
      el('span', { class: 'sub' }, `${a.totals.limits} limits · ${a.totals.switches} switches`),
      el('div', { class: 'tools' }, el('button', { type: 'button', class: 'link-btn', 'aria-pressed': String(st.feedAll),
        onclick: () => { st.feedAll = !st.feedAll; renderFeed(); } }, 'Starts and ends')));
    if (!a.runs.length) { card.replaceChildren(head, journalEmpty(true)); return; }
    if (!items.length) {
      card.replaceChildren(head, empty('No limits, waits or switches', [el('p', {}, 'Every run in this range stayed on its account. Turn on “Starts and ends” to list the runs here too.')], 'check'));
      return;
    }
    const list = el('ul', { class: 'feed' });
    let day = null;
    for (const e of items.slice(0, 200)) {
      const d = dayLabel(e.ts);
      if (d !== day) { day = d; list.append(el('li', { style: 'display:block;border:0;padding:10px 2px 2px;font-size:12px;font-weight:600;color:var(--ink2)' }, d)); }
      list.append(feedItem(e));
    }
    card.replaceChildren(head, list);
  }

  function renderRuns() {
    const card = $('#runs-card');
    const a = st.act;
    const head = el('div', { class: 'card-h' }, el('h3', {}, 'Runs'), el('span', { class: 'sub' }, `${a.runs.length} in range`));
    if (!a.runs.length) { card.replaceChildren(head, journalEmpty(true)); return; }
    const key = {
      start: (r) => r.start, project: (r) => r.project.toLowerCase(), mode: (r) => r.mode,
      dur: (r) => r.end - r.start, tokens: (r) => tokTotal(r.tokens), acct: (r) => acct(r.profile).label.toLowerCase(),
      status: (r) => (r.live ? 0 : r.exit == null ? 2 : r.exit === 0 ? 1 : 3),
    };
    const { k, dir } = st.sort;
    const runs = a.runs.slice().sort((r1, r2) => { const v1 = key[k](r1), v2 = key[k](r2); return (v1 < v2 ? -1 : v1 > v2 ? 1 : 0) * dir; });
    const shown = st.runsAll ? runs : runs.slice(0, 25);
    const cols = [['start', 'Started'], ['project', 'Project'], ['mode', 'Mode'], ['acct', 'Accounts'], ['dur', 'Length', 1], ['tokens', 'Tokens', 1], ['status', 'Status']];
    const th = cols.map(([ck, label, num]) => el('th', { class: num ? 'num' : '', scope: 'col', 'aria-sort': ck === k ? (dir > 0 ? 'ascending' : 'descending') : 'none' },
      el('button', { type: 'button', onclick: () => { st.sort = { k: ck, dir: ck === k ? -dir : ck === 'project' || ck === 'acct' ? 1 : -1 }; renderRuns(); } },
        label, ck === k ? (dir > 0 ? '↑' : '↓') : '')));
    const rows = shown.map((r) => {
      const path = [];
      for (const sg of r.segments) if (!sg.waiting && path[path.length - 1] !== sg.profile) path.push(sg.profile);
      if (!path.length) path.push(r.profile);
      const lim = r.marks.filter((m) => m.ev === 'limit').length;
      let status;
      if (r.live) status = el('span', { class: 'status live' }, 'live');
      else if (r.exit != null) status = el('span', { class: `status ${r.exit === 0 ? 'ok' : 'fail'}` }, `exit ${r.exit}`);
      else if (r.mode === 'exec') status = el('span', { class: 'status', title: 'agp handed the process to the tool; no end is recorded' }, 'handed off');
      else status = el('span', { class: 'status', title: 'No end event: the process may have been killed' }, 'no end');
      const len = r.end - r.start;
      const tr = el('tr', {},
        el('td', { title: new Date(r.start * 1000).toLocaleString() }, when(r.start)),
        el('td', { title: r.cwd || '' }, r.project, r.tool === 'codex' ? el('span', { class: 'plan', style: 'margin-left:6px' }, 'Codex') : null),
        el('td', { title: MODE[r.mode] ? MODE[r.mode][1] : '' }, MODE[r.mode] ? MODE[r.mode][0] : r.mode),
        el('td', { class: 'wrap' }, el('span', { class: 'path' }, path.map((p, i) => [i ? el('span', { class: 'arrow' }, '→') : null, who(p)]),
          lim ? el('span', { class: 'chip crit', style: 'margin-left:6px;padding:0 6px' }, icon('alert', 11), String(lim)) : null)),
        el('td', { class: 'num' }, len > 0 ? dur(len) : '—'),
        el('td', { class: 'num' }, tokTotal(r.tokens) ? compact(tokTotal(r.tokens)) : '—'),
        el('td', {}, status));
      return tr;
    });
    const kids = [head, el('div', { class: 'table-wrap' }, el('table', {}, el('thead', {}, el('tr', {}, th)), el('tbody', {}, rows)))];
    if (runs.length > 25) {
      kids.push(el('div', { class: 'more' }, el('button', { type: 'button', class: 'link-btn', onclick: () => { st.runsAll = !st.runsAll; renderRuns(); } },
        st.runsAll ? 'Show fewer' : `Show all ${runs.length} runs`)));
    }
    card.replaceChildren(...kids);
  }

  function renderSlow() {
    if (!st.state || !st.hist || !st.act) return;
    renderHistory();
    renderTimeline();
    renderDist();
    renderFeed();
    renderRuns();
    const a = st.act;
    $('#dist-sub').textContent = a.runs.length
      ? `${a.totals.runs} runs and ${dur(a.totals.secs)} of active time in the ${st.range === '24h' ? 'last 24 hours' : 'last 7 days'}.`
      : 'How runs, time and tokens spread over accounts, projects, modes and models.';
  }

  // ---------- Ticking ----------
  function tick() {
    const n = nowS();
    document.querySelectorAll('[data-until]').forEach((e) => { e.textContent = dur(Number(e.dataset.until) - n); });
    document.querySelectorAll('[data-ago]').forEach((e) => { e.textContent = ago(Number(e.dataset.ago)); });
    renderLive();
  }

  // ---------- Start ----------
  applyTheme();
  setPressed($('#range'), 'range', st.range);
  setPressed($('#tool'), 'tool', st.tool);
  loadState().then(loadSlow);
  setInterval(() => { if (!document.hidden) loadState(); }, 5000);
  setInterval(() => { if (!document.hidden) loadSlow(); }, 30000);
  setInterval(tick, 1000);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) { loadState(); loadSlow(); } });
  let lastW = 0, rt = 0;
  new ResizeObserver(() => {
    const w = $('main').clientWidth;
    if (Math.abs(w - lastW) < 8) return;
    lastW = w;
    clearTimeout(rt);
    rt = setTimeout(renderSlow, 120);
  }).observe($('main'));
  window.addEventListener('scroll', () => { if (!tipByKey) hideTip(); }, { passive: true });
})();
