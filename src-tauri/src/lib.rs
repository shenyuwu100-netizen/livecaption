//! LiveCaption: Local-first real-time accessibility captioning and grounded meeting summary verifier.
use reqwest::{redirect::Policy, Client};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, sync::Mutex};
use tauri::{AppHandle, Manager, State};
use tokio_util::sync::CancellationToken;
use url::Url;

pub mod asr;
pub mod audio;
pub mod audio_flow;
pub mod audio_health;
pub mod audio_trace;
pub mod caption;
pub mod capture_worker;
pub mod launch;
pub mod platform;
pub mod relay_socket;
pub mod routing;
pub mod sync;
pub mod web_viewer;

fn default_audio_language() -> String {
    "zh".into()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSettings {
    #[serde(default = "default_audio_language")]
    pub audio_language: String,
    #[serde(default)]
    pub audio_source: String,
    #[serde(default)]
    pub consent_acknowledged: bool,
}

pub struct AppState {
    pub caption: Mutex<caption::Runtime>,
    pub recognizer: Mutex<Option<asr::Engine>>,
    pub audio: Mutex<audio::Runtime>,
    pub sync: Mutex<sync::Runtime>,
    pub client: Client,
    pub settings: Mutex<LocalSettings>,
}

impl AppState {
    pub fn new() -> Result<Self, String> {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("LiveCaption/0.1.0")
            .build()
            .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

        Ok(Self {
            caption: Mutex::new(caption::Runtime::default()),
            recognizer: Mutex::new(None),
            audio: Mutex::new(audio::Runtime::default()),
            sync: Mutex::new(sync::Runtime::default()),
            client,
            settings: Mutex::new(LocalSettings::default()),
        })
    }
}

pub fn app_config_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法获取应用配置目录: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建应用配置目录: {e}"))?;
    Ok(dir)
}

fn settings_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(app_config_dir(app)?.join("settings.json"))
}

fn load_local_state(app: &AppHandle, state: &AppState) {
    if let Ok(path) = settings_path(app) {
        if let Ok(raw) = fs::read_to_string(path) {
            if let Ok(settings) = serde_json::from_str::<LocalSettings>(&raw) {
                if let Ok(mut guard) = state.settings.lock() {
                    *guard = settings;
                }
            }
        }
    }
}

pub fn persist_settings(app: &AppHandle, settings: &LocalSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    let raw = serde_json::to_string_pretty(settings).map_err(|e| format!("序列化设置失败: {e}"))?;
    platform::atomic_write(&path, raw.as_bytes())
}

pub fn validate_base_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("Base URL 无效: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Base URL 不能包含用户名或密码".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "Base URL 缺少主机名".to_string())?;
    let is_local = matches!(host, "localhost" | "127.0.0.1" | "::1");
    match url.scheme() {
        "https" => {}
        "http" if is_local => {}
        _ => return Err("API 仅允许 HTTPS；本机 localhost/127.0.0.1 可使用 HTTP".into()),
    }
    Ok(url)
}

pub fn chat_endpoint(base_url: &str) -> Result<Url, String> {
    let url = validate_base_url(base_url)?;
    let normalized = url.as_str().trim_end_matches('/');
    if normalized.ends_with("/chat/completions") {
        return Url::parse(normalized).map_err(|e| format!("API 地址无效: {e}"));
    }
    if normalized.ends_with("/v1") {
        return Url::parse(&format!("{normalized}/chat/completions"))
            .map_err(|e| format!("API 地址无效: {e}"));
    }
    Url::parse(&format!("{normalized}/v1/chat/completions"))
        .map_err(|e| format!("API 地址无效: {e}"))
}

pub fn extract_stream_text(value: &Value) -> Option<String> {
    value
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
        })
        .map(ToOwned::to_owned)
}

pub fn consume_sse_chunk(buffer: &mut String, chunk: &str) -> Vec<String> {
    buffer.push_str(chunk);
    let mut deltas = Vec::new();
    while let Some(line_end) = buffer.find('\n') {
        let line = buffer[..line_end].trim().to_string();
        buffer.drain(..=line_end);
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            if let Some(text) = extract_stream_text(&value) {
                if !text.is_empty() {
                    deltas.push(text);
                }
            }
        }
    }
    deltas
}

#[tauri::command]
fn get_caption_status(state: State<'_, AppState>) -> caption::PublicStatus {
    caption::snapshot(&state)
}

#[tauri::command]
fn get_captions(state: State<'_, AppState>, query: Option<String>) -> Vec<caption::CaptionItem> {
    let items = caption::list_items(&state);
    match query {
        Some(q) if !q.trim().is_empty() => caption::search_captions(&items, &q),
        _ => items,
    }
}

