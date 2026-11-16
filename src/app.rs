//! UI state machine, window procedure and hit testing.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::core::{w, Result as WinResult, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    PAINTSTRUCT,
};
use windows::Win32::System::Ole::{IDropTarget, RegisterDragDrop, RevokeDragDrop};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, ReleaseCapture, SetCapture, TrackMouseEvent, LASTINPUTINFO, TME_LEAVE,
    TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, ShellExecuteW, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GetCursorPos, GetWindowRect, KillTimer, LoadCursorW, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, IDC_ARROW, IDC_HAND, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, WM_APP, WM_CAPTURECHANGED, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_DROPFILES, WM_DWMCOLORIZATIONCOLORCHANGED, WM_ERASEBKGND,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WM_SETTINGCHANGE,
    WM_THEMECHANGED, WM_TIMER,
};
use zeroize::Zeroizing;

use crate::config::{self, BackdropMode, Config};
use crate::drop_target::DropTarget;
use crate::model::{ErrorInfo, Phase, ScanResult, WorkerEvent};
use crate::theme::{self, Theme};
use crate::ui::animation::{lerp, ring_arc, Animation, FrameClock, FAST_MS, NORMAL_MS};
use crate::ui::composition::WM_APP_ANIM_DONE;
use crate::ui::menu::{self, MenuItem};
use crate::ui::tooltip::Tooltip;
use crate::ui::window;
use crate::ui::{DrawState, Renderer, Scene};
use crate::worker::WorkerHandle;

pub const WM_APP_EVENT: u32 = WM_APP + 1;
pub const WM_APP_SHOW: u32 = WM_APP + 2;

const TIMER_AUTOCLOSE: usize = 2;
const TIMER_TRIM: usize = 5;
const TIMER_TOOLTIP: usize = 6;
const TIMER_LABEL: usize = 7;
const TIMER_REVEAL: usize = 8;

const MENU_TOPMOST: usize = 1;
const MENU_STARTUP: usize = 2;
const MENU_API_KEY: usize = 3;
const MENU_LAST_RESULT: usize = 4;
const MENU_QUIT: usize = 5;
const MENU_CANCEL: usize = 6;

#[derive(Debug, Clone)]
enum UiState {
    Idle,
    Dragging,
    Busy {
        index: usize,
        total: usize,
        phase: Phase,
        /// Determinate progress of hashing/uploading, when known.
        progress: Option<f32>,
    },
    AskUpload {
        id: u64,
        name: String,
        index: usize,
        total: usize,
    },
    Result {
        result: ScanResult,
        caption: Option<String>,
    },
    Error {
        info: ErrorInfo,
    },
}

struct DragSession {
    start_cursor: POINT,
    start_window: RECT,
    moved: bool,
}

pub struct App {
    hwnd: HWND,
    config: Config,
    theme: Theme,
    renderer: Option<Renderer>,
    state: UiState,
    scene: Scene,
    pending_state: Option<UiState>,
    expand: f32,
    expand_anim: Option<Animation>,
    hover: f32,
    hover_anim: Option<Animation>,
    dpi: u32,
    compact_pos: (i32, i32),
    window_rect: RECT,
    mouse_inside: bool,
    mouse_tracking: bool,
    dragging: Option<DragSession>,
    drag_valid: bool,
    drop_target: Option<IDropTarget>,
    events: Arc<Mutex<VecDeque<WorkerEvent>>>,
    batch_total: usize,
    batch_results: Vec<ScanResult>,
    batch_errors: usize,
    batch_skipped: usize,
    last_result: Option<ScanResult>,
    busy_started: Option<Instant>,
    hover_accept: bool,
    hover_reject: bool,
    auto_close_active: bool,
    tooltip: Option<Tooltip>,
    tooltip_scheduled: bool,
    worker: Option<WorkerHandle>,
    has_api_key: bool,
    /// Actual window size in DIPs (may be larger than the pill mid-transition).
    window_dip: (f32, f32),
    /// True while an expand/collapse runs with the window pre-sized.
    transition: bool,
    /// `GetTickCount` when the current result/error was shown.
    shown_tick: u32,
    /// When the user was first seen active while the result was up.
    seen_active: Option<Instant>,
    /// Result reveal: ratio counts up and the colour settles, 0..1.
    reveal: f32,
    reveal_anim: Option<Animation>,
    /// Displayed (eased) determinate progress.
    shown_progress: f32,
    /// When VirusTotal started analysing the current upload.
    polling_since: Option<Instant>,
    /// A cancel was sent; worker events of that batch are ignored until its
    /// `BatchDone` arrives.
    cancelling: bool,
    clock: FrameClock,
    /// Backdrop requested at renderer creation (Win11 + transparency on).
    acrylic: bool,
    /// Keep the elapsed-time label fresh while polling with the compositor.
    label_timer: bool,
}

impl App {
    pub fn new(
        hwnd: HWND,
        config: Config,
        theme: Theme,
        events: Arc<Mutex<VecDeque<WorkerEvent>>>,
    ) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let (compact_w, compact_h) = window::compact_px(dpi);
        let default = default_position(hwnd, compact_w, compact_h);
        let mut position = config.position.map(|p| (p[0], p[1])).unwrap_or(default);
        position = clamp_compact(position, compact_w, compact_h);

