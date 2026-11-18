//! Drops Miner: the window, the tray icon, notifications, start with Windows, the Twitch sign-in window and the
//! commands the window calls. All the real work (Twitch, drops, channels, claiming) lives in the
//! drops-miner-core crate (../core); this file connects it to Tauri.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drops_miner_core::engine::{CampaignView, Engine, LogLine, Sink, Snapshot, TrayIcon};
use drops_miner_core::settings::Settings;
use drops_miner_core::twitch::{login_url, token_from_url};
use serde::{Deserialize, Serialize};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_notification::NotificationExt as _;
use tauri_plugin_opener::OpenerExt as _;

/// What the app keeps while it runs.
pub struct App {
    engine: Arc<Engine>,
    dir: PathBuf,
    prefs: Mutex<Prefs>,
    run: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

/// Window preferences, kept next to the miner's settings (app.json).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    /// Closing the window keeps mining in the tray (otherwise closing quits).
    close_to_tray: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { close_to_tray: true }
    }
}

impl Prefs {
    fn load(dir: &Path) -> Prefs {
        std::fs::read_to_string(dir.join("app.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    fn save(&self, dir: &Path) {
        let _ = std::fs::write(dir.join("app.json"), serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

// ---------------------------------------------------------------------------------------------------------------
// How the engine reaches the window

struct TauriSink {
    app: AppHandle,
    dirty: Arc<AtomicBool>,
}

fn tray_image(icon: TrayIcon) -> Option<Image<'static>> {
    let bytes: &'static [u8] = match icon {
        TrayIcon::Active => include_bytes!("../icons/tray-active.png"),
        TrayIcon::Idle => include_bytes!("../icons/tray-idle.png"),
        TrayIcon::Maint => include_bytes!("../icons/tray-maint.png"),
        TrayIcon::Error => include_bytes!("../icons/tray-error.png"),
        TrayIcon::Pickaxe => include_bytes!("../icons/tray-pickaxe.png"),
    };
    Image::from_bytes(bytes).ok()
}

impl Sink for TauriSink {
    fn changed(&self) {
        // many changes arrive at once; a timer sends one refresh to the window (see `run`)
        self.dirty.store(true, Ordering::Relaxed);
    }
    fn log(&self, line: &LogLine) {
        let _ = self.app.emit("miner-log", line);
    }
    fn notify(&self, title: &str, body: &str) {
        let _ = self.app.notification().builder().title(title).body(body).show();
    }
    fn login_needed(&self, url: &str) {
        let app = self.app.clone();
        let url = url.to_string();
        let _ = self.app.run_on_main_thread(move || {
            show_main(&app);
            open_login(&app, &url);
        });
    }
    fn tray(&self, icon: TrayIcon, tooltip: &str) {
        let app = self.app.clone();
        let tip = tooltip.to_string();
        let _ = self.app.run_on_main_thread(move || {
            if let Some(tray) = app.tray_by_id("main") {
                if let Some(img) = tray_image(icon) {
                    let _ = tray.set_icon(Some(img));
                }
                let _ = tray.set_tooltip(Some(tip));
            }
        });
        self.dirty.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The Twitch sign-in window

/// Opens Twitch's sign-in page in its own window. When Twitch redirects to
/// https://www.twitch.tv/#access_token=…, the token goes to the engine and the window closes.
/// The window is private (nothing is remembered) and can't call any of the app's commands.
fn open_login(app: &AppHandle, url: &str) {
    if let Some(w) = app.get_webview_window("login") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let Ok(parsed) = url.parse() else { return };
    let taken = Arc::new(AtomicBool::new(false));
    let handoff = {
        let app = app.clone();
        let taken = taken.clone();
        move |token: String| {
            if taken.swap(true, Ordering::SeqCst) {
                return;
            }
            app.state::<App>().engine.submit_token(token);
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Some(w) = app2.get_webview_window("login") {
                    let _ = w.close();
                }
                show_main(&app2);
            });
        }
    };
    let on_nav = handoff.clone();
    let built = WebviewWindowBuilder::new(app, "login", WebviewUrl::External(parsed))
        .title("Sign in to Twitch · Drops Miner")
        .inner_size(500.0, 760.0)
        .center()
        .incognito(true)
        .on_navigation(move |u| {
            if let Some(token) = token_from_url(u.as_str()) {
                on_nav(token);
                return false;
            }
            true
        })
        .build();
    let Ok(window) = built else { return };
    // backup: some redirects only change the address's #part, which isn't always reported as a navigation
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        if taken.load(Ordering::SeqCst) {
            return;
        }
        match window.url() {
            Ok(u) => {
                if let Some(token) = token_from_url(u.as_str()) {
                    handoff(token);
                    return;
                }
            }
            Err(_) => return, // the window was closed
        }
    });
}

// ---------------------------------------------------------------------------------------------------------------
// Commands the window calls

#[tauri::command]
fn get_snapshot(app: State<'_, App>) -> Snapshot {
    app.engine.snapshot()
}

#[tauri::command]
fn get_inventory(app: State<'_, App>) -> Vec<CampaignView> {
    app.engine.inventory()
}

#[tauri::command]
fn get_log(since: u64, app: State<'_, App>) -> Vec<LogLine> {
    app.engine.log_since(since)
}

#[derive(Serialize)]
struct AppPrefsOut {
    close_to_tray: bool,
    start_with_windows: bool,
    data_dir: String,
}

#[tauri::command]
fn get_prefs(app: State<'_, App>, handle: AppHandle) -> AppPrefsOut {
    let p = app.prefs.lock().unwrap().clone();
    AppPrefsOut {
        close_to_tray: p.close_to_tray,
        start_with_windows: handle.autolaunch().is_enabled().unwrap_or(false),
        data_dir: app.dir.display().to_string(),
    }
}

#[tauri::command]
fn save_prefs(close_to_tray: bool, start_with_windows: bool, app: State<'_, App>, handle: AppHandle) -> Result<(), String> {
    {
        let mut p = app.prefs.lock().unwrap();
        p.close_to_tray = close_to_tray;
        p.save(&app.dir);
    }
    let auto = handle.autolaunch();
    let on = auto.is_enabled().unwrap_or(false);
    if start_with_windows && !on {
        auto.enable().map_err(|e| format!("Couldn't turn on start with Windows: {e}"))?;
    } else if !start_with_windows && on {
        auto.disable().map_err(|e| format!("Couldn't turn off start with Windows: {e}"))?;
    }
    Ok(())
}

/// Saves the miner's settings. `reload`: apply them now (reloads the inventory).
#[tauri::command]
fn save_settings(settings: Settings, reload: bool, app: State<'_, App>) {
    app.engine.update_settings(|s| *s = settings);
    if reload {
        app.engine.reload();
    }
}

#[tauri::command]
fn sign_in(app: State<'_, App>, handle: AppHandle) {
    let url = app.engine.login_url().unwrap_or_else(login_url);
    open_login(&handle, &url);
}

#[tauri::command]
fn sign_out(app: State<'_, App>) {
    app.engine.logout();
}

#[tauri::command]
fn reload(app: State<'_, App>) {
    app.engine.reload();
}

/// Watch this channel (None: let the miner choose).
#[tauri::command]
fn switch_channel(id: Option<u64>, app: State<'_, App>) {
    app.engine.switch_to(id);
}

/// Opens a Twitch page (or a campaign's account-linking page) in the default browser. Only https.
#[tauri::command]
fn open_url(url: String, handle: AppHandle) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("Only https links can be opened.".into());
    }
    handle.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_data_folder(app: State<'_, App>, handle: AppHandle) -> Result<(), String> {
    handle.opener().open_path(app.dir.display().to_string(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn quit(handle: AppHandle) {
    quit_app(&handle);
}

// ---------------------------------------------------------------------------------------------------------------

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Stops the miner (it saves its state), then exits. Gives up waiting after a few seconds.
fn quit_app(app: &AppHandle) {
    let state = app.state::<App>();
    state.engine.exit();
    let run = state.run.lock().unwrap().take();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(run) = run {
            let _ = tokio::time::timeout(Duration::from_secs(4), run).await;
        }
        app.exit(0);
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    // Only one Drops Miner at a time: starting it again just opens the running one's window. Must be registered first.
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)));
    }

    builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_inventory,
            get_log,
            get_prefs,
            save_prefs,
            save_settings,
            sign_in,
            sign_out,
            reload,
            switch_channel,
            open_url,
            open_data_folder,
            quit
        ])
        .setup(|app| {
            let dir = app.path().app_data_dir().unwrap_or_else(|_| PathBuf::from("."));
            let _ = std::fs::create_dir_all(&dir);
            let dirty = Arc::new(AtomicBool::new(true));
            let engine = Engine::new(dir.clone(), Arc::new(TauriSink { app: app.handle().clone(), dirty: dirty.clone() }));
            let run = tauri::async_runtime::spawn(engine.clone().run());
            app.manage(App { engine, prefs: Mutex::new(Prefs::load(&dir)), dir, run: Mutex::new(Some(run)) });

            // at most four window refreshes a second, however busy the miner is
            let h = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                if dirty.swap(false, Ordering::Relaxed) {
                    let _ = h.emit("miner-changed", ());
                }
            });

            // the tray icon and its menu
            let open = MenuItem::with_id(app, "open", "Open Drops Miner", true, None::<&str>)?;
            let reload = MenuItem::with_id(app, "reload", "Reload campaigns", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Drops Miner", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &reload, &sep, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main").tooltip("Drops Miner").menu(&menu).show_menu_on_left_click(false);
            if let Some(icon) = tray_image(TrayIcon::Pickaxe).or_else(|| app.default_window_icon().cloned()) {
                tray = tray.icon(icon);
            }
            tray.on_menu_event(|app, event| match event.id.as_ref() {
                "open" => show_main(app),
                "reload" => app.state::<App>().engine.reload(),
                "quit" => quit_app(app),
                _ => {}
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                    show_main(tray.app_handle());
                }
            })
            .build(app)?;

            // the window starts hidden (tauri.conf.json); it opens unless Windows started the app at sign-in
            if !std::env::args().any(|a| a == "--minimized") {
                show_main(app.handle());
            }
            Ok(())
        })
        // closing the main window keeps mining in the tray (unless that's switched off)
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let to_tray = window.app_handle().state::<App>().prefs.lock().unwrap().close_to_tray;
                if to_tray {
                    let _ = window.hide();
                } else {
                    quit_app(window.app_handle());
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("Drops Miner couldn't start");
}
