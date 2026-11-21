//! Private bounded playback-worker protocol. Audio is never written to disk.

use std::{
    io::{self, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
pub(crate) const PCM: u8 = 1;
pub(crate) const READY: u8 = 2;
pub(crate) const ERROR: u8 = 3;
const MAX_FRAME: usize = 64 * 1024;
type Frame = (u8, Vec<u8>);

pub(crate) fn read_frame(reader: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME
        || !matches!(header[0], PCM | READY | ERROR)
        || (header[0] == PCM && !len.is_multiple_of(2))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid capture frame",
        ));
    }
    let mut data = vec![0; len];
    reader.read_exact(&mut data)?;
    Ok((header[0], data))
}
fn write_frame(writer: &mut impl Write, kind: u8, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "capture frame too large",
        ));
    }
    writer.write_all(&[kind])?;
    writer.write_all(&(data.len() as u32).to_le_bytes())?;
    writer.write_all(data)?;
    writer.flush()
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Closing the parent's control pipe also terminates the child if the
        // parent exits abnormally. Normal cancellation kills and reaps here.
        self.0.stdin.take();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) struct Capture {
    child: OwnedChild,
    frames: Option<mpsc::Receiver<io::Result<Frame>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    last_frame: Instant,
    ready: bool,
}
impl Capture {
    pub(crate) fn start(source: &str) -> Result<Self, String> {
        let mut cmd = Command::new(std::env::current_exe().map_err(|_| "无法定位采音程序")?);
        cmd.args(["--capture-worker", source])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // normal background helper, no extra console
        }
        let mut child = OwnedChild(cmd.spawn().map_err(|_| "无法启动采音子进程")?);
        let mut stdout = child.0.stdout.take().ok_or("无法读取采音子进程")?;
        let (tx, rx) = mpsc::sync_channel(8);
        let reader = std::thread::spawn(move || loop {
            let frame = read_frame(&mut stdout);
            let failed = frame.is_err();
            if tx.send(frame).is_err() || failed {
                break;
            }
        });
        Ok(Self {
            child,
            frames: Some(rx),
            reader: Some(reader),
            last_frame: Instant::now(),
            ready: false,
        })
    }
    pub(crate) fn next(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        match self
            .frames
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_millis(100))
        {
            Ok(Ok(frame)) => {
                self.last_frame = Instant::now();
                self.ready |= frame.0 == READY;
                Ok(Some(frame))
            }
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("采音子进程已断开".into())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let deadline = if self.ready {
                    Duration::from_secs(10)
                } else {
                    Duration::from_secs(15)
                };
                if self.last_frame.elapsed() >= deadline {
                    Err("采音子进程无响应".into())
                } else {
                    Ok(None)
                }
            }
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.0.kill();
        let _ = self.child.0.wait();
        self.frames.take(); // unblock a full bounded reader queue before joining
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn exit_on_parent_eof() {
    std::thread::spawn(|| {
        let mut byte = [0];
        let mut stdin = io::stdin().lock();
        while matches!(stdin.read(&mut byte), Ok(n) if n > 0) {}
        std::process::exit(0);
    });
}

/// Called before Tauri initialization, so the helper never starts UI or services.
pub fn run_if_requested() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--capture-worker") {
        return false;
    }
    let source = args.next().unwrap_or_default();
    // EOF is the parent's lifetime fence, even when WASAPI or stdout is blocked.
    exit_on_parent_eof();
    let mut stdout = io::stdout().lock();
    if let Err(error) = capture(&source, &mut stdout) {
        let _ = write_frame(&mut stdout, ERROR, error.as_bytes());
    }
    true
}

