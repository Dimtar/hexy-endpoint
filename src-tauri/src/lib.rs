mod platform;
mod remote;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const KEYRING_SERVICE: &str = "hexy-endpoint";
const KEYRING_ACCOUNT: &str = "smb-password";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    host: String,
    username: String,
    /// Mount every share the server offers (the default).
    map_all: bool,
    /// Shares to mount when `map_all` is off.
    selected: BTreeSet<String>,
    autostart: bool,
    auto_update: bool,
    /// Shares this app mounted itself. Only these are ever unmounted, so a
    /// share the user mounted by hand is left alone.
    managed: BTreeSet<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            username: String::new(),
            map_all: true,
            selected: BTreeSet::new(),
            autostart: true,
            auto_update: true,
            managed: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ShareView {
    name: String,
    comment: String,
    wanted: bool,
    mounted: bool,
    location: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
struct Inner {
    config: Config,
    shares: Vec<ShareView>,
    syncing: bool,
    error: Option<String>,
    last_sync: Option<u64>,
    update: Option<String>,
}

struct Shared {
    inner: Mutex<Inner>,
    config_path: PathBuf,
    trigger: UnboundedSender<()>,
}

#[derive(Clone, Serialize)]
struct Snapshot {
    config: Config,
    has_password: bool,
    shares: Vec<ShareView>,
    syncing: bool,
    error: Option<String>,
    last_sync: Option<u64>,
    update: Option<String>,
    version: String,
}

fn keyring_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(|e| e.to_string())
}

fn load_password() -> Option<String> {
    keyring_entry().ok()?.get_password().ok()
}

fn key(name: &str) -> String {
    name.to_lowercase()
}

fn save_config(shared: &Shared, config: &Config) {
    if let Some(dir) = shared.config_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(&shared.config_path, json);
    }
}

fn snapshot(app: &AppHandle) -> Snapshot {
    let shared = app.state::<Shared>();
    let inner = shared.inner.lock().unwrap();
    Snapshot {
        config: inner.config.clone(),
        has_password: load_password().is_some(),
        shares: inner.shares.clone(),
        syncing: inner.syncing,
        error: inner.error.clone(),
        last_sync: inner.last_sync,
        update: inner.update.clone(),
        version: app.package_info().version.to_string(),
    }
}

/// Pushes the current state to the window and the tray tooltip.
fn publish(app: &AppHandle) {
    let snap = snapshot(app);
    let mounted = snap.shares.iter().filter(|s| s.mounted).count();
    let tooltip = if snap.config.host.is_empty() || !snap.has_password {
        "Hexy Endpoint: not signed in".to_string()
    } else if let Some(err) = &snap.error {
        format!("Hexy Endpoint: {err}")
    } else {
        format!(
            "Hexy Endpoint: {mounted} share{} mounted from {}",
            if mounted == 1 { "" } else { "s" },
            snap.config.host
        )
    };
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(&tooltip));
    }
    let _ = app.emit("state", snap);
}

struct SyncResult {
    shares: Vec<ShareView>,
    managed: BTreeSet<String>,
}

/// One reconciliation pass: ask the server which shares exist, mount the
/// wanted ones that are missing, and unmount ones we mounted that are no
/// longer wanted or no longer exist.
async fn reconcile(config: &Config, password: &str) -> Result<SyncResult, String> {
    let remote = remote::list_shares(&config.host, &config.username, password).await?;
    let wanted: BTreeSet<String> = remote
        .iter()
        .filter(|s| config.map_all || config.selected.iter().any(|sel| key(sel) == key(&s.name)))
        .map(|s| key(&s.name))
        .collect();

    let before: BTreeMap<String, platform::Mounted> = platform::mounted(&config.host)
        .into_iter()
        .map(|m| (key(&m.share), m))
        .collect();
    let mut managed: BTreeSet<String> = config.managed.iter().map(|s| key(s)).collect();
    let mut errors: BTreeMap<String, String> = BTreeMap::new();

    for share in &remote {
        let k = key(&share.name);
        if wanted.contains(&k) && !before.contains_key(&k) {
            match platform::mount(&config.host, &share.name, &config.username, password) {
                Ok(()) => {
                    managed.insert(k);
                }
                Err(e) => {
                    errors.insert(k, e);
                }
            }
        }
    }
    for (k, m) in &before {
        if managed.contains(k) && !wanted.contains(k) {
            match platform::unmount(&config.host, m) {
                Ok(()) => {
                    managed.remove(k);
                }
                Err(e) => {
                    errors.insert(k.clone(), e);
                }
            }
        }
    }

    let after: BTreeMap<String, platform::Mounted> = platform::mounted(&config.host)
        .into_iter()
        .map(|m| (key(&m.share), m))
        .collect();
    managed.retain(|k| after.contains_key(k));

    let shares = remote
        .into_iter()
        .map(|s| {
            let k = key(&s.name);
            let m = after.get(&k);
            ShareView {
                wanted: wanted.contains(&k),
                mounted: m.is_some(),
                location: m.map(|m| m.location.clone()),
                error: errors.remove(&k),
                name: s.name,
                comment: s.comment,
            }
        })
        .collect();
    Ok(SyncResult { shares, managed })
}

