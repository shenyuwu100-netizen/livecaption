//! Cross-device live-caption synchronization with monotonic version fencing and heartbeat telemetry.
use super::{audio, caption::CaptionItem, AppState};
#[path = "desktop_control.rs"]
pub mod desktop_control;
#[path = "desktop_telemetry.rs"]
pub mod desktop_telemetry;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSnapshot {
    pub enabled: bool,
    pub connected: bool,
    pub endpoint: String,
    pub revision: u64,
    pub synced_captions: u64,
    pub last_caption: Option<CaptionItem>,
    pub message: String,
}

#[derive(Default)]
pub struct Runtime {
    pub enabled: bool,
    pub connected: bool,
    pub endpoint: String,
    pub revision: u64,
    pub synced_captions: u64,
    pub last_caption: Option<CaptionItem>,
    pub pending_control: desktop_control::Pending,
    pub transitions: desktop_telemetry::ConnectionTransitions,
    pub message: String,
}

impl Runtime {
    pub fn snapshot(&self) -> SyncSnapshot {
        SyncSnapshot {
            enabled: self.enabled,
            connected: self.connected,
            endpoint: if self.endpoint.is_empty() {
                "http://127.0.0.1:8788/captions".into()
            } else {
                self.endpoint.clone()
            },
            revision: self.revision,
            synced_captions: self.synced_captions,
            last_caption: self.last_caption.clone(),
            message: if self.message.is_empty() {
                "本地字幕流就绪；可同步至局域网/云端无障碍阅读页".into()
            } else {
                self.message.clone()
            },
        }
    }

    /// Apply a remote or local state version update only if `incoming_revision >= self.revision`.
    pub fn apply_versioned_caption(
        &mut self,
        incoming_revision: u64,
        item: CaptionItem,
    ) -> Result<bool, &'static str> {
        if !desktop_telemetry::accepts_command(1, 1, false, Some(incoming_revision), self.revision)
        {
            return Err("stale_revision_rejected");
        }
        self.revision = incoming_revision;
        self.synced_captions = self.synced_captions.saturating_add(1);
        self.last_caption = Some(item);
        Ok(true)
    }
}

pub fn publish_caption(state: &AppState, revision: u64, item: CaptionItem) {
    if let Ok(mut rt) = state.sync.lock() {
        let _ = rt.apply_versioned_caption(revision, item);
    }
}

#[tauri::command]
pub fn get_sync_status(state: State<'_, AppState>) -> SyncSnapshot {
    state
        .sync
        .lock()
        .map(|rt| rt.snapshot())
        .unwrap_or_default()
}

#[tauri::command]
pub async fn set_caption_listening(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<SyncSnapshot, String> {
    let revision = {
        let mut rt = state.sync.lock().map_err(|_| "同步状态锁异常")?;
        let rev = rt.revision.saturating_add(1);
        rt.revision = rev;
        rt.enabled = enabled;
        let _ = rt.pending_control.issue(
            1,
            enabled,
            desktop_control::Version::Session(rev),
            "livecaption",
        );
        rt.message = if enabled {
            "本地实时字幕采集中（WASAPI 播放音频 -> 常驻 whisper.cpp）".into()
        } else {
            "实时字幕已暂停".into()
        };
        rt.revision
    };
    audio::sync(&app, enabled, revision);
    state
        .sync
        .lock()
        .map(|rt| rt.snapshot())
        .map_err(|_| "同步状态锁异常".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_revision_prevents_out_of_order_caption_overwrite() {
        let mut rt = Runtime::default();
        let first = CaptionItem {
            id: 1,
            start_ms: 1000,
            end_ms: 2500,
            timestamp: "2026-11-21 20:00:01".into(),
            text: "第一条实时字幕".into(),
            decode_ms: 1720,
            continued: false,
        };
        let second = CaptionItem {
            id: 2,
            start_ms: 2600,
            end_ms: 4200,
            timestamp: "2026-11-21 20:00:03".into(),
            text: "第二条更新字幕".into(),
            decode_ms: 1690,
            continued: false,
        };
        assert_eq!(rt.apply_versioned_caption(5, second.clone()), Ok(true));
        assert_eq!(
            rt.apply_versioned_caption(4, first),
            Err("stale_revision_rejected")
        );
        assert_eq!(rt.revision, 5);
        assert_eq!(rt.last_caption.unwrap().text, "第二条更新字幕");
    }
}
