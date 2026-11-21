//! Durable local live-caption timeline, SRT/TXT exporter, and grounded summary citation verifier.
use super::{app_config_dir, platform, AppState};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::AppHandle;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CaptionItem {
    pub id: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub timestamp: String,
    pub text: String,
    pub decode_ms: u64,
    pub continued: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GapItem {
    pub recorded_ms: u64,
    pub timestamp: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicStatus {
    pub recording: bool,
    pub session_id: Option<String>,
    pub caption_count: u64,
    pub gap_count: u64,
    pub avg_decode_ms: u64,
    pub last_decode_ms: u64,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SummaryVerification {
    pub valid: bool,
    pub cited_ids: Vec<u64>,
    pub missing_citation_ids: Vec<u64>,
    pub unsupported_numbers: Vec<String>,
}

#[derive(Default)]
pub struct Runtime {
    pub recording: bool,
    pub session_id: Option<String>,
    pub session_started_ms: u64,
    pub items: Vec<CaptionItem>,
    pub gaps: Vec<GapItem>,
    pub seen: HashSet<(u128, u64)>,
    pub file: Option<File>,
    pub total_decode_ms: u64,
    pub last_decode_ms: u64,
    pub message: String,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub fn format_wall_timestamp(ms: u64) -> String {
    let seconds = ms / 1000 + 8 * 3600;
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub fn format_srt_timestamp(offset_ms: u64) -> String {
    let millis = offset_ms % 1000;
    let total_secs = offset_ms / 1000;
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let hours = total_mins / 60;
    format!("{hours:02}:{mins:02}:{secs:02},{millis:03}")
}

pub fn format_srt(items: &[CaptionItem], base_ms: u64) -> String {
    let mut out = String::new();
    for (idx, item) in items.iter().enumerate() {
        let start = item.start_ms.saturating_sub(base_ms);
        let end = item.end_ms.max(item.start_ms + 800).saturating_sub(base_ms);
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            idx + 1,
            format_srt_timestamp(start),
            format_srt_timestamp(end),
            item.text.trim()
        ));
    }
    out
}

pub fn format_txt(items: &[CaptionItem]) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&format!(
            "[#{} {}] ({} ms) {}\n",
            item.id,
            item.timestamp,
            item.decode_ms,
            item.text.trim()
        ));
    }
    out
}

/// Verify that every `[#id]` citation in a post-meeting summary points to a real
/// caption segment and that every numeric claim appears verbatim in the cited segments.
pub fn verify_summary(summary: &str, items: &[CaptionItem]) -> SummaryVerification {
    let cite_re = Regex::new(r"\[#(\d+)\]").expect("valid citation regex");
    let num_re = Regex::new(
        r"(?:[$¥￥]\s*)?(?:20\d{2}|\d+(?:,\d{3})*(?:\.\d+)?\s*%|\d{2,}(?:,\d{3})*(?:\.\d+)?)(?:\s*(?:万|亿|千|百|元|美元|人民币|家|倍|个月|天|年|ms|秒))?",
    )
    .expect("valid numeric regex");

    let mut cited_ids = Vec::new();
    let mut missing_citation_ids = Vec::new();

    for cap in cite_re.captures_iter(summary) {
        if let Ok(id) = cap[1].parse::<u64>() {
            if !cited_ids.contains(&id) {
                cited_ids.push(id);
            }
            if !items.iter().any(|item| item.id == id) && !missing_citation_ids.contains(&id) {
                missing_citation_ids.push(id);
            }
        }
    }

    let summary_without_tags = cite_re.replace_all(summary, "");
    let evidence_corpus = items
        .iter()
        .filter(|item| cited_ids.is_empty() || cited_ids.contains(&item.id))
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut unsupported_numbers = Vec::new();
    for mat in num_re.find_iter(&summary_without_tags) {
        let token = mat.as_str().trim().to_string();
        if !evidence_corpus.contains(&token) && !unsupported_numbers.contains(&token) {
            unsupported_numbers.push(token);
        }
    }

    let valid =
        !cited_ids.is_empty() && missing_citation_ids.is_empty() && unsupported_numbers.is_empty();
    SummaryVerification {
        valid,
        cited_ids,
        missing_citation_ids,
        unsupported_numbers,
    }
}