        Self {
            hwnd,
            config,
            theme,
            renderer: None,
            state: UiState::Idle,
            scene: Scene::Compact,
            pending_state: None,
            expand: 0.0,
            expand_anim: None,
            hover: 0.0,
            hover_anim: None,
            dpi,
            compact_pos: position,
            window_rect: RECT::default(),
            mouse_inside: false,
            mouse_tracking: false,
            dragging: None,
            drag_valid: false,
            drop_target: None,
            events,
            batch_total: 0,
            batch_results: Vec::new(),
            batch_errors: 0,
            batch_skipped: 0,
            last_result: None,
            busy_started: None,
            hover_accept: false,
            hover_reject: false,
            auto_close_active: false,
            tooltip: None,
            tooltip_scheduled: false,
            worker: None,
            has_api_key: config::load_api_key().ok().flatten().is_some(),
            window_dip: (64.0, 64.0),
            transition: false,
            shown_tick: 0,
            seen_active: None,
            reveal: 1.0,
            reveal_anim: None,
            shown_progress: 0.0,
            polling_since: None,
            cancelling: false,
            clock: FrameClock::new(hwnd),
            acrylic: false,
            label_timer: false,
        }
    }

    pub fn set_worker(&mut self, worker: WorkerHandle) {
        self.worker = Some(worker);
    }

    /// True when DWM owns the animation (no frame clock on the UI thread).
    fn uses_composition(&self) -> bool {
        self.renderer
            .as_ref()
            .map(Renderer::is_composition)
            .unwrap_or(false)
    }

    pub fn initialize(&mut self) -> WinResult<()> {
        let prefer_composition = crate::ui::composition_supported(&self.theme);
        self.acrylic = self.theme.is_win11() && !self.theme.effective_solid();
        self.renderer = Some(Renderer::new(
            self.hwnd,
            self.dpi,
            self.theme,
            prefer_composition,
            self.acrylic,
        )?);
        theme::apply_native_menu_theme(self.theme.dark);
        window::allow_dark_mode(self.hwnd, self.theme.dark);
        self.apply_backdrop();
        self.update_geometry(true)?;
        self.create_tooltip();
        self.update_tooltip();
        if !self.has_api_key {
            self.prompt_api_key();
        }
        unsafe {
            let drop_target: IDropTarget = DropTarget::new(self.hwnd).into();
            RegisterDragDrop(self.hwnd, &drop_target)?;
            self.drop_target = Some(drop_target);
            DragAcceptFiles(self.hwnd, true);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.render_frame();
        self.schedule_trim();
        Ok(())
    }

    pub fn show_existing(&mut self) {
        window::bring_to_front(self.hwnd, self.config.always_on_top);
        self.render_frame();
    }

    pub fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                unsafe {
                    let _ = BeginPaint(self.hwnd, &mut paint);
                    let _ = EndPaint(self.hwnd, &paint);
                }
                // The frame clock invalidates the window once per vsync.
                if self.clock.is_running() {
                    self.tick();
                } else {
                    self.render_frame();
                }
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_MOUSEMOVE => {
                self.on_mouse_move(lparam);
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.on_mouse_leave();
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                self.on_left_down();
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                self.on_left_up();
                Some(LRESULT(0))
            }
            WM_RBUTTONUP => {
                self.show_context_menu();
                Some(LRESULT(0))
            }
            WM_CAPTURECHANGED => {
                self.finish_drag();
                Some(LRESULT(0))
            }
            WM_TIMER => {
                self.on_timer(wparam.0);
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.on_dpi_changed(wparam, lparam);
                Some(LRESULT(0))
            }
            WM_SETTINGCHANGE => {
                if setting_affects_theme(wparam, lparam) {
                    self.refresh_theme();
                }
                Some(LRESULT(0))
            }
            WM_THEMECHANGED | WM_DWMCOLORIZATIONCOLORCHANGED => {
                self.refresh_theme();
                Some(LRESULT(0))
            }
            WM_DROPFILES => {
                self.on_drop_files(wparam);
                Some(LRESULT(0))
            }
            WM_APP_EVENT => {
                self.drain_worker_events();
                Some(LRESULT(0))
            }
            WM_APP_SHOW => {
                self.show_existing();
                Some(LRESULT(0))
            }
            WM_APP_ANIM_DONE => {
                self.on_animation_done();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.save_config();
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                self.save_config();
                self.kill_all_timers();
                if let Some(worker) = self.worker.take() {
                    worker.shutdown();
                }
                unsafe {
                    DragAcceptFiles(self.hwnd, false);
                    let _ = RevokeDragDrop(self.hwnd);
                    self.drop_target = None;
                    windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    // ----- Drop target callbacks -------------------------------------------------

    pub fn on_drag_enter(&mut self) -> bool {
        self.drag_valid = true;
        if matches!(self.state, UiState::Busy { .. }) {
            // Keep scanning; the glow says "drop here to add it to the queue".
            self.cancel_tooltip();
            self.set_hover(1.0);
            return true;
        }
        self.cancel_auto_close();
        self.cancel_tooltip();
        if self.scene.is_expanded() {
            // Never resize under the cursor: shrinking the pill would push the
            // cursor outside the window, OLE would send DragLeave, the result
            // would come back, DragEnter again… an endless flicker loop. The
            // pill just lights up and accepts the drop as it is.
            self.render_frame();
            return true;
        }
        self.state = UiState::Dragging;
        self.scene = Scene::Compact;
        self.expand = 0.0;
        self.expand_anim = None;
        self.settle_transition();
        self.set_hover(1.0);
        self.update_geometry(true).ok();
        self.render_frame();
        true
    }

    pub fn drag_hover_active(&self) -> bool {
        self.drag_valid
    }

    pub fn on_drag_leave(&mut self) {
        if self.drag_valid {
            self.drag_valid = false;
            if matches!(self.state, UiState::Busy { .. }) {
                self.set_hover(0.0);
            } else if matches!(self.state, UiState::Dragging) {
                self.set_hover(0.0);
                self.state = UiState::Idle;
                self.scene = Scene::Compact;
            } else {
                self.schedule_auto_close();
            }
            self.render_frame();
        }
    }

    fn on_drop_files(&mut self, wparam: WPARAM) {
        let hdrop = HDROP(wparam.0 as *mut std::ffi::c_void);
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
        let mut files = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = unsafe { DragQueryFileW(hdrop, index, None) };
            if length == 0 {
                continue;
            }
            let mut buffer = vec![0u16; length as usize + 1];
            if unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) } > 0 {
                files.push(std::path::PathBuf::from(crate::util::from_wide_lossy(
                    &buffer,
                )));
            }
        }
        unsafe {
            DragFinish(hdrop);
        }
        self.on_files_dropped(files);
    }

    pub fn on_files_dropped(&mut self, files: Vec<std::path::PathBuf>) -> bool {
        if files.is_empty() {
            self.on_drag_leave();
            return false;
        }
        self.drag_valid = false;
        if matches!(self.state, UiState::Busy { .. } | UiState::AskUpload { .. }) {
            // Joins the running batch; the counter grows (1/2, 2/3…) right
            // away. The worker corrects it once it has expanded folders and
            // skipped duplicates.
            if let UiState::Busy { total, .. } | UiState::AskUpload { total, .. } = &mut self.state
            {
                *total += files.len();
            }
            self.set_hover(0.0);
            self.render_frame();
            return match &self.worker {
                Some(worker) => {
                    worker.scan(files);
                    true
                }
                None => false,
            };
        }
        if !self.has_api_key {
            self.set_state(UiState::Error {
                info: ErrorInfo {
                    title: "Falta la API key".into(),
                    detail:
                        "Configura tu API key de VirusTotal desde el menú contextual (clic derecho)."
                            .into(),
                },
            });
            return false;
        }

        let total = files.len();
        self.batch_total = total;
        self.batch_results.clear();
        self.batch_errors = 0;
        self.batch_skipped = 0;
        self.busy_started = Some(Instant::now());
        self.set_state(UiState::Busy {
            index: 1,
            total,
            phase: Phase::Hashing,
            progress: None,
        });
        if let Some(worker) = &self.worker {
            worker.scan(files);
        } else {
            self.busy_started = None;
            self.set_state(UiState::Error {
                info: ErrorInfo {
                    title: "Trabajo no disponible".into(),
                    detail: "El hilo de análisis no está disponible.".into(),
                },
            });
        }
        true
    }

    // ----- Input -----------------------------------------------------------------

    fn on_mouse_move(&mut self, lparam: LPARAM) {
        self.cancel_trim();
        if !self.mouse_tracking {
            let mut event = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut event);
            }
            self.mouse_tracking = true;
        }

        if !self.mouse_inside {
            self.mouse_inside = true;
            self.render_frame();
        }

        let x = (lparam.0 as i32 & 0xFFFF) as i16 as f32;
        let y = ((lparam.0 as i32 >> 16) & 0xFFFF) as i16 as f32;
        if self.tooltip_text().is_some() {
            self.schedule_tooltip();
        } else {
            self.cancel_tooltip();
        }
        if self.dragging.is_some() {
            self.cancel_tooltip();
            self.drag_to_cursor();
            return;
        }

        let (accept, reject) = self.button_hit(x, y);
        if accept != self.hover_accept || reject != self.hover_reject {
            self.hover_accept = accept;
            self.hover_reject = reject;
            self.render_frame();
        }
        self.update_cursor(x, y);
    }

    fn on_mouse_leave(&mut self) {
        self.mouse_tracking = false;
        self.mouse_inside = false;
        self.hover_accept = false;
        self.hover_reject = false;
        self.cancel_tooltip();
        if matches!(self.state, UiState::Idle) && self.expand_anim.is_none() {
            self.schedule_trim();
        }
        self.render_frame();
    }

    fn on_left_down(&mut self) {
        self.cancel_tooltip();
        if matches!(self.state, UiState::AskUpload { .. }) {
            // Only the ✓/✕ buttons are interactive; the rest of the pill is not
            // a drag surface.
            return;
        }
        if self.scene.is_expanded() {
            return;
        }

        let mut cursor = POINT::default();
        let mut rect = RECT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
            let _ = GetWindowRect(self.hwnd, &mut rect);
            SetCapture(self.hwnd);
        }
        self.dragging = Some(DragSession {
            start_cursor: cursor,
            start_window: rect,
            moved: false,
        });
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    fn on_left_up(&mut self) {
        let was_dragging = self.dragging.as_ref().map(|d| d.moved).unwrap_or(false);
        if self.dragging.is_some() {
            self.finish_drag();
        }
        if was_dragging {
            return;
        }

        if let UiState::AskUpload {
            id, index, total, ..
        } = &self.state
        {
            if self.hover_accept {
                let (id, index, total) = (*id, *index, *total);
                self.answer_upload(id, true, index, total);
            } else if self.hover_reject {
                let (id, index, total) = (*id, *index, *total);
                self.answer_upload(id, false, index, total);
            }
            return;
        }

        match &self.state {
            UiState::Result { result, .. } => open_virustotal(&result.sha256),
            UiState::Busy { .. } => self.cancel_scan(),
            _ => {}
        }
    }

    /// Stops the running batch. The UI returns to idle at once; the worker
    /// stops hashing/uploading mid-read and reports back with `BatchDone`.
    fn cancel_scan(&mut self) {
        if !matches!(self.state, UiState::Busy { .. } | UiState::AskUpload { .. }) {
            return;
        }
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
        self.cancelling = true;
        self.busy_started = None;
        self.polling_since = None;
        self.set_state(UiState::Idle);
    }

    fn finish_drag(&mut self) {
        if let Some(session) = self.dragging.take() {
            let _ = session;
            unsafe {
                let _ = ReleaseCapture();
            }
            if let Some((x, y, _, _)) = self.current_rect_tuple() {
                self.compact_pos = (x, y);
            }
            self.save_config();
            self.render_frame();
        }
    }

    fn drag_to_cursor(&mut self) {
        let Some((start_cursor, start_window, moved)) = self
            .dragging
            .as_ref()
            .map(|session| (session.start_cursor, session.start_window, session.moved))
        else {
            return;
        };
        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
        }
        let dx = cursor.x - start_cursor.x;
        let dy = cursor.y - start_cursor.y;
        if !moved && (dx.abs() > 2 || dy.abs() > 2) {
            if let Some(session) = &mut self.dragging {
                session.moved = true;
            }
        }
        let (compact_w, compact_h) = window::compact_px(self.dpi);
        let mut x = start_window.left + dx;
        let mut y = start_window.top + dy;
        (x, y) = clamp_compact((x, y), compact_w, compact_h);
        self.compact_pos = (x, y);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                compact_w,
                compact_h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.window_rect = RECT {
            left: x,
            top: y,
            right: x + compact_w,
            bottom: y + compact_h,
        };
        self.window_dip = (
            window::px_to_dip(compact_w, self.dpi),
            window::px_to_dip(compact_h, self.dpi),
        );
    }

    fn button_hit(&self, x: f32, y: f32) -> (bool, bool) {
        if !matches!(self.state, UiState::AskUpload { .. }) {
            return (false, false);
        }
        let scale = self.dpi.max(96) as f32 / 96.0;
        let (x, y) = (x / scale, y / scale);
        let (left, top, width, height) = self.pill_rect();
        let accept = ((x - (left + width - 30.0)).powi(2) + (y - (top + height - 30.0)).powi(2))
            .sqrt()
            <= 18.0;
        let reject = ((x - (left + width - 66.0)).powi(2) + (y - (top + height - 30.0)).powi(2))
            .sqrt()
            <= 18.0;
        (accept, reject)
    }

    fn update_cursor(&self, x: f32, y: f32) {
        let (accept, reject) = self.button_hit(x, y);
        let hand =
            accept || reject || matches!(self.state, UiState::Result { .. } | UiState::Busy { .. });
        let cursor = unsafe { LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }) };
        if let Ok(cursor) = cursor {
            unsafe {
                SetCursor(Some(cursor));
            }
        }
    }

    // ----- State / animations ----------------------------------------------------

    fn set_state(&mut self, state: UiState) {
        let composition = self.uses_composition();
        let old_expanded = self.scene.is_expanded();
        let new_scene = self.scene_for_state(&state);
        let new_expanded = new_scene.is_expanded();

        if new_expanded {
            self.pending_state = None;
            self.state = state;
            self.scene = new_scene;
            if old_expanded {
                self.expand = 1.0;
                self.expand_anim = None;
            } else if self.theme.animations_enabled {
                if composition {
                    // The renderer animates from the compact circle.
                    self.start_expand(1.0, NORMAL_MS);
                } else {
                    self.expand = 0.0;
                    self.start_expand(1.0, NORMAL_MS);
                }
            } else {
                self.expand = 1.0;
                self.expand_anim = None;
            }
            self.schedule_auto_close();
        } else if old_expanded {
            self.cancel_auto_close();
            if self.theme.animations_enabled {
                // Keep the outgoing pill on screen while it shrinks.
                self.pending_state = Some(state);
                self.start_expand(0.0, FAST_MS);
            } else {
                self.pending_state = None;
                self.state = state;
                self.scene = new_scene;
                self.expand = 0.0;
                self.expand_anim = None;
            }
        } else {
            self.pending_state = None;
            self.state = state;
            self.scene = new_scene;
            self.expand = 0.0;
            self.expand_anim = None;
            self.cancel_auto_close();
        }

        if matches!(self.state, UiState::Busy { .. }) {
            self.busy_started.get_or_insert_with(Instant::now);
            if self.theme.animations_enabled && !composition {
                self.ensure_frame_timer();
            }
        }
        if !composition
            && matches!(self.state, UiState::Result { .. })
            && self.pending_state.is_none()
        {
            if self.theme.animations_enabled && self.expand_anim.is_some() {
                // Count up once the panel has mostly opened.
                self.reveal = 0.0;
                self.reveal_anim = Some(Animation::fluent(700, 0.0, 1.0).delayed(120));
            } else {
                self.reveal = 1.0;
                self.reveal_anim = None;
            }
        }
        if composition {
            self.cancel_reveal();
            if matches!(self.state, UiState::Result { .. }) && self.pending_state.is_none() {
                if self.theme.animations_enabled {
                    // Text is content: count up in a few steps instead of
                    // repainting the panel every frame.
                    self.reveal = 0.0;
                    self.reveal_anim = Some(Animation::fluent(700, 0.0, 1.0).delayed(120));
                    unsafe {
                        SetTimer(Some(self.hwnd), TIMER_REVEAL, 33, None);
                    }
                } else {
                    self.reveal = 1.0;
                    self.reveal_anim = None;
                }
            }
        }
        if !composition {
            self.settle_transition();
        }
        if matches!(self.state, UiState::Idle) && self.expand_anim.is_none() {
            self.schedule_trim();
        } else {
            self.cancel_trim();
        }
        self.update_label_timer();
        self.update_tooltip();
        self.update_geometry(false).ok();
        self.render_frame();
    }

    /// Ends the collapse animation. Expansion does not need it: the window is
    /// already sized and only visuals move.
    fn on_animation_done(&mut self) {
        if self.pending_state.is_some() {
            if let Some(next) = self.pending_state.take() {
                self.state = next;
                self.scene = self.scene_for_state(&self.state);
                self.expand = 0.0;
                self.expand_anim = None;
                self.transition = false;
                self.update_geometry(true).ok();
                if matches!(self.state, UiState::Idle) {
                    self.schedule_trim();
                }
                self.update_label_timer();
                self.update_tooltip();
                self.render_frame();
            }
            return;
        }
        // Expansion finished: the window is already at its final size, but the
        // anchor no longer applies.
        if self.transition {
            let still_animating = self
                .renderer
                .as_ref()
                .map(Renderer::is_animating)
                .unwrap_or(false);
            if !still_animating {
                self.transition = false;
                self.update_geometry(true).ok();
                self.render_frame();
            }
        }
    }

    fn scene_for_state(&self, state: &UiState) -> Scene {
        match state {
            UiState::Idle | UiState::Dragging | UiState::Busy { .. } => Scene::Compact,
            UiState::AskUpload { name, .. } => Scene::AskUpload {
                filename: name.clone(),
                accept_hover: self.hover_accept,
                reject_hover: self.hover_reject,
            },
            UiState::Result { result, caption } => {
                let verdict = result
                    .stats
                    .verdict(self.config.threshold_amber, self.config.threshold_red);
                let color = match verdict {
                    crate::model::Verdict::Clean => self.theme.green(),
                    crate::model::Verdict::Suspicious => self.theme.amber(),
                    crate::model::Verdict::Malicious => self.theme.red(),
                };
                let reveal = self.reveal.clamp(0.0, 1.0);
                let (found, total) = result.stats.ratio();
                let shown = (found as f32 * reveal).round() as u32;
                let verdict = match verdict {
                    crate::model::Verdict::Clean => "Sin detecciones",
                    crate::model::Verdict::Suspicious => "Sospechoso",
                    crate::model::Verdict::Malicious => "Malicioso",
                };
                Scene::Result {
                    ratio: format!("{shown}/{total}"),
                    color: self.theme.accent.mix(color, reveal),
                    caption: Some(match caption {
                        Some(batch) => format!("{verdict} · {batch}"),
                        None => verdict.to_owned(),
                    }),
                }
            }
            UiState::Error { info } => Scene::Error {
                title: info.title.clone(),
            },
        }
    }

    fn refresh_scene(&mut self) {
        self.scene = self.scene_for_state(&self.state);
    }

    fn start_expand(&mut self, target: f32, duration: u64) {
        if self.uses_composition() {
            self.expand = target;
            self.expand_anim = None;
            self.begin_transition();
            return;
        }
        if !self.theme.animations_enabled {
            self.expand = target;
            self.expand_anim = None;
            return;
        }
        self.expand_anim = Some(Animation::fluent(duration, self.expand, target));
        self.begin_transition();
        self.ensure_frame_timer();
    }

    /// Sizes the window once for the whole expand/collapse so every frame is
    /// pure DirectComposition: no window changes until it settles.
    fn begin_transition(&mut self) {
        if self.transition {
            return;
        }
        self.transition = true;
        self.update_geometry(true).ok();
    }

    /// Ends the transition once no size animation is left.
    fn settle_transition(&mut self) {
        if !self.transition || self.expand_anim.is_some() {
            return;
        }
        self.transition = false;
        self.update_geometry(true).ok();
    }

    fn set_hover(&mut self, target: f32) {
        if !self.theme.animations_enabled {
            self.hover = target;
            self.hover_anim = None;
            self.render_frame();
            return;
        }
        self.hover_anim = Some(Animation::fluent(FAST_MS, self.hover, target));
        self.ensure_frame_timer();
    }

    fn ensure_frame_timer(&mut self) {
        if !self.uses_composition() {
            self.clock.start();
        }
    }

    fn cancel_reveal(&mut self) {
        if self.uses_composition() && self.reveal_anim.is_some() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_REVEAL);
            }
            self.reveal_anim = None;
        }
    }

    /// While polling, the elapsed "m:ss" is text: refresh it once a second
    /// instead of redrawing every frame.
    fn update_label_timer(&mut self) {
        let should_run = self.uses_composition() && matches!(self.state, UiState::Busy { .. });
        if should_run && !self.label_timer {
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_LABEL, 1_000, None);
            }
            self.label_timer = true;
        } else if !should_run && self.label_timer {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_LABEL);
            }
            self.label_timer = false;
        }
    }

    fn schedule_trim(&self) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_TRIM, 1_500, None);
        }
    }

    fn cancel_trim(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_TRIM);
        }
    }

    fn trim_working_set(&self) {
        unsafe {
            let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        }
    }

    fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_AUTOCLOSE => {
                if self.auto_close_due() {
                    self.cancel_auto_close();
                    if matches!(self.state, UiState::Result { .. } | UiState::Error { .. }) {
                        self.set_state(UiState::Idle);
                    }
                }
            }
            TIMER_TRIM => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_TRIM);
                }
                if matches!(self.state, UiState::Idle)
                    && self.expand_anim.is_none()
                    && !self.mouse_inside
                {
                    self.trim_working_set();
                }
            }
            TIMER_TOOLTIP => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_TOOLTIP);
                }
                if self.tooltip_text().is_some() && self.mouse_inside {
                    self.show_tooltip();
                } else {
                    self.tooltip_scheduled = false;
                }
            }
            TIMER_LABEL => {
                if matches!(self.state, UiState::Busy { .. }) {
                    self.render_frame();
                } else {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_LABEL);
                    }
                    self.label_timer = false;
                }
            }
            TIMER_REVEAL => {
                if let Some(animation) = self.reveal_anim {
                    self.reveal = animation.value_at(Instant::now());
                    if animation.is_finished() {
                        self.reveal = 1.0;
                        self.reveal_anim = None;
                        unsafe {
                            let _ = KillTimer(Some(self.hwnd), TIMER_REVEAL);
                        }
                    }
                    self.refresh_scene();
                    self.render_frame();
                } else {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_REVEAL);
                    }
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(animation) = self.hover_anim {
            self.hover = animation.value_at(now);
            if animation.is_finished() {
                self.hover = animation.finished_value();
                self.hover_anim = None;
            }
        }
        if let Some(animation) = self.reveal_anim {
            self.reveal = animation.value_at(now);
            if animation.is_finished() {
                self.reveal = animation.finished_value();
                self.reveal_anim = None;
            }
        }
        if let Some(animation) = self.expand_anim {
            self.expand = animation.value_at(now).clamp(0.0, 1.0);
            if animation.is_finished() {
                self.expand = animation.finished_value();
                self.expand_anim = None;
                if self.expand <= 0.0 {
                    if let Some(next) = self.pending_state.take() {
                        self.state = next;
                        self.scene = self.scene_for_state(&self.state);
                        self.expand = 0.0;
                        if matches!(self.state, UiState::Idle) {
                            self.schedule_trim();
                        }
                    }
                }
                self.settle_transition();
            }
        }

        self.refresh_scene();
        self.update_geometry(false).ok();
        self.render_frame();

        let busy = matches!(self.state, UiState::Busy { .. });
        if self.hover_anim.is_none()
            && self.expand_anim.is_none()
            && self.reveal_anim.is_none()
            && !busy
            && self.pending_state.is_none()
        {
            self.clock.stop();
        }
    }

    fn schedule_auto_close(&mut self) {
        self.cancel_auto_close();
        if !matches!(self.state, UiState::Result { .. } | UiState::Error { .. }) {
            return;
        }
        if self.config.auto_close_secs == 0 {
            return;
        }
        self.shown_tick = unsafe { GetTickCount() };
        self.seen_active = None;
        // One cheap check per second while a result is up.
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_AUTOCLOSE, 1_000, None);
        }
        self.auto_close_active = true;
    }

    /// A result only closes after it was visible for `auto_close_secs` while
    /// the user was actually at the PC and not hovering it, so a scan that
    /// finishes while they are away is still there when they come back.
    fn auto_close_due(&mut self) -> bool {
        if self.mouse_inside {
            self.seen_active = None;
            return false;
        }
        let mut input = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        let active = unsafe { GetLastInputInfo(&mut input) }.as_bool()
            && input.dwTime.wrapping_sub(self.shown_tick) as i32 >= 0;
        if active {
            let since = *self.seen_active.get_or_insert_with(Instant::now);
            since.elapsed().as_secs() >= self.config.auto_close_secs
        } else {
            false
        }
    }

    fn cancel_auto_close(&mut self) {
        if self.auto_close_active {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_AUTOCLOSE);
            }
            self.auto_close_active = false;
        }
    }

    fn answer_upload(&mut self, id: u64, accept: bool, index: usize, total: usize) {
        if let Some(worker) = &self.worker {
            worker.decision(id, accept);
        }
        self.busy_started.get_or_insert_with(Instant::now);
        let phase = if accept {
            Phase::Uploading
        } else {
            Phase::Querying
        };
        self.set_state(UiState::Busy {
            index,
            total,
            phase,
            progress: None,
        });
    }

    fn drain_worker_events(&mut self) {
        let events: Vec<WorkerEvent> = {
            let Ok(mut queue) = self.events.lock() else {
                return;
            };
            queue.drain(..).collect()
        };
        for event in events {
            self.handle_worker_event(event);
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        if self.cancelling {
            // Leftovers of the cancelled batch; nothing to show.
            if matches!(event, WorkerEvent::BatchDone { .. }) {
                self.cancelling = false;
            }
            return;
        }
        match event {
            WorkerEvent::Queued { total } => {
                self.batch_total = total;
                if let UiState::Busy { total: shown, .. }
                | UiState::AskUpload { total: shown, .. } = &mut self.state
                {
                    *shown = total;
                }
                self.render_frame();
            }
            WorkerEvent::BatchStarted { total } => {
                self.batch_total = total;
                self.batch_results.clear();
                self.batch_errors = 0;
                self.batch_skipped = 0;
            }
            WorkerEvent::Progress {
                index,
                total,
                phase,
            } => {
                self.batch_total = total;
                self.busy_started.get_or_insert_with(Instant::now);
                self.polling_since = match phase {
                    Phase::Polling => Some(self.polling_since.unwrap_or_else(Instant::now)),
                    _ => None,
                };
                self.hover_accept = false;
                self.hover_reject = false;
                self.set_state(UiState::Busy {
                    index,
                    total,
                    phase,
                    progress: None,
                });
            }
            WorkerEvent::Percent(value) => {
                if let UiState::Busy { progress, .. } = &mut self.state {
                    *progress = Some(value);
                    if !self.theme.animations_enabled {
                        self.render_frame();
                    }
                }
            }
            WorkerEvent::AskUpload {
                id,
                name,
                sha256: _,
                size: _,
                index,
                total,
            } => {
                self.busy_started = None;
                self.set_state(UiState::AskUpload {
                    id,
                    name,
                    index,
                    total,
                });
            }
            WorkerEvent::FileResult {
                index,
                total,
                result,
            } => {
                self.last_result = Some(result.clone());
                self.batch_results.push(result.clone());
                self.batch_total = total;
                if total == 1 {
                    self.busy_started = None;
                    self.set_state(UiState::Result {
                        result,
                        caption: None,
                    });
                } else {
                    self.busy_started.get_or_insert_with(Instant::now);
                    let _ = index;
                }
            }
            WorkerEvent::FileSkipped { total, name, .. } => {
                let _ = name;
                self.batch_total = total;
                self.batch_skipped += 1;
            }
            WorkerEvent::FileError { total, error, .. } => {
                self.batch_total = total;
                self.batch_errors += 1;
                if total == 1 {
                    self.busy_started = None;
                    self.set_state(UiState::Error { info: error });
                }
            }
            WorkerEvent::BatchDone {
                total,
                results,
                errors,
                skipped,
            } => {
                self.busy_started = None;
                self.batch_errors = errors;
                self.batch_skipped = skipped;
                if total > 1 {
                    crate::toast::show_summary(&results, errors, skipped);
                    if let Some(worst) = results
                        .iter()
                        .max_by_key(|result| result.stats.severity())
                        .cloned()
                    {
                        self.last_result = Some(worst.clone());
                        self.set_state(UiState::Result {
                            result: worst,
                            caption: Some(format!("peor de {total}")),
                        });
                    } else if errors > 0 {
                        self.set_state(UiState::Error {
                            info: ErrorInfo {
                                title: "Sin resultados".into(),
                                detail: "No se pudo analizar ningún archivo del lote.".into(),
                            },
                        });
                    } else {
                        self.set_state(UiState::Idle);
                    }
                } else if total == 0 {
                    self.set_state(UiState::Error {
                        info: ErrorInfo {
                            title: "Nada que analizar".into(),
                            detail: "Se ignoraron las carpetas. Activa la opción en config.json."
                                .into(),
                        },
                    });
                } else if total == 1 && results.is_empty() && errors == 0 && skipped > 0 {
                    self.set_state(UiState::Idle);
                }
            }
        }
    }

    // ----- Rendering/geometry ----------------------------------------------------

    /// "42%" while hashing/uploading, elapsed "m:ss" while VirusTotal analyses
    /// an upload (it has no percentage; this shows it is alive). The frame
    /// clock already runs while busy, so the seconds tick on their own.
    /// ponytail: frozen when Windows animations are off (no clock then).
    fn busy_label(&self, progress: Option<f32>) -> Option<String> {
        if progress.is_some() {
            return Some(format!(
                "{}%",
                (self.shown_progress.clamp(0.0, 1.0) * 100.0).round() as u32
            ));
        }
        match (&self.state, self.polling_since) {
            (
                UiState::Busy {
                    phase: Phase::Polling,
                    ..
                },
                Some(since),
            ) => {
                let seconds = since.elapsed().as_secs();
                Some(format!("{}:{:02}", seconds / 60, seconds % 60))
            }
            _ => None,
        }
    }

    fn current_dip_size(&self) -> (f32, f32) {
        let (target_w, target_h) = self.scene.target_size();
        let t = self.expand.clamp(0.0, 1.0);
        (lerp(64.0, target_w, t), lerp(64.0, target_h, t))
    }

    /// Offset of the pill centre from the window centre, in DIPs. Non-zero only
    /// mid-transition, when the window was clamped to the work area and the
    /// pill must still grow out of the compact shield's position.
    fn anchor(&self) -> (f32, f32) {
        if !self.transition {
            return (0.0, 0.0);
        }
        let (compact_w, compact_h) = window::compact_px(self.dpi);
        let center_x = self.compact_pos.0 + compact_w / 2 - self.window_rect.left;
        let center_y = self.compact_pos.1 + compact_h / 2 - self.window_rect.top;
        let rest = 1.0 - self.expand.clamp(0.0, 1.0);
        (
            (window::px_to_dip(center_x, self.dpi) - self.window_dip.0 * 0.5) * rest,
            (window::px_to_dip(center_y, self.dpi) - self.window_dip.1 * 0.5) * rest,
        )
    }

    /// Same offset without the animation factor: the compositor scales it.
    fn anchor_raw(&self) -> (f32, f32) {
        if !self.transition {
            return (0.0, 0.0);
        }
        let (compact_w, compact_h) = window::compact_px(self.dpi);
        let center_x = self.compact_pos.0 + compact_w / 2 - self.window_rect.left;
        let center_y = self.compact_pos.1 + compact_h / 2 - self.window_rect.top;
        (
            window::px_to_dip(center_x, self.dpi) - self.window_dip.0 * 0.5,
            window::px_to_dip(center_y, self.dpi) - self.window_dip.1 * 0.5,
        )
    }

    /// The pill's rectangle in window client DIPs.
    fn pill_rect(&self) -> (f32, f32, f32, f32) {
        let (w, h) = self.current_dip_size();
        let (anchor_x, anchor_y) = self.anchor();
        (
            (self.window_dip.0 - w) * 0.5 + anchor_x,
            (self.window_dip.1 - h) * 0.5 + anchor_y,
            w,
            h,
        )
    }

    fn current_rect_tuple(&self) -> Option<(i32, i32, i32, i32)> {
        if self.window_rect.right <= self.window_rect.left {
            None
        } else {
            Some((
                self.window_rect.left,
                self.window_rect.top,
                self.window_rect.right - self.window_rect.left,
                self.window_rect.bottom - self.window_rect.top,
            ))
        }
    }

    fn update_geometry(&mut self, force: bool) -> WinResult<()> {
        let (dip_w, dip_h) = if self.transition {
            let (target_w, target_h) = self.scene.target_size();
            (target_w.max(64.0), target_h.max(64.0))
        } else {
            self.current_dip_size()
        };
        let px_w = window::dip_to_px(dip_w, self.dpi).max(1);
        let px_h = window::dip_to_px(dip_h, self.dpi).max(1);
        let (compact_w, compact_h) = window::compact_px(self.dpi);
        let center_x = self.compact_pos.0 + compact_w / 2;
        let center_y = self.compact_pos.1 + compact_h / 2;
        let mut x = center_x - px_w / 2;
        let mut y = center_y - px_h / 2;
        let (work_left, work_top, work_right, work_bottom) = work_area_for(POINT {
            x: center_x,
            y: center_y,
        });
        x = x.clamp(work_left, (work_right - px_w).max(work_left));
        y = y.clamp(work_top, (work_bottom - px_h).max(work_top));

        let new_rect = RECT {
            left: x,
            top: y,
            right: x + px_w,
            bottom: y + px_h,
        };
        let changed = self.window_rect.left != new_rect.left
            || self.window_rect.top != new_rect.top
            || self.window_rect.right != new_rect.right
            || self.window_rect.bottom != new_rect.bottom;
        if changed || force {
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    None,
                    x,
                    y,
                    px_w,
                    px_h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )?;
            }
            self.window_rect = new_rect;
            self.window_dip = (
                window::px_to_dip(px_w, self.dpi),
                window::px_to_dip(px_h, self.dpi),
            );
        }
        Ok(())
    }

    fn render_frame(&mut self) {
        self.refresh_scene();
        let composition = self.uses_composition();
        let hover = self.hover.clamp(0.0, 1.0);
        let progress = match &self.state {
            UiState::Busy { progress, .. } => *progress,
            _ => None,
        };
        // Glide towards the reported value instead of jumping in 1 % steps.
        if !composition {
            self.shown_progress = match progress {
                Some(target) if self.theme.animations_enabled => {
                    lerp(self.shown_progress, target, 0.25)
                }
                Some(target) => target,
                None => 0.0,
            };
        }
        let ring = if !composition && matches!(self.state, UiState::Busy { .. }) {
            let started = self.busy_started.filter(|_| self.theme.animations_enabled);
            Some(match (progress, started) {
                (Some(_), _) => (
                    -std::f32::consts::FRAC_PI_2,
                    std::f32::consts::TAU * self.shown_progress.clamp(0.001, 1.0),
                ),
                (None, Some(started)) => ring_arc(started.elapsed()),
                (None, None) => (-std::f32::consts::FRAC_PI_2, std::f32::consts::PI * 1.5),
            })
        } else {
            None
        };
        let badge = match &self.state {
            UiState::Busy { index, total, .. } if *total > 1 => Some((*index, *total)),
            _ => None,
        };
        let (window_w, window_h) = self.current_dip_size();
        let compact_alpha = if self.pending_state.is_some() {
            (1.0 - self.expand).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let busy = matches!(self.state, UiState::Busy { .. });
        let state = DrawState {
            scene: &self.scene,
            theme: self.theme,
            window_w,
            window_h,
            expand: self.expand.clamp(0.0, 1.0),
            compact_alpha,
            alpha: 1.0,
            hover_scale: 1.0 + 0.1 * hover,
            glow: if composition { 0.0 } else { hover },
            ring,
            label: if composition {
                self.composition_label(progress)
            } else {
                self.busy_label(progress)
            },
            badge,
            hovered: self.mouse_inside || self.drag_valid,
            cancel_hint: self.mouse_inside
                && self.dragging.is_none()
                && matches!(self.state, UiState::Busy { .. }),
            busy,
            progress,
        };
        let (layout_w, layout_h) = self.window_dip;
        let (anchor_x, anchor_y) = if composition {
            self.anchor_raw()
        } else {
            self.anchor()
        };
        let failed = self
            .renderer
            .as_mut()
            .map(|renderer| {
                renderer
                    .set_layout(layout_w, layout_h, anchor_x, anchor_y)
                    .and_then(|_| renderer.render(&state))
                    .is_err()
            })
            .unwrap_or(false);
        if failed {
            // Release the old composition target before creating another one
            // for the same HWND.
            self.renderer = None;
            let prefer_composition = crate::ui::composition_supported(&self.theme);
            if let Ok(mut renderer) = Renderer::new(
                self.hwnd,
                self.dpi,
                self.theme,
                prefer_composition,
                self.acrylic,
            ) {
                let _ = renderer.set_layout(layout_w, layout_h, anchor_x, anchor_y);
                let _ = renderer.render(&state);
                self.renderer = Some(renderer);
            }
        }
    }

    /// Elapsed "m:ss" while polling, "42%" while hashing/uploading. The
    /// compositor only redraws the text when this string changes.
    fn composition_label(&self, progress: Option<f32>) -> Option<String> {
        if let Some(progress) = progress {
            return Some(format!(
                "{}%",
                (progress.clamp(0.0, 1.0) * 100.0).round() as u32
            ));
        }
        match (&self.state, self.polling_since) {
            (
                UiState::Busy {
                    phase: Phase::Polling,
                    ..
                },
                Some(since),
            ) => {
                let seconds = since.elapsed().as_secs();
                Some(format!("{}:{:02}", seconds / 60, seconds % 60))
            }
            _ => None,
        }
    }

    // ----- Theme/backdrop --------------------------------------------------------

    /// The widget is a circle/capsule drawn with per-pixel alpha, so DWM must
    /// not add a backdrop, rounded corners or a square border around it.
    fn apply_backdrop(&mut self) {
        if self.uses_composition() {
            // The composition tree owns the shape and the blur.
            window::make_frameless(self.hwnd);
            window::set_host_backdrop(self.hwnd, self.acrylic);
            return;
        }
        window::apply_backdrop(self.hwnd, &self.theme, BackdropMode::Solid);
        window::make_frameless(self.hwnd);
    }

    fn refresh_theme(&mut self) {
        let theme = Theme::detect();
        let dark_changed = theme.dark != self.theme.dark;
        let acrylic = theme.is_win11() && !theme.effective_solid();
        let rebuild = acrylic != self.acrylic;
        self.theme = theme;
        if rebuild {
            // Transparency toggled: the backdrop brush cannot be swapped, so
            // rebuild the target once.
            self.acrylic = acrylic;
            let prefer_composition = crate::ui::composition_supported(&theme);
            self.renderer = None;
            self.renderer =
                Renderer::new(self.hwnd, self.dpi, theme, prefer_composition, acrylic).ok();
        } else if let Some(renderer) = self.renderer.as_mut() {
            let _ = renderer.set_theme(theme);
        }
        if dark_changed {
            theme::apply_native_menu_theme(theme.dark);
            window::allow_dark_mode(self.hwnd, theme.dark);
        }
        if let Some(tooltip) = &mut self.tooltip {
            tooltip.set_theme(&theme);
        }
        self.apply_backdrop();
        self.update_tooltip();
        self.refresh_scene();
        self.render_frame();
    }

    fn on_dpi_changed(&mut self, wparam: WPARAM, lparam: LPARAM) {
        let dpi = ((wparam.0 >> 16) & 0xFFFF) as u32;
        if dpi == 0 {
            return;
        }
        let (old_compact_w, old_compact_h) = window::compact_px(self.dpi);
        let center = (
            self.compact_pos.0 + old_compact_w / 2,
            self.compact_pos.1 + old_compact_h / 2,
        );
        self.dpi = dpi;
        let (new_w, new_h) = window::compact_px(dpi);
        self.compact_pos = (center.0 - new_w / 2, center.1 - new_h / 2);
        self.compact_pos = clamp_compact(self.compact_pos, new_w, new_h);

        self.renderer = None;
        let prefer_composition = crate::ui::composition_supported(&self.theme);
        self.renderer =
            Renderer::new(self.hwnd, dpi, self.theme, prefer_composition, self.acrylic).ok();
        if let Some(tooltip) = &mut self.tooltip {
            tooltip.set_theme(&self.theme);
        }
        self.apply_backdrop();
        if let Some(rect) = unsafe { (lparam.0 as *const RECT).as_ref() } {
            // Respect the suggested monitor rectangle for the compact state.
            if !self.scene.is_expanded() {
                self.compact_pos = (rect.left, rect.top);
            }
        }
        let _ = self.update_geometry(true);
        if let Some(renderer) = self.renderer.as_mut() {
            let _ = renderer.set_theme(self.theme);
        }
        self.render_frame();
    }

    // ----- Menu/tooltip ----------------------------------------------------------

    fn show_context_menu(&mut self) {
        self.cancel_tooltip();
        let startup = config::is_start_with_windows();
        let mut items = vec![
            MenuItem::new("Siempre encima", MENU_TOPMOST, self.config.always_on_top),
            MenuItem::new("Iniciar con Windows", MENU_STARTUP, startup),
            MenuItem::new("Configurar API key…", MENU_API_KEY, false),
            if self.last_result.is_some() {
                MenuItem::new("Abrir último resultado", MENU_LAST_RESULT, false)
            } else {
                MenuItem::disabled("Abrir último resultado", MENU_LAST_RESULT, false)
            },
            MenuItem::separator(),
            MenuItem::new("Salir", MENU_QUIT, false),
        ];
        if matches!(self.state, UiState::Busy { .. } | UiState::AskUpload { .. }) {
            items.insert(0, MenuItem::new("Cancelar análisis", MENU_CANCEL, false));
            items.insert(1, MenuItem::separator());
        }
        let mut anchor = POINT::default();
        let cursor_ok = unsafe { GetCursorPos(&mut anchor) }.is_ok();
        if !cursor_ok || (anchor.x == 0 && anchor.y == 0) {
            let mut rect = RECT::default();
            if unsafe { GetWindowRect(self.hwnd, &mut rect) }.is_ok() {
                anchor = POINT {
                    x: rect.right - 8,
                    y: rect.bottom - 8,
                };
            }
        }
        let command = menu::show(self.hwnd, self.theme, items, anchor);
        self.render_frame();
        match command {
            Some(MENU_TOPMOST) => {
                self.config.always_on_top = !self.config.always_on_top;
                window::set_topmost(self.hwnd, self.config.always_on_top);
                self.save_config();
            }
            Some(MENU_STARTUP) => {
                let enabled = !config::is_start_with_windows();
                let _ = config::set_start_with_windows(enabled);
            }
            Some(MENU_API_KEY) => {
                self.open_key_dialog();
            }
            Some(MENU_LAST_RESULT) => {
                if let Some(result) = &self.last_result {
                    open_virustotal(&result.sha256);
                }
            }
            Some(MENU_CANCEL) => self.cancel_scan(),
            Some(MENU_QUIT) => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
            _ => {}
        }
    }
    fn open_key_dialog(&mut self) {
        self.prompt_api_key();
    }

    fn prompt_api_key(&mut self) {
        if matches!(self.state, UiState::Busy { .. } | UiState::AskUpload { .. }) {
            return;
        }
        let existing = self.has_api_key;
        if let Some(key) = config::key_dialog::show(self.hwnd, &self.theme, existing) {
            self.install_api_key(key);
        }
    }

    fn install_api_key(&mut self, key: Zeroizing<String>) {
        if let Some(old) = self.worker.take() {
            old.shutdown();
        }
        self.worker = Some(WorkerHandle::spawn(
            self.hwnd,
            self.events.clone(),
            Some(key),
            self.config.clone(),
        ));
        self.has_api_key = true;
        self.update_tooltip();
    }

    fn create_tooltip(&mut self) {
        self.tooltip = Tooltip::new(self.hwnd, &self.theme);
        if let Some(tooltip) = &mut self.tooltip {
            tooltip.attach();
        }
    }

    fn tooltip_text(&self) -> Option<String> {
        match &self.state {
            UiState::Error { info } => Some(info.detail.clone()),
            UiState::Busy {
                index,
                total,
                phase,
                ..
            } => {
                let what = match phase {
                    Phase::Hashing => "Calculando la huella SHA-256…",
                    Phase::Querying => "Consultando VirusTotal…",
                    Phase::Uploading => "Subiendo el archivo a VirusTotal…",
                    Phase::Polling => {
                        "VirusTotal está analizando el archivo; puede tardar unos minutos. El resultado quedará visible aquí."
                    }
                };
                let what = if *total > 1 {
                    format!("Archivo {index} de {total}: {what}")
                } else {
                    what.to_owned()
                };
                Some(format!(
                    "{what}
Puedes soltar más archivos. Clic para cancelar."
                ))
            }
            _ => None,
        }
    }

    fn schedule_tooltip(&mut self) {
        if self.tooltip_scheduled || self.tooltip_text().is_none() {
            return;
        }
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_TOOLTIP, 450, None);
        }
        self.tooltip_scheduled = true;
    }

    fn cancel_tooltip(&mut self) {
        if self.tooltip_scheduled {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_TOOLTIP);
            }
            self.tooltip_scheduled = false;
        }
        if let Some(tooltip) = &mut self.tooltip {
            tooltip.hide();
        }
    }

    fn show_tooltip(&mut self) {
        if let (Some(text), Some(tooltip)) = (self.tooltip_text(), self.tooltip.as_mut()) {
            tooltip.show(&text);
        }
    }

    fn update_tooltip(&mut self) {
        self.cancel_tooltip();
        if self.tooltip_text().is_some() && self.mouse_inside {
            self.schedule_tooltip();
        }
    }

    // ----- Persistence -----------------------------------------------------------

    fn save_config(&mut self) {
        let (compact_w, compact_h) = window::compact_px(self.dpi);
        let (x, y) = if self.scene.is_expanded() {
            self.compact_pos
        } else {
            (self.window_rect.left, self.window_rect.top)
        };
        let (_, _) = (compact_w, compact_h);
        self.config.position = Some([x, y]);
        let _ = self.config.save();
    }

    fn kill_all_timers(&mut self) {
        self.clock.stop();
        self.label_timer = false;
        for id in [
            TIMER_AUTOCLOSE,
            TIMER_TRIM,
            TIMER_TOOLTIP,
            TIMER_LABEL,
            TIMER_REVEAL,
        ] {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), id);
            }
        }
    }
}

