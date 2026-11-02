mod config;
mod motion;
mod onvif;
mod stream;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::webview::WebviewWindowBuilder;
use tauri::{Manager, RunEvent, State, WebviewUrl, WebviewWindow};

use config::CameraConfig;
use onvif::{ConnectionReport, OnvifEndpoint, Preset};
use stream::{Hub, Stats, StreamEngine};

struct PtzWatch {
    last: Mutex<Option<Instant>>,
    onvif: Arc<Mutex<Option<OnvifEndpoint>>>,
}

impl PtzWatch {
    fn touch(&self) {
        *self.last.lock().expect("ptz") = Some(Instant::now());
    }

    fn cancel(&self) {
        *self.last.lock().expect("ptz") = None;
    }

    async fn tick(&self) {
        let due = {
            let mut last = self.last.lock().expect("ptz");
            match *last {
                Some(at) if at.elapsed() > Duration::from_millis(1100) => {
                    *last = None;
                    true
                }
                _ => false,
            }
        };
        if !due {
            return;
        }
        let endpoint = self.onvif.lock().expect("onvif").clone();
        if let Some(endpoint) = endpoint {
            let _ = endpoint.stop().await;
        }
    }
}

pub struct AppState {
    config: Mutex<Option<CameraConfig>>,
    engine: Arc<Mutex<StreamEngine>>,
    hub: Arc<Hub>,
    onvif: Arc<Mutex<Option<OnvifEndpoint>>>,
    ptz: Arc<PtzWatch>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CameraInfo {
    manufacturer: String,
    model: String,
    firmware: String,
    profile_token: String,
}

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let onvif = Arc::new(Mutex::new(None));
    let ptz = Arc::new(PtzWatch {
        last: Mutex::new(None),
        onvif: onvif.clone(),
    });
    let ptz_task = Arc::clone(&ptz);
    let initial = config::load().ok().flatten();
    let state = AppState {
        config: Mutex::new(initial.clone()),
        engine: Arc::new(Mutex::new(StreamEngine::new())),
        hub: Arc::new(Hub::new()),
        onvif,
        ptz,
    };

    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    ptz_task.tick().await;
                }
            });
            if let Some(cfg) = initial {
                let app_state = app.state::<AppState>();
                app_state
                    .hub
                    .configure_motion(
                        cfg.motion,
                        cfg.motion_borders,
                        cfg.motion_sensitivity,
                        &cfg.name,
                    );
                let engine = Arc::clone(&app_state.engine);
                let hub = Arc::clone(&app_state.hub);
                std::thread::spawn(move || {
                    if let Ok(mut engine) = engine.lock() {
                        engine.start(cfg, hub);
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            test_connection,
            start_stream,
            stop_stream,
            watch_frames,
            stream_stats,
            camera_info,
            ptz_move,
            ptz_stop,
            ptz_home,
            ptz_presets,
            ptz_goto,
            ptz_save,
            save_snapshot,
            toggle_fullscreen,
            pop_out_view,
            set_view_on_top,
            set_motion,
            set_borders,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Camera Live")
        .run(|app, event| {
            if matches!(event, RunEvent::Exit) {
                if let Some(state) = app.try_state::<AppState>() {
                    if let Ok(mut engine) = state.engine.lock() {
                        engine.stop();
                    }
                }
            }
        });
}

fn restart(state: &AppState, cfg: CameraConfig) {
    let engine = Arc::clone(&state.engine);
    let hub = Arc::clone(&state.hub);
    std::thread::spawn(move || {
        if let Ok(mut engine) = engine.lock() {
            engine.start(cfg, hub);
        }
    });
}

async fn ensure_onvif(state: &AppState) -> Result<OnvifEndpoint, String> {
    if let Some(endpoint) = state.onvif.lock().expect("onvif").clone() {
        return Ok(endpoint);
    }
    let cfg = state
        .config
        .lock()
        .expect("config")
        .clone()
        .ok_or_else(|| "Add the camera in settings first.".to_string())?;
    let endpoint = OnvifEndpoint::connect(&cfg).await?;
    *state.onvif.lock().expect("onvif") = Some(endpoint.clone());
    Ok(endpoint)
}

#[tauri::command]
fn load_config(state: State<'_, AppState>) -> Option<CameraConfig> {
    state.config.lock().expect("config").clone()
}

#[tauri::command]
fn save_config(state: State<'_, AppState>, config: CameraConfig) -> Result<CameraConfig, String> {
    let config = config::normalize(config)?;
    config::save(&config)?;
    *state.config.lock().expect("config") = Some(config.clone());
    *state.onvif.lock().expect("onvif") = None;
    state.ptz.cancel();
    state
        .hub
        .configure_motion(
            config.motion,
            config.motion_borders,
            config.motion_sensitivity,
            &config.name,
        );
    restart(&state, config.clone());
    Ok(config)
}

#[tauri::command]
async fn test_connection(config: CameraConfig) -> Result<ConnectionReport, String> {
    let config = config::normalize(config)?;
    Ok(onvif::test_connection(&config).await)
}

#[tauri::command]
fn start_stream(state: State<'_, AppState>) -> Result<(), String> {
    let cfg = state
        .config
        .lock()
        .expect("config")
        .clone()
        .ok_or_else(|| "Add the camera in settings first.".to_string())?;
    restart(&state, cfg);
    Ok(())
}

#[tauri::command]
fn stop_stream(state: State<'_, AppState>) {
    if let Ok(mut engine) = state.engine.lock() {
        engine.stop();
    }
}

#[tauri::command]
fn watch_frames(state: State<'_, AppState>, frames: Channel<InvokeResponseBody>) {
    state.hub.subscribe(frames);
}

#[tauri::command]
fn stream_stats(state: State<'_, AppState>) -> Stats {
    state.hub.stats()
}

#[tauri::command]
async fn camera_info(state: State<'_, AppState>) -> Result<CameraInfo, String> {
    let endpoint = ensure_onvif(&state).await?;
    Ok(CameraInfo {
        manufacturer: endpoint.manufacturer,
        model: endpoint.model,
        firmware: endpoint.firmware,
        profile_token: endpoint.profile.clone(),
    })
}

#[tauri::command]
async fn ptz_move(state: State<'_, AppState>, pan: f64, tilt: f64) -> Result<(), String> {
    let endpoint = ensure_onvif(&state).await?;
    state.ptz.touch();
    state.hub.suppress_motion(Duration::from_secs(2));
    endpoint.continuous_move(pan, tilt).await
}

#[tauri::command]
async fn ptz_stop(state: State<'_, AppState>) -> Result<(), String> {
    state.ptz.cancel();
    let endpoint = state.onvif.lock().expect("onvif").clone();
    if let Some(endpoint) = endpoint {
        endpoint.stop().await?;
    }
    Ok(())
}

#[tauri::command]
async fn ptz_home(state: State<'_, AppState>) -> Result<(), String> {
    state.ptz.cancel();
    state.hub.suppress_motion(Duration::from_secs(8));
    ensure_onvif(&state).await?.home().await
}

#[tauri::command]
async fn ptz_presets(state: State<'_, AppState>) -> Result<Vec<Preset>, String> {
    ensure_onvif(&state).await?.presets().await
}

#[tauri::command]
async fn ptz_goto(state: State<'_, AppState>, token: String) -> Result<(), String> {
    state.ptz.cancel();
    state.hub.suppress_motion(Duration::from_secs(8));
    ensure_onvif(&state).await?.goto_preset(&token).await
}

#[tauri::command]
async fn ptz_save(state: State<'_, AppState>, name: String) -> Result<(), String> {
    ensure_onvif(&state).await?.set_preset(&name).await
}

#[tauri::command]
fn save_snapshot(state: State<'_, AppState>) -> Result<String, String> {
    let jpeg = state
        .hub
        .latest()
        .ok_or_else(|| "Wait for a picture from the camera first.".to_string())?;
    let dir = dirs::picture_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| "Could not find a Pictures folder.".to_string())?
        .join("Camera Live");
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let name = state
        .config
        .lock()
        .expect("config")
        .as_ref()
        .map(|cfg| cfg.name.clone())
        .unwrap_or_else(|| "tapo".into());
    let slug: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "tapo" } else { slug };
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("{slug}-{stamp}.jpg"));
    std::fs::write(&path, jpeg).map_err(|err| err.to_string())?;
    Ok(path.display().to_string())
}

