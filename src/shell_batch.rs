//! Explorer multi-selection broker.
//! Explorer invokes legacy static verbs once per selected object. Collect those
//! invocations at medium integrity before a single elevation request.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::{StartupItem, types::*};
use crate::ui::framework::{WindowAlignment, WindowBuilder, WindowHandler};
use crate::ui::state::BatchAction;
use crate::engine::wof::WofAlgorithm;
use crate::utils::to_wstring;
use std::ptr;
use std::sync::{Arc, atomic::{AtomicU8, Ordering}};
use std::time::{Duration, Instant};

const BROKER_CLASS: &str = "CompactRS_ExplorerBatchBroker";
const BROKER_MUTEX: &str = "Local\\CompactRS_ExplorerBatchBroker_Mutex";
const BROKER_TIMER: usize = 81;
const BROKER_DELAY: Duration = Duration::from_millis(650);
const BROKER_LIFETIME: Duration = Duration::from_secs(60);
const LAUNCH_WAIT: Duration = Duration::from_secs(180);
const LAUNCH_PENDING: u8 = 0;
const LAUNCH_SUCCEEDED: u8 = 1;
const LAUNCH_FAILED: u8 = 2;
const WAIT_OBJECT_0: u32 = 0;
const ERROR_ALREADY_EXISTS: u32 = 183;
const INFINITE: u32 = 0xFFFFFFFF;
const SMTO_ABORTIFHUNG: u32 = 0x0002;
const SMTO_BLOCK: u32 = 0x0001;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *mut c_void, initial_owner: BOOL, name: LPCWSTR) -> HANDLE;
    fn ReleaseMutex(handle: HANDLE) -> BOOL;
    fn WaitForSingleObject(handle: HANDLE, milliseconds: DWORD) -> DWORD;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SendMessageTimeoutW(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
                           flags: u32, timeout: u32, result: *mut usize) -> isize;
}


pub fn encode_item(item: &StartupItem) -> String {
    let algo = match item.algorithm {
        WofAlgorithm::Xpress4K => "xpress4k",
        WofAlgorithm::Xpress8K => "xpress8k",
        WofAlgorithm::Xpress16K => "xpress16k",
        WofAlgorithm::Lzx => "lzx",
        WofAlgorithm::Lznt1 => "lznt1",
    };
    let action = match item.action {
        BatchAction::Compress => "compress",
        BatchAction::Decompress => "decompress",
    };
    format!("{}|{}|{}", item.path, algo, action)
}

pub unsafe fn decode_copydata(lparam: LPARAM) -> Option<StartupItem> {
    if lparam == 0 { return None; }
    let cds = &*(lparam as *const COPYDATASTRUCT);
    if cds.dwData != 0xB00B || cds.lpData.is_null() || cds.cbData < 6
        || cds.cbData > 131072 || cds.cbData % 2 != 0 { return None; }
    let words = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
    let payload = String::from_utf16_lossy(words);
    let mut fields = payload.trim_end_matches('\0').splitn(3, '|');
    let path = fields.next()?.to_string();
    let algorithm = match fields.next()? {
        "xpress4k" => WofAlgorithm::Xpress4K,
        "xpress8k" => WofAlgorithm::Xpress8K,
        "xpress16k" => WofAlgorithm::Xpress16K,
        "lzx" => WofAlgorithm::Lzx,
        "lznt1" => WofAlgorithm::Lznt1,
        _ => return None,
    };
    let action = match fields.next()? {
        "compress" => BatchAction::Compress,
        "decompress" => BatchAction::Decompress,
        _ => return None,
    };
    if path.is_empty() { return None; }
    Some(StartupItem { path, algorithm, action })
}