pub fn initialize(app: &AppHandle, state: &AppState) -> Result<PublicStatus, String> {
    let dir = app_config_dir(app)?.join("captions");
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建字幕目录: {e}"))?;
    let mut rt = state.caption.lock().map_err(|_| "字幕状态锁异常")?;
    if !rt.recording {
        let started = now_ms();
        let session_id = format!("session-{started}");
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("无法打开字幕日志文件: {e}"))?;
        rt.recording = true;
        rt.session_id = Some(session_id);
        rt.session_started_ms = started;
        rt.file = Some(file);
        rt.message = "实时字幕记录已就绪（音频仅在本地内存识别，不落盘、不外传）".into();
    }
    Ok(snapshot_from(&rt))
}

fn snapshot_from(rt: &Runtime) -> PublicStatus {
    let count = rt.items.len() as u64;
    let avg = rt.total_decode_ms.checked_div(count).unwrap_or(0);
    PublicStatus {
        recording: rt.recording,
        session_id: rt.session_id.clone(),
        caption_count: count,
        gap_count: rt.gaps.len() as u64,
        avg_decode_ms: avg,
        last_decode_ms: rt.last_decode_ms,
        message: rt.message.clone(),
    }
}

pub fn snapshot(state: &AppState) -> PublicStatus {
    state
        .caption
        .lock()
        .map(|rt| snapshot_from(&rt))
        .unwrap_or_default()
}

pub fn list_items(state: &AppState) -> Vec<CaptionItem> {
    state
        .caption
        .lock()
        .map(|rt| rt.items.clone())
        .unwrap_or_default()
}

pub fn clear_items(state: &AppState) -> PublicStatus {
    let Ok(mut rt) = state.caption.lock() else {
        return PublicStatus::default();
    };
    rt.items.clear();
    rt.gaps.clear();
    rt.seen.clear();
    rt.total_decode_ms = 0;
    rt.last_decode_ms = 0;
    rt.session_started_ms = now_ms();
    rt.message = "已清空当前会话字幕".into();
    snapshot_from(&rt)
}

pub fn record(
    state: &AppState,
    text: &str,
    duration_ms: u64,
    decode_ms: u64,
    continued: bool,
    capture_id: u128,
    sequence: u64,
) -> Result<Option<CaptionItem>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut rt = state.caption.lock().map_err(|_| "字幕状态锁异常")?;
    if !rt.seen.insert((capture_id, sequence)) {
        return Ok(None);
    }
    let end_ms = now_ms();
    let start_ms = end_ms.saturating_sub(duration_ms.max(600));
    if rt.session_started_ms == 0 {
        rt.session_started_ms = start_ms;
    }
    let id = rt.items.len() as u64 + 1;
    let item = CaptionItem {
        id,
        start_ms,
        end_ms,
        timestamp: format_wall_timestamp(end_ms),
        text: trimmed.to_string(),
        decode_ms,
        continued,
    };
    if let Some(file) = rt.file.as_mut() {
        if let Ok(line) = serde_json::to_string(&item) {
            let _ = writeln!(file, "{line}");
        }
    }
    rt.total_decode_ms = rt.total_decode_ms.saturating_add(decode_ms);
    rt.last_decode_ms = decode_ms;
    rt.message = format!("最新字幕 #{id}（本地识别耗时 {decode_ms} ms）");
    rt.items.push(item.clone());
    Ok(Some(item))
}

