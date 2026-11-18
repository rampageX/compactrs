#![allow(unsafe_op_in_unsafe_fn)]
use crate::types::*;
use crate::ui::framework::{WindowHandler, WindowBuilder, WindowAlignment};
use crate::ui::builder::ControlBuilder;
use crate::ui::controls::apply_button_theme;
use crate::ui::layout::{LayoutNode, SizePolicy};
use crate::config::AppConfig;
use crate::StartupItem;
use std::sync::{Arc, Mutex, atomic::{AtomicU8, AtomicU64, AtomicU32, Ordering}};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::time::Instant;
use crate::ui::state::{UiMessage, ProcessingState, BatchAction};
use crate::engine::wof::WofAlgorithm;
use crate::ui::taskbar::{TaskbarProgress, TaskbarState};

const TIMER_ID: usize = 1;
const IDC_LBL_PATH: u16 = 101;
const IDC_LBL_STATUS: u16 = 102;
const IDC_PROGRESS: u16 = 103;
const IDC_LBL_STATS: u16 = 104;
const IDC_BTN_CANCEL: u16 = 105;

pub struct ContextDialogState {
    pub items: Vec<StartupItem>,
    pub pending: Arc<Mutex<VecDeque<StartupItem>>>,
    pub next_id: Arc<AtomicU32>,
    pub config: AppConfig,
    pub tx: Sender<UiMessage>,
    pub rx: Receiver<UiMessage>,
    pub global_state: Arc<AtomicU8>,
    pub global_current: Arc<AtomicU64>,
    pub global_total: Arc<AtomicU64>,
    pub is_dark: bool,
    pub taskbar: Option<TaskbarProgress>,
    
    pub start_time: Instant, 
    
    pub hwnd_path: HWND,
    pub hwnd_status: HWND,
    pub hwnd_progress: HWND,
    pub hwnd_stats: HWND,
    pub hwnd_cancel: HWND,
    
    pub item_metrics: std::collections::HashMap<u32, (u64, u64)>,
}

