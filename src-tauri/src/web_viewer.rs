//! Built-in local accessibility Web viewer (`/captions`) and ETag-fenced JSON feed (`/api/captions`).
use super::{caption, AppState};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const VIEWER_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<title>LiveCaption · 无障碍实时字幕阅读端</title>
<style>
  body { margin:0; background:#090d16; color:#f8fafc; font-family:"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif; }
  header { position:sticky; top:0; background:#111827ee; border-bottom:1px solid #1e293b; padding:12px 20px; display:flex; justify-content:space-between; align-items:center; backdrop-filter:blur(8px); }
  .title { font-size:15px; font-weight:700; color:#93c5fd; }
  .meta { font-size:12px; color:#94a3b8; font-family:Consolas,monospace; }
  .controls button { background:#1e293b; color:#f8fafc; border:1px solid #334155; border-radius:6px; padding:5px 10px; margin-left:6px; cursor:pointer; font-weight:600; }
  main { max-width:920px; margin:0 auto; padding:20px; display:flex; flex-direction:column; gap:14px; }
  .cue { border-bottom:1px solid #1e293b; padding-bottom:10px; }
  .cue-head { font-size:12px; color:#64748b; font-family:Consolas,monospace; margin-bottom:4px; }
  .cue-text { font-size:var(--font-size, 26px); line-height:1.48; font-weight:600; letter-spacing:0.01em; }
</style>
</head>
<body>
<header>
  <div>
    <div class="title">LiveCaption · 无障碍大字号实时字幕</div>
    <div class="meta" id="status">正在连接本机字幕流...</div>
  </div>
  <div class="controls">
    <button onclick="scale(-2)">A-</button>
    <button onclick="scale(2)">A+</button>
  </div>
</header>
<main id="feed" role="log" aria-live="polite"></main>
<script>
let fontSize = 26, etag = "";
function scale(d){ fontSize = Math.max(16, Math.min(48, fontSize + d)); document.documentElement.style.setProperty("--font-size", fontSize + "px"); }
async function poll(){
  try {
    const headers = etag ? { "If-None-Match": etag } : {};
    const res = await fetch("/api/captions", { headers });
    if (res.status === 304) return;
    etag = res.headers.get("ETag") || "";
    const data = await res.json();
    document.getElementById("status").textContent =
      `rev=${data.revision} · 共 ${data.items.length} 条 · P50=${data.p50DecodeMs}ms / P95=${data.p95DecodeMs}ms`;
    const feed = document.getElementById("feed");
    feed.innerHTML = data.items.map(i =>
      `<div class="cue"><div class="cue-head">[#${i.id} · ${i.timestamp} · ${i.decodeMs} ms]</div><div class="cue-text">${i.text.replace(/</g,"&lt;")}</div></div>`
    ).join("");
    window.scrollTo({ top: document.body.scrollHeight, behavior: "smooth" });
  } catch {}
}
setInterval(poll, 700); poll();
</script>
</body>
</html>"#;

pub fn build_api_payload(
    revision: u64,
    items: &[caption::CaptionItem],
) -> (String, String) {
    let etag = format!("\"rev-{revision}-{}\"", items.len());
    let (p50, p95) = caption::latency_percentiles(items);
    let body = json!({
        "revision": revision,
        "count": items.len(),
        "p50DecodeMs": p50,
        "p95DecodeMs": p95,
        "items": items,
    })
    .to_string();
    (etag, body)
}

pub fn handle_http_request(
    raw_request: &str,
    revision: u64,
    items: &[caption::CaptionItem],
) -> Vec<u8> {
    let first_line = raw_request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
    let clean_path = path.split('?').next().unwrap_or("/");

    if clean_path == "/api/captions" {
        let (etag, body) = build_api_payload(revision, items);
        let client_etag = raw_request
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim())
            .unwrap_or("");
        if client_etag == etag {
            return format!(
                "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nConnection: close\r\n\r\n"
            )
            .into_bytes();
        }
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nETag: {etag}\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes();
    }

    if matches!(clean_path, "/" | "/captions") {
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            VIEWER_HTML.len(),
            VIEWER_HTML
        )
        .into_bytes();
    }

    let not_found = "Not Found";
    format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        not_found.len(),
        not_found
    )
    .into_bytes()
}

pub fn spawn_local_viewer(app: tauri::AppHandle) {
    use tauri::Manager;
    tauri::async_runtime::spawn(async move {
        let Ok(listener) = TcpListener::bind("127.0.0.1:8788").await else {
            return;
        };
        if let Ok(mut sync) = app.state::<AppState>().sync.lock() {
            sync.connected = true;
            sync.endpoint = "http://127.0.0.1:8788/captions".into();
        }
        while let Ok((mut socket, _)) = listener.accept().await {
            let handle = app.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(n) = socket.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let state = handle.state::<AppState>();
                let rev = state.sync.lock().map(|s| s.revision).unwrap_or(0);
                let items = caption::list_items(&state);
                let resp = handle_http_request(&req, rev, &items);
                let _ = socket.write_all(&resp).await;
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<caption::CaptionItem> {
        vec![
            caption::CaptionItem {
                id: 1,
                start_ms: 1000,
                end_ms: 3000,
                timestamp: "2026-11-21 20:00:01".into(),
                text: "第一条无障碍实时字幕".into(),
                decode_ms: 250,
                continued: false,
            },
            caption::CaptionItem {
                id: 2,
                start_ms: 3200,
                end_ms: 5200,
                timestamp: "2026-11-21 20:00:03".into(),
                text: "第二条无障碍实时字幕".into(),
                decode_ms: 270,
                continued: false,
            },
        ]
    }

    #[test]
    fn viewer_html_endpoint_serves_high_contrast_accessibility_page() {
        let resp = String::from_utf8(handle_http_request(
            "GET /captions HTTP/1.1\r\nHost: 127.0.0.1:8788\r\n\r\n",
            3,
            &sample(),
        ))
        .unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("text/html"));
        assert!(resp.contains("无障碍大字号实时字幕"));
    }

    #[test]
    fn api_captions_returns_json_with_p50_p95_and_honors_if_none_match_304() {
        let items = sample();
        let first = String::from_utf8(handle_http_request(
            "GET /api/captions HTTP/1.1\r\nHost: 127.0.0.1:8788\r\n\r\n",
            7,
            &items,
        ))
        .unwrap();
        assert!(first.starts_with("HTTP/1.1 200 OK"));
        assert!(first.contains("ETag: \"rev-7-2\""));
        assert!(first.contains("\"p50DecodeMs\":250"));
        assert!(first.contains("\"p95DecodeMs\":270"));

        let cached = String::from_utf8(handle_http_request(
            "GET /api/captions HTTP/1.1\r\nIf-None-Match: \"rev-7-2\"\r\n\r\n",
            7,
            &items,
        ))
        .unwrap();
        assert!(cached.starts_with("HTTP/1.1 304 Not Modified"));
    }
}
