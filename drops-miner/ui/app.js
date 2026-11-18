// Bonkr Twitch Miner window. Plain JavaScript, no build step: it asks the Rust side for a snapshot whenever the miner
// says something changed ("miner-changed") and draws the current page from it.
'use strict';

const T = window.__TAURI__;
const api = T
  ? { call: (cmd, args) => T.core.invoke(cmd, args), on: (ev, fn) => T.event.listen(ev, (e) => fn(e.payload)) }
  : window.MockApi; // the preview page supplies a stand-in

const $ = (sel, root = document) => root.querySelector(sel);
const esc = (v) => String(v ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const pct = (x) => `${(Math.max(0, Math.min(1, x || 0)) * 100).toFixed(1)}%`;
const num = (n) => (n == null ? '–' : Number(n).toLocaleString());
const clock = (secs) => {
  secs = Math.max(0, Math.floor(secs));
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
};
const mins = (m) => {
  if (m == null) return '–';
  const h = Math.floor(m / 60);
  return h ? `${h}h ${String(m % 60).padStart(2, '0')}m` : `${m}m`;
};
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
const when = (iso) => {
  const d = new Date(iso);
  return d.toLocaleString(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
};
const fromNow = (iso) => {
  const ms = new Date(iso) - Date.now();
  const abs = Math.abs(ms), h = abs / 3600000;
  const txt = h >= 48 ? `${Math.round(h / 24)} days` : h >= 1 ? `${Math.round(h)} hours` : `${Math.max(1, Math.round(abs / 60000))} min`;
  return ms >= 0 ? `in ${txt}` : `${txt} ago`;
};

const STATE_LABEL = {
  idle: ['Idle', ''], inventory_fetch: ['Loading', 'warn'], games_update: ['Loading', 'warn'],
  channels_cleanup: ['Updating', 'warn'], channels_fetch: ['Updating', 'warn'], channel_switch: ['Switching', 'warn'],
  restart: ['Restarting', 'warn'], exit: ['Stopping', ''],
};
const MODES = [
  ['priority_only', 'Priority list only', 'Mine only the games on your priority list, in that order.'],
  ['ending_soonest', 'Ending soonest', 'Your priority list first, then every other game, the campaign that ends soonest first.'],
  ['low_availability', 'Least time to spare', 'Your priority list first, then every other game, the campaign with the least spare time first.'],
];

// ---------------------------------------------------------------------------------------------------------------
// State

const S = {
  route: (location.hash || '#overview').slice(1),
  snap: null,
  inv: [],
  invRev: -1,
  log: [],
  logSeq: 0,
  prefs: null,
  needsReload: false,
  chTab: 'all',
  chQuery: '',
  invQuery: '',
  invShow: { active: true, upcoming: true, expired: false, not_linked: false, excluded: false, finished: false },
  autoscroll: true,
};
try {
  const saved = JSON.parse(localStorage.getItem('dm.invShow') || 'null');
  if (saved) Object.assign(S.invShow, saved);
} catch (_) { /* storage unavailable */ }

function toast(msg) {
  const t = $('#toast');
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toast.t);
  toast.t = setTimeout(() => (t.hidden = true), 3200);
}

async function call(cmd, args) {
  try {
    return await api.call(cmd, args);
  } catch (e) {
    toast(String(e));
    throw e;
  }
}

// ---------------------------------------------------------------------------------------------------------------
// Refresh loop

let refreshing = false, again = false;
async function refresh() {
  if (refreshing) { again = true; return; }
  refreshing = true;
  try {
    S.snap = await api.call('get_snapshot');
    const needInv = ['inventory', 'overview'].includes(S.route) && S.snap.rev !== S.invRev;
    if (needInv) {
      S.inv = await api.call('get_inventory');
      S.invRev = S.snap.rev;
    }
    if (S.snap.log_seq > S.logSeq) {
      const lines = await api.call('get_log', { since: S.logSeq });
      lines.forEach(addLog);
    }
    renderSide();
    renderPage(false);
  } catch (e) {
    console.error(e);
  } finally {
    refreshing = false;
    if (again) { again = false; refresh(); }
  }
}

function addLog(line) {
  if (line.seq <= S.logSeq) return;
  S.logSeq = line.seq;
  S.log.push(line);
  if (S.log.length > 600) S.log.shift();
  if (S.route === 'activity') appendLogLine(line);
}

// ---------------------------------------------------------------------------------------------------------------
// Sidebar

function renderSide() {
  const s = S.snap;
  if (!s) return;
  document.querySelectorAll('.side nav a').forEach((a) => (a.dataset.route === S.route ? a.setAttribute('aria-current', 'page') : a.removeAttribute('aria-current')));
  $('#navChannels').textContent = s.channels.length || '';
  $('#navCampaigns').textContent = s.campaigns || '';
  $('#sideVer').textContent = `v${s.version}`;
  $('#sideWho').textContent = s.user ? s.user.login || `User ${s.user.id}` : 'Not signed in';
  $('#sideState').innerHTML = stateTag(s);
}

function stateTag(s) {
  if (s.login_needed) return '<span class="tag warn">Sign in</span>';
  if (s.icon === 'error') return '<span class="tag bad">Problem</span>';
  if (s.watching && s.state !== 'exit') return '<span class="tag live">Mining</span>';
  const [label, cls] = STATE_LABEL[s.state] || ['Idle', ''];
  return `<span class="tag ${cls}">${label}</span>`;
}

// ---------------------------------------------------------------------------------------------------------------
// Pages

const PAGES = { overview, channels, inventory, activity, settings, account };
// pages the person types into are only redrawn when they open them or change something there
const STABLE = new Set(['settings']);

function renderPage(entering) {
  if (!S.snap) return;
  if (!entering && STABLE.has(S.route)) return;
  const fn = PAGES[S.route] || overview;
  const main = $('#main');
  const scroll = main.scrollTop;
  const focusId = document.activeElement && document.activeElement.id;
  const caret = document.activeElement && document.activeElement.selectionStart;
  main.innerHTML = `<div class="page">${reloadBanner()}${fn(S.snap)}</div>`;
  if (!entering) main.scrollTop = scroll;
  if (focusId) {
    const el = document.getElementById(focusId);
    if (el) { el.focus(); if (caret != null && el.setSelectionRange) el.setSelectionRange(caret, caret); }
  }
  if (S.route === 'activity') fillLog();
  main.querySelectorAll('[data-w]').forEach((e) => (e.style.width = e.dataset.w));
  tick();
}

function reloadBanner() {
  if (!S.needsReload) return '';
  return `<div class="flash live"><span>Your changes are saved. Reload the campaigns to mine with them now.</span><button class="btn sm" data-act="reload">Reload now</button></div>`;
}

function signInPanel() {
  return `<section class="panel signin"><div class="panel-head"><span class="label">Twitch sign-in</span><span class="tag warn">Needed</span></div>
  <div class="panel-body"><h2>Sign in to <span class="v">start mining.</span></h2>
  <ol><li>A Twitch sign-in window opens. Sign in as you normally would (two-factor codes work too).</li>
  <li>When Twitch finishes, the window closes by itself and mining starts.</li>
  <li>The window keeps nothing: no cookies or passwords are saved. Only the sign-in token, in this PC's app data.</li></ol>
  <div class="inline"><button class="btn" data-act="sign-in">Open the sign-in window</button></div></div></section>`;
}

function overview(s) {
  if (s.login_needed) return signInPanel();
  const d = s.drop, w = s.watching;
  const title = s.icon === 'error' ? 'Hit a <span class="v">snag.</span>'
    : !s.signed_in ? 'Signing <span class="v">in.</span>'
    : w && d ? `Mining <span class="v">${esc(d.game)}.</span>`
    : w ? `Watching <span class="v">${esc(w.name)}.</span>`
    : s.state === 'idle' ? 'Idle. <span class="v">Waiting.</span>' : 'Getting <span class="v">ready.</span>';
  const head = `<div class="page-head"><div><p class="label">Miner / Overview</p><h1>${title}</h1><p class="lede">${esc(s.status)}</p></div>
    <div class="inline"><button class="btn ghost" data-act="reload">Reload campaigns</button></div></div>`;

  const dropRem = d ? remainingSecs(d.remaining_minutes, d) : 0;
  const campRem = d ? remainingSecs(d.campaign_remaining_minutes, d) : 0;
  const online = s.channels.filter((c) => c.status === 'online').length;
  const stats = `<div class="stats">
    <div class="stat"><span>Watching</span><b class="word">${w ? esc(w.name) : '–'}</b><small>${w ? `${esc(w.game || 'No game')} · ${num(w.viewers)} viewers` : 'No channel yet'}</small></div>
    <div class="stat"><span>This drop</span><b>${d ? pct(d.progress) : '–'}</b><small>${d ? `<span data-tick="drop">${clock(dropRem)}</span> left` : 'Nothing being mined'}</small></div>
    <div class="stat"><span>Campaign</span><b>${d ? `${d.claimed}/${d.total}` : '–'}</b><small>${d ? `<span data-tick="camp">${clock(campRem)}</span> to finish` : '–'}</small></div>
    <div class="stat"><span>Channels</span><b>${online}<span class="faint of">/${s.channels.length}</span></b><small>live / followed</small></div>
  </div>`;

  const now = d
    ? `<section class="panel"><div class="panel-head"><span class="label">Now mining</span>${w ? `<button class="btn ghost sm" data-url="${esc(w.url)}">Open stream</button>` : ''}</div>
      <div class="panel-body"><div class="mining"><img class="boxart" src="${esc(d.image)}" alt="">
      <div><p class="label">${esc(d.game)}</p><h2>${esc(d.campaign)}</h2>
      <div class="rewards">${d.rewards.map((r) => `<div class="reward"><img src="${esc(r.image)}" alt=""><span>${esc(r.name)}</span></div>`).join('') || `<div class="reward"><span>${esc(d.name)}</span></div>`}</div></div></div>
      <div class="meter"><div class="meter-top"><span class="label">Drop · <b>${d.current_minutes} of ${d.required_minutes} min</b> · <b data-tick="drop">${clock(dropRem)}</b> left</span><span class="big">${pct(d.progress)}</span></div>
      <div class="bar"><i data-w="${pct(d.progress)}"></i></div></div>
      <div class="meter"><div class="meter-top"><span class="label">Campaign · <b>${d.claimed} of ${d.total} claimed</b> · <b data-tick="camp">${clock(campRem)}</b> to go</span><span class="big">${pct(d.campaign_progress)}</span></div>
      <div class="bar thin"><i data-w="${pct(d.campaign_progress)}"></i></div></div>
      </div></section>`
    : `<section class="panel"><div class="panel-head"><span class="label">Now mining</span></div><div class="empty"><p>${!s.signed_in ? 'Connecting to Twitch… If this takes long, the Activity page says why.' : s.state === 'idle' ? 'Nothing to mine right now. The miner checks again when a channel goes live or a campaign starts.' : 'Picking a channel…'}</p>
      ${s.wanted.length || !s.signed_in ? '' : '<p class="small">No games are queued. Add games to your priority list in Settings, or switch the priority mode to mine every linked campaign.</p><a class="btn ghost sm" href="#settings">Open settings</a>'}</div></section>`;

  const camps = S.inv.filter((c) => c.wanted && c.status === 'active' && !c.finished);
  const top = s.wanted.slice(0, 5);
  const queue = `<section class="panel"><div class="panel-head"><span class="label">Up next</span><span class="label">${plural(s.wanted.length, 'game')}</span></div><div class="panel-body">
    ${top.length ? `<ol class="queue">${top.map((g) => {
      const cs = camps.filter((c) => c.game === g);
      const left = cs.reduce((a, c) => a + c.total - c.claimed, 0);
      const on = d && d.game === g;
      return `<li class="${on ? 'on' : ''}"><button class="qbtn" data-find="${esc(g)}"><b>${esc(g)}</b><span class="small muted">${plural(cs.length, 'campaign')} · ${plural(left, 'drop')} to go</span></button>${on ? '<span class="tag live">Mining</span>' : ''}</li>`;
    }).join('')}</ol>` : '<p class="hint">Nothing queued. <a href="#settings">Add games</a>.</p>'}
    </div><div class="panel-foot"><span class="label">${s.websockets.filter((x) => x.status === 'Connected').length}/${s.websockets.length} live connections</span>${s.wanted.length > top.length ? `<a class="label" href="#settings">${s.wanted.length - top.length} more</a>` : ''}</div></section>`;

  return `${head}${stats}<div class="cols">${now}${queue}</div>${featured(s)}`;
}

// Featured for you: the next rewards from campaigns for the games on your priority list
// (or, with no list yet, the games being mined), best first.
function featured(s) {
  const prio = s.settings.priority;
  const games = prio.length ? prio : s.wanted;
  if (!games.length) return '';
  const rank = (g) => { const i = games.indexOf(g); return i < 0 ? 999 : i; };
  const picks = [];
  S.inv
    .filter((c) => games.includes(c.game) && c.status !== 'expired' && !c.finished && !c.excluded)
    .sort((a, b) => rank(a.game) - rank(b.game) || (a.status === 'active' ? 0 : 1) - (b.status === 'active' ? 0 : 1) || new Date(a.ends_at) - new Date(b.ends_at))
    .forEach((c) => {
      // up to two rewards per campaign: what's being earned now and what comes next
      c.drops.filter((d) => !d.claimed && d.rewards.length).sort((a, b) => a.required_minutes - b.required_minutes).slice(0, 2)
        .forEach((d) => picks.push({ c, d }));
    });
  if (!picks.length) return '';
  const shown = picks.slice(0, 8);
  const soon = (iso) => new Date(iso) - Date.now() < 24 * 3600e3;
  const left = (iso) => { const h = (new Date(iso) - Date.now()) / 3600e3; return h >= 48 ? `${Math.round(h / 24)}d left` : h >= 1 ? `${Math.round(h)}h left` : `${Math.max(1, Math.round(h * 60))}m left`; };
  const cards = shown.map(({ c, d }) => {
    const r = d.rewards[0];
    const mining = s.drop && s.drop.id === d.id;
    let cls, line;
    if (mining) { cls = 'st-live'; line = `Mining · ${d.current_minutes}/${d.required_minutes} min`; }
    else if (d.can_claim) { cls = 'st-warn'; line = 'Ready to claim'; }
    else if (c.status === 'upcoming') { cls = 'st-info'; line = `Starts ${fromNow(c.starts_at)}`; }
    else if (!c.eligible) { cls = 'st-bad'; line = 'Link account to earn'; }
    else if (soon(c.ends_at)) { cls = 'st-warn'; line = `Ends soon · ${d.current_minutes}/${d.required_minutes} min`; }
    else { cls = ''; line = `${d.current_minutes}/${d.required_minutes} min · ${left(c.ends_at)}`; }
    const action = !c.eligible && c.link_url ? `data-url="${esc(c.link_url)}"` : `data-find="${esc(c.name)}"`;
    return `<button class="feat ${mining ? 'on' : ''}" ${action} title="${esc(d.rewards.map((x) => x.name).join(', '))} · ${esc(c.game)}">
      <img src="${esc(r.image)}" alt="">
      <span class="feat-body"><b>${esc(r.name)}${d.rewards.length > 1 ? ` <span class="faint">+${d.rewards.length - 1}</span>` : ''}</b>
      <span class="feat-line ${cls}">${esc(line)}</span>
      <span class="bar thin ${c.eligible ? '' : 'muted'}"><i data-w="${pct(d.progress)}"></i></span></span></button>`;
  }).join('');
  const more = picks.length > shown.length ? `<a class="label" href="#inventory">${picks.length - shown.length} more in Inventory</a>` : '';
  return `<section class="panel"><div class="panel-head"><span class="label">Featured for you</span><span class="inline">${more}<span class="label">${prio.length ? 'From your priority list' : 'From the games being mined'}</span></span></div>
    <div class="featured">${cards}</div></section>`;
}

function channels(s) {
  if (s.login_needed) return signInPanel();
  const q = S.chQuery.toLowerCase();
  let list = s.channels.slice();
  if (S.chTab === 'live') list = list.filter((c) => c.status === 'online');
  if (S.chTab === 'watchable') list = list.filter((c) => c.can_watch);
  if (q) list = list.filter((c) => `${c.name} ${c.login} ${c.game || ''}`.toLowerCase().includes(q));
  const tab = (v, label) => `<button class="${S.chTab === v ? 'on' : ''}" data-chtab="${v}">${label}</button>`;
  const pinned = s.pinned != null;
  const rows = list.map((c) => {
    const st = c.status === 'online' ? '<span class="tag live">Live</span>' : c.status === 'pending' ? '<span class="tag warn">Starting</span>' : '<span class="tag">Offline</span>';
    const act = c.watching
      ? `<span class="tag good">${s.pinned === c.id ? 'Pinned' : 'Watching'}</span>`
      : `<button class="btn ${c.can_watch ? '' : 'ghost'} sm" data-watch="${c.id}" ${c.can_watch ? '' : 'disabled title="Not live with drops for a wanted game"'}>Watch</button>`;
    return `<tr class="${c.watching ? 'on' : ''}"><td class="key"><button class="link plainlink" data-url="https://www.twitch.tv/${esc(c.login)}">${esc(c.name)}</button>${c.acl ? '<span class="sub">From a campaign allow-list</span>' : ''}</td>
      <td>${st}</td><td>${esc(c.game || '–')}</td><td class="num">${num(c.viewers)}</td>
      <td>${c.drops ? '<span class="ok">✓ Drops</span>' : '<span class="no">–</span>'}</td>
      <td class="num">${c.priority != null ? `#${c.priority + 1}` : '–'}</td><td class="act">${act}</td></tr>`;
  }).join('');
  return `<div class="page-head"><div><p class="label">Miner / Channels</p><h1>Channels <span class="v">in play.</span></h1>
    <p class="lede">Live channels streaming your queued games with drops on. ${pinned ? '<b>You pinned a channel</b>, so the miner stays on it while it can.' : 'The miner picks the best one; choose <b>Watch</b> to pin a different one.'}</p></div>
    <div class="inline">${pinned ? '<button class="btn ghost" data-watch="auto">Let the miner choose</button>' : ''}</div></div>
    <div class="toolbar"><nav class="tabs">${tab('all', `All · ${s.channels.length}`)}${tab('live', `Live · ${s.channels.filter((c) => c.status === 'online').length}`)}${tab('watchable', 'Watchable')}</nav>
    <input type="search" id="chSearch" placeholder="Search channels or games" value="${esc(S.chQuery)}" aria-label="Search channels"></div>
    ${list.length ? `<div class="tablewrap"><table class="tbl"><thead><tr><th>Channel</th><th>Status</th><th>Game</th><th>Viewers</th><th>Drops</th><th>Priority</th><th><span hidden>Action</span></th></tr></thead><tbody>${rows}</tbody></table></div>`
      : `<section class="panel"><div class="empty"><p>${s.channels.length ? 'No channels match.' : 'No channels yet. They appear once campaigns are loaded and a queued game has live streams.'}</p></div></section>`}`;
}

function inventory(s) {
  if (s.login_needed) return signInPanel();
  const sh = S.invShow, q = S.invQuery.toLowerCase();
  const list = S.inv.filter((c) => {
    if (c.status === 'active' && !sh.active) return false;
    if (c.status === 'upcoming' && !sh.upcoming) return false;
    if (c.status === 'expired' && !sh.expired) return false;
    if (!c.eligible && !sh.not_linked) return false;
    if (c.excluded && !sh.excluded) return false;
    if (c.finished && !sh.finished) return false;
    return !q || `${c.name} ${c.game}`.toLowerCase().includes(q);
  });
  const chip = (k, label) => `<label class="chip"><input type="checkbox" data-show="${k}" ${sh[k] ? 'checked' : ''}>${label}</label>`;
  const mining = s.drop ? s.drop.campaign_id : null;
  const card = (c) => {
    const status = c.status === 'active' ? '<span class="tag good">Active</span>' : c.status === 'upcoming' ? '<span class="tag warn">Upcoming</span>' : '<span class="tag bad">Ended</span>';
    const link = c.linked ? '' : `<span class="tag ${c.eligible ? 'info' : 'bad'}">${c.eligible ? 'Badge/emote' : 'Not linked'}</span>`;
    const prio = c.priority != null ? `<span class="tag live plain">Priority #${c.priority + 1}</span>` : c.excluded ? '<span class="tag plain">Excluded</span>' : '';
    const minedHere = c.id === mining ? '<span class="tag live">Mining</span>' : '';
    const drops = c.drops.map((d) => {
      const st = d.claimed ? '<small class="ok">CLAIMED</small>'
        : d.can_claim ? '<small class="ready">READY TO CLAIM</small>'
        : d.required_minutes > 0 ? `<small>${d.current_minutes} / ${d.required_minutes} MIN</small>` : '<small>SUBSCRIPTION</small>';
      const isMining = s.drop && d.id === s.drop.id;
      return `<div class="drop ${d.claimed ? 'claimed' : ''} ${isMining ? 'mining' : ''}"><div class="drop-imgs">${d.rewards.slice(0, 3).map((r) => `<img src="${esc(r.image)}" alt="" title="${esc(r.name)}">`).join('')}</div>
        <b title="${esc(d.rewards.map((r) => r.name).join(', ') || d.name)}">${esc(d.rewards.map((r) => r.name).join(', ') || d.name)}</b>
        <div class="bar thin ${d.claimed ? 'done' : d.earnable ? '' : 'muted'}"><i data-w="${pct(d.progress)}"></i></div>${st}</div>`;
    }).join('');
    const acl = c.acl_count ? `Only on: ${c.acl.map(esc).join(', ')}${c.acl_count > c.acl.length ? ` and ${c.acl_count - c.acl.length} more` : ''}` : 'Any channel with drops on';
    const actions = [
      !c.linked && !c.eligible && c.link_url ? `<button class="btn sm" data-url="${esc(c.link_url)}">Link account</button>` : '',
      c.priority == null && !c.excluded ? `<button class="btn ghost sm" data-prio="${esc(c.game)}">Add to priority</button>` : '',
      !c.excluded && c.priority == null ? `<button class="btn ghost sm" data-excl="${esc(c.game)}">Exclude game</button>` : '',
    ].join('');
    return `<article class="panel camp"><img class="boxart" src="${esc(c.image)}" alt="">
      <div><div class="camp-head"><div><p class="label">${esc(c.game)}</p><h3>${esc(c.name)}</h3></div><div class="tags">${minedHere}${status}${link}${prio}</div></div>
      <div class="camp-meta"><span>${c.status === 'upcoming' ? `Starts <b>${esc(when(c.starts_at))}</b>` : `Ends <b>${esc(when(c.ends_at))}</b> · ${esc(fromNow(c.ends_at))}`}</span><span>Claimed <b>${c.claimed}/${c.total}</b></span>${c.remaining_minutes > 0 && !c.finished ? `<span>Left <b>${mins(c.remaining_minutes)}</b></span>` : ''}</div>
      <div class="drops">${drops}</div>
      <div class="camp-foot"><span>${acl}</span><span class="inline">${actions}</span></div></div></article>`;
  };
  return `<div class="page-head"><div><p class="label">Miner / Inventory</p><h1>Campaigns and <span class="v">drops.</span></h1>
    <p class="lede">Every drops campaign on your account. Campaigns that need a linked game account show <b>Not linked</b> until you link it.</p></div>
    <div class="inline"><button class="btn ghost" data-act="reload">Reload</button></div></div>
    <div class="toolbar"><div class="chips">${chip('active', 'Active')}${chip('upcoming', 'Upcoming')}${chip('expired', 'Ended')}${chip('not_linked', 'Not linked')}${chip('excluded', 'Excluded')}${chip('finished', 'Finished')}</div>
    <input type="search" id="invSearch" placeholder="Search campaigns or games" value="${esc(S.invQuery)}" aria-label="Search campaigns"></div>
    ${list.length ? `<div class="campaigns">${list.map(card).join('')}</div>` : `<section class="panel"><div class="empty"><p>${S.inv.length ? 'No campaigns match these filters.' : 'No campaigns loaded yet.'}</p></div></section>`}`;
}

function activity() {
  return `<div class="page-head"><div><p class="label">Miner / Activity</p><h1>What the miner <span class="v">did.</span></h1>
    <p class="lede">Channel switches, progress problems and claimed drops, newest at the bottom. Also saved to <code>miner.log</code> in the data folder.</p></div>
    <div class="inline"><button class="btn ghost" data-act="copy-log">Copy</button></div></div>
    <section class="panel"><div class="panel-head"><span class="label">Log</span><label class="check small"><input type="checkbox" id="autoscroll" ${S.autoscroll ? 'checked' : ''}><span>Follow new lines</span></label></div>
    <pre class="console" id="console"></pre></section>`;
}

function logLineHtml(l) {
  const cls = /^Claimed/.test(l.text) ? 'claim' : /fail|problem|couldn't|error|expired/i.test(l.text) ? 'warn' : '';
  return `<span class="t">${esc(l.time)}</span><span class="${cls}">${esc(l.text)}</span>\n`;
}
function fillLog() {
  const c = $('#console');
  if (!c) return;
  c.innerHTML = S.log.map(logLineHtml).join('');
  if (S.autoscroll) c.scrollTop = c.scrollHeight;
}
function appendLogLine(l) {
  const c = $('#console');
  if (!c) return;
  c.insertAdjacentHTML('beforeend', logLineHtml(l));
  if (S.autoscroll) c.scrollTop = c.scrollHeight;
}

function settings(s) {
  const st = s.settings, p = S.prefs || { close_to_tray: true, start_with_windows: false };
  const games = s.games.filter((g) => !st.priority.includes(g));
  const exGames = s.games.filter((g) => !st.exclude.includes(g));
  const prioList = st.priority.length
    ? st.priority.map((g, i) => `<li><span class="n">${String(i + 1).padStart(2, '0')}</span><span class="g">${esc(g)}</span><span class="btns">
        <button class="btn ghost icon" title="Move up" data-move="${i}" data-dir="-1" ${i === 0 ? 'disabled' : ''}>↑</button>
        <button class="btn ghost icon" title="Move down" data-move="${i}" data-dir="1" ${i === st.priority.length - 1 ? 'disabled' : ''}>↓</button>
        <button class="btn ghost icon" title="Remove" data-unprio="${i}">✕</button></span></li>`).join('')
    : '<li class="none">No games yet. Add the games you want drops for, most wanted first.</li>';
  const exList = st.exclude.length
    ? st.exclude.map((g) => `<li><span class="g">${esc(g)}</span><span class="btns"><button class="btn ghost icon" title="Remove" data-unexcl="${esc(g)}">✕</button></span></li>`).join('')
    : '<li class="none">No excluded games.</li>';
  const check = (id, on, title, desc) => `<label class="check"><input type="checkbox" id="${id}" ${on ? 'checked' : ''}><span>${title}<small>${desc}</small></span></label>`;
  return `<div class="page-head"><div><p class="label">Setup / Settings</p><h1>What to mine, <span class="v">and how.</span></h1>
    <p class="lede">Changes are saved as you make them. Game lists and mining options apply after the next reload.</p></div>
    <div class="inline"><button class="btn" data-act="reload">Save and reload</button></div></div>
    <div class="cols even">
      <section class="panel"><div class="panel-head"><span class="label">Priority list</span><span class="label">${plural(st.priority.length, 'game')}</span></div><div class="panel-body">
        <p class="hint">Games are mined top to bottom. A game higher up wins whenever both have live channels.</p>
        <ol class="rank">${prioList}</ol>
        <form class="inline" id="prioForm"><input id="prioInput" list="prioGames" placeholder="Game name" autocomplete="off"><datalist id="prioGames">${games.map((g) => `<option value="${esc(g)}">`).join('')}</datalist><button class="btn ghost">Add</button></form>
      </div></section>
      <div class="stack">
        <section class="panel"><div class="panel-head"><span class="label">Priority mode</span></div><div class="panel-body"><div class="modes">
          ${MODES.map(([v, t, d]) => `<label class="check"><input type="radio" name="mode" value="${v}" ${st.priority_mode === v ? 'checked' : ''}><span>${t}<small>${d}</small></span></label>`).join('')}
        </div></div></section>
        <section class="panel"><div class="panel-head"><span class="label">Never mine</span><span class="label">${plural(st.exclude.length, 'game')}</span></div><div class="panel-body">
          <ol class="rank">${exList}</ol>
          <form class="inline" id="exclForm"><input id="exclInput" list="exclGames" placeholder="Game name" autocomplete="off"><datalist id="exclGames">${exGames.map((g) => `<option value="${esc(g)}">`).join('')}</datalist><button class="btn ghost">Exclude</button></form>
        </div></section>
      </div>
      <section class="panel"><div class="panel-head"><span class="label">Mining</span></div><div class="panel-body">
        ${check('setBadges', st.enable_badges_emotes, 'Mine badge and emote campaigns', 'These need no linked game account, so they would otherwise fill the queue.')}
        ${check('setAvail', st.available_drops_check, 'Double-check streams for drops', 'Asks Twitch which campaigns each stream really has. Slower, but skips streams that only look eligible.')}
        <label>Connection<select id="setQuality">${[1, 2, 3, 4, 5, 6].map((n) => `<option value="${n}" ${st.connection_quality === n ? 'selected' : ''}>${n === 1 ? '1 · Fast (default)' : n === 6 ? '6 · Very slow' : n}</option>`).join('')}</select><span class="hint">Raise this if you see timeouts: requests wait longer before retrying.</span></label>
        <label>Proxy <span class="faint light">(optional)</span><input id="setProxy" placeholder="http://user:password@host:port" value="${esc(st.proxy)}"><span class="hint">Used for Twitch's web requests. Live updates always connect directly.</span></label>
      </div></section>
      <section class="panel"><div class="panel-head"><span class="label">App</span></div><div class="panel-body">
        ${check('setStart', p.start_with_windows, 'Start with Windows', 'Starts in the tray when you sign in to Windows and starts mining.')}
        ${check('setTray', p.close_to_tray, 'Keep mining when the window closes', 'Closing the window leaves Bonkr Twitch Miner in the tray. Quit from the tray menu.')}
        ${check('setNotify', st.notifications, 'Notify me about claimed drops', 'A Windows notification each time a drop is claimed.')}
        <div class="inline"><button class="btn ghost sm" data-act="data-folder">Open data folder</button><span class="hint">Settings, sign-in and the log live here. A copy of your settings is also kept in <code>Documents\\Bonkr Twitch Miner</code>, so reinstalling never loses your game lists.</span></div>
      </div></section>
    </div>`;
}

function account(s) {
  const u = s.user;
  return `<div class="page-head"><div><p class="label">Setup / Account</p><h1>Your Twitch <span class="v">account.</span></h1>
    <p class="lede">Bonkr Twitch Miner signs in as the Twitch Android app, like the original miner, and only uses the sign-in to read your drops and claim them.</p></div></div>
    <div class="cols even">
      <section class="panel"><div class="panel-head"><span class="label">Sign-in</span>${s.signed_in ? '<span class="tag good">Signed in</span>' : s.login_needed ? '<span class="tag warn">Needed</span>' : '<span class="tag">Checking</span>'}</div>
        <div class="panel-body"><dl class="kv"><dt>Account</dt><dd>${u ? esc(u.login) : '–'}</dd><dt>User ID</dt><dd class="mono">${u ? u.id : '–'}</dd><dt>Status</dt><dd>${esc(s.status)}</dd></dl>
        <div class="inline">${s.login_needed ? '<button class="btn" data-act="sign-in">Sign in</button>' : ''}${s.signed_in ? '<button class="btn danger sm" data-act="sign-out">Sign out</button>' : ''}</div></div></section>
      <section class="panel"><div class="panel-head"><span class="label">Live connections</span><span class="label">${s.websockets.reduce((a, w) => a + w.topics, 0)} topics</span></div>
        ${s.websockets.length ? `<div class="tablewrap flush"><table class="tbl"><thead><tr><th>#</th><th>Status</th><th>Topics</th></tr></thead><tbody>
        ${s.websockets.map((w) => `<tr><td class="num">${w.index + 1}</td><td>${w.status === 'Connected' ? '<span class="tag good">Connected</span>' : `<span class="tag warn">${esc(w.status)}</span>`}</td><td class="num">${w.topics}/50</td></tr>`).join('')}
        </tbody></table></div>` : '<div class="empty"><p>Not connected yet.</p></div>'}</section>
      <section class="panel"><div class="panel-head"><span class="label">About</span><span class="label">v${esc(s.version)}</span></div><div class="panel-body">
        <p class="hint">Bonkr Twitch Miner is a port of <button class="link" data-url="https://github.com/DevilXD/TwitchDropsMiner">Twitch Drops Miner by DevilXD</button>. All credit for how it works (signing in, choosing campaigns and channels, following progress and claiming drops) goes to DevilXD and the original project's contributors. We ported it from Python to Rust and Tauri and built a new window around it.</p>
        <p class="hint">MIT licence: © 2024 DevilXD, port © 2026 wayn2deep4u. Not affiliated with Twitch.</p>
      </div></section>
    </div>`;
}

// countdowns tick every second without asking the miner
function remainingSecs(minutes, d) {
  if (!minutes || minutes <= 0) return 0;
  if (d.timer_started_ms) {
    const el = Math.floor((Date.now() - d.timer_started_ms) / 1000);
    return (minutes - 1) * 60 + Math.max(0, 60 - el);
  }
  return d.subone ? (minutes - 1) * 60 : minutes * 60;
}
function tick() {
  const d = S.snap && S.snap.drop;
  if (!d) return;
  document.querySelectorAll('[data-tick="drop"]').forEach((e) => (e.textContent = clock(remainingSecs(d.remaining_minutes, d))));
  document.querySelectorAll('[data-tick="camp"]').forEach((e) => (e.textContent = clock(remainingSecs(d.campaign_remaining_minutes, d))));
}

// ---------------------------------------------------------------------------------------------------------------
// Actions

async function saveSettings(mutate, { reload = false } = {}) {
  const st = JSON.parse(JSON.stringify(S.snap.settings));
  mutate(st);
  await call('save_settings', { settings: st, reload });
  S.snap.settings = st;
  S.needsReload = !reload;
  if (reload) toast('Saved. Reloading campaigns…');
  renderPage(true);
}

async function savePrefs() {
  const close = $('#setTray').checked, start = $('#setStart').checked;
  await call('save_prefs', { closeToTray: close, startWithWindows: start });
  S.prefs = await api.call('get_prefs');
}

document.addEventListener('click', async (e) => {
  const t = e.target.closest('button, [data-url]');
  if (!t) return;
  const ds = t.dataset;
  if (ds.url) { e.preventDefault(); return call('open_url', { url: ds.url }); }
  if (ds.find) { S.invQuery = ds.find; location.hash = '#inventory'; return; }
  if (ds.act === 'reload') { S.needsReload = false; await call('reload'); toast('Reloading campaigns…'); return renderPage(true); }
  if (ds.act === 'sign-in') return call('sign_in');
  if (ds.act === 'sign-out') { if (armed(t, 'Click again to sign out')) await call('sign_out'); return; }
  if (ds.act === 'data-folder') return call('open_data_folder');
  if (ds.act === 'copy-log') { await navigator.clipboard.writeText(S.log.map((l) => `${l.time} ${l.text}`).join('\n')); return toast('Log copied.'); }
  if (ds.watch) { await call('switch_channel', { id: ds.watch === 'auto' ? null : Number(ds.watch) }); return toast(ds.watch === 'auto' ? 'The miner will choose.' : 'Switching…'); }
  if (ds.chtab) { S.chTab = ds.chtab; return renderPage(true); }
  if (ds.move) return saveSettings((st) => { const i = +ds.move, j = i + +ds.dir; [st.priority[i], st.priority[j]] = [st.priority[j], st.priority[i]]; });
  if (ds.unprio) return saveSettings((st) => st.priority.splice(+ds.unprio, 1));
  if (ds.unexcl) return saveSettings((st) => { st.exclude = st.exclude.filter((g) => g !== ds.unexcl); });
  if (ds.prio) { await saveSettings((st) => { if (!st.priority.includes(ds.prio)) st.priority.push(ds.prio); }); return toast(`${ds.prio} added to your priority list.`); }
  if (ds.excl) { await saveSettings((st) => { if (!st.exclude.includes(ds.excl)) st.exclude.push(ds.excl); st.exclude.sort(); }); return toast(`${ds.excl} won't be mined.`); }
});

document.addEventListener('submit', (e) => {
  e.preventDefault();
  if (e.target.id === 'prioForm') {
    const g = $('#prioInput').value.trim();
    if (g) saveSettings((st) => { if (!st.priority.includes(g)) st.priority.push(g); });
  }
  if (e.target.id === 'exclForm') {
    const g = $('#exclInput').value.trim();
    if (g) saveSettings((st) => { if (!st.exclude.includes(g)) st.exclude.push(g); st.exclude.sort(); });
  }
});

document.addEventListener('change', (e) => {
  const t = e.target;
  if (t.name === 'mode') return saveSettings((st) => (st.priority_mode = t.value));
  if (t.id === 'setBadges') return saveSettings((st) => (st.enable_badges_emotes = t.checked));
  if (t.id === 'setAvail') return saveSettings((st) => (st.available_drops_check = t.checked));
  if (t.id === 'setNotify') return saveSettings((st) => (st.notifications = t.checked));
  if (t.id === 'setQuality') return saveSettings((st) => (st.connection_quality = Number(t.value)));
  if (t.id === 'setProxy') return saveSettings((st) => (st.proxy = t.value.trim()));
  if (t.id === 'setTray' || t.id === 'setStart') return savePrefs();
  if (t.id === 'autoscroll') { S.autoscroll = t.checked; return; }
  if (t.dataset.show) {
    S.invShow[t.dataset.show] = t.checked;
    try { localStorage.setItem('dm.invShow', JSON.stringify(S.invShow)); } catch (_) { /* fine */ }
    renderPage(true);
  }
});

document.addEventListener('input', (e) => {
  if (e.target.id === 'chSearch') { S.chQuery = e.target.value; renderPage(false); }
  if (e.target.id === 'invSearch') { S.invQuery = e.target.value; renderPage(false); }
});

// two-step buttons: the first click asks, a second click within 4 seconds does it
function armed(btn, question) {
  if (btn.dataset.armed) return true;
  const label = btn.textContent;
  btn.dataset.armed = '1';
  btn.textContent = question;
  setTimeout(() => { delete btn.dataset.armed; btn.textContent = label; }, 4000);
  return false;
}

$('#quitBtn').addEventListener('click', (e) => {
  e.stopPropagation();
  if (armed(e.currentTarget, 'Click again to quit')) call('quit');
});

window.addEventListener('hashchange', async () => {
  S.route = (location.hash || '#overview').slice(1);
  if (S.route === 'inventory' || S.route === 'overview') {
    S.inv = await api.call('get_inventory');
    S.invRev = S.snap ? S.snap.rev : -1;
  }
  if (S.route === 'settings') S.prefs = await api.call('get_prefs');
  renderSide();
  renderPage(true);
  $('#main').scrollTop = 0;
});

// ---------------------------------------------------------------------------------------------------------------

(async function start() {
  api.on('miner-changed', refresh);
  api.on('miner-log', addLog);
  S.prefs = await api.call('get_prefs').catch(() => null);
  await refresh();
  renderPage(true);
  setInterval(tick, 1000);
  // a slow safety refresh, in case an event was missed while the window was hidden
  setInterval(refresh, 15000);
})();
