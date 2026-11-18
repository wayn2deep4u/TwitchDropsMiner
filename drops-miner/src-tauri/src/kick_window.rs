//! The Kick side of the app: a hidden browser window on kick.com that makes Kick's requests and holds the viewer
//! websocket (Kick's firewall only lets real browsers through), and the window where you sign in to Kick.
//!
//! The hidden window never gets access to the app's commands. Scripts are run in it with `eval`, results come back
//! through `eval_with_callback`, and the page script reads the sign-in cookie and sends it with each request.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use drops_miner_core::kick::{BoxFut, KickBridge, KickResp, KickSink, CLIENT_TOKEN};
use serde_json::{json, Value};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_notification::NotificationExt as _;

pub const WORKER: &str = "kick-worker";
pub const LOGIN: &str = "kick-login";
/// A light page on kick.com: the hidden window needs to be on Kick's site, but not run Kick's web app or video.
const WORKER_URL: &str = "https://kick.com/robots.txt";

pub struct KickWindow {
    app: AppHandle,
    seq: AtomicU64,
    ready: Arc<AtomicBool>,
    /// The sign-in window is open: the hidden window is closed meanwhile, so only one Kick page exists at a time.
    login_open: Arc<AtomicBool>,
    trace: Arc<Trace>,
}

/// A small step log in the app's data folder (kick-debug.log), plus a watchdog that notes when the app's main
/// thread stops answering, and what the Kick side was doing just before.
pub struct Trace {
    path: std::path::PathBuf,
    last: Mutex<String>,
}

impl Trace {
    pub fn step(&self, what: &str) {
        *self.last.lock().unwrap() = what.to_string();
        self.write(what);
    }
    fn write(&self, what: &str) {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{} {what}", chrono::Local::now().format("%H:%M:%S%.3f"));
        }
    }
}

impl KickWindow {
    pub fn new(app: AppHandle, dir: &std::path::Path) -> Arc<KickWindow> {
        let path = dir.join("kick-debug.log");
        // start each run with a fresh log
        let _ = std::fs::write(&path, "");
        let trace = Arc::new(Trace {
            path,
            last: Mutex::new("start".into()),
        });
        trace.step("app started");
        let (t, h) = (trace.clone(), app.clone());
        std::thread::spawn(move || {
            let mut stuck = false;
            loop {
                let answered = Arc::new(AtomicBool::new(false));
                let a = answered.clone();
                if h.run_on_main_thread(move || a.store(true, Ordering::SeqCst))
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(Duration::from_secs(3));
                let ok = answered.load(Ordering::SeqCst);
                if !ok && !stuck {
                    let last = t.last.lock().unwrap().clone();
                    t.write(&format!("MAIN THREAD NOT ANSWERING (last step: {last})"));
                } else if ok && stuck {
                    t.write("main thread answering again");
                }
                stuck = !ok;
            }
        });
        Arc::new(KickWindow {
            app,
            seq: AtomicU64::new(1),
            ready: Arc::new(AtomicBool::new(false)),
            login_open: Arc::new(AtomicBool::new(false)),
            trace,
        })
    }

