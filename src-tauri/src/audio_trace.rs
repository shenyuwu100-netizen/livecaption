//! Bounded, local, metadata-only speech diagnostics. Never accepts transcript/audio payloads.
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::Mutex,
};
use tauri::AppHandle;

const MAX_FILE_BYTES: u64 = 512 * 1024;
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub capture_id: String,
    pub sequence: u64,
    pub audio_revision: u64,
    pub audio_fence: Option<u64>,
    pub captured_command_id: u64,
    pub audio_ms: u64,
    pub continued: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event<'a> {
    #[serde(flatten)]
    pub meta: &'a Meta,
    pub stage: &'static str,
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_decode_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_stage_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_chars: Option<usize>,
}

// Construct through this method rather than accepting arbitrary text in diagnostics.
impl Meta {
    pub fn event(&self, stage: &'static str, reason: &'static str) -> Event<'_> {
        Event {
            meta: self,
            stage,
            reason,
            command_id: None,
            queue_ms: None,
            asr_decode_ms: None,
            asr_stage_ms: None,
            submit_ms: None,
            wait_ms: None,
            text_chars: None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope<'a, 'b> {
    schema_version: u8,
    at_ms: u64,
    pid: u32,
    #[serde(flatten)]
    event: &'a Event<'b>,
}

fn append_bounded(directory: &Path, bytes: &[u8], limit: u64) -> io::Result<()> {
    if bytes.len() as u64 + 1 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic event exceeds limit",
        ));
    }
    fs::create_dir_all(directory)?;
    let path = directory.join("audio-segments.jsonl");
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) + bytes.len() as u64 + 1 > limit {
        let previous = directory.join("audio-segments.previous.jsonl");
        match fs::remove_file(&previous) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::rename(&path, previous)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    file.write_all(b"\n")
}

pub fn record(app: &AppHandle, event: Event<'_>) {
    let Ok(_guard) = WRITE_LOCK.lock() else {
        return;
    };
    let envelope = Envelope {
        schema_version: 1,
        at_ms: now_millis(),
        pid: std::process::id(),
        event: &event,
    };
    let result = super::app_config_dir(app)
        .ok()
        .zip(serde_json::to_vec(&envelope).ok())
        .map(|(root, bytes)| append_bounded(&root.join("diagnostics"), &bytes, MAX_FILE_BYTES));
    if !matches!(result, Some(Ok(()))) {
        eprintln!("audio metadata diagnostic write unavailable");
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_event_has_no_transcript_audio_or_credentials() {
        let meta = Meta {
            capture_id: "1234".into(),
            sequence: 9,
            audio_revision: 7,
            audio_fence: Some(4),
            captured_command_id: 40,
            audio_ms: 2500,
            continued: false,
        };
        let mut event = meta.event("transcribed", "ok");
        event.asr_decode_ms = Some(351);
        event.text_chars = Some(18);
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["asrDecodeMs"], 351);
        assert_eq!(encoded["textChars"], 18);
        for key in [
            "text",
            "question",
            "transcript",
            "samples",
            "audio",
            "token",
            "apiKey",
        ] {
            assert!(
                encoded.get(key).is_none(),
                "unexpected sensitive field {key}"
            );
        }
    }
    #[test]
    fn rotations_preserve_latest_events_with_a_fixed_disk_bound() {
        let directory = std::env::temp_dir().join(format!(
            "livecaption-audio-trace-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let _ = fs::remove_dir_all(&directory);
        for index in 0..50 {
            append_bounded(
                &directory,
                format!("{{\"sequence\":{index}}}").as_bytes(),
                80,
            )
            .unwrap();
        }
        let files: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.metadata().unwrap().len() <= 80));
        assert!(fs::read_to_string(directory.join("audio-segments.jsonl"))
            .unwrap()
            .contains("49"));
        assert!(append_bounded(&directory, &[0; 100], 80).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