/// Only theme, transparency, accent, reduced-motion and contrast changes need a
/// refresh; `WM_SETTINGCHANGE` also fires for unrelated things like env vars.
fn setting_affects_theme(wparam: WPARAM, lparam: LPARAM) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_SETCLIENTAREAANIMATION, SPI_SETHIGHCONTRAST,
    };
    let action = wparam.0 as u32;
    if action == SPI_SETCLIENTAREAANIMATION.0 || action == SPI_SETHIGHCONTRAST.0 {
        return true;
    }
    if lparam.0 == 0 {
        return false;
    }
    let area = unsafe { windows::core::PCWSTR(lparam.0 as *const u16).to_string() };
    area.map(|area| area == "ImmersiveColorSet")
        .unwrap_or(false)
}

fn open_virustotal(sha256: &str) {
    let url = HSTRING::from(format!("https://www.virustotal.com/gui/file/{sha256}"));
    unsafe {
        let _ = ShellExecuteW(None, w!("open"), &url, None, None, SW_SHOWNORMAL);
    }
}

fn default_position(hwnd: HWND, width: i32, height: i32) -> (i32, i32) {
    let mut rect = RECT::default();
    let ok = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
            windows::Win32::UI::WindowsAndMessaging::SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut RECT as *mut _),
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        let _ = hwnd;
        return (100, 100);
    }
    (rect.right - width - 24, rect.bottom - height - 24)
}

fn work_area_for(point: POINT) -> (i32, i32, i32, i32) {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            (
                info.rcWork.left,
                info.rcWork.top,
                info.rcWork.right,
                info.rcWork.bottom,
            )
        } else {
            (0, 0, 1920, 1080)
        }
    }
}

fn clamp_compact(position: (i32, i32), width: i32, height: i32) -> (i32, i32) {
    let (left, top, right, bottom) = work_area_for(POINT {
        x: position.0 + width / 2,
        y: position.1 + height / 2,
    });
    (
        position.0.clamp(left, (right - width).max(left)),
        position.1.clamp(top, (bottom - height).max(top)),
    )
}

#[allow(dead_code)]
fn _unused(_: &App) {}