unsafe fn send_item(hwnd: HWND, item: &StartupItem) -> bool {
    let payload = to_wstring(&encode_item(item));
    let cds = COPYDATASTRUCT {
        dwData: 0xB00B,
        cbData: (payload.len() * 2) as u32,
        lpData: payload.as_ptr() as *mut _,
    };
    let mut acknowledgement = 0usize;
    SendMessageTimeoutW(
        hwnd, WM_COPYDATA, 0, &cds as *const _ as isize,
        SMTO_ABORTIFHUNG | SMTO_BLOCK, 1500, &mut acknowledgement,
    ) != 0 && acknowledgement == 1
}

unsafe fn send_to_window(class_name: &str, items: &[StartupItem]) -> bool {
    let class = to_wstring(class_name);
    let hwnd = FindWindowW(class.as_ptr(), ptr::null());
    if hwnd.is_null() { return false; }
    for item in items {
        if !send_item(hwnd, item) { return false; }
    }
    true
}

pub fn quote_windows_arg(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
        } else {
            if ch == '"' {
                result.push_str(&"\\".repeat(slashes * 2 + 1));
            } else {
                result.push_str(&"\\".repeat(slashes));
            }
            slashes = 0;
            result.push(ch);
        }
    }
    result.push_str(&"\\".repeat(slashes * 2));
    result.push('"');
    result
}

unsafe fn launch_as_admin(items: &[StartupItem]) -> bool {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return false,
    };
    let mut arguments = Vec::with_capacity(items.len() * 6 + 1);
    // Child MUST NOT request a second elevation, regardless of token detection.
    arguments.push("--elevated-once".to_owned());
    for item in items {
        arguments.push("--path".to_owned());
        arguments.push(quote_windows_arg(&item.path));
        arguments.push("--algo".to_owned());
        arguments.push(match item.algorithm {
            WofAlgorithm::Xpress4K => "xpress4k",
            WofAlgorithm::Xpress8K => "xpress8k",
            WofAlgorithm::Xpress16K => "xpress16k",
            WofAlgorithm::Lzx => "lzx",
            WofAlgorithm::Lznt1 => "lznt1",
        }.to_owned());
        arguments.push("--action".to_owned());
        arguments.push(match item.action {
            BatchAction::Compress => "compress",
            BatchAction::Decompress => "decompress",
        }.to_owned());
    }
    let command = arguments.join(" ");
    // ShellExecuteEx/Windows command lines have a 32K character limit.
    if command.encode_utf16().count() >= 32000 { return false; }
    let exe_w = to_wstring(&exe.to_string_lossy());
    let args_w = to_wstring(&command);
    let res = ShellExecuteW(
        ptr::null_mut(), crate::w!("runas").as_ptr(), exe_w.as_ptr(),
        if command.is_empty() { ptr::null() } else { args_w.as_ptr() },
        ptr::null(), SW_SHOW,
    );
    res as isize > 32
}

enum LaunchPhase {
    Collecting,
    Requesting(Instant),
    Approved(Instant),
}

struct Broker {
    items: Vec<StartupItem>,
    dialog_only: bool,
    last_item: Instant,
    phase: LaunchPhase,
    launch_result: Arc<AtomicU8>,
}

impl Broker {
    fn add(&mut self, item: StartupItem) {
        if !self.items.iter().any(|old| old.path == item.path
            && old.algorithm == item.algorithm && old.action == item.action) {
            self.items.push(item);
        }
        self.last_item = Instant::now();
    }

    unsafe fn target_window(&self) -> HWND {
        let name = if self.dialog_only { "CompactRS_CtxDialog" } else { "CompactRS_Class" };
        FindWindowW(to_wstring(name).as_ptr(), ptr::null())
    }
}

impl WindowHandler for Broker {
    fn on_create(&mut self, hwnd: HWND) -> LRESULT {
        unsafe { SetTimer(hwnd, BROKER_TIMER, 100, None); }
        0
    }

