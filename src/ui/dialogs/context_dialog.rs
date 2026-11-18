#![allow(unsafe_op_in_unsafe_fn)]
//! Explorer Dialog mode: one elevated window with up to five stacked folder
//! progress cards. Processing remains strictly serial between directories.
use crate::types::*;
use crate::ui::framework::{WindowHandler, WindowBuilder, WindowAlignment};
use crate::ui::builder::ControlBuilder;
use crate::ui::controls::apply_button_theme;
use crate::config::AppConfig;
use crate::StartupItem;
use std::sync::{Arc, Mutex, atomic::{AtomicU8, AtomicU64, AtomicU32, Ordering}};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::time::Instant;
use crate::ui::state::{UiMessage, ProcessingState, BatchAction};
use crate::engine::wof::WofAlgorithm;
use crate::ui::taskbar::{TaskbarProgress, TaskbarState};
use crate::ui::wrappers::{Label, Button, ProgressBar};

const TIMER_ID: usize = 1;
const MAX_VISIBLE: usize = 5;
const ROW_HEIGHT: i32 = 100;
const ELLIPSIS_HEIGHT: i32 = 22;
const WINDOW_WIDTH: i32 = 520;
const FOOTER_HEIGHT: i32 = 50;
const IDC_ELLIPSIS: u16 = 110;
const IDC_BTN_CANCEL: u16 = 111;

fn dialog_height(item_count: usize) -> i32 {
    let visible = item_count.max(1).min(MAX_VISIBLE) as i32;
    // Caption/nonclient allowance + top padding + visible cards + action footer.
    42 + 12 + visible * ROW_HEIGHT + FOOTER_HEIGHT
        + if item_count > MAX_VISIBLE { ELLIPSIS_HEIGHT } else { 0 }
}

#[derive(Clone)]
struct FolderProgress {
    path: String,
    action: BatchAction,
    algorithm: WofAlgorithm,
    id: Option<u32>,
    logical: u64,
    disk: u64,
    current: u64,
    total: u64,
    status: String,
    started: Option<Instant>,
    finished: bool,
}

impl FolderProgress {
    fn queued(item: &StartupItem) -> Self {
        Self {
            path: item.path.clone(),
            action: item.action,
            algorithm: item.algorithm,
            id: None,
            logical: 0,
            disk: 0,
            current: 0,
            total: 0,
            status: "Queued".to_string(),
            started: None,
            finished: false,
        }
    }

    fn stats(&self) -> Vec<u16> {
        let action = match self.action {
            BatchAction::Decompress => crate::w!("[Decompress]  Logical: "),
            BatchAction::Compress => match self.algorithm {
                WofAlgorithm::Xpress4K => crate::w!("[XPRESS4K]  Logical: "),
                WofAlgorithm::Xpress8K => crate::w!("[XPRESS8K]  Logical: "),
                WofAlgorithm::Xpress16K => crate::w!("[XPRESS16K]  Logical: "),
                WofAlgorithm::Lzx => crate::w!("[LZX]  Logical: "),
                WofAlgorithm::Lznt1 => crate::w!("[LZNT1]  Logical: "),
            }
        };
        crate::utils::concat_wstrings(&[
            action,
            &crate::utils::format_size(self.logical),
            crate::w!("  |  On Disk: "),
            &crate::utils::format_size(self.disk),
            crate::w!("  |  Saved: "),
            &crate::utils::calculate_ratio_string(self.logical, self.disk),
        ])
    }
}

#[derive(Clone, Copy)]
struct ProgressSlot {
    path: HWND,
    status: HWND,
    progress: HWND,
    stats: HWND,
}

impl ProgressSlot {
    unsafe fn set_visible(&self, visible: bool) {
        let cmd = if visible { SW_SHOW } else { SW_HIDE };
        for hwnd in [self.path, self.status, self.progress, self.stats] {
            ShowWindow(hwnd, cmd);
        }
    }
}