pub unsafe fn show(items: Vec<StartupItem>, config: AppConfig) {
    let is_dark = crate::ui::theme::resolve_mode(config.theme);
    
    let (tx, rx) = channel();
    let mut state = ContextDialogState {
        pending: Arc::new(Mutex::new(items.iter().cloned().collect())),
        next_id: Arc::new(AtomicU32::new(1)),
        items,
        config,
        tx,
        rx,
        global_state: Arc::new(AtomicU8::new(ProcessingState::Idle as u8)),
        global_current: Arc::new(AtomicU64::new(0)),
        global_total: Arc::new(AtomicU64::new(0)),
        is_dark,
        taskbar: None,
        start_time: Instant::now(),
        hwnd_path: std::ptr::null_mut(),
        hwnd_status: std::ptr::null_mut(),
        hwnd_progress: std::ptr::null_mut(),
        hwnd_stats: std::ptr::null_mut(),
        hwnd_cancel: std::ptr::null_mut(),
        item_metrics: std::collections::HashMap::new(),
    };
    
    let instance = GetModuleHandleW(std::ptr::null_mut());
    let icon = crate::ui::framework::load_app_icon(instance);
    let bg_brush = crate::ui::theme::get_background_brush(is_dark);
    
    let hwnd_res = WindowBuilder::new(&mut state, "CompactRS_CtxDialog", "CompactRS - Processing")
        .style(WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE)
        .size(crate::ui::theme::scale(500), crate::ui::theme::scale(180))
        .align(WindowAlignment::CenterOnScreen)
        .icon(icon)
        .background(bg_brush)
        .build(std::ptr::null_mut());
        
    if let Ok(hwnd) = hwnd_res {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl ContextDialogState {
    /// One directory at a time. Parallel worker threads are kept *within* that directory.
    unsafe fn spawn_processing(&mut self, hwnd: HWND) {
        if self.pending.lock().map_or(true, |items| items.is_empty()) { return; }

        self.global_state.store(ProcessingState::Running as u8, Ordering::Relaxed);
        self.start_time = Instant::now();
        crate::ui::wrappers::Button::new(self.hwnd_cancel).set_text("Cancel");
        crate::ui::wrappers::Button::new(self.hwnd_cancel).set_enabled(true);

        let tx = self.tx.clone();
        let state = self.global_state.clone();
        let queue = self.pending.clone();
        let next_id = self.next_id.clone();
        let current = self.global_current.clone();
        let total = self.global_total.clone();
        let force = self.config.force_compress;
        let guard = self.config.enable_system_guard;
        let low_power = self.config.low_power_mode;
        let max_threads = self.config.max_threads;
        let enable_skip = self.config.enable_skip_heuristics;
        let skip_ext = String::from_utf16_lossy(&self.config.skip_extensions_buf).trim_matches('\0').to_string();
        let set_attr = self.config.set_compressed_attr;
        let process_hidden = self.config.process_hidden_files;
        let hwnd_usize = hwnd as usize;

        std::thread::spawn(move || {
            loop {
                if state.load(Ordering::Relaxed) == ProcessingState::Stopped as u8 { break; }
                let next = queue.lock().ok().and_then(|mut jobs| jobs.pop_front());
                let item = match next {
                    Some(item) => item,
                    None => {
                        // Allow any final Explorer invocations to join before the batch ends.
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        match queue.lock().ok().and_then(|mut jobs| jobs.pop_front()) {
                            Some(item) => item,
                            None => break,
                        }
                    }
                };
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "shell-broker-smoke")]
                crate::shell_batch::record_mock(&format!("START|{}", item.path));
                #[cfg(feature = "shell-broker-smoke")]
                let mock_path = item.path.clone();
                let _ = tx.send(UiMessage::ContextPath(item.path.clone()));
                crate::engine::scanner::scan_path_streaming(
                    id, &item.path, tx.clone(), Some(&state), process_hidden
                );
                if state.load(Ordering::Relaxed) == ProcessingState::Stopped as u8 { break; }

                crate::engine::worker::batch_process_worker(
                    vec![(item.path, item.action, id, item.algorithm)],
                    tx.clone(), state.clone(), force, hwnd_usize, guard, low_power,
                    max_threads, current.clone(), total.clone(), enable_skip,
                    skip_ext.clone(), set_attr, process_hidden,
                    false // Only the encompassing serial batch emits Finished.
                );
                #[cfg(feature = "shell-broker-smoke")]
                crate::shell_batch::record_mock(&format!("DONE|{}", mock_path));
            }
            let _ = tx.send(UiMessage::Finished);
        });
    }

    unsafe fn update_stats_label(&self) {
        let mut total_logical = 0;
        let mut total_disk = 0;
        for (logical, disk) in self.item_metrics.values() {
            total_logical += *logical;
            total_disk += *disk;
        }
        
        let logical_str = crate::utils::format_size(total_logical);
        let disk_str = crate::utils::format_size(total_disk);
        let ratio_str = crate::utils::calculate_ratio_string(total_logical, total_disk);
        
        let first_algo = self.items.first().map(|i| i.algorithm).unwrap_or(WofAlgorithm::Xpress8K);
        let first_action = self.items.first().map(|i| i.action).unwrap_or(BatchAction::Compress);
        let action_tag = match first_action {
            BatchAction::Decompress => crate::w!("[Decompress]  Logical: "),
            BatchAction::Compress => match first_algo {
                WofAlgorithm::Xpress4K => crate::w!("[XPRESS4K]  Logical: "),
                WofAlgorithm::Xpress8K => crate::w!("[XPRESS8K]  Logical: "),
                WofAlgorithm::Xpress16K => crate::w!("[XPRESS16K]  Logical: "),
                WofAlgorithm::Lzx => crate::w!("[LZX]  Logical: "),
                WofAlgorithm::Lznt1 => crate::w!("[LZNT1]  Logical: "),
            }
        };

        let msg_w = crate::utils::concat_wstrings(&[
            action_tag,
            &logical_str,
            crate::w!("  |  On Disk: "),
            &disk_str,
            crate::w!("  |  Saved: "),
            &ratio_str
        ]);
        
        crate::ui::wrappers::Label::new(self.hwnd_stats).set_text_w(&msg_w);
    }
    
    unsafe fn do_layout(&mut self, hwnd: HWND) {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        
        use SizePolicy::{Fixed, Flex};
        
        LayoutNode::col(15, 8)
            .with(self.hwnd_path, Fixed(20))
            .with(self.hwnd_status, Fixed(20))
            .with(self.hwnd_progress, Fixed(22))
            .with_child(LayoutNode::row(0, 10)
                .with(self.hwnd_stats, Flex(1.0))
                .with(self.hwnd_cancel, Fixed(80))
                .with_policy(Fixed(30))
            )
            .apply_layout(rc);
    }
}