    fn on_message(&mut self, hwnd: HWND, msg: u32, _wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_COPYDATA => {
                    if let Some(item) = decode_copydata(lparam) {
                        self.add(item);
                        Some(1)
                    } else {
                        Some(0)
                    }
                }
                WM_TIMER => {
                    match self.phase {
                        LaunchPhase::Collecting => {
                            if !self.items.is_empty() && self.last_item.elapsed() >= BROKER_DELAY {
                                // Mark first! ShellExecuteW("runas") may pump nested Windows
                                // messages while a consent dialog is open. A timer callback
                                // MUST NOT issue another elevation request.
                                self.phase = LaunchPhase::Requesting(Instant::now());
                                let initial = std::mem::take(&mut self.items);
                                let status = self.launch_result.clone();
                                std::thread::spawn(move || {
                                    let _ = unsafe {
                                        CoInitializeEx(ptr::null_mut(), COINIT_APARTMENTTHREADED)
                                    };
                                    let launched = unsafe { launch_as_admin(&initial) };
                                    unsafe { CoUninitialize(); }
                                    status.store(
                                        if launched { LAUNCH_SUCCEEDED } else { LAUNCH_FAILED },
                                        Ordering::Release,
                                    );
                                });
                            }
                        }
                        LaunchPhase::Requesting(started) => {
                            let result = self.launch_result.load(Ordering::Acquire);
                            if result == LAUNCH_SUCCEEDED {
                                self.phase = LaunchPhase::Approved(Instant::now());
                            } else if result == LAUNCH_FAILED || started.elapsed() >= LAUNCH_WAIT {
                                DestroyWindow(hwnd);
                            }
                        }
                        LaunchPhase::Approved(started) => {
                            let target = self.target_window();
                            if !target.is_null() && !self.items.is_empty() {
                                let pending = std::mem::take(&mut self.items);
                                for (index, item) in pending.iter().enumerate() {
                                    if !send_item(target, item) {
                                        self.items.extend_from_slice(&pending[index..]);
                                        break;
                                    }
                                }
                            }
                            if self.items.is_empty()
                                && self.last_item.elapsed() >= Duration::from_secs(3)
                            {
                                DestroyWindow(hwnd);
                            } else if started.elapsed() >= BROKER_LIFETIME {
                                DestroyWindow(hwnd);
                            }
                        }
                    }
                    Some(0)
                }
                _ => None,
            }
        }
    }
}

/// Run a short-lived non-elevated coordinator for Explorer's per-item launches.
/// Returns when the paths have been handed off, or UAC has been declined.
pub unsafe fn collect(items: Vec<StartupItem>, dialog_only: bool) {
    if items.is_empty() { return; }

    // Existing elevated window accepts requests from Explorer (UIPI message filter).
    let elevated_class = if dialog_only { "CompactRS_CtxDialog" } else { "CompactRS_Class" };
    if send_to_window(elevated_class, &items) { return; }

    let mutex = CreateMutexW(ptr::null_mut(), TRUE, to_wstring(BROKER_MUTEX).as_ptr());
    if mutex.is_null() { return; }
    let existed = GetLastError() == ERROR_ALREADY_EXISTS;
    if existed && WaitForSingleObject(mutex, INFINITE) != WAIT_OBJECT_0 {
        CloseHandle(mutex);
        return;
    }

    if send_to_window(BROKER_CLASS, &items) {
        ReleaseMutex(mutex);
        CloseHandle(mutex);
        return;
    }

    let mut broker = Broker {
        items,
        dialog_only,
        last_item: Instant::now(),
        phase: LaunchPhase::Collecting,
        launch_result: Arc::new(AtomicU8::new(LAUNCH_PENDING)),
    };
    let window = WindowBuilder::new(&mut broker, BROKER_CLASS, "")
        .style(0).size(0, 0).align(WindowAlignment::Manual(0, 0))
        .build(ptr::null_mut());
    ReleaseMutex(mutex);
    CloseHandle(mutex);

    if let Ok(hwnd) = window {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // The window is destroyed by its timer handler.
        let _ = hwnd;
    }
}