#[tauri::command]
fn clear_captions(state: State<'_, AppState>) -> caption::PublicStatus {
    caption::clear_items(&state)
}

#[tauri::command]
fn export_captions(
    state: State<'_, AppState>,
    path: String,
    format: String,
) -> Result<String, String> {
    caption::export_to_path(&state, &path, &format)
}

#[tauri::command]
fn verify_meeting_summary(
    state: State<'_, AppState>,
    summary: String,
) -> caption::SummaryVerification {
    let items = caption::list_items(&state);
    caption::verify_summary(&summary, &items)
}

#[tauri::command]
async fn generate_meeting_summary(
    state: State<'_, AppState>,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> Result<caption::GeneratedSummary, String> {
    let items = caption::list_items(&state);
    let client = state.client.clone();
    caption::generate_and_verify(
        &client,
        &items,
        base_url.as_deref(),
        api_key.as_deref(),
        model.as_deref(),
    )
    .await
}

#[tauri::command]
fn hide_control(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.hide().map_err(|e| format!("无法隐藏窗口: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    let state = app.state::<AppState>();
    audio::stop(&state);
    if let Ok(mut recognizer) = state.recognizer.lock() {
        *recognizer = None;
    }
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mode = launch::LaunchMode::from_args(std::env::args());
    let _instance = match platform::instance(mode) {
        Ok(Some(lock)) => lock,
        Ok(None) => return,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    let state = AppState::new().expect("failed to initialize application state");

    tauri::Builder::default()
        .manage(state)
        .manage(mode)
        .setup(move |app| {
            let state = app.state::<AppState>();
            load_local_state(app.handle(), &state);
            let _ = caption::initialize(app.handle(), &state);
            web_viewer::spawn_local_viewer(app.handle().clone());

            if mode.create_tray() {
                let show = tauri::menu::MenuItem::with_id(
                    app,
                    "show",
                    "打开 LiveCaption 字幕工作台",
                    true,
                    None::<&str>,
                )?;
                let pause = tauri::menu::MenuItem::with_id(
                    app,
                    "pause",
                    "暂停实时字幕",
                    true,
                    None::<&str>,
                )?;
                let quit = tauri::menu::MenuItem::with_id(
                    app,
                    "quit",
                    "退出程序",
                    true,
                    None::<&str>,
                )?;
                let menu = tauri::menu::Menu::with_items(app, &[&show, &pause, &quit])?;
                let mut tray = tauri::tray::TrayIconBuilder::with_id("livecaption-tray")
                    .tooltip("LiveCaption · 本地实时字幕与无障碍同步")
                    .menu(&menu)
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "show" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                        "pause" => {
                            let state = app.state::<AppState>();
                            audio::stop(&state);
                        }
                        "quit" => {
                            quit_app(app.clone());
                        }
                        _ => {}
                    });
                if let Some(icon) = app.default_window_icon() {
                    tray = tray.icon(icon.clone());
                }
                tray.build(app)?;
            }

            if mode.show_control_on_launch() {
                if let Some(window) = app.get_webview_window("main") {
                    window.show()?;
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            audio::get_audio_status,
            audio::get_audio_source,
            audio::list_audio_sources,
            audio::save_audio_source,
            audio::get_audio_language,
            audio::save_audio_language,
            sync::get_sync_status,
            sync::set_caption_listening,
            get_caption_status,
            get_captions,
            clear_captions,
            export_captions,
            verify_meeting_summary,
            generate_meeting_summary,
            hide_control,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running LiveCaption");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_endpoint_appends_openai_path() {
        assert_eq!(
            chat_endpoint("https://example.com").unwrap().as_str(),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_endpoint("https://example.com/v1").unwrap().as_str(),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn insecure_remote_http_is_rejected() {
        assert!(validate_base_url("http://example.com/v1").is_err());
        assert!(validate_base_url("http://127.0.0.1:11434/v1").is_ok());
    }

    #[test]
    fn sse_parser_handles_split_chunks() {
        let mut buffer = String::new();
        let first = "data: {\"choices\":[{\"delta\":{\"content\":\"第一条纪要\"}}]}\n";
        let second_a = "data: {\"choices\":[{\"delta\":{\"content\":\" [#1";
        let second_b = "]\"}}]}\n\ndata: [DONE]\n";

        let mut output = consume_sse_chunk(&mut buffer, first);
        assert_eq!(output, vec!["第一条纪要"]);
        output.extend(consume_sse_chunk(&mut buffer, second_a));
        output.extend(consume_sse_chunk(&mut buffer, second_b));
        assert_eq!(output, vec!["第一条纪要", " [#1]"]);
        assert!(buffer.is_empty());
    }
}