    /// The hidden window, created (on the main thread) and loaded on first use.
    async fn worker(&self) -> Result<WebviewWindow, String> {
        if self.login_open.load(Ordering::SeqCst) {
            return Err("waiting for the Kick sign-in window to close".into());
        }
        let w = match self.app.get_webview_window(WORKER) {
            Some(w) => w,
            None => {
                self.ready.store(false, Ordering::SeqCst);
                self.trace.step("creating hidden Kick window");
                let (app, ready) = (self.app.clone(), self.ready.clone());
                self.app
                    .run_on_main_thread(move || {
                        if app.get_webview_window(WORKER).is_some() {
                            return;
                        }
                        let built = WebviewWindowBuilder::new(
                            &app,
                            WORKER,
                            WebviewUrl::External(WORKER_URL.parse().unwrap()),
                        )
                        .title("Kick (Bonkr Twitch Miner)")
                        .visible(false)
                        .skip_taskbar(true)
                        .inner_size(400.0, 300.0)
                        .on_page_load(move |_w, p| {
                            if p.event() == PageLoadEvent::Finished
                                && p.url().host_str() == Some("kick.com")
                            {
                                ready.store(true, Ordering::SeqCst);
                            }
                        })
                        .build();
                        if let Err(e) = built {
                            eprintln!("couldn't open the Kick window: {e}");
                        }
                    })
                    .map_err(|e| e.to_string())?;
                let mut found = None;
                for _ in 0..100 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if let Some(w) = self.app.get_webview_window(WORKER) {
                        found = Some(w);
                        break;
                    }
                }
                self.trace.step("hidden Kick window created");
                found.ok_or("couldn't open the hidden Kick window")?
            }
        };
        for _ in 0..100 {
            if self.ready.load(Ordering::SeqCst) {
                return Ok(w);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Kick may be showing a browser check; reloading usually gets past it
        let _ = w.eval(format!("location.href = '{WORKER_URL}'"));
        Err(
            "Kick's site didn't load. If it keeps happening, open Account › Sign in to Kick once."
                .into(),
        )
    }

    /// Runs a script in the hidden window and returns what it evaluates to.
    async fn eval(w: &WebviewWindow, js: String) -> Result<Value, String> {
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        let tx = Mutex::new(Some(tx));
        w.eval_with_callback(js, move |res| {
            if let Some(t) = tx.lock().unwrap().take() {
                let _ = t.send(res);
            }
        })
        .map_err(|e| e.to_string())?;
        let text = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| "the Kick window didn't answer".to_string())?
            .map_err(|e| e.to_string())?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// The Kick sign-in token from the browser's cookie store. Only used off Windows: on Windows, WebView2's cookie
    /// call runs a message loop inside the app's main thread and can lock the whole app up (white windows, stuck at
    /// "Updating"). There the page script reads the cookie itself instead.
    #[cfg(not(target_os = "windows"))]
    async fn session_token(&self, w: &WebviewWindow) -> Option<String> {
        let w = w.clone();
        let job = tauri::async_runtime::spawn_blocking(move || {
            w.cookies_for_url("https://kick.com".parse().unwrap()).ok()
        });
        let cookies = tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .ok()?
            .ok()
            .flatten()?;
        cookies
            .iter()
            .find(|c| c.name() == "session_token")
            .map(|c| percent_decode(c.value()))
    }

    #[cfg(target_os = "windows")]
    async fn session_token(&self, _w: &WebviewWindow) -> Option<String> {
        None
    }

    /// Opens the Kick sign-in window. The hidden window is closed first so only one Kick page runs at a time, and
    /// the window is created on the app's main thread (creating it inside a command freezes the app on Windows).
    /// Closing the sign-in window lets the miner check the sign-in.
    pub fn open_login(&self) {
        self.trace.step("opening Kick sign-in window");
        self.login_open.store(true, Ordering::SeqCst);
        let (app, flag, trace) = (
            self.app.clone(),
            self.login_open.clone(),
            self.trace.clone(),
        );
        // Queued from a separate thread: called on the main thread, run_on_main_thread runs the closure on the spot,
        // inside the button's command, and building a window there is what froze the app on Windows.
        let handle = self.app.clone();
        std::thread::spawn(move || {
            handle.run_on_main_thread(move || {
            if let Some(w) = app.get_webview_window(LOGIN) {
                let _ = w.show();
                let _ = w.set_focus();
                return;
            }
            if let Some(worker) = app.get_webview_window(WORKER) {
                trace.step("closing hidden Kick window");
                let _ = worker.destroy();
            }
            trace.step("building Kick sign-in window");
            let built = WebviewWindowBuilder::new(
                &app,
                LOGIN,
                WebviewUrl::External("https://kick.com/".parse().unwrap()),
            )
            .title("Sign in to Kick (Log In at the top right), then close this window · Bonkr Twitch Miner")
            .inner_size(1100.0, 800.0)
            .center()
            .build();
            match built {
                Ok(w) => {
                    trace.step("Kick sign-in window built");
                    let (app2, trace2) = (app.clone(), trace.clone());
                    w.on_window_event(move |e| {
                        if let tauri::WindowEvent::Destroyed = e {
                            trace2.step("Kick sign-in window closed");
                            flag.store(false, Ordering::SeqCst);
                            let _ = app2.emit("kick-login-closed", ());
                        }
                    });
                }
                Err(e) => {
                    trace.step(&format!("Kick sign-in window failed: {e}"));
                    flag.store(false, Ordering::SeqCst);
                }
            }
        })
        });
    }

    pub fn close_login(&self) {
        if let Some(w) = self.app.get_webview_window(LOGIN) {
            let _ = w.close();
        }
    }

    /// Signs out of Kick by clearing the Kick browser data.
    pub async fn sign_out(&self) -> Result<(), String> {
        let w = self.worker().await?;
        w.clear_all_browsing_data().map_err(|e| e.to_string())?;
        let _ = w.eval(format!("location.href = '{WORKER_URL}'"));
        Ok(())
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl KickBridge for KickWindow {
    fn paused(&self) -> bool {
        self.login_open.load(Ordering::SeqCst)
    }

    fn fetch(
        &self,
        method: &'static str,
        url: String,
        body: Option<String>,
        client_token: bool,
    ) -> BoxFut<'_, Result<KickResp, String>> {
        Box::pin(async move {
            let w = self.worker().await?;
            self.trace.step(&format!("request {method} {url}"));
            let id = self.seq.fetch_add(1, Ordering::Relaxed);
            let mut headers = json!({ "Accept": "application/json, text/plain, */*" });
            if client_token {
                headers["X-Client-Token"] = json!(CLIENT_TOKEN);
            }
            if body.is_some() {
                headers["Content-Type"] = json!("application/json");
            }
            if let Some(t) = self.session_token(&w).await {
                headers["Authorization"] = json!(format!("Bearer {t}"));
            }
            // Start the request in the page. Its result is parked in window.__bonkr until collected.
            let start = format!(
                r#"(() => {{
  const D = window.__bonkr || (window.__bonkr = {{ res: {{}}, ws: null, wsState: 'closed' }});
  const h = {headers};
  const x = (document.cookie.match(/(?:^|; )XSRF-TOKEN=([^;]+)/) || [])[1];
  if (x) h['X-XSRF-TOKEN'] = decodeURIComponent(x);
  const st = (document.cookie.match(/(?:^|; )session_token=([^;]+)/) || [])[1];
  if (st && !h['Authorization']) h['Authorization'] = 'Bearer ' + decodeURIComponent(st);
  const opts = (cred) => ({{ method: {method}, headers: h, body: {body}, credentials: cred }});
  const done = async (r) => {{ D.res[{id}] = {{ s: r.status, t: await r.text() }}; }};
  fetch({url}, opts('include')).then(done)
    .catch(() => fetch({url}, opts('omit')).then(done))
    .catch((e) => {{ D.res[{id}] = {{ e: String(e) }}; }});
  return 1;
}})()"#,
                headers = headers,
                method = json!(method),
                body = body
                    .map(|b| json!(b).to_string())
                    .unwrap_or_else(|| "undefined".into()),
                url = json!(url),
                id = id,
            );
            Self::eval(&w, start).await?;
            let collect = format!("(() => {{ const D = window.__bonkr; if (!D || !D.res[{id}]) return null; const v = D.res[{id}]; delete D.res[{id}]; return v; }})()");
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let v = Self::eval(&w, collect.clone()).await?;
                if v.is_null() {
                    continue;
                }
                if let Some(e) = v["e"].as_str() {
                    return Err(format!("Kick request failed ({e})"));
                }
                return Ok(KickResp {
                    status: v["s"].as_u64().unwrap_or(0) as u16,
                    body: v["t"].as_str().unwrap_or("").to_string(),
                });
            }
            Err("Kick took too long to answer".into())
        })
    }

    fn ws_open(&self, token: String) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async move {
            let w = self.worker().await?;
            let js = format!(
                r#"(() => {{
  const D = window.__bonkr || (window.__bonkr = {{ res: {{}}, ws: null, wsState: 'closed' }});
  try {{ if (D.ws) D.ws.close(); }} catch (e) {{}}
  D.wsState = 'connecting';
  const ws = new WebSocket('wss://websockets.kick.com/viewer/v1/connect?token=' + encodeURIComponent({token}));
  D.ws = ws;
  ws.onopen = () => {{ if (D.ws === ws) D.wsState = 'open'; }};
  ws.onclose = () => {{ if (D.ws === ws) D.wsState = 'closed'; }};
  return 1;
}})()"#,
                token = json!(token),
            );
            Self::eval(&w, js).await.map(|_| ())
        })
    }

    fn ws_send(&self, message: String) -> BoxFut<'_, Result<String, String>> {
        Box::pin(async move {
            let w = self.worker().await?;
            let js = format!(
                r#"(() => {{
  const D = window.__bonkr;
  if (!D || !D.ws) return 'closed';
  if (D.ws.readyState === 1) {{ D.ws.send({msg}); return 'open'; }}
  return D.wsState || 'closed';
}})()"#,
                msg = json!(message),
            );
            Ok(Self::eval(&w, js)
                .await?
                .as_str()
                .unwrap_or("closed")
                .to_string())
        })
    }

    fn ws_close(&self) {
        if let Some(w) = self.app.get_webview_window(WORKER) {
            let _ = w.eval("(() => { const D = window.__bonkr; if (D && D.ws) { try { D.ws.close(); } catch (e) {} D.ws = null; D.wsState = 'closed'; } })()");
        }
    }
}

/// How the Kick engine reaches the window: refreshes, the shared Activity log, notifications, sign-in done.
pub struct KickToWindow {
    pub app: AppHandle,
    pub dirty: Arc<AtomicBool>,
    pub log: Arc<dyn Fn(String) + Send + Sync>,
    pub kick: Arc<KickWindow>,
}

impl KickSink for KickToWindow {
    fn changed(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }
    fn log(&self, text: String) {
        (self.log)(text);
    }
    fn notify(&self, title: &str, body: &str) {
        let _ = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show();
    }
    fn signed_in(&self, _username: &str) {
        let kick = self.kick.clone();
        let _ = self.app.run_on_main_thread(move || kick.close_login());
        let _ = self.app.emit("kick-signed-in", ());
    }
}

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn decodes_cookie_values() {
        assert_eq!(percent_decode("123%7Cabc"), "123|abc");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("end%7"), "end%7");
    }
}
