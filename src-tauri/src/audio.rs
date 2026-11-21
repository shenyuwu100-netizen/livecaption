//! WASAPI playback capture + adaptive-noise-floor VAD segmenter + resident whisper.cpp live captioning.
use super::*;
#[path = "capture_recovery.rs"]
mod capture_recovery;

use serde::Serialize;
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub enabled: bool,
    pub recognition_health: String,
    pub status: String,
    pub message: String,
    pub source: String,
}

#[derive(Default)]
pub struct Runtime {
    pub public: Status,
    pub revision: Option<u64>,
    pub command_id: u64,
    pub audio_fence: Option<u64>,
    pub cancel: Option<CancellationToken>,
    pub capture_health: audio_health::Tracker,
    pub recognition: audio_health::Recognition,
}

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub id: String,
    pub name: String,
}

struct Clip {
    samples: Vec<i16>,
    stamp: audio_flow::Stamp,
    capture_id: u128,
    sequence: u64,
    ended: Instant,
    continued: bool,
}

impl Clip {
    fn trace(&self) -> audio_trace::Meta {
        audio_trace::Meta {
            capture_id: self.capture_id.to_string(),
            sequence: self.sequence,
            audio_revision: self.stamp.revision,
            audio_fence: self.stamp.audio_fence,
            captured_command_id: self.stamp.command_id,
            audio_ms: (self.samples.len() as u64 * 1000) / 16000,
            continued: self.continued,
        }
    }
}

fn queue_clip(app: &AppHandle, tx: &tokio::sync::mpsc::Sender<Clip>, clip: Clip) -> bool {
    let trace = clip.trace();
    match tx.try_send(clip) {
        Ok(()) => {
            audio_trace::record(app, trace.event("captured", "queued"));
            true
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            audio_trace::record(app, trace.event("dropped", "capture_queue_full"));
            false
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            audio_trace::record(app, trace.event("dropped", "capture_queue_closed"));
            false
        }
    }
}

fn capture_stamp(state: &AppState, revision: u64) -> audio_flow::Stamp {
    state
        .audio
        .lock()
        .map(|a| audio_flow::Stamp {
            revision,
            audio_fence: a.audio_fence,
            command_id: a.command_id,
        })
        .unwrap_or(audio_flow::Stamp {
            revision,
            audio_fence: None,
            command_id: 0,
        })
}

