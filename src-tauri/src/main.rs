// Блокнот для Windows: окно с вашим Блокнотом на сервере, значок в трее,
// автозапуск и уведомления Windows о новых версиях Блокнота на сервере.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_window_state::StateFlags;

/// Как часто спрашивать сервер о новой версии.
const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);

/// Локальные страницы приложения (экран выбора сервера).
#[cfg(windows)]
const SETUP_URL: &str = "http://tauri.localhost/index.html";
#[cfg(not(windows))]
const SETUP_URL: &str = "tauri://localhost/index.html";

/// Ссылки на другие сайты открываются в обычном браузере, а не внутри окна Блокнота.
const LINK_SCRIPT: &str = r#"
(function () {
  function external(href) {
    try {
      var u = new URL(href, location.href);
      return u.origin !== location.origin && /^(https?|mailto):$/.test(u.protocol) ? u.href : null;
    } catch (e) {
      return null;
    }
  }
  document.addEventListener('click', function (e) {
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a || a.closest('[contenteditable="true"]')) return;
    if (a.target === '_blank' || e.ctrlKey || e.shiftKey || e.metaKey) {
      var href = external(a.href);
      if (href) {
        e.preventDefault();
        location.href = href;
      }
    }
  }, true);
  var open = window.open;
  window.open = function (u) {
    var href = u ? external(String(u)) : null;
    if (href) {
      location.href = href;
      return null;
    }
    return open.apply(window, arguments);
  };
})();
"#;

#[derive(Default, Clone, Serialize, Deserialize)]
struct Config {
    /// Адрес Блокнота, например https://notes.example.ru/
    server: Option<String>,
    /// Версия Блокнота на сервере, о которой уже сообщили.
    last_version: Option<String>,
    /// Подсказку «Блокнот продолжает работать в трее» уже показывали.
    #[serde(default)]
    tray_hint_shown: bool,
}

struct AppState {
    config: Mutex<Config>,
}

#[derive(Deserialize)]
struct Health {
    version: Option<String>,
    #[serde(default)]
    notes: Vec<String>,
}

fn config_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("config.json"))
}

fn load_config(app: &AppHandle) -> Config {
    config_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(app: &AppHandle, c: &Config) {
    if let Some(p) = config_path(app) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(c) {
            let _ = std::fs::write(p, s);
        }
    }
}

fn current_server(app: &AppHandle) -> Option<String> {
    app.state::<AppState>()
        .config
        .lock()
        .unwrap()
        .server
        .clone()
}

/// «notes.example.ru», «https://site.ru/notes/index.php#/p/5» → «https://…/notes/»
fn normalize_server(input: &str) -> Result<String, String> {
    let mut s = input.trim().to_string();
    if s.is_empty() {
        return Err("Укажите адрес Блокнота".into());
    }
    if !s.starts_with("http://") && !s.starts_with("https://") {
        s = format!("https://{s}");
    }
    let mut u = Url::parse(&s).map_err(|_| "Адрес выглядит неправильно".to_string())?;
    u.set_query(None);
    u.set_fragment(None);
    let mut path = u.path().to_string();
    for file in ["index.php", "api.php"] {
        if path.ends_with(file) {
            path.truncate(path.len() - file.len());
        }
    }
    if !path.ends_with('/') {
        path.push('/');
    }
    u.set_path(&path);
    Ok(u.to_string())
}

/// Версия Блокнота на сервере (открытый запрос api.php?a=health, вход не нужен).
fn health(server: &str) -> Result<Health, String> {
    let url = format!("{server}api.php?a=health");
    ureq::get(&url)
        .timeout(Duration::from_secs(15))
        .set("Cache-Control", "no-cache")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("сервер ответил ошибкой {code}"),
            ureq::Error::Transport(t) => format!("нет связи с сервером ({t})"),
        })?
        .into_json::<Health>()
        .map_err(|_| "по этому адресу нет Блокнота".to_string())
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn open_setup(app: &AppHandle) {
    show_main(app);
    if let (Some(w), Ok(u)) = (app.get_webview_window("main"), Url::parse(SETUP_URL)) {
        let _ = w.navigate(u);
    }
}

/// Окно показывает только сам Блокнот и локальные страницы; остальное открывается в браузере.
fn allow_navigation(app: &AppHandle, url: &Url) -> bool {
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("");
    if matches!(scheme, "tauri" | "about" | "data" | "blob") || host == "tauri.localhost" {
        return true;
    }
    if let Some(server) = current_server(app) {
        if let Ok(su) = Url::parse(&server) {
            if su.host_str() == url.host_str()
                && su.port_or_known_default() == url.port_or_known_default()
            {
                return true;
            }
        }
    }
    if matches!(scheme, "http" | "https" | "mailto") {
        let _ = app.opener().open_url(url.as_str(), None::<&str>);
    }
    false
}