pub struct ContextDialogState {
    items: Vec<StartupItem>,
    pending: Arc<Mutex<VecDeque<StartupItem>>>,
    next_id: Arc<AtomicU32>,
    config: AppConfig,
    tx: Sender<UiMessage>,
    rx: Receiver<UiMessage>,
    global_state: Arc<AtomicU8>,
    global_current: Arc<AtomicU64>,
    global_total: Arc<AtomicU64>,
    is_dark: bool,
    taskbar: Option<TaskbarProgress>,
    start_time: Instant,
    current_id: Option<u32>,
    folders: Vec<FolderProgress>,
    slots: Vec<ProgressSlot>,
    hwnd_ellipsis: HWND,
    hwnd_cancel: HWND,
    layout_count: usize,
    layout_clipped: bool,
}

pub unsafe fn show(items: Vec<StartupItem>, config: AppConfig) {
    let is_dark = crate::ui::theme::resolve_mode(config.theme);
    let initial_count = items.len();
    let (tx, rx) = channel();
    let mut state = ContextDialogState {
        folders: items.iter().map(FolderProgress::queued).collect(),
        pending: Arc::new(Mutex::new(items.iter().cloned().collect())),
        items,
        next_id: Arc::new(AtomicU32::new(1)),
        config, tx, rx,
        global_state: Arc::new(AtomicU8::new(ProcessingState::Idle as u8)),
        global_current: Arc::new(AtomicU64::new(0)),
        global_total: Arc::new(AtomicU64::new(0)),
        is_dark,
        taskbar: None,
        start_time: Instant::now(),
        current_id: None,
        slots: Vec::new(),
        hwnd_ellipsis: std::ptr::null_mut(),
        hwnd_cancel: std::ptr::null_mut(),
        layout_count: 0,
        layout_clipped: false,
    };

    let instance = GetModuleHandleW(std::ptr::null_mut());
    let icon = crate::ui::framework::load_app_icon(instance);
    let bg_brush = crate::ui::theme::get_background_brush(is_dark);
    let hwnd_res = WindowBuilder::new(&mut state, "CompactRS_CtxDialog", "CompactRS - Processing")
        .style(WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE)
        .size(crate::ui::theme::scale(WINDOW_WIDTH), crate::ui::theme::scale(dialog_height(initial_count)))
        .align(WindowAlignment::CenterOnScreen)
        .icon(icon)
        .background(bg_brush)
        .build(std::ptr::null_mut());

    if let Ok(hwnd) = hwnd_res {
        #[cfg(feature = "shell-broker-smoke")]
        crate::shell_batch::record_mock(&format!("DIALOG_CREATED|{:x}", hwnd as usize));
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
    /// The worker processes one complete directory per invocation. It never
    /// shares active worker threads across two different directories.
    unsafe fn spawn_processing(&mut self, hwnd: HWND) {
        if self.pending.lock().map_or(true, |q| q.is_empty()) { return; }
        self.global_state.store(ProcessingState::Running as u8, Ordering::Relaxed);
        self.start_time = Instant::now();
        Button::new(self.hwnd_cancel).set_text("Cancel");
        Button::new(self.hwnd_cancel).set_enabled(true);

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
        let skip_ext = String::from_utf16_lossy(&self.config.skip_extensions_buf)
            .trim_matches('\0').to_string();
        let set_attr = self.config.set_compressed_attr;
        let process_hidden = self.config.process_hidden_files;
        let hwnd_usize = hwnd as usize;

        std::thread::spawn(move || {
            loop {
                if state.load(Ordering::Relaxed) == ProcessingState::Stopped as u8 { break; }
                let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                let item = match next {
                    Some(item) => item,
                    None => {
                        // Explorer may send its last selected directory slightly later.
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        match queue.lock().ok().and_then(|mut q| q.pop_front()) {
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

                let _ = tx.send(UiMessage::ContextPath(id, item.path.clone()));
                crate::engine::scanner::scan_path_streaming(
                    id, &item.path, tx.clone(), Some(&state), process_hidden
                );
                if state.load(Ordering::Relaxed) == ProcessingState::Stopped as u8 { break; }

                crate::engine::worker::batch_process_worker(
                    vec![(item.path, item.action, id, item.algorithm)],
                    tx.clone(), state.clone(), force, hwnd_usize, guard, low_power,
                    max_threads, current.clone(), total.clone(), enable_skip,
                    skip_ext.clone(), set_attr, process_hidden,
                    false // A single Finished marks the entire directory queue.
                );
                #[cfg(feature = "shell-broker-smoke")]
                crate::shell_batch::record_mock(&format!("DONE|{}", mock_path));
            }
            let _ = tx.send(UiMessage::Finished);
        });
    }

    fn active_folder_mut(&mut self) -> Option<&mut FolderProgress> {
        let id = self.current_id?;
        self.folders.iter_mut().find(|row| row.id == Some(id))
    }

    fn row_by_id_mut(&mut self, id: u32) -> Option<&mut FolderProgress> {
        self.folders.iter_mut().find(|row| row.id == Some(id))
    }

    /// Layout sizes are converted once from app scale to pixels. Five cards
    /// are reused for an arbitrarily long folder history.
    unsafe fn layout(&mut self, hwnd: HWND) {
        if self.slots.is_empty() { return; }
        // Build the visual stack from folders that have actually begun.
        // Never hide the active first directory merely because 6+ future
        // folders are already in the pending queue.
        let begun = self.folders.iter().filter(|f| f.id.is_some()).count();
        let clipped = begun > MAX_VISIBLE;
        let count = begun.max(1).min(MAX_VISIBLE);
        if self.layout_count != count || self.layout_clipped != clipped {
            self.layout_count = count;
            self.layout_clipped = clipped;
            #[cfg(feature = "shell-broker-smoke")]
            crate::shell_batch::record_mock(&format!(
                "STACK_LAYOUT|{}|{}|{}", begun, clipped, begun.saturating_sub(MAX_VISIBLE)
            ));
            SetWindowPos(
                hwnd, std::ptr::null_mut(), 0, 0,
                crate::ui::theme::scale(WINDOW_WIDTH),
                crate::ui::theme::scale(dialog_height(begun.max(1))),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }

        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        let s = crate::ui::theme::scale;
        let x = s(14);
        let width = (rc.right - rc.left - 2 * x).max(s(150));
        let mut y = s(10);

        if clipped {
            MoveWindow(self.hwnd_ellipsis, x, y, width, s(ELLIPSIS_HEIGHT), 0);
            ShowWindow(self.hwnd_ellipsis, SW_SHOW);
            y += s(ELLIPSIS_HEIGHT);
        } else {
            ShowWindow(self.hwnd_ellipsis, SW_HIDE);
        }

        for (i, slot) in self.slots.iter().enumerate() {
            let visible = i < count;
            slot.set_visible(visible);
            if !visible { continue; }
            MoveWindow(slot.path, x, y, width, s(20), 0);
            MoveWindow(slot.status, x, y + s(23), width, s(18), 0);
            MoveWindow(slot.progress, x, y + s(44), width, s(22), 0);
            MoveWindow(slot.stats, x, y + s(72), width, s(20), 0);
            y += s(ROW_HEIGHT);
        }
        MoveWindow(self.hwnd_cancel, rc.right - x - s(86),
            y + s(2), s(86), s(30), 0);
        InvalidateRect(hwnd, std::ptr::null_mut(), 0);
    }

    unsafe fn refresh_cards(&mut self, hwnd: HWND) {
        self.layout(hwnd);
        let begun = self.folders.iter().filter(|f| f.id.is_some()).count();
        let start = begun.saturating_sub(MAX_VISIBLE);
        let candidates: Vec<&FolderProgress> = if begun == 0 {
            self.folders.iter().take(1).collect()
        } else {
            self.folders.iter().filter(|f| f.id.is_some()).skip(start).collect()
        };
        for (slot, folder) in self.slots.iter().zip(candidates) {
            Label::new(slot.path).set_text(&folder.path);
            Label::new(slot.status).set_text(&folder.status);
            Label::new(slot.stats).set_text_w(&folder.stats());
            let max = folder.total.min(i32::MAX as u64) as i32;
            let bar = ProgressBar::new(slot.progress);
            bar.set_range(0, max.max(1));
            let complete = folder.finished && folder.total == 0;
            let position = if complete { 1 } else {
                folder.current.min(i32::MAX as u64) as i32
            };
            bar.set_pos(position);
        }
    }
}

impl WindowHandler for ContextDialogState {
    fn is_dark_mode(&self) -> bool { self.is_dark }

    fn on_create(&mut self, hwnd: HWND) -> LRESULT {
        unsafe {
            crate::ui::theme::set_window_frame_theme(hwnd, self.is_dark);
            self.taskbar = Some(TaskbarProgress::new(hwnd));
            ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, std::ptr::null_mut());

            let font = crate::ui::theme::get_app_font();
            let label = |id| ControlBuilder::new(hwnd, id).dark_mode(self.is_dark).font(font);
            self.hwnd_ellipsis = label(IDC_ELLIPSIS).label(true).text("…").build();
            self.slots = (0..MAX_VISIBLE).map(|idx| {
                let offset = 200 + idx as u16 * 4;
                let path = label(offset).label(false).text("").build();
                let status = label(offset + 1).label(false).text("").build();
                let progress = CreateWindowExW(
                    0, PROGRESS_CLASSW, std::ptr::null(),
                    WS_VISIBLE | WS_CHILD | PBS_SMOOTH,
                    0, 0, 0, 0, hwnd, (offset + 2) as usize as HMENU,
                    GetModuleHandleW(std::ptr::null()), std::ptr::null_mut()
                );
                crate::ui::theme::apply_theme(
                    progress, crate::ui::theme::ControlType::ProgressBar, self.is_dark
                );
                let stats = label(offset + 3).label(false).text("").build();
                ProgressSlot { path, status, progress, stats }
            }).collect();

            self.hwnd_cancel = label(IDC_BTN_CANCEL).button().text("Cancel").build();
            apply_button_theme(self.hwnd_cancel, self.is_dark);

            self.refresh_cards(hwnd);
            SetTimer(hwnd, TIMER_ID, 50, None);
            self.spawn_processing(hwnd);
        }
        0
    }

    fn on_message(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_TIMER if wparam == TIMER_ID => {
                    let mut redraw = false;
                    while let Ok(event) = self.rx.try_recv() {
                        redraw = true;
                        match event {
                            UiMessage::ContextPath(id, path) => {
                                self.current_id = Some(id);
                                if let Some(folder) = self.folders.iter_mut().find(|f| f.path == path && f.id.is_none()) {
                                    folder.id = Some(id);
                                    folder.started = Some(Instant::now());
                                    folder.status = "Scanning...".to_string();
                                }
                            }
                            UiMessage::ScanProgress(id, logical, disk, count) => {
                                if let Some(folder) = self.row_by_id_mut(id) {
                                    folder.logical = logical;
                                    folder.disk = disk;
                                    folder.total = count;
                                    folder.status = format!("Scanning... {} files", count);
                                }
                            }
                            UiMessage::RowProgress(id, cur, tot, disk) => {
                                if let Some(folder) = self.row_by_id_mut(id) {
                                    folder.current = cur;
                                    folder.total = tot;
                                    folder.disk = disk;
                                    folder.status = format!("Processing {} / {} files", cur, tot);
                                }
                            }
                            UiMessage::RowFinished(id, disk, tot, _) => {
                                if let Some(folder) = self.row_by_id_mut(id) {
                                    folder.current = tot;
                                    folder.total = tot;
                                    folder.disk = disk;
                                    folder.finished = true;
                                    let seconds = folder.started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                                    folder.status = format!("Finished in {}s ({} files)", seconds, tot);
                                }
                            }
                            UiMessage::Progress(cur, total) => {
                                if let Some(tb) = &self.taskbar {
                                    tb.set_value(cur, total);
                                    tb.set_state(TaskbarState::Normal);
                                }
                            }
                            UiMessage::StatusText(text) => {
                                let status = String::from_utf16_lossy(&text)
                                    .trim_end_matches('\0').to_string();
                                if let Some(folder) = self.active_folder_mut() {
                                    if !folder.finished { folder.status = status; }
                                }
                            }
                            UiMessage::Finished => {
                                let cancelled = self.global_state.load(Ordering::Relaxed)
                                    == ProcessingState::Stopped as u8;
                                let more = self.pending.lock().map_or(false, |q| !q.is_empty());
                                if more && !cancelled {
                                    self.spawn_processing(hwnd);
                                    continue;
                                }
                                self.global_state.store(ProcessingState::Idle as u8, Ordering::Relaxed);
                                self.current_id = None;
                                if cancelled {
                                    for folder in &mut self.folders {
                                        if !folder.finished { folder.status = "Cancelled".to_string(); }
                                    }
                                }
                                Button::new(self.hwnd_cancel).set_text("Close");
                                Button::new(self.hwnd_cancel).set_enabled(true);
                                if let Some(tb) = &self.taskbar {
                                    tb.set_state(TaskbarState::NoProgress);
                                }
                            }
                            _ => {}
                        }
                    }
                    if redraw { self.refresh_cards(hwnd); }
                    Some(0)
                }
                WM_SIZE => {
                    self.layout(hwnd);
                    Some(0)
                }
                WM_COPYDATA => {
                    if self.global_state.load(Ordering::Relaxed) == ProcessingState::Stopped as u8 {
                        return Some(0);
                    }
                    if let Some(item) = crate::shell_batch::decode_copydata(lparam) {
                        if !self.items.iter().any(|old| old.path == item.path
                            && old.algorithm == item.algorithm && old.action == item.action)
                        {
                            if self.global_state.load(Ordering::Relaxed) == ProcessingState::Idle as u8 {
                                self.items.clear();
                                self.folders.clear();
                                self.current_id = None;
                                self.global_current.store(0, Ordering::Relaxed);
                                self.global_total.store(0, Ordering::Relaxed);
                            }
                            self.items.push(item.clone());
                            self.folders.push(FolderProgress::queued(&item));
                            if let Ok(mut q) = self.pending.lock() { q.push_back(item); }
                            self.refresh_cards(hwnd);
                            if self.global_state.load(Ordering::Relaxed) == ProcessingState::Idle as u8 {
                                self.spawn_processing(hwnd);
                            }
                        }
                        Some(1)
                    } else { Some(0) }
                }
                WM_COMMAND if (wparam & 0xFFFF) as u16 == IDC_BTN_CANCEL => {
                    if self.global_state.load(Ordering::Relaxed) == ProcessingState::Running as u8 {
                        self.global_state.store(ProcessingState::Stopped as u8, Ordering::Relaxed);
                        if let Some(folder) = self.active_folder_mut() {
                            folder.status = "Cancelling...".to_string();
                        }
                        Button::new(self.hwnd_cancel).set_enabled(false);
                        self.refresh_cards(hwnd);
                    } else {
                        DestroyWindow(hwnd);
                    }
                    Some(0)
                }
                WM_CLOSE => {
                    if self.global_state.load(Ordering::Relaxed) == ProcessingState::Running as u8 {
                        self.global_state.store(ProcessingState::Stopped as u8, Ordering::Relaxed);
                        Button::new(self.hwnd_cancel).set_enabled(false);
                        Some(0)
                    } else {
                        DestroyWindow(hwnd);
                        Some(0)
                    }
                }
                WM_DESTROY => {
                    self.global_state.store(ProcessingState::Stopped as u8, Ordering::Relaxed);
                    KillTimer(hwnd, TIMER_ID);
                    PostQuitMessage(0);
                    Some(0)
                }
                _ => None,
            }
        }
    }
}