fn current(state: &AppState) -> audio_flow::Current {
    state
        .audio
        .lock()
        .map(|a| audio_flow::Current {
            enabled: a.public.enabled,
            revision: a.revision,
            audio_fence: a.audio_fence,
            command_id: a.command_id,
            answer_busy: false,
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct Segmenter {
    samples: Vec<i16>,
    pre_roll: VecDeque<i16>,
    voiced: usize,
    last_voice: Option<Instant>,
    noise_floor: f64,
    last_cut: Option<Instant>,
}

impl Segmenter {
    fn push(&mut self, pcm: &[i16], now: Instant) -> Option<(Vec<i16>, bool)> {
        let energy =
            pcm.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / pcm.len().max(1) as f64;
        let rms = energy.sqrt();
        // Learn only quiet frames; sustained speech must never raise its own gate.
        let voiced = rms > (self.noise_floor * 2.8).clamp(110.0, 400.0);
        if !voiced && self.last_voice.is_none() && !pcm.is_empty() && rms < 180.0 {
            self.noise_floor = self.noise_floor * 0.95 + rms * 0.05;
        }
        if !voiced
            && self.samples.is_empty()
            && self
                .last_cut
                .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_millis(1200))
        {
            self.last_cut = None;
            return Some((Vec::new(), false));
        }
        if voiced {
            self.last_cut = None;
            if self.samples.is_empty() {
                self.samples.extend(self.pre_roll.drain(..));
            }
            self.last_voice = Some(now);
            self.voiced += pcm.len();
        }
        if self.last_voice.is_some() {
            self.samples.extend_from_slice(pcm);
        } else {
            self.pre_roll.extend(pcm.iter().copied());
            while self.pre_roll.len() > 3200 {
                self.pre_roll.pop_front();
            }
        }
        let expired = self
            .last_voice
            .is_some_and(|t| now.duration_since(t) >= Duration::from_millis(1200));
        let word_boundary = self.samples.len() >= 16000 * 8
            && self
                .last_voice
                .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_millis(250));
        if expired || word_boundary || self.samples.len() >= 16000 * 10 {
            let result = if self.voiced >= 4800 {
                Some((std::mem::take(&mut self.samples), !expired))
            } else {
                self.samples.clear();
                None
            };
            self.last_cut = if !expired { Some(now) } else { None };
            self.last_voice = None;
            self.voiced = 0;
            return result;
        }
        None
    }
}

pub(crate) fn parse_process_source(source: &str) -> Result<(u32, u64), String> {
    let Some((pid, stamp)) = source.split_once(':') else {
        return Err("旧应用音源已失效，请刷新音源重新选择".into());
    };
    let pid = pid.parse::<u32>().map_err(|_| "应用进程号无效")?;
    let stamp = stamp.parse::<u64>().map_err(|_| "应用身份无效")?;
    if pid == 0 || stamp == 0 {
        return Err("应用身份无效".into());
    }
    Ok((pid, stamp))
}

pub fn speech_dir() -> PathBuf {
    let adjacent = std::env::current_exe()
        .unwrap_or_default()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("speech");
    if adjacent.join("LiveCaptionSpeech.exe").exists() {
        return adjacent;
    }
    let workspace_speech = PathBuf::from(r"D:\CodexWorkspaces\LiveCaption\speech");
    if workspace_speech.join("LiveCaptionSpeech.exe").exists() {
        return workspace_speech;
    }
    adjacent
}

fn update(app: &AppHandle, cancel: &CancellationToken, status: &str, message: &str) {
    if let Ok(mut a) = app.state::<AppState>().audio.lock() {
        if cancel.is_cancelled() {
            return;
        }
        a.public.status = status.into();
        a.public.message = message.chars().take(180).collect();
    }
}

fn capture_tick(app: &AppHandle, cancel: &CancellationToken, revision: u64) {
    if let Ok(mut a) = app.state::<AppState>().audio.lock() {
        if a.revision == Some(revision) && a.public.enabled {
            a.capture_health
                .tick(revision, cancel.is_cancelled(), Instant::now());
        }
    }
}

fn capture_failed(
    app: &AppHandle,
    cancel: &CancellationToken,
    revision: u64,
    reason: &'static str,
) -> bool {
    if let Ok(mut a) = app.state::<AppState>().audio.lock() {
        if a.revision == Some(revision) && a.public.enabled {
            return a
                .capture_health
                .fail(revision, cancel.is_cancelled(), reason);
        }
    }
    false
}

fn capture_failure_cleanup(
    app: &AppHandle,
    cancel: &CancellationToken,
    revision: u64,
    error: &str,
) -> bool {
    let state = app.state::<AppState>();
    capture_recovery::cleanup_current(
        &state.audio,
        |a| a.revision == Some(revision) && a.public.enabled && !cancel.is_cancelled(),
        |a| {
            if !a.capture_health.fail(revision, false, "capture_failed") {
                return;
            }
            let _ = caption::note_gap(app, &state, error);
            a.public.status = "error".into();
            a.public.message = format!("{error}；正在重试").chars().take(180).collect();
        },
    )
}

fn recognition_result(app: &AppHandle, cancel: &CancellationToken, revision: u64, success: bool) {
    if let Ok(mut a) = app.state::<AppState>().audio.lock() {
        if a.public.enabled && a.revision == Some(revision) && !cancel.is_cancelled() {
            a.recognition.result(revision, false, success);
            a.public.recognition_health = a.recognition.health().into();
        }
    }
}

pub fn stop(state: &AppState) {
    if let Ok(mut a) = state.audio.lock() {
        if let Some(c) = a.cancel.take() {
            c.cancel();
        }
        a.capture_health.stop();
        a.recognition.stop();
        a.public.recognition_health = "off".into();
        a.public.enabled = false;
        a.public.status = "off".into();
        a.public.message = "实时字幕采集已暂停".into();
    }
}

pub fn audio_language(state: &AppState) -> String {
    state
        .settings
        .lock()
        .map(|s| s.audio_language.clone())
        .unwrap_or_else(|_| "zh".into())
}

pub fn sync(app: &AppHandle, desired: bool, revision: u64) {
    let state = app.state::<AppState>();
    {
        let Ok(mut a) = state.audio.lock() else {
            return;
        };
        if a.revision == Some(revision) {
            return;
        }
        a.revision = Some(revision);
    }
    stop(&state);
    if !desired {
        return;
    }
    let source = state
        .settings
        .lock()
        .map(|s| s.audio_source.clone())
        .unwrap_or_default();
    let cancel = CancellationToken::new();
    {
        let Ok(mut a) = state.audio.lock() else {
            return;
        };
        a.cancel = Some(cancel.clone());
        a.public = Status {
            enabled: true,
            recognition_health: "healthy".into(),
            status: "starting".into(),
            message: "正在连接电脑播放音频".into(),
            source: source.clone(),
        };
        a.recognition.start(revision);
    }
    let dir = speech_dir();
    if !dir.join("ggml-small-q5_1.bin").exists() || !dir.join("LiveCaptionSpeech.exe").exists() {
        recognition_result(app, &cancel, revision, false);
        update(
            app,
            &cancel,
            "error",
            "本机语音识别组件缺失，请检查 speech 目录",
        );
        return;
    }
    {
        let Ok(mut a) = state.audio.lock() else {
            return;
        };
        if cancel.is_cancelled() || a.revision != Some(revision) || !a.public.enabled {
            return;
        }
        a.capture_health.start(revision, true, Instant::now());
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Clip>(16);
    let handle = app.clone();
    let recording_cancel = cancel.clone();
    std::thread::spawn(move || {
        while !recording_cancel.is_cancelled() {
            match capture(&handle, &source, &recording_cancel, revision, &tx) {
                Ok(()) => break,
                Err(e) => {
                    if !capture_failure_cleanup(&handle, &recording_cancel, revision, &e) {
                        return;
                    }
                }
            }
            for _ in 0..20 {
                if recording_cancel.is_cancelled() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if capture_failed(&handle, &recording_cancel, revision, "worker_ended") {
            update(
                &handle,
                &recording_cancel,
                "error",
                "电脑采音线程已结束，请重新开启实时字幕",
            );
        }
    });

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let warm_handle = handle.clone();
        let warm_cancel = cancel.clone();
        let warm_result = tauri::async_runtime::spawn_blocking(move || {
            asr::recognize(
                &warm_handle.state::<AppState>(),
                &speech_dir(),
                &[],
                &audio_language(&warm_handle.state::<AppState>()),
                &warm_cancel,
            )
        })
        .await;
        recognition_result(
            &handle,
            &cancel,
            revision,
            matches!(warm_result, Ok(Ok(_))),
        );
        loop {
            let clip = tokio::select! {
                _ = cancel.cancelled() => break,
                clip = rx.recv() => clip,
            };
            let Some(clip) = clip else {
                break;
            };
            if clip.samples.is_empty() {
                continue;
            }
            let trace = clip.trace();
            let stale = audio_flow::skip_reason(
                clip.stamp,
                current(&handle.state::<AppState>()),
                cancel.is_cancelled(),
            );
            if let Some(reason) = stale {
                audio_trace::record(&handle, trace.event("skipped", reason));
                continue;
            }
            let duration_ms = (clip.samples.len() as u64 * 1000) / 16000;
            let capture_id = clip.capture_id;
            let sequence = clip.sequence;
            let continued = clip.continued;
            let ended = clip.ended;
            let mut event = trace.event("transcribing", "started");
            event.queue_ms = Some(ended.elapsed().as_millis() as u64);
            audio_trace::record(&handle, event);
            update(&handle, &cancel, "transcribing", "正在本机转写实时字幕");
            let job_cancel = cancel.clone();
            let job_handle = handle.clone();
            let asr_started = Instant::now();
            let result = tauri::async_runtime::spawn_blocking(move || {
                asr::recognize(
                    &job_handle.state::<AppState>(),
                    &speech_dir(),
                    &clip.samples,
                    &audio_language(&job_handle.state::<AppState>()),
                    &job_cancel,
                )
            })
            .await;
            let asr_stage_ms = asr_started.elapsed().as_millis() as u64;
            if cancel.is_cancelled() {
                break;
            }
            recognition_result(&handle, &cancel, revision, matches!(&result, Ok(Ok(_))));
            let transcript = match result {
                Ok(Ok(t)) => t,
                _ => {
                    let mut event = trace.event("skipped", "asr_failed");
                    event.asr_stage_ms = Some(asr_stage_ms);
                    audio_trace::record(&handle, event);
                    let _ = caption::note_gap(
                        &handle,
                        &handle.state::<AppState>(),
                        "语音识别失败，该片段未能生成字幕",
                    );
                    update(
                        &handle,
                        &cancel,
                        "error",
                        "语音识别失败；下一段音频将自动重试",
                    );
                    continue;
                }
            };
            let text = transcript.text;
            let decode_ms = transcript.decode_ms;
            let mut event =
                trace.event("transcribed", if text.is_empty() { "empty" } else { "ok" });
            event.asr_decode_ms = Some(decode_ms);
            event.asr_stage_ms = Some(asr_stage_ms);
            event.text_chars = Some(text.chars().count());
            audio_trace::record(&handle, event);

            match caption::record(
                &handle.state::<AppState>(),
                &text,
                duration_ms,
                decode_ms,
                continued,
                capture_id,
                sequence,
            ) {
                Ok(Some(item)) => {
                    sync::publish_caption(&handle.state::<AppState>(), revision, item);
                    update(
                        &handle,
                        &cancel,
                        "listening",
                        &format!("字幕已更新（识别耗时 {decode_ms} ms）"),
                    );
                }
                Ok(None) => {
                    update(&handle, &cancel, "listening", "正在监听播放音频");
                }
                Err(err) => {
                    update(&handle, &cancel, "error", &format!("保存字幕失败：{err}"));
                }
            }
        }
        rx.close();
    });
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[tauri::command]
pub fn get_audio_status(state: State<'_, AppState>) -> Status {
    state
        .audio
        .lock()
        .map(|a| a.public.clone())
        .unwrap_or_default()
}

#[tauri::command]
pub fn get_audio_source(state: State<'_, AppState>) -> String {
    state
        .settings
        .lock()
        .map(|s| s.audio_source.clone())
        .unwrap_or_default()
}

#[tauri::command]
pub fn save_audio_source(
    app: AppHandle,
    state: State<'_, AppState>,
    source: String,
) -> Result<(), String> {
    if source.len() > 500 {
        return Err("音源参数过长".into());
    }
    if let Some(pid) = source.strip_prefix("pid:") {
        parse_process_source(pid)?;
    }
    stop(&state);
    let mut settings = state.settings.lock().map_err(|_| "设置锁异常")?;
    settings.audio_source = source;
    persist_settings(&app, &settings)?;
    if let Ok(mut a) = state.audio.lock() {
        a.revision = None;
    }
    Ok(())
}

#[tauri::command]
pub fn get_audio_language(state: State<'_, AppState>) -> String {
    audio_language(&state)
}

#[tauri::command]
pub fn save_audio_language(
    app: AppHandle,
    state: State<'_, AppState>,
    language: String,
) -> Result<(), String> {
    if !matches!(language.as_str(), "zh" | "en" | "auto") {
        return Err("不支持的语言选项".into());
    }
    let mut settings = state.settings.lock().map_err(|_| "设置锁异常")?;
    settings.audio_language = language;
    persist_settings(&app, &settings)
}

#[tauri::command]
pub async fn list_audio_sources() -> Result<Vec<Source>, String> {
    tauri::async_runtime::spawn_blocking(enumerate_sources)
        .await
        .map_err(|_| "读取音源失败".to_string())?
}

#[cfg(windows)]
fn enumerate_sources() -> Result<Vec<Source>, String> {
    use wasapi::*;
    initialize_mta().ok().map_err(|_| "无法初始化音频系统")?;
    let result = (|| {
        let e = DeviceEnumerator::new().map_err(|_| "无法读取播放设备")?;
        let devices = e
            .get_device_collection(&Direction::Render)
            .map_err(|_| "无法读取播放设备")?;
        let mut sources = vec![
            Source {
                id: String::new(),
                name: "通用模式 · 跟随默认播放设备".into(),
            },
            Source {
                id: "tencent".into(),
                name: "腾讯会议 · 自动定位播放进程".into(),
            },
            Source {
                id: "communications".into(),
                name: "通用模式 · 默认通信设备".into(),
            },
        ];
        for device in &devices {
            let d = device.map_err(|_| "读取设备失败")?;
            sources.push(Source {
                id: d.get_id().unwrap_or_default(),
                name: d.get_friendlyname().unwrap_or_else(|_| "播放设备".into()),
            });
            if let Ok(sessions) = d
                .get_iaudiosessionmanager()
                .and_then(|m| m.get_audiosessionenumerator())
            {
                for i in 0..sessions.get_count().unwrap_or(0) {
                    if let Ok(session) = sessions.get_session(i) {
                        let pid = session.get_process_id().unwrap_or(0);
                        if pid == 0 {
                            continue;
                        }
                        let Ok((process_name, stamp)) = platform::process_identity(pid) else {
                            continue;
                        };
                        let id = format!("pid:{pid}:{stamp}");
                        if sources.iter().any(|s| s.id == id) {
                            continue;
                        }
                        let name = session.get_display_name().unwrap_or_default();
                        sources.push(Source {
                            id,
                            name: format!(
                                "只听应用 · {} (进程 {pid})",
                                if name.is_empty() {
                                    &process_name
                                } else {
                                    &name
                                }
                            ),
                        });
                    }
                }
            }
        }
        Ok(sources)
    })();
    deinitialize();
    result
}

#[cfg(not(windows))]
fn enumerate_sources() -> Result<Vec<Source>, String> {
    Err("WASAPI 播放音频采集仅支持 Windows".into())
}

fn capture(
    app: &AppHandle,
    source: &str,
    cancel: &CancellationToken,
    revision: u64,
    tx: &tokio::sync::mpsc::Sender<Clip>,
) -> Result<(), String> {
    let resolved = if source == "tencent" {
        let (pid, stamp) = platform::meeting_process()?;
        format!("pid:{pid}:{stamp}")
    } else {
        source.to_string()
    };
    let mut worker = capture_worker::Capture::start(&resolved)?;
    let mut segmenter = Segmenter::default();
    let mut segment_stamp = capture_stamp(&app.state::<AppState>(), revision);
    let capture_id = now_millis();
    let mut sequence = 0;
    while !cancel.is_cancelled() {
        let Some((kind, data)) = worker.next()? else {
            continue;
        };
        let pcm = match kind {
            capture_worker::READY => {
                if let Ok(mut a) = app.state::<AppState>().audio.lock() {
                    if !cancel.is_cancelled() && a.revision == Some(revision) {
                        a.public.source =
                            String::from_utf8_lossy(&data).chars().take(180).collect();
                    }
                }
                update(app, cancel, "listening", "正在监听播放音频（麦克风未开启）");
                capture_tick(app, cancel, revision);
                continue;
            }
            capture_worker::ERROR => {
                return Err(String::from_utf8_lossy(&data).chars().take(180).collect())
            }
            _ => data
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        };
        if segmenter.last_voice.is_none() {
            segment_stamp = capture_stamp(&app.state::<AppState>(), revision);
        }
        if let Some((samples, continued)) = segmenter.push(&pcm, Instant::now()) {
            sequence += 1;
            if !queue_clip(
                app,
                tx,
                Clip {
                    samples,
                    stamp: segment_stamp,
                    capture_id,
                    sequence,
                    ended: Instant::now(),
                    continued,
                },
            ) {
                let _ = caption::note_gap(
                    app,
                    &app.state::<AppState>(),
                    "语音缓冲队列已满，有音频片段未能生成字幕",
                );
                update(app, cancel, "error", "语音缓冲队列已满，字幕存在缺口");
            }
        }
        capture_tick(app, cancel, revision);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_source_parser_validates_pid_and_creation_timestamp() {
        assert_eq!(parse_process_source("pid:4120:987654321".strip_prefix("pid:").unwrap()), Ok((4120, 987654321)));
        assert!(parse_process_source("0:123").is_err());
        assert!(parse_process_source("4120:0").is_err());
        assert!(parse_process_source("invalid").is_err());
    }

    #[test]
    fn segmenter_cuts_voiced_utterance_after_silence_pause() {
        let mut seg = Segmenter::default();
        let t0 = Instant::now();
        // Feed 0.4s of 16kHz loud sine/square samples (6400 samples > 4800 minimum voiced threshold)
        let voiced_frame = vec![2000i16; 1600];
        for i in 0..4 {
            assert!(seg.push(&voiced_frame, t0 + Duration::from_millis(i * 100)).is_none());
        }
        // Feed silence 1.3s later -> triggers utterance boundary
        let silent_frame = vec![0i16; 1600];
        let cut = seg.push(&silent_frame, t0 + Duration::from_millis(1700));
        assert!(cut.is_some());
        let (samples, continued) = cut.unwrap();
        assert!(!continued);
        assert!(samples.len() >= 6400);
    }
}