async fn sync_once(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let config = shared.inner.lock().unwrap().config.clone();
    if config.host.is_empty() || config.username.is_empty() {
        return;
    }
    let Some(password) = load_password() else {
        return;
    };
    shared.inner.lock().unwrap().syncing = true;
    publish(app);

    let result = reconcile(&config, &password).await;
    {
        let mut inner = shared.inner.lock().unwrap();
        inner.syncing = false;
        inner.last_sync = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        match result {
            Ok(r) => {
                inner.shares = r.shares;
                inner.error = None;
                // Settings may have been saved while we were syncing; only
                // the bookkeeping of what we mounted is ours to write back.
                if inner.config.managed != r.managed {
                    inner.config.managed = r.managed;
                    save_config(&shared, &inner.config);
                }
            }
            Err(e) => inner.error = Some(e),
        }
    }
    publish(app);
}

/// Runs on its own thread with its own runtime: one sync per minute, plus
/// one whenever something sends on the trigger channel.
fn spawn_sync_loop(app: AppHandle, mut rx: tokio::sync::mpsc::UnboundedReceiver<()>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("sync runtime");
        rt.block_on(async move {
            loop {
                sync_once(&app).await;
                tokio::select! {
                    _ = rx.recv() => {}
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
                // Collapse a burst of triggers into a single pass.
                while rx.try_recv().is_ok() {}
            }
        });
    });
}

/// Checks GitHub Releases for a newer signed build. With `install`, downloads
/// it, swaps it in and restarts; mounts are owned by the OS and survive that.
async fn check_update(app: &AppHandle, install: bool) -> Result<Option<String>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        app.state::<Shared>().inner.lock().unwrap().update = None;
        return Ok(None);
    };
    let version = update.version.clone();
    app.state::<Shared>().inner.lock().unwrap().update = Some(version.clone());
    publish(app);
    if install {
        update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(|e| e.to_string())?;
        app.restart();
    }
    Ok(Some(version))
}