#[tauri::command]
fn toggle_fullscreen(window: WebviewWindow) -> Result<bool, String> {
    let next = !window.is_fullscreen().unwrap_or(false);
    window.set_fullscreen(next).map_err(|err| err.to_string())?;
    Ok(next)
}

#[tauri::command]
fn pop_out_view(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("view") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, "view", WebviewUrl::App("view.html".into()))
        .title("Camera view")
        .inner_size(960.0, 540.0)
        .min_inner_size(420.0, 240.0)
        .background_color(tauri::window::Color(7, 8, 7, 255))
        .build()
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[tauri::command]
fn set_view_on_top(window: WebviewWindow, on: bool) -> Result<(), String> {
    window.set_always_on_top(on).map_err(|err| err.to_string())
}

#[tauri::command]
fn set_motion(state: State<'_, AppState>, enabled: bool, sensitivity: u8) -> Result<(), String> {
    let sensitivity = sensitivity.clamp(1, 10);
    let (name, borders) = {
        let mut config = state.config.lock().expect("config");
        if let Some(cfg) = config.as_mut() {
            cfg.motion = enabled;
            cfg.motion_sensitivity = sensitivity;
            config::save(cfg)?;
            (cfg.name.clone(), cfg.motion_borders)
        } else {
            ("Tapo C200".into(), false)
        }
    };
    state
        .hub
        .configure_motion(enabled, borders, sensitivity, &name);
    Ok(())
}

#[tauri::command]
fn set_borders(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    let (notify, sensitivity, name) = {
        let mut config = state.config.lock().expect("config");
        if let Some(cfg) = config.as_mut() {
            cfg.motion_borders = enabled;
            config::save(cfg)?;
            (cfg.motion, cfg.motion_sensitivity, cfg.name.clone())
        } else {
            (true, 5, "Tapo C200".into())
        }
    };
    state
        .hub
        .configure_motion(notify, enabled, sensitivity, &name);
    Ok(())
}