/// Сравнивает версию на сервере с той, о которой уже сообщали; при новой — уведомление Windows.
fn check_updates(app: &AppHandle, manual: bool) {
    let Some(server) = current_server(app) else {
        if manual {
            notify(app, "Блокнот", "Сначала укажите адрес Блокнота.");
        }
        return;
    };
    let h = match health(&server) {
        Ok(h) => h,
        Err(e) => {
            if manual {
                notify(app, "Не удалось проверить обновления", &e);
            }
            return;
        }
    };
    let Some(version) = h.version else { return };
    let previous = {
        let state = app.state::<AppState>();
        let mut c = state.config.lock().unwrap();
        let p = c.last_version.clone();
        if p.as_deref() != Some(version.as_str()) {
            c.last_version = Some(version.clone());
            save_config(app, &c);
        }
        p
    };
    match previous {
        Some(p) if p != version => {
            let body = if h.notes.is_empty() {
                "Откройте Блокнот, чтобы увидеть изменения.".to_string()
            } else {
                h.notes
                    .iter()
                    .take(3)
                    .map(|n| format!("• {n}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            notify(app, &format!("Блокнот обновлён до версии {version}"), &body);
            if let Some(w) = app.get_webview_window("main") {
                if w.is_visible().unwrap_or(false) {
                    // в открытом окне Блокнот сам предложит «Обновить»
                    let _ = w.eval("document.dispatchEvent(new Event('visibilitychange'))");
                } else {
                    // окно спрятано в трей — тихо загружаем новую версию
                    let _ = w.eval("location.reload()");
                }
            }
        }
        _ if manual => notify(
            app,
            "Обновлений нет",
            &format!("У вас последняя версия Блокнота — {version}."),
        ),
        _ => {}
    }
}

fn start_update_checks(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        loop {
            check_updates(&app, false);
            std::thread::sleep(CHECK_EVERY);
        }
    });
}

#[tauri::command]
fn get_server(app: AppHandle) -> Option<String> {
    current_server(&app)
}

/// Экран первого запуска: проверяет адрес и открывает Блокнот.
#[tauri::command]
async fn set_server(app: AppHandle, url: String) -> Result<String, String> {
    let server = normalize_server(&url)?;
    let probe = server.clone();
    let h = tauri::async_runtime::spawn_blocking(move || health(&probe))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Не удалось открыть Блокнот по этому адресу: {e}"))?;
    {
        let state = app.state::<AppState>();
        let mut c = state.config.lock().unwrap();
        if c.server.as_deref() != Some(server.as_str()) {
            c.last_version = h.version.clone();
        }
        c.server = Some(server.clone());
        save_config(&app, &c);
    }
    if let Some(w) = app.get_webview_window("main") {
        let u = Url::parse(&server).map_err(|e| e.to_string())?;
        w.navigate(u).map_err(|e| e.to_string())?;
    }
    Ok(server)
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Открыть Блокнот", true, None::<&str>)?;
    let check = MenuItem::with_id(app, "check", "Проверить обновления", true, None::<&str>)?;
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Запускать вместе с Windows",
        true,
        autostart_on,
        None::<&str>,
    )?;
    let server = MenuItem::with_id(app, "server", "Сменить адрес Блокнота…", true, None::<&str>)?;
    let browser = MenuItem::with_id(app, "browser", "Открыть в браузере", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Выйти", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open, &check, &sep1, &autostart, &server, &browser, &sep2, &quit,
        ],
    )?;
    let autostart_item = autostart.clone();
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Блокнот")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "check" => {
                let a = app.clone();
                std::thread::spawn(move || check_updates(&a, true));
            }
            "autostart" => {
                let al = app.autolaunch();
                let was = al.is_enabled().unwrap_or(false);
                let _ = if was { al.disable() } else { al.enable() };
                let _ = autostart_item.set_checked(al.is_enabled().unwrap_or(!was));
            }
            "server" => open_setup(app),
            "browser" => {
                if let Some(s) = current_server(app) {
                    let _ = app.opener().open_url(s, None::<&str>);
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    // автозапуск с Windows: сразу в трей, без окна
    let hidden = std::env::args().any(|a| a == "--hidden");
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--hidden"])))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::all() & !StateFlags::VISIBLE)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![get_server, set_server])
        .setup(move |app| {
            let config = load_config(app.handle());
            let start = match config.server.as_deref().and_then(|s| Url::parse(s).ok()) {
                Some(u) => WebviewUrl::External(u),
                None => WebviewUrl::App("index.html".into()),
            };
            app.manage(AppState { config: Mutex::new(config) });
            let handle = app.handle().clone();
            WebviewWindowBuilder::new(app, "main", start)
                .title("Блокнот")
                .inner_size(1200.0, 820.0)
                .min_inner_size(420.0, 320.0)
                .visible(!hidden)
                .initialization_script(LINK_SCRIPT)
                .on_navigation(move |url| allow_navigation(&handle, url))
                .build()?;
            build_tray(app)?;
            start_update_checks(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // крестик прячет окно в трей: Блокнот продолжает следить за обновлениями
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    let mut c = state.config.lock().unwrap();
                    if !c.tray_hint_shown {
                        c.tray_hint_shown = true;
                        save_config(app, &c);
                        notify(app, "Блокнот работает в трее", "Окно закрыто, но Блокнот продолжает следить за обновлениями. Выйти — правой кнопкой по значку в трее.");
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("Не удалось запустить Блокнот");
}