impl WindowHandler for ContextDialogState {
    fn is_dark_mode(&self) -> bool {
        self.is_dark
    }

    fn on_create(&mut self, hwnd: HWND) -> LRESULT {
        unsafe {
            crate::ui::theme::set_window_frame_theme(hwnd, self.is_dark);
            self.taskbar = Some(TaskbarProgress::new(hwnd));
            // Permit the unelevated Explorer broker to add late arrivals.
            ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, std::ptr::null_mut());
            
            let font = crate::ui::theme::get_app_font();
            
            let builder = |id| ControlBuilder::new(hwnd, id).dark_mode(self.is_dark).font(font);
            
            self.hwnd_path = builder(IDC_LBL_PATH).label(false).style(0x00004000).text("Initializing...").build();
            self.hwnd_status = builder(IDC_LBL_STATUS).label(false).text("Preparing to scan...").build();
            
            self.hwnd_progress = CreateWindowExW(
                0, PROGRESS_CLASSW, std::ptr::null(),
                WS_VISIBLE | WS_CHILD | PBS_SMOOTH,
                0, 0, 0, 0, hwnd, IDC_PROGRESS as usize as HMENU,
                GetModuleHandleW(std::ptr::null()), std::ptr::null_mut()
            );
            
            crate::ui::theme::apply_theme(self.hwnd_progress, crate::ui::theme::ControlType::ProgressBar, self.is_dark);
            
            self.hwnd_stats = builder(IDC_LBL_STATS).label(false).text("Logical: --  |  On Disk: --  |  Saved: --").build();
            self.hwnd_cancel = builder(IDC_BTN_CANCEL).button().text("Cancel").build();
            apply_button_theme(self.hwnd_cancel, self.is_dark);
            
            self.do_layout(hwnd);
            
            if self.items.len() == 1 {
                let path_w = crate::utils::to_wstring(&self.items[0].path);
                crate::ui::wrappers::Label::new(self.hwnd_path).set_text_w(&path_w);
            } else {
                let len_w = crate::utils::u64_to_wstring(self.items.len() as u64);
                let msg_w = crate::utils::concat_wstrings(&[crate::w!("Processing "), &len_w, crate::w!(" items...")]);
                crate::ui::wrappers::Label::new(self.hwnd_path).set_text_w(&msg_w);
            }
            
            SetTimer(hwnd, TIMER_ID, 50, None);
            
            self.start_time = Instant::now();
            
            self.spawn_processing(hwnd);
        }
        0
    }

    fn on_message(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, _lparam: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_TIMER => {
                    if wparam == TIMER_ID {
                        while let Ok(uimsg) = self.rx.try_recv() {
                            match uimsg {
                                UiMessage::ScanProgress(id, logical, disk, count) => {
                                    self.item_metrics.insert(id, (logical, disk));
                                    self.update_stats_label();
                                    
                                    let count_w = crate::utils::u64_to_wstring(count);
                                    let status_w = crate::utils::concat_wstrings(&[crate::w!("Scanning... "), &count_w, crate::w!(" files")]);
                                    crate::ui::wrappers::Label::new(self.hwnd_status).set_text_w(&status_w);
                                },
                                UiMessage::ContextPath(path) => {
                                    crate::ui::wrappers::Label::new(self.hwnd_path)
                                        .set_text_w(&crate::utils::to_wstring(&path));
                                },
                                UiMessage::Progress(cur, tot) => {
                                    let pb = crate::ui::wrappers::ProgressBar::new(self.hwnd_progress);
                                    pb.set_range(0, tot as i32);
                                    pb.set_pos(cur as i32);
                                    
                                    let progress_str = crate::utils::fmt_progress(cur, tot);
                                    let status_w = crate::utils::concat_wstrings(&[crate::w!("Processing "), &progress_str, crate::w!(" files")]);
                                    
                                    crate::ui::wrappers::Label::new(self.hwnd_status).set_text_w(&status_w);
                                    if let Some(tb) = &self.taskbar {
                                        tb.set_value(cur, tot);
                                        tb.set_state(TaskbarState::Normal);
                                    }
                                },
                                UiMessage::RowProgress(id, _cur, _tot, disk) => {
                                    if let Some(metrics) = self.item_metrics.get_mut(&id) {
                                        metrics.1 = disk;
                                    }
                                    self.update_stats_label();
                                },
                                UiMessage::RowFinished(id, disk, _tot, _state) => {
                                    if let Some(metrics) = self.item_metrics.get_mut(&id) {
                                        metrics.1 = disk;
                                    }
                                    self.update_stats_label();
                                },
                                UiMessage::StatusText(w_str) => {
                                    crate::ui::wrappers::Label::new(self.hwnd_status).set_text_w(&w_str);
                                },
                                UiMessage::Finished => {
                                    let cancelled = self.global_state.load(Ordering::Relaxed)
                                        == ProcessingState::Stopped as u8;
                                    let more_queued = self.pending.lock().map_or(false, |q| !q.is_empty());
                                    if !cancelled && more_queued {
                                        self.spawn_processing(hwnd);
                                        continue;
                                    }
                                    self.global_state.store(ProcessingState::Idle as u8, Ordering::Relaxed);
                                    
                                    let elapsed_secs = self.start_time.elapsed().as_secs();
                                    let total_files = self.global_total.load(Ordering::Relaxed);
                                    
                                    let secs_w = crate::utils::u64_to_wstring(elapsed_secs);
                                    let tot_w = crate::utils::u64_to_wstring(total_files);
                                    let final_msg = if cancelled {
                                        crate::utils::to_wstring("Cancelled.")
                                    } else {
                                        crate::utils::concat_wstrings(&[
                                            crate::w!("Finished in "), &secs_w, crate::w!("s ("), &tot_w, crate::w!(" files)")
                                        ])
                                    };
                                    
                                    crate::ui::wrappers::Label::new(self.hwnd_status).set_text_w(&final_msg);
                                    crate::ui::wrappers::Button::new(self.hwnd_cancel).set_text("Close");
                                    if let Some(tb) = &self.taskbar {
                                        tb.set_state(TaskbarState::NoProgress);
                                    }
                                },
                                _ => {}
                            }
                        }
                    }
                    Some(0)
                },
                WM_COPYDATA => {
                    if self.global_state.load(Ordering::Relaxed) == ProcessingState::Stopped as u8 {
                        return Some(0);
                    }
                    if let Some(item) = crate::shell_batch::decode_copydata(_lparam) {
                        // Coalesce repeats of the same shell selection.
                        if !self.items.iter().any(|old| old.path == item.path
                            && old.algorithm == item.algorithm && old.action == item.action) {
                            if self.global_state.load(Ordering::Relaxed) == ProcessingState::Idle as u8 {
                                self.items.clear();
                                self.item_metrics.clear();
                                self.global_current.store(0, Ordering::Relaxed);
                                self.global_total.store(0, Ordering::Relaxed);
                            }
                            self.items.push(item.clone());
                            if let Ok(mut q) = self.pending.lock() { q.push_back(item); }
                            if self.global_state.load(Ordering::Relaxed) == ProcessingState::Idle as u8 {
                                self.spawn_processing(hwnd);
                            }
                        }
                        Some(1)
                    } else {
                        Some(0)
                    }
                },
                WM_COMMAND => {
                    let id = (wparam & 0xFFFF) as u16;
                    if id == IDC_BTN_CANCEL {
                        if self.global_state.load(Ordering::Relaxed) == ProcessingState::Running as u8 {
                            self.global_state.store(ProcessingState::Stopped as u8, Ordering::Relaxed);
                            crate::ui::wrappers::Label::new(self.hwnd_status).set_text("Cancelling...");
                            crate::ui::wrappers::Button::new(self.hwnd_cancel).set_enabled(false);
                            if let Some(tb) = &self.taskbar {
                                tb.set_state(TaskbarState::Paused);
                            }
                        } else {
                            DestroyWindow(hwnd);
                        }
                    }
                    Some(0)
                },
                WM_DESTROY => {
                    KillTimer(hwnd, TIMER_ID);
                    PostQuitMessage(0);
                    Some(0)
                },
                _ => None,
            }
        }
    }
}