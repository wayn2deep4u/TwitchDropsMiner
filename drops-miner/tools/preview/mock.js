// Stand-in for the Rust side, for previewing the window in a browser.
(function () {
  const now = Date.now();
  const iso = (h) => new Date(now + h * 3600e3).toISOString();
  const img = (c, t) => 'data:image/svg+xml,' + encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" width="80" height="80"><rect width="80" height="80" fill="${c}"/><text x="40" y="48" font-size="22" text-anchor="middle" fill="#fff" font-family="sans-serif">${t}</text></svg>`);
  const box = (c, t) => 'data:image/svg+xml,' + encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" width="285" height="380"><rect width="285" height="380" fill="${c}"/><text x="142" y="200" font-size="34" text-anchor="middle" fill="#fff" font-family="sans-serif">${t}</text></svg>`);
  const settings = { priority: ['Rust', 'Marvel Rivals', 'World of Warcraft'], exclude: ['Fortnite'], priority_mode: 'priority_only', enable_badges_emotes: false, available_drops_check: false, notifications: true, connection_quality: 1, proxy: '' };
  const snap = {
    version: '1.0.0', rev: 5, state: 'channel_switch', status: 'Watching Shroud', icon: 'active', signed_in: true,
    user: { id: 123456789, login: 'smuggz' }, login_needed: false,
    watching: { id: 1, name: 'Shroud', login: 'shroud', game: 'Rust', viewers: 18342, title: 'wipe day', url: 'https://www.twitch.tv/shroud' },
    pinned: null,
    drop: { id: 'd3', campaign_id: 'c1', campaign: 'Rust Twitch Drops: Round 41', game: 'Rust', image: box('#7a3b1c', 'RUST'), name: 'Garage Door',
      rewards: [{ name: 'Metal Garage Door', image: img('#444', 'GD') }, { name: 'Hazmat Hood', image: img('#2d6a4f', 'HZ') }],
      progress: 0.6167, current_minutes: 37, required_minutes: 60, remaining_minutes: 23, campaign_progress: 0.41, campaign_remaining_minutes: 263,
      claimed: 2, total: 6, timer_started_ms: now - 12000, subone: false, ends_at: iso(80) },
    channels: [
      { id: 1, name: 'Shroud', login: 'shroud', status: 'online', game: 'Rust', viewers: 18342, drops: true, acl: true, watching: true, can_watch: true, priority: 0 },
      { id: 2, name: 'Blooprint', login: 'blooprint', status: 'online', game: 'Rust', viewers: 6120, drops: true, acl: true, watching: false, can_watch: true, priority: 0 },
      { id: 3, name: 'hJune', login: 'hjune', status: 'pending', game: null, viewers: null, drops: false, acl: true, watching: false, can_watch: false, priority: null },
      { id: 4, name: 'Sacriel', login: 'sacriel', status: 'online', game: 'Marvel Rivals', viewers: 2210, drops: true, acl: false, watching: false, can_watch: true, priority: 1 },
      { id: 5, name: 'Asmongold', login: 'zackrawrr', status: 'online', game: 'World of Warcraft', viewers: 41200, drops: true, acl: false, watching: false, can_watch: true, priority: 2 },
      { id: 6, name: 'Willjum', login: 'willjum', status: 'offline', game: null, viewers: null, drops: false, acl: true, watching: false, can_watch: false, priority: null },
    ],
    wanted: ['Rust', 'Marvel Rivals', 'World of Warcraft'],
    games: ['Fortnite', 'Marvel Rivals', 'Overwatch 2', 'Rust', 'World of Warcraft'],
    websockets: [{ index: 0, status: 'Connected', topics: 14 }],
    settings, log_seq: 6, campaigns: 5,
  };
  const drop = (id, name, req, cur, claimed, color, extra = {}) => ({ id, name, rewards: [{ name, image: img(color, name.slice(0, 2).toUpperCase()) }], progress: req ? Math.min(1, cur / req) : 0, current_minutes: cur, required_minutes: req, claimed, can_claim: false, earnable: !claimed, starts_at: iso(-100), ends_at: iso(80), ...extra });
  const inv = [
    { id: 'c1', name: 'Rust Twitch Drops: Round 41', game: 'Rust', image: box('#7a3b1c', 'RUST'), status: 'active', linked: true, eligible: true, link_url: '', starts_at: iso(-100), ends_at: iso(80), excluded: false, priority: 0, wanted: true, finished: false, claimed: 2, total: 6, progress: 0.41, remaining_minutes: 263, acl: ['Shroud', 'Blooprint', 'hJune', 'Willjum'], acl_count: 9,
      drops: [drop('d1', 'Rock Hammer', 60, 60, true, '#555'), drop('d2', 'Sleeping Bag', 60, 60, true, '#6b4226'), drop('d3', 'Metal Garage Door', 60, 37, false, '#444'), drop('d4', 'Hazmat Hood', 120, 37, false, '#2d6a4f'), drop('d5', 'Large Box', 180, 37, false, '#5c4033'), drop('d6', 'AK-47 Skin', 240, 37, false, '#8b0000')] },
    { id: 'c2', name: 'Season 4 Viewer Rewards', game: 'Marvel Rivals', image: box('#1d3557', 'MR'), status: 'active', linked: true, eligible: true, link_url: '', starts_at: iso(-50), ends_at: iso(200), excluded: false, priority: 1, wanted: true, finished: false, claimed: 0, total: 3, progress: 0, remaining_minutes: 360, acl: [], acl_count: 0,
      drops: [drop('m1', 'Spray', 60, 0, false, '#457b9d'), drop('m2', 'Nameplate', 120, 0, false, '#1d3557'), drop('m3', 'Costume', 180, 0, false, '#e63946')] },
    { id: 'c3', name: 'The War Within Watch Party', game: 'World of Warcraft', image: box('#14213d', 'WOW'), status: 'upcoming', linked: true, eligible: true, link_url: '', starts_at: iso(30), ends_at: iso(300), excluded: false, priority: 2, wanted: false, finished: false, claimed: 0, total: 2, progress: 0, remaining_minutes: 240, acl: [], acl_count: 0,
      drops: [drop('w1', 'Mount: Twitch Drake', 120, 0, false, '#6a4c93'), drop('w2', 'Pet: Glimmerfly', 120, 0, false, '#fca311')] },
    { id: 'c4', name: 'Overwatch 2 Season Drops', game: 'Overwatch 2', image: box('#f77f00', 'OW2'), status: 'active', linked: false, eligible: false, link_url: 'https://example.com/link', starts_at: iso(-10), ends_at: iso(120), excluded: false, priority: null, wanted: false, finished: false, claimed: 0, total: 2, progress: 0, remaining_minutes: 240, acl: [], acl_count: 0,
      drops: [drop('o1', 'Legendary Skin', 120, 0, false, '#f77f00'), drop('o2', 'Player Icon', 120, 0, false, '#264653')] },
  ];
  const log = [
    [1, 'Drops Miner 1.0.0 started.'], [2, 'Signed in as smuggz (123456789).'], [3, 'Loaded 5 campaigns.'], [4, 'Following 6 channels.'],
    [5, 'Claimed drop: Rust: Sleeping Bag (2/6)'], [6, 'Watching Shroud'],
  ].map(([seq, text]) => ({ seq, time: new Date(now - (7 - seq) * 60000).toLocaleTimeString([], { hour12: false }), text }));
  const mode = new URLSearchParams(location.search).get('mode');
  if (mode === 'login') { snap.login_needed = true; snap.signed_in = false; snap.user = null; snap.watching = null; snap.drop = null; snap.status = 'Waiting for you to sign in to Twitch'; snap.channels = []; }
  window.MockApi = {
    on() {},
    async call(cmd, args) {
      switch (cmd) {
        case 'get_snapshot': return JSON.parse(JSON.stringify(snap));
        case 'get_inventory': return inv;
        case 'get_log': return log.filter((l) => l.seq > (args?.since || 0));
        case 'get_prefs': return { close_to_tray: true, start_with_windows: true, data_dir: 'C:\\Users\\Jeremy\\AppData\\Roaming\\com.jbolton.dropsminer' };
        case 'save_settings': Object.assign(settings, args.settings); return null;
        default: return null;
      }
    },
  };
})();