fn spawn_update_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let auto = app.state::<Shared>().inner.lock().unwrap().config.auto_update;
            if let Err(e) = check_update(&app, auto).await {
                eprintln!("update check failed: {e}");
            }
            tokio::time::sleep(UPDATE_INTERVAL).await;
        }
    });
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn apply_autostart(app: &AppHandle, enabled: bool) {
    // A dev build would register the debug binary to run at login.
    if cfg!(debug_assertions) {
        return;
    }
    let launcher = app.autolaunch();
    let result = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    if let Err(e) = result {
        eprintln!("autostart: {e}");
    }
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

#[derive(Deserialize)]
struct SettingsInput {
    host: String,
    username: String,
    password: Option<String>,
    map_all: bool,
    selected: BTreeSet<String>,
    autostart: bool,
    auto_update: bool,
}

#[tauri::command]
fn get_state(app: AppHandle) -> Snapshot {
    snapshot(&app)
}

#[tauri::command]
fn save_settings(app: AppHandle, input: SettingsInput) -> Result<Snapshot, String> {
    let host = input.host.trim().to_string();
    let username = input.username.trim().to_string();
    if !valid_host(&host) {
        return Err("Enter the server's IP address or hostname, without smb:// or slashes.".into());
    }
    if username.is_empty() {
        return Err("Enter a username.".into());
    }
    let shared = app.state::<Shared>();
    let password = input.password.filter(|p| !p.is_empty());
    let server_changed = {
        let inner = shared.inner.lock().unwrap();
        !inner.config.host.eq_ignore_ascii_case(&host) || inner.config.username != username
    };
    if password.is_none() && (server_changed || load_password().is_none()) {
        return Err("Enter the password for this account.".into());
    }
    if server_changed {
        unmount_managed(&app);
    }
    if let Some(password) = password {
        keyring_entry()?
            .set_password(&password)
            .map_err(|e| format!("Could not store the password in the system keychain: {e}"))?;
    }
    {
        let mut inner = shared.inner.lock().unwrap();
        if server_changed {
            inner.shares.clear();
            inner.error = None;
        }
        inner.config.host = host;
        inner.config.username = username;
        inner.config.map_all = input.map_all;
        inner.config.selected = input.selected;
        inner.config.autostart = input.autostart;
        inner.config.auto_update = input.auto_update;
        save_config(&shared, &inner.config);
    }
    apply_autostart(&app, input.autostart);
    let _ = shared.trigger.send(());
    Ok(snapshot(&app))
}

/// Unmounts everything this app mounted from the current server.
fn unmount_managed(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let mut inner = shared.inner.lock().unwrap();
    let host = inner.config.host.clone();
    if host.is_empty() {
        return;
    }
    for m in platform::mounted(&host) {
        if inner.config.managed.contains(&key(&m.share)) {
            let _ = platform::unmount(&host, &m);
        }
    }
    inner.config.managed.clear();
    save_config(&shared, &inner.config);
}

#[tauri::command]
fn sign_out(app: AppHandle) -> Snapshot {
    unmount_managed(&app);
    if let Ok(entry) = keyring_entry() {
        let _ = entry.delete_credential();
    }
    {
        let shared = app.state::<Shared>();
        let mut inner = shared.inner.lock().unwrap();
        inner.shares.clear();
        inner.error = None;
    }
    publish(&app);
    snapshot(&app)
}

#[tauri::command]
fn sync_now(app: AppHandle) {
    let _ = app.state::<Shared>().trigger.send(());
}

#[tauri::command]
fn open_share(app: AppHandle, name: String) -> Result<(), String> {
    let host = app.state::<Shared>().inner.lock().unwrap().config.host.clone();
    let m = platform::mounted(&host)
        .into_iter()
        .find(|m| key(&m.share) == key(&name))
        .ok_or("That share is not mounted.")?;
    platform::open(&host, &m)
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<Option<String>, String> {
    check_update(&app, true).await
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_settings,
            sign_out,
            sync_now,
            open_share,
            install_update
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let config_path = app.path().app_config_dir()?.join("config.json");
            let config: Config = std::fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let (tx, rx) = unbounded_channel();
            let signed_in = !config.host.is_empty() && load_password().is_some();
            let autostart = config.autostart;
            app.manage(Shared {
                inner: Mutex::new(Inner {
                    config,
                    ..Default::default()
                }),
                config_path,
                trigger: tx,
            });

            // Tray-only: no dock icon on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let open = MenuItem::with_id(app, "open", "Open Hexy Endpoint", true, None::<&str>)?;
            let sync = MenuItem::with_id(app, "sync", "Sync Now", true, None::<&str>)?;
            let update =
                MenuItem::with_id(app, "update", "Check for Updates", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open,
                    &sync,
                    &PredefinedMenuItem::separator(app)?,
                    &update,
                    &PredefinedMenuItem::separator(app)?,
                    &quit,
                ],
            )?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("Hexy Endpoint")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show_window(app),
                    "sync" => {
                        let _ = app.state::<Shared>().trigger.send(());
                    }
                    "update" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = check_update(&app, true).await;
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            apply_autostart(&handle, autostart);
            spawn_sync_loop(handle.clone(), rx);
            spawn_update_loop(handle.clone());

            // Launched at login it stays in the tray; otherwise, or when
            // there is nothing configured yet, show the window.
            let hidden = std::env::args().any(|a| a == "--hidden");
            if !hidden || !signed_in {
                show_window(&handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the app running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Hexy Endpoint");
}