pub fn note_gap(_app: &AppHandle, state: &AppState, reason: &str) -> Result<PublicStatus, String> {
    let mut rt = state.caption.lock().map_err(|_| "字幕状态锁异常")?;
    let now = now_ms();
    rt.gaps.push(GapItem {
        recorded_ms: now,
        timestamp: format_wall_timestamp(now),
        reason: reason.to_string(),
    });
    rt.message = reason.to_string();
    Ok(snapshot_from(&rt))
}

pub fn export_to_path(state: &AppState, path: &str, format: &str) -> Result<String, String> {
    let (items, base_ms) = {
        let rt = state.caption.lock().map_err(|_| "字幕状态锁异常")?;
        let base = rt
            .items
            .first()
            .map(|i| i.start_ms)
            .unwrap_or(rt.session_started_ms);
        (rt.items.clone(), base)
    };
    if items.is_empty() {
        return Err("当前尚无字幕可导出".into());
    }
    let content = match format {
        "srt" => format_srt(&items, base_ms),
        "txt" => format_txt(&items),
        _ => return Err("不支持的导出格式（仅支持 srt 或 txt）".into()),
    };
    let target = PathBuf::from(path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("无法创建导出目录: {e}"))?;
        }
    }
    platform::atomic_write(&target, content.as_bytes())?;
    Ok(format!("已导出 {} 条字幕至 {}", items.len(), target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_captions() -> Vec<CaptionItem> {
        vec![
            CaptionItem {
                id: 1,
                start_ms: 1_000,
                end_ms: 4_500,
                timestamp: "2026-11-21 20:00:01".into(),
                text: "本季度线上无障碍字幕覆盖率提升至 96.4%，平均延迟 1780 ms。".into(),
                decode_ms: 1780,
                continued: false,
            },
            CaptionItem {
                id: 2,
                start_ms: 5_000,
                end_ms: 8_200,
                timestamp: "2026-11-21 20:00:05".into(),
                text: "下周二完成 3 场跨语种讲座字幕测试。".into(),
                decode_ms: 1650,
                continued: false,
            },
        ]
    }

    #[test]
    fn srt_exporter_formats_standard_subrip_blocks() {
        let items = sample_captions();
        let srt = format_srt(&items, 1_000);
        assert!(srt.contains("1\n00:00:00,000 --> 00:00:03,500\n本季度线上无障碍字幕覆盖率提升至 96.4%"));
        assert!(srt.contains("2\n00:00:04,000 --> 00:00:07,200\n下周二完成 3 场跨语种讲座字幕测试。"));
    }

    #[test]
    fn txt_exporter_includes_segment_id_timestamp_and_latency() {
        let items = sample_captions();
        let txt = format_txt(&items);
        assert!(txt.contains("[#1 2026-11-21 20:00:01] (1780 ms)"));
        assert!(txt.contains("[#2 2026-11-21 20:00:05] (1650 ms)"));
    }

    #[test]
    fn summary_verifier_accepts_grounded_citations_and_numbers() {
        let items = sample_captions();
        let summary = "- 字幕覆盖率已达 96.4%，平均延迟 1780 ms [#1]\n- 计划完成 3 场讲座测试 [#2]";
        let result = verify_summary(summary, &items);
        assert!(result.valid);
        assert_eq!(result.cited_ids, vec![1, 2]);
        assert!(result.missing_citation_ids.is_empty());
        assert!(result.unsupported_numbers.is_empty());
    }

    #[test]
    fn summary_verifier_flags_hallucinated_citations_and_unsupported_numbers() {
        let items = sample_captions();
        let summary = "- 覆盖率提升至 99.1% [#1]\n- 预计 2028 年扩展到 50% 课程 [#9]";
        let result = verify_summary(summary, &items);
        assert!(!result.valid);
        assert_eq!(result.missing_citation_ids, vec![9]);
        assert!(result.unsupported_numbers.iter().any(|n| n.contains("99.1%")));
        assert!(result.unsupported_numbers.iter().any(|n| n.contains("2028")));
    }
}
