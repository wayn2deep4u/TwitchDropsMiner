# Drops Miner

Drops Miner is a Windows tray app that mines Twitch drops in the background. It's a rebuild of
[TwitchDropsMiner by DevilXD](https://github.com/DevilXD/TwitchDropsMiner) (MIT) using [Tauri 2](https://tauri.app):
a Rust engine plus a window made of plain HTML, CSS and JavaScript (no npm). The look is mxops Ingress with Twitch's
purple as the accent.

It follows the original's rules: it never downloads or plays video. It "watches" one live channel at a time by
fetching that stream's playlist and touching its newest chunks, the same way the original does. Twitch's live updates
(PubSub) report drop progress and claims. Every finished drop is claimed automatically.

## What it does

* **Sign-in.** A Twitch sign-in window opens (private: no cookies or passwords are kept). When Twitch redirects to
  `https://www.twitch.tv/#access_token=…`, the app takes the token and closes the window. Like the original, it signs
  in as the Twitch Android app. The token, user ID and device ID are kept in `session.json` in the data folder.
* **Inventory.** Loads your in-progress drops and every active or upcoming campaign, with each campaign's details
  (batches of 20). It claims anything that's ready, then reloads every hour and whenever a campaign or drop starts or
  ends.
* **Which games.** From your priority list and priority mode (*Priority list only*, *Ending soonest*,
  *Least time to spare*). Excluded games are skipped. Only campaigns that can be earned within the next hour count.
* **Which channels.** Each queued game's allow-listed channels (checked for being live) plus up to 20 live channels
  with drops on per game, up to 199 in total. They're sorted by game priority, then allow-listed first, then viewers.
  The miner picks the best one and switches when a better one goes live or the current one stops qualifying.
  **Watch** on the Channels page pins a channel.
* **Progress.** Drop progress comes from PubSub. When it goes quiet, the app asks Twitch's GQL for the current drop.
  As a last resort it estimates a minute, up to 15, then tries another channel (all as upstream).
* **Window.** Overview (now mining, countdowns, up next), Channels, Inventory (filters, link-account buttons,
  add-to-priority and exclude), Activity (the log), Settings and Account (sign-in, live connections).
* **Tray.** Closing the window keeps mining in the tray (switchable in Settings). The tray icon's diamond shows the
  state: purple = mining, grey = idle, amber = loading, red = a problem, white = starting. Start with Windows starts
  it in the tray. A Windows notification is shown for each claimed drop (switchable). Only one copy runs at a time.

Data folder: `%APPDATA%\com.jbolton.dropsminer\` holds `settings.json`, `session.json`, `app.json` (window
preferences) and `miner.log` (rolled over at 2 MB). **Settings › Open data folder** opens it.

## Folders

| Path | What's in it |
|---|---|
| `Cargo.toml` | Builds the two Rust parts below together (a Cargo workspace). |
| `core/` | The engine, with no window: Twitch API (`twitch.rs`), HTTP + GQL with retries and the rate limit (`http.rs`), PubSub websockets (`pubsub.rs`), campaigns/drops/channels and their rules (`models.rs`), the state machine, watch loop and claiming (`engine.rs`), settings (`settings.rs`). `cargo test -p drops-miner-core` runs its tests. |
| `src-tauri/` | The app: window, tray, sign-in window, notifications, start with Windows, and the commands the window calls (`src/lib.rs`). Icons in `icons/`. |
| `ui/` | The window: `index.html`, `app.css` (Ingress design, Twitch purple), `app.js`. Titillium Web, Inter and IBM Plex Mono are bundled in `ui/fonts/` with their SIL Open Font License files. |
| `tools/preview/` | Open `index.html` in a browser to see the window with sample data. |

## Building

**On Windows** (one time): install [Rust](https://rustup.rs) (choose the "MSVC" option; it installs the Visual Studio
C++ build tools). Windows 10/11 already has WebView2. Then double-click **`build.bat`**. It installs the Tauri CLI the
first time (a few minutes), builds, and opens the folder with the installer
(`target\release\bundle\nsis\Drops Miner_1.0.0_x64-setup.exe`).

To try it without installing: **`dev.bat`** (`cargo tauri dev`) opens the app straight from the source.

**On GitHub:** pushing changes under `drops-miner/` runs *Drops Miner for Windows* (Actions tab). It tests the
engine and builds the installer, which is under the run's **Artifacts**.

## Keeping up with DevilXD's fixes

Twitch changes its private API every so often and DevilXD's miner gets fixed first. After merging his changes into
your fork (GitHub Desktop: **Fetch origin**, then **Branch › Merge into current branch › upstream/master**), look at
what changed in the Python files and port it:

| Upstream (Python) | Here (Rust) |
|---|---|
| `constants.py`: `ClientType`, `GQL_QUERIES` hashes and variables, `WEBSOCKET_TOPICS`, intervals | `core/src/twitch.rs` (most fixes are a hash or a user agent here) |
| `twitch.py`: `_AuthState` sign-in | `core/src/engine.rs` `sign_in`, `src-tauri/src/lib.rs` `open_login`, `twitch::login_url` |
| `twitch.py`: `request`, `gql_request` | `core/src/http.rs` |
| `twitch.py`: `_run` state machine, `_watch_loop`, `fetch_inventory`, `process_*` | `core/src/engine.rs` (same names in snake case) |
| `inventory.py`, `channel.py` | `core/src/models.rs`; `Channel.send_watch` is `Engine::send_watch` |
| `websocket.py` | `core/src/pubsub.rs` |

Bump the version in `core/Cargo.toml`, `src-tauri/Cargo.toml` and `src-tauri/tauri.conf.json`, then build.

## Differences from the original

* The window is new. The rules for what to mine, when to switch and when to claim follow upstream.
* Sign-in uses the app's own private browser window instead of starting Chrome through zendriver.
* A problem (Twitch down, an unexpected answer) is logged and retried after 30 s, growing to 5 min, instead of
  stopping the miner. A sign-in Twitch no longer accepts asks for a new sign-in.
* A pinned channel (**Watch**) stays pinned while it can be mined; **Let the miner choose** un-pins it.
* The proxy setting applies to Twitch's web requests. The live-update websockets always connect directly.
* Not ported: the language files (the window is in English), `--dump`, the old username/password sign-in.

## Can only be confirmed on Windows, with a Twitch account

The engine's rules are covered by tests (`cargo test -p drops-miner-core`), and the window was checked with sample
data. These parts talk to Twitch and need a first real run:

1. The sign-in window picks up the token and closes by itself. If it doesn't, the Activity page says so. Close the
   window and use **Account › Sign in** again.
2. After sign-in, **Activity** shows "Loaded N campaigns" and "Following N channels", and Overview shows a channel
   being watched.
3. Within a few minutes the drop's minutes go up (Overview and Inventory), and the countdown restarts each minute.
4. A finished drop shows "Claimed drop: …" in Activity, with a Windows notification.
