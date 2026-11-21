//! Small Windows lifecycle helpers; no microphone or credential access.
use crate::launch::LaunchMode;

#[cfg(windows)]
pub struct Instance(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(windows)]
pub fn instance(mode: LaunchMode) -> Result<Option<Instance>, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE},
    };
    let name = "Local\\LiveCaption.SingleInstance.v1\0"
        .encode_utf16()
        .collect::<Vec<_>>();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return Err("无法创建程序实例锁".into());
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            if mode.show_control_on_launch() {
                let title = "LiveCaption Control\0"
                    .encode_utf16()
                    .collect::<Vec<_>>();
                let window = FindWindowW(std::ptr::null(), title.as_ptr());
                if !window.is_null() {
                    ShowWindow(window, SW_RESTORE);
                    SetForegroundWindow(window);
                }
            }
            return Ok(None);
        }
        Ok(Some(Instance(handle)))
    }
}
#[cfg(not(windows))]
pub fn instance(_mode: LaunchMode) -> Result<Option<()>, String> {
    Ok(Some(()))
}

pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::{
        fs,
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|_| "无法创建设置临时文件")?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "无法完整保存设置")?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            };
            let src = temp
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            let dst = path
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            if unsafe {
                MoveFileExW(
                    src.as_ptr(),
                    dst.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err("无法替换设置文件，旧设置已保留".into());
            }
        }
        #[cfg(not(windows))]
        fs::rename(&temp, path).map_err(|_| "无法替换设置文件，旧设置已保留")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(windows)]
pub fn process_identity(pid: u32) -> Result<(String, u64), String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return Err("所选应用未运行，请刷新音源重新选择".into());
        }
        let result = (|| {
            let mut code = 0;
            if GetExitCodeProcess(handle, &mut code) == 0 || code != 259 {
                return Err("所选应用已退出，请刷新音源重新选择".into());
            }
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
                return Err("无法确认应用身份，请重新选择音源".into());
            }
            let mut path = vec![0u16; 32768];
            let mut size = path.len() as u32;
            let name = if QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut size) != 0 {
                String::from_utf16_lossy(&path[..size as usize])
                    .rsplit('\\')
                    .next()
                    .unwrap_or("应用")
                    .to_string()
            } else {
                format!("应用 {pid}")
            };
            Ok((
                name,
                ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64,
            ))
        })();
        CloseHandle(handle);
        result
    }
}

/// Resolve the application's current process tree before its first audio session exists.
#[cfg(windows)]
pub fn meeting_process() -> Result<(u32, u64), String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };
    let mut matches = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err("无法读取腾讯会议进程".into());
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut present = Process32FirstW(snapshot, &mut entry) != 0;
        while present {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase();
            if ["wemeetapp.exe", "tencentmeeting.exe", "voovmeeting.exe"].contains(&name.as_str()) {
                matches.push((entry.th32ProcessID, entry.th32ParentProcessID));
            }
            present = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    let root = matches
        .iter()
        .filter(|(_, parent)| !matches.iter().any(|(pid, _)| pid == parent))
        .filter_map(|(pid, _)| process_identity(*pid).ok().map(|(_, stamp)| (*pid, stamp)))
        .min_by_key(|(_, stamp)| *stamp);
    root.ok_or_else(|| "请先打开腾讯会议，正在等待会议进程".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_replace_complete_content_and_preserve_old_on_failure() {
        let dir = std::env::temp_dir().join(format!("livecaption-atomic-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"complete new value").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete new value");
        assert!(atomic_write(&dir, b"cannot replace directory").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"complete new value");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn current_process_identity_is_stable() {
        let a = process_identity(std::process::id()).unwrap();
        let b = process_identity(std::process::id()).unwrap();
        assert_eq!(a, b);
        assert!(a.1 > 0);
        assert!(process_identity(u32::MAX).is_err());
    }
}
