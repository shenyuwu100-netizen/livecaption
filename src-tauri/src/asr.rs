//! Resident local recognizer. Audio only travels over private child-process pipes.
use super::*;
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
#[derive(Debug)]
struct Packet {
    kind: u8,
    text: String,
}
pub struct Engine {
    child: Child,
    input: mpsc::SyncSender<Vec<u8>>,
    output: mpsc::Receiver<Result<Packet, String>>,
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn read_packet(reader: &mut impl Read) -> Result<Packet, String> {
    let mut header = [0; 5];
    reader
        .read_exact(&mut header)
        .map_err(|_| "本机识别进程已断开")?;
    let size = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
    if size > 24000 || header[0] > 2 {
        return Err("本机识别响应无效".into());
    }
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| "本机识别响应不完整")?;
    Ok(Packet {
        kind: header[0],
        text: String::from_utf8(bytes).map_err(|_| "本机识别响应编码无效")?,
    })
}
fn encode_audio(samples: &[i16], language: &str) -> Result<Vec<u8>, String> {
    if samples.is_empty() || samples.len() > 16000 * 15 {
        return Err("语音片段长度无效".into());
    }
    let language = match language {
        "zh" => 1,
        "en" => 2,
        "auto" => 0,
        _ => return Err("语音语言设置无效".into()),
    };
    let mut input = Vec::with_capacity(5 + samples.len() * 2);
    input.push(language);
    input.extend((samples.len() as u32).to_le_bytes());
    for value in samples {
        input.extend(value.to_le_bytes());
    }
    Ok(input)
}
fn receive_packet(
    output: &mpsc::Receiver<Result<Packet, String>>,
    cancel: &CancellationToken,
    limit: Duration,
) -> Result<Packet, String> {
    let started = Instant::now();
    loop {
        if cancel.is_cancelled() {
            return Err("语音识别已停止".into());
        }
        let remaining = limit.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("语音识别超时，下一段会重启识别器".into());
        }
        match output.recv_timeout(remaining.min(Duration::from_millis(30))) {
            Ok(packet) => {
                if cancel.is_cancelled() {
                    return Err("语音识别已停止".into());
                }
                return packet;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err("本机识别进程已断开".into()),
        }
    }
}
impl Engine {
    fn start(dir: &Path, cancel: &CancellationToken) -> Result<Self, String> {
        #[cfg(windows)]
        use std::os::windows::process::CommandExt;
        let worker_bin = dir.join("LiveCaptionSpeech.exe");
        let mut command = Command::new(worker_bin);
        command
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command
            .spawn()
            .map_err(|_| "无法启动常驻语音识别，请检查 speech 目录")?;
        let mut stdin = child.stdin.take().ok_or("无法连接识别输入")?;
        let (input, inputs) = mpsc::sync_channel::<Vec<u8>>(1);
        std::thread::spawn(move || {
            while let Ok(bytes) = inputs.recv() {
                if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let mut stdout = child.stdout.take().ok_or("无法连接识别输出")?;
        let (tx, output) = mpsc::sync_channel(2);
        std::thread::spawn(move || loop {
            let result = read_packet(&mut stdout);
            let failed = result.is_err();
            if tx.send(result).is_err() || failed {
                break;
            }
        });
        let engine = Self {
            child,
            input,
            output,
        };
        let ready = engine.receive(cancel, Duration::from_secs(20))?;
        if ready.kind != 0 || ready.text != "ready:2" {
            return Err("本机语音模型加载失败".into());
        }
        Ok(engine)
    }
    fn receive(&self, cancel: &CancellationToken, limit: Duration) -> Result<Packet, String> {
        receive_packet(&self.output, cancel, limit)
    }
    fn transcribe(
        &mut self,
        samples: &[i16],
        language: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let input = encode_audio(samples, language)?;
        self.input.send(input).map_err(|_| "无法发送本机语音片段")?;
        let packet = self.receive(cancel, Duration::from_secs(12))?;
        if packet.kind != 1 {
            return Err("本机语音识别失败".into());
        }
        Ok(packet.text.trim().chars().take(2000).collect())
    }
}
pub struct Transcript {
    pub text: String,
    // Monotonic wall time around the resident worker's per-clip request/response.
    pub decode_ms: u64,
}
pub fn recognize(
    state: &AppState,
    dir: &Path,
    samples: &[i16],
    language: &str,
    cancel: &CancellationToken,
) -> Result<Transcript, String> {
    let mut slot = state.recognizer.lock().map_err(|_| "识别器状态不可用")?;
    if cancel.is_cancelled() {
        return Err("语音识别已停止".into());
    }
    if slot.is_none() {
        *slot = Some(Engine::start(dir, cancel)?);
    }
    if samples.is_empty() {
        return Ok(Transcript {
            text: String::new(),
            decode_ms: 0,
        });
    }
    let decode_started = Instant::now();
    let result = slot.as_mut().unwrap().transcribe(samples, language, cancel);
    if result.is_err() {
        *slot = None;
    }
    result.map(|text| Transcript {
        text,
        decode_ms: decode_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_oversized_and_truncated_worker_packets() {
        assert!(read_packet(&mut &[1, 255, 255, 255, 127][..]).is_err());
        assert!(read_packet(&mut &[1, 2, 0, 0, 0, 65][..]).is_err());
        assert!(read_packet(&mut &[3, 0, 0, 0, 0][..]).is_err());
    }
    #[test]
    fn decodes_unicode_worker_response() {
        let text = "为什么？";
        let mut bytes = vec![1];
        bytes.extend((text.len() as u32).to_le_bytes());
        bytes.extend(text.as_bytes());
        assert_eq!(read_packet(&mut &bytes[..]).unwrap().text, text);
    }
    #[test]
    fn audio_protocol_preserves_pcm_sign_and_language() {
        let pcm = [-32768, 0, 32767];
        for (language, byte) in [("zh", 1), ("en", 2), ("auto", 0)] {
            let encoded = encode_audio(&pcm, language).unwrap();
            assert_eq!(&encoded[..5], &[byte, 3, 0, 0, 0]);
            let decoded: Vec<i16> = encoded[5..]
                .chunks_exact(2)
                .map(|value| i16::from_le_bytes([value[0], value[1]]))
                .collect();
            assert_eq!(decoded, pcm);
        }
    }
    #[test]
    fn invalid_audio_cannot_corrupt_worker_framing() {
        assert!(encode_audio(&[], "zh").is_err());
        assert!(encode_audio(&[0], "fr").is_err());
        assert!(encode_audio(&vec![0; 16000 * 15 + 1], "zh").is_err());
        assert!(encode_audio(&vec![0; 16000 * 15], "zh").is_ok());
    }
    #[test]
    fn cancellation_wins_over_already_buffered_response() {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(Packet {
            kind: 1,
            text: "已取消的旧识别".into(),
        }))
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(receive_packet(&rx, &cancel, Duration::from_secs(1))
            .unwrap_err()
            .contains("已停止"));
    }
    #[test]
    fn response_wait_handles_timeout_and_worker_disconnect() {
        let (tx, rx) = mpsc::channel();
        let cancel = CancellationToken::new();
        assert!(receive_packet(&rx, &cancel, Duration::ZERO)
            .unwrap_err()
            .contains("超时"));
        drop(tx);
        assert!(receive_packet(&rx, &cancel, Duration::from_secs(1))
            .unwrap_err()
            .contains("已断开"));
    }
    #[test]
    fn cancelled_wait_never_returns_late_transcription() {
        let (tx, rx) = mpsc::channel();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            worker_cancel.cancel();
            let _ = tx.send(Ok(Packet {
                kind: 1,
                text: "迟到的识别".into(),
            }));
        });
        assert!(receive_packet(&rx, &cancel, Duration::from_secs(1))
            .unwrap_err()
            .contains("已停止"));
        worker.join().unwrap();
    }
}