#[cfg(not(windows))]
fn capture(_: &str, _: &mut impl Write) -> Result<(), String> {
    Err("自动听音仅支持 Windows".into())
}
#[cfg(windows)]
fn capture(source: &str, output: &mut impl Write) -> Result<(), String> {
    use super::audio::parse_process_source;
    use super::platform;
    use std::collections::VecDeque;
    use wasapi::*;
    let resolved = if source == "tencent" {
        let (pid, stamp) = platform::meeting_process()?;
        format!("pid:{pid}:{stamp}")
    } else {
        source.to_string()
    };
    let source = resolved.as_str();
    initialize_mta().ok().map_err(|_| "无法初始化音频线程")?;
    let result = (|| {
        let enumerator = DeviceEnumerator::new().map_err(|_| "无法读取播放设备")?;
        let mut device_id = None;
        let (mut client, label) = if let Some(pid) = source.strip_prefix("pid:") {
            let (pid, stamp) = parse_process_source(pid)?;
            if platform::process_identity(pid)?.1 != stamp {
                return Err("应用已重启，请刷新音源重新选择".into());
            }
            (
                AudioClient::new_application_loopback_client(pid, true)
                    .map_err(|_| "无法采集指定应用，请改用通用模式")?,
                format!("应用进程 {pid} 的播放声音"),
            )
        } else {
            let device = if source.is_empty() {
                enumerator.get_default_device(&Direction::Render)
            } else if source == "communications" {
                enumerator.get_default_device_for_role(&Direction::Render, &Role::Communications)
            } else {
                enumerator.get_device(source)
            }
            .map_err(|_| "找不到所选播放设备")?;
            if device.get_direction() != Direction::Render {
                return Err("只允许播放设备，禁止麦克风采集".into());
            }
            device_id = device.get_id().ok();
            (
                device.get_iaudioclient().map_err(|_| "无法打开播放声音")?,
                device
                    .get_friendlyname()
                    .unwrap_or_else(|_| "电脑播放音频".into()),
            )
        };
        let format = WaveFormat::new(32, 32, &SampleType::Float, 48000, 2, None);
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 200_000,
        };
        client
            .initialize_client(&format, &Direction::Capture, &mode)
            .map_err(|_| "无法启动播放音频回环")?;
        let event = client
            .set_get_eventhandle()
            .map_err(|_| "无法创建音频事件")?;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|_| "无法读取播放音频")?;
        client.start_stream().map_err(|_| "播放音频采集启动失败")?;
        let result = (|| {
            write_frame(output, READY, label.as_bytes()).map_err(|_| "采音连接已关闭")?;
            let mut bytes = VecDeque::new();
            let mut checked = Instant::now();
            loop {
                // Capture direction here reads the render-loopback buffer, never a microphone endpoint.
                match capture.get_next_packet_size() {
                    Ok(Some(n)) if n > 0 => {
                        if capture.read_from_device_to_deque(&mut bytes).is_err() {
                            return Err("播放设备已断开".into());
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        return Err("播放设备已断开".into());
                    }
                }
                let mut pcm = Vec::with_capacity(bytes.len() / 24);
                while bytes.len() >= 24 {
                    let mut sum = 0.0f32;
                    for _ in 0..6 {
                        let b = [
                            bytes.pop_front().unwrap(),
                            bytes.pop_front().unwrap(),
                            bytes.pop_front().unwrap(),
                            bytes.pop_front().unwrap(),
                        ];
                        sum += f32::from_le_bytes(b);
                    }
                    let value = (sum / 6.0).clamp(-1.0, 1.0);
                    pcm.push((value * 32767.0) as i16);
                }
                // One bounded PCM frame also serves as the capture-loop heartbeat.
                // Silence sends an empty frame and never means recognition failure.
                if pcm.is_empty() {
                    write_frame(output, PCM, &[]).map_err(|_| "采音连接已关闭")?;
                }
                for chunk in pcm.chunks(MAX_FRAME / 2) {
                    let data: Vec<u8> = chunk.iter().flat_map(|x| x.to_le_bytes()).collect();
                    write_frame(output, PCM, &data).map_err(|_| "采音连接已关闭")?;
                }
                if checked.elapsed() > Duration::from_secs(2) && source.starts_with("pid:") {
                    checked = Instant::now();
                    let (pid, stamp) = parse_process_source(&source[4..])?;
                    if platform::process_identity(pid).map(|(_, s)| s).ok() != Some(stamp) {
                        return Err("所选应用已退出或重启，请刷新音源重新选择".into());
                    }
                }
                if checked.elapsed() > Duration::from_secs(2)
                    && (source.is_empty() || source == "communications")
                {
                    checked = Instant::now();
                    let role = if source.is_empty() {
                        Role::Console
                    } else {
                        Role::Communications
                    };
                    if enumerator
                        .get_default_device_for_role(&Direction::Render, &role)
                        .ok()
                        .and_then(|d| d.get_id().ok())
                        != device_id
                    {
                        return Err("默认播放设备已改变".into());
                    }
                }
                let _ = event.wait_for_event(100);
            }
        })();
        let _ = client.stop_stream();
        result
    })();
    deinitialize();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_frame_and_oversize_are_rejected_before_allocation() {
        assert!(read_frame(&mut &b"\x01\x04\0\0\0x"[..]).is_err());
        let mut oversized = vec![1];
        oversized.extend_from_slice(&(MAX_FRAME as u32 + 1).to_le_bytes());
        assert!(read_frame(&mut oversized.as_slice()).is_err());
    }
    #[test]
    fn silence_heartbeat_and_pcm_roundtrip() {
        for (kind, data) in [(PCM, vec![]), (PCM, vec![1, 0, 255, 127])] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, kind, &data).unwrap();
            assert_eq!(read_frame(&mut bytes.as_slice()).unwrap(), (kind, data));
        }
    }
    #[cfg(unix)]
    #[test]
    fn owner_drop_terminates_and_reaps_child() {
        use std::process::Command;
        let child = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let guard = OwnedChild(child);
        drop(guard);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    fn helper() -> OwnedChild {
        let module = module_path!().split_once("::").unwrap().1;
        OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("{module}::process_helper"),
                    "--ignored",
                    "--nocapture",
                ])
                .env("LIVECAPTION_CAPTURE_TEST_HELPER", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    #[test]
    #[ignore]
    fn process_helper() {
        if std::env::var_os("LIVECAPTION_CAPTURE_TEST_HELPER").is_none() {
            return;
        }
        exit_on_parent_eof();
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn closed_parent_control_pipe_terminates_child() {
        let mut child = helper();
        child.0.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(3);
        while child.0.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "parent EOF did not terminate worker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn cancel_drop_unblocks_full_reader_and_reaps_child() {
        let child = helper();
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Ok((PCM, vec![]))).unwrap();
        let reader = std::thread::spawn(move || {
            let _ = tx.send(Ok((PCM, vec![])));
        });
        let worker = Capture {
            child,
            frames: Some(rx),
            reader: Some(reader),
            last_frame: Instant::now(),
            ready: true,
        };
        let start = Instant::now();
        drop(worker);
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn stalled_or_disconnected_child_is_an_error() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut worker = Capture {
            child: helper(),
            frames: Some(rx),
            reader: None,
            last_frame: Instant::now() - Duration::from_secs(16),
            ready: false,
        };
        assert!(worker.next().is_err());
        worker.last_frame = Instant::now();
        drop(tx);
        assert!(worker.next().is_err());
    }
}
