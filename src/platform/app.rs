//! The UI owns its windows and hooks. Callbacks only enqueue small notifications.

use super::observer::{self, Focus, Request, Sample};
use crate::model::{
    composition_with_idle_evidence, place_badge, Composition, CompositionPolicy, IdleGate,
    InputMode, QueryRetry, Rect,
};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::Instant,
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
        UI::{Accessibility::*, HiDpi::*, Shell::*, WindowsAndMessaging::*},
    },
};

const CLASS_NAME: PCWSTR = w!("CaretModeController.v1");
const OVERLAY_CLASS: PCWSTR = w!("CaretModeOverlay.v1");
// Resource ID in assets/caret-mode.rc.
const APP_ICON: PCWSTR = PCWSTR(std::ptr::without_provenance(1));
const WM_CHANGED: u32 = WM_APP + 1;
pub const WM_SAMPLE: u32 = WM_APP + 2;
const WM_TRAY: u32 = WM_APP + 3;
const TIMER_WAIT: usize = 1;
const TIMER_REFRESH: usize = 2;
const TIMER_SMOKE: usize = 3;
pub const CHANGE_FOCUS: u32 = 1;
pub const CHANGE_EDIT: u32 = 2;
pub const CHANGE_CARET: u32 = 4;
pub const CHANGE_USER_EDIT: u32 = 8;
pub const CHANGE_FINISH: u32 = 16;
pub const CHANGE_COMPOSITION_START: u32 = 32;
static ACTIVE_NOTIFY: OnceLock<Arc<Notify>> = OnceLock::new();

pub struct Notify {
    window: AtomicUsize,
    pending: AtomicU32,
    last_edit_action: AtomicU32,
}
impl Notify {
    pub fn new(window: usize) -> Self {
        Self {
            window: AtomicUsize::new(window),
            pending: AtomicU32::new(0),
            last_edit_action: AtomicU32::new(0),
        }
    }
    pub fn post(&self, message: u32) {
        let window = self.window.load(Ordering::Acquire);
        if window != 0 {
            unsafe {
                let _ = PostMessageW(Some(observer::hwnd(window)), message, WPARAM(0), LPARAM(0));
            }
        }
    }
    pub fn changed(&self, flags: u32) {
        // Coalescing must preserve whether typing occurred after completion.
        // Retain only the latest action class, never the key itself.
        if flags & CHANGE_USER_EDIT != 0 {
            self.last_edit_action
                .store(CHANGE_USER_EDIT, Ordering::Release);
        } else if flags & CHANGE_FINISH != 0 {
            self.last_edit_action
                .store(CHANGE_FINISH, Ordering::Release);
        }
        if self.pending.fetch_or(flags, Ordering::AcqRel) == 0 {
            self.post(WM_CHANGED);
        }
    }
}

struct Hooks {
    keyboard: HHOOK,
    events: Vec<HWINEVENTHOOK>,
}
impl Hooks {
    unsafe fn install() -> windows::core::Result<Self> {
        let keyboard = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_event),
            Some(GetModuleHandleW(None)?.into()),
            0,
        )?;
        let mut hooks = Self {
            keyboard,
            events: Vec::new(),
        };
        for event in [
            EVENT_SYSTEM_FOREGROUND,
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_HIDE,
            EVENT_OBJECT_DESTROY,
        ] {
            let hook = SetWinEventHook(
                event,
                event,
                None,
                Some(window_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if hook.0.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            hooks.events.push(hook);
        }
        Ok(hooks)
    }
}
impl Drop for Hooks {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.keyboard);
            for hook in &self.events {
                let _ = UnhookWinEvent(*hook);
            }
        }
    }
}

unsafe extern "system" fn keyboard_event(code: i32, event: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0
        && (event.0 == WM_KEYDOWN as usize || event.0 == WM_SYSKEYDOWN as usize)
        && data.0 != 0
    {
        // KBDLLHOOKSTRUCT is valid only during this callback; no pointer escapes.
        let key = &*(data.0 as *const KBDLLHOOKSTRUCT);
        if let Some(notify) = ACTIVE_NOTIFY.get() {
            let alt = key.flags.0 & LLKHF_ALTDOWN.0 != 0;
            // Only the layout-specific keys need this small native query; no
            // accessibility/IME calls are performed inside the keyboard hook.
            let japanese = if (0xf0..=0xf5).contains(&key.vkCode) || (key.vkCode == 0xc0 && alt) {
                let thread = Focus::current()
                    .map(|focus| GetWindowThreadProcessId(observer::hwnd(focus.input), None))
                    .unwrap_or_else(|| GetWindowThreadProcessId(GetForegroundWindow(), None));
                windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout(thread).0 as usize
                    & 0x3ff
                    == 0x11
            } else {
                false
            };
            let flags = keyboard_change(key.vkCode, alt, japanese);
            if flags != 0 {
                notify.changed(flags);
            }
        }
    }
    CallNextHookEx(None, code, event, data)
}

fn keyboard_change(key: u32, alt: bool, japanese: bool) -> u32 {
    match key {
        // Modifier keys do not start an edit.
        0x10..=0x12 | 0x5b..=0x5c | 0xa0..=0xa5 => 0,
        // Cancellation ends an edit and must restart the idle deadline.
        0x0d | 0x1b => CHANGE_EDIT | CHANGE_FINISH,
        // Japanese DBE/OEM mode keys and Alt+backquote are not character input.
        0xf0..=0xf5 if japanese => CHANGE_CARET,
        0xc0 if japanese && alt => CHANGE_CARET,
        // Caret navigation, IME toggles and Caps Lock trigger a fresh sample.
        0x14..=0x1f | 0x21..=0x28 | 0x09 => CHANGE_CARET,
        _ => CHANGE_EDIT | CHANGE_USER_EDIT,
    }
}

unsafe extern "system" fn window_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if let Some(notify) = ACTIVE_NOTIFY.get() {
        if event == EVENT_SYSTEM_FOREGROUND || event == EVENT_OBJECT_FOCUS {
            notify.changed(CHANGE_FOCUS);
        } else if object == OBJID_CARET.0
            || (event == EVENT_OBJECT_DESTROY
                && Focus::current().is_some_and(|focus| focus.input == window.0 as usize))
        {
            notify.changed(CHANGE_CARET);
        }
    }
}

struct Tray {
    data: NOTIFYICONDATAW,
    _icon: AppIcon,
}

struct AppIcon(HICON);
impl AppIcon {
    unsafe fn load(width: i32, height: i32) -> windows::core::Result<Self> {
        let image = LoadImageW(
            Some(GetModuleHandleW(None)?.into()),
            APP_ICON,
            IMAGE_ICON,
            width,
            height,
            LR_DEFAULTCOLOR,
        )?;
        Ok(Self(HICON(image.0)))
    }
}
impl Drop for AppIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.0);
        }
    }
}

struct WindowGuard(HWND);
impl Drop for WindowGuard {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(Some(self.0)).as_bool() {
                let _ = DestroyWindow(self.0);
            }
        }
    }
}

struct NotificationGuard(Arc<Notify>);
impl Drop for NotificationGuard {
    fn drop(&mut self) {
        self.0.window.store(0, Ordering::Release);
    }
}

struct UserDataGuard(HWND);
impl Drop for UserDataGuard {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(Some(self.0)).as_bool() {
                SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
            }
        }
    }
}
impl Tray {
    unsafe fn new(window: HWND) -> windows::core::Result<Self> {
        let icon = AppIcon::load(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))?;
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon.0,
            ..Default::default()
        };
        let tip: Vec<_> = "Caret Mode — 待機中の入力モード表示"
            .encode_utf16()
            .collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self { data, _icon: icon })
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
        }
    }
}

struct State {
    window: HWND,
    overlay: HWND,
    notify: Arc<Notify>,
    requests: Option<mpsc::SyncSender<Request>>,
    samples: mpsc::Receiver<Sample>,
    gate: IdleGate,
    start: Instant,
    focus: Option<Focus>,
    busy: bool,
    redraw: Option<(Rect, InputMode)>,
    tray: Tray,
    taskbar_created: u32,
    hooks: Hooks,
    smoke_report: Option<std::path::PathBuf>,
    samples_received: u64,
    retry: QueryRetry,
    trace_report: Option<std::path::PathBuf>,
    composition_policy: CompositionPolicy,
    menu_open: bool,
    last_input: Option<Focus>,
    finished_focus: Option<Focus>,
    finished_at_ms: u64,
    finished_element: u64,
    observed_element: u64,
    inactive_observed: bool,
    last_change_flags: u32,
}

impl State {
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn remaining_ms(&self) -> u64 {
        let now = self.now();
        let finish_wait = if self.finished_focus.is_some() {
            self.gate
                .delay_ms()
                .saturating_sub(now.saturating_sub(self.finished_at_ms))
        } else {
            0
        };
        self.gate.remaining_ms(now).max(finish_wait)
    }

    unsafe fn hide(&mut self) {
        if self.redraw.take().is_some() {
            let _ = ShowWindow(self.overlay, SW_HIDE);
        }
    }

    unsafe fn schedule(&self, timer: usize, milliseconds: u32) {
        SetTimer(Some(self.window), timer, milliseconds.max(1), None);
    }

    unsafe fn refresh(&mut self) {
        if self.menu_open {
            self.hide();
            return;
        }
        let current = Focus::current();
        if self.finished_focus != current {
            self.finished_focus = None;
        }
        if current != self.focus {
            self.focus = current;
            self.observed_element = 0;
            self.inactive_observed = false;
            self.gate.focus_changed();
            self.hide();
        }
        if self.gate.paused || current.is_none() {
            self.hide();
            let _ = KillTimer(Some(self.window), TIMER_REFRESH);
            return;
        }
        let remaining = self.remaining_ms();
        if remaining != 0 {
            self.hide();
            self.schedule(TIMER_WAIT, remaining as u32);
            return;
        }
        if !self.busy {
            if let (Some(requests), Some(focus)) = (&self.requests, current) {
                if requests
                    .try_send(Request {
                        focus,
                        revision: self.gate.revision(),
                    })
                    .is_ok()
                {
                    self.busy = true;
                }
            }
        }
    }

    unsafe fn changed(&mut self) {
        let flags = self.notify.pending.swap(0, Ordering::AcqRel);
        self.last_change_flags = flags;
        self.hide();
        let _ = KillTimer(Some(self.window), TIMER_REFRESH);
        if self.menu_open {
            // Drain the coalesced notification even while the menu owns focus.
            // Otherwise pending != 0 prevents any future WM_CHANGED from posting.
            self.gate.invalidate();
            return;
        }
        // A provider can deliver the final composition update after Enter/Esc.
        // Real typing always invalidates the evidence immediately. A short
        // grace period prevents the delayed update from undoing that finish.
        let late_update = self.finished_focus == Focus::current()
            && self.finished_focus.is_some()
            && self.now().saturating_sub(self.finished_at_ms) <= 500;
        if flags & CHANGE_USER_EDIT != 0 || (flags & CHANGE_COMPOSITION_START != 0 && !late_update)
        {
            self.finished_focus = None;
            self.inactive_observed = false;
        }
        if flags & CHANGE_FINISH != 0
            && (flags & CHANGE_USER_EDIT == 0
                || self.notify.last_edit_action.load(Ordering::Acquire) == CHANGE_FINISH)
        {
            self.finished_focus = Focus::current();
            self.finished_at_ms = self.now();
            self.finished_element = self.observed_element;
        }
        if flags & CHANGE_FOCUS != 0 {
            if Focus::current() != self.focus {
                self.gate.focus_changed();
            } else {
                // Logical focus can move into autocomplete without a new
                // native input window. Keep the last edit's idle deadline.
                self.gate.invalidate();
            }
        }
        // An edit and a focus event can arrive together. The edit takes precedence.
        if flags & CHANGE_EDIT != 0 {
            self.gate.edited(self.now());
            self.schedule(TIMER_WAIT, self.gate.remaining_ms(self.now()) as u32);
        } else {
            self.gate.invalidate();
            self.refresh();
        }
    }

    unsafe fn results(&mut self) {
        while let Ok(sample) = self.samples.try_recv() {
            self.samples_received += 1;
            self.busy = false;
            if self.menu_open {
                self.hide();
                self.trace(&sample);
                continue;
            }
            if self.finished_focus == Some(sample.request.focus) && self.remaining_ms() != 0 {
                self.hide();
                self.schedule(TIMER_WAIT, self.remaining_ms() as u32);
                self.trace(&sample);
                continue;
            }
            if self.notify.pending.load(Ordering::Acquire) != 0
                || Focus::current() != Some(sample.request.focus)
                || !self
                    .gate
                    .allows(self.now(), sample.request.revision, Composition::Inactive)
            {
                self.hide();
                if self.notify.pending.load(Ordering::Acquire) == 0
                    && sample.request.revision != self.gate.revision()
                {
                    self.refresh();
                }
                self.trace(&sample);
                continue;
            }
            if sample.element != self.observed_element {
                self.observed_element = sample.element;
                self.inactive_observed = false;
                self.finished_focus = None;
            }
            if sample.element != 0 && sample.composition == Composition::Inactive {
                self.inactive_observed = true;
            }
            // An IME-off sample is inactive. If the same field later reports
            // an old active range without a new edit/composition event, the
            // provider's range conflicts with that observation. Compatibility
            // mode treats it as unknown; strict mode still trusts the range.
            let composition = composition_with_idle_evidence(
                sample.composition,
                self.inactive_observed
                    || (self.finished_focus == Some(sample.request.focus)
                        && self.finished_element == sample.element
                        && sample.element != 0),
                self.composition_policy,
            );
            if !self.gate.allows_with_policy(
                self.now(),
                sample.request.revision,
                composition,
                self.composition_policy,
            ) {
                self.hide();
                if sample.composition == Composition::Active {
                    // Finalization/cancellation events are not guaranteed. Keep
                    // checking while idle, without guessing that composition ended.
                    self.schedule(TIMER_REFRESH, 500);
                } else {
                    self.retry_unavailable(&sample);
                }
                self.trace(&sample);
                continue;
            }
            if let (Some(caret), Some(mode)) = (sample.caret, sample.mode) {
                self.last_input = Some(sample.request.focus);
                self.retry.recovered();
                self.show(caret, mode);
                // Supplement missing IME mode-change notifications only while idle.
                self.schedule(TIMER_REFRESH, 250);
            } else {
                self.hide();
                self.retry_unavailable(&sample);
            }
            self.trace(&sample);
        }
    }

    unsafe fn retry_unavailable(&mut self, sample: &Sample) {
        if matches!(
            sample.reason,
            "caret-unavailable"
                | "uia-focus-unavailable"
                | "element-not-focused"
                | "composition-unavailable"
                | "ime-mode-unavailable"
                // During a Chromium focus transition UIA can briefly expose
                // the old document/container before the editable child. These
                // states still hide the badge, but need the same bounded retry.
                | "not-text-input"
                | "read-only"
                | "not-editable-document"
        ) && self.retry.take(self.gate.revision())
        {
            self.schedule(TIMER_REFRESH, 250);
        }
    }

    fn trace(&self, sample: &Sample) {
        use std::io::Write;
        if let Some(path) = &self.trace_report {
            // Explicit diagnostic mode only; no key codes, text or titles.
            // Stop growing the file after 256 KiB.
            if std::fs::metadata(path)
                .map(|m| m.len() >= 262144)
                .unwrap_or(false)
            {
                return;
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "ms={} reason={} source={} caret={:?} mode={:?} composition={:?} request_revision={} current_revision={} pending={} remaining={} paused={} same_focus={} visible={} policy={:?} focus={:?} finish_seen={} element={} inactive_seen={} changes={} control_type={:?}",
                    self.now(), sample.reason, sample.source, sample.caret, sample.mode, sample.composition,
                    sample.request.revision, self.gate.revision(), self.notify.pending.load(Ordering::Acquire),
                    self.remaining_ms(), self.gate.paused, Focus::current() == Some(sample.request.focus),
                    unsafe { IsWindowVisible(self.overlay).as_bool() }, self.composition_policy, sample.request.focus,
                    self.finished_focus == Some(sample.request.focus), sample.element, self.inactive_observed, self.last_change_flags, sample.control_type);
            }
        }
    }

    unsafe fn show(&mut self, caret: Rect, mode: InputMode) {
        let native = RECT {
            left: caret.left,
            top: caret.top,
            right: caret.right,
            bottom: caret.bottom,
        };
        let monitor = MonitorFromRect(&native, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            self.hide();
            return;
        }
        if MonitorFromWindow(self.overlay, MONITOR_DEFAULTTONEAREST) != monitor {
            self.hide();
            // GetDpiForWindow reports the caret monitor's DPI after the hidden
            // overlay moves there, including windows spanning multiple monitors.
            let _ = SetWindowPos(
                self.overlay,
                None,
                caret.left,
                caret.top,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
        let xdpi = GetDpiForWindow(self.overlay).max(96);
        let work = Rect {
            left: info.rcWork.left,
            top: info.rcWork.top,
            right: info.rcWork.right,
            bottom: info.rcWork.bottom,
        };
        if caret.left < info.rcMonitor.left
            || caret.left > info.rcMonitor.right
            || caret.top < info.rcMonitor.top
            || caret.bottom > info.rcMonitor.bottom
        {
            self.hide();
            return;
        }
        let badge = place_badge(caret, work, xdpi, mode);
        if self.redraw == Some((badge, mode)) {
            return;
        }
        let label = wide(mode.label());
        let _ = SetWindowTextW(self.overlay, PCWSTR(label.as_ptr()));
        let width = badge.right - badge.left;
        let height = badge.bottom - badge.top;
        let _ = SetWindowPos(
            self.overlay,
            Some(HWND_TOPMOST),
            badge.left,
            badge.top,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, 8, 8);
        if SetWindowRgn(self.overlay, Some(region), true) == 0 {
            let _ = DeleteObject(region.into());
        }
        let _ = InvalidateRect(Some(self.overlay), None, false);
        self.redraw = Some((badge, mode));
    }

    unsafe fn finish_menu(&mut self, command: i32) {
        self.menu_open = false;
        self.focus = Focus::current();
        self.gate.focus_changed();
        match command {
            1 => {
                self.gate.paused = !self.gate.paused;
                self.gate.invalidate();
            }
            2 => {
                let _ = PostMessageW(Some(self.window), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            3..=5 => {
                let now = self.now();
                self.gate.set_delay(
                    match command {
                        3 => 300,
                        4 => 700,
                        _ => 1200,
                    },
                    now,
                );
            }
            6 => {
                self.composition_policy = if self.composition_policy == CompositionPolicy::Strict {
                    CompositionPolicy::IdleFallback
                } else {
                    CompositionPolicy::Strict
                };
                self.gate.invalidate();
            }
            _ => {}
        }
        let _ = PostMessageW(Some(self.window), WM_NULL, WPARAM(0), LPARAM(0));
        self.refresh();
    }
}

/// TrackPopupMenu runs a nested message loop. Keep the RefCell unborrowed while
/// it runs so queued worker results and coalesced input notifications are handled.
unsafe fn dispatch_tray_menu(
    state: &RefCell<State>,
    show: impl FnOnce(HWND, bool, CompositionPolicy) -> i32,
) {
    let (window, paused, policy, restore) = {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        if state.menu_open {
            return;
        }
        let previous = GetForegroundWindow();
        let mut class = [0u16; 64];
        let length = GetClassNameW(previous, &mut class).max(0) as usize;
        let shell = matches!(
            String::from_utf16_lossy(&class[..length]).as_str(),
            "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        );
        let restore = if previous == state.window || shell {
            state
                .last_input
                .filter(|focus| {
                    GetWindowThreadProcessId(observer::hwnd(focus.foreground), None) == focus.thread
                })
                .map(|focus| observer::hwnd(focus.foreground))
        } else {
            Some(previous)
        };
        state.menu_open = true;
        state.hide();
        state.gate.invalidate();
        let _ = KillTimer(Some(state.window), TIMER_WAIT);
        let _ = KillTimer(Some(state.window), TIMER_REFRESH);
        (
            state.window,
            state.gate.paused,
            state.composition_policy,
            restore,
        )
    };
    let command = show(window, paused, policy);
    // Selecting a menu item leaves the hidden controller active. Restore its
    // previous foreground window, but respect a click that dismissed the menu by
    // activating another app. No stale caret position or mode is reused.
    if GetForegroundWindow() == window {
        if let Some(previous) = restore.filter(|window| IsWindow(Some(*window)).as_bool()) {
            let _ = SetForegroundWindow(previous);
        }
    }
    if let Ok(mut state) = state.try_borrow_mut() {
        state.finish_menu(command);
    }
}

unsafe fn show_tray_menu(window: HWND, paused: bool, policy: CompositionPolicy) -> i32 {
    let Ok(menu) = CreatePopupMenu() else {
        return 0;
    };
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        1,
        if paused {
            w!("再開")
        } else {
            w!("一時停止")
        },
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    for (id, label) in [
        (3, w!("待機時間：300ms")),
        (4, w!("待機時間：700ms")),
        (5, w!("待機時間：1200ms")),
    ] {
        let _ = AppendMenuW(menu, MF_STRING, id, label);
    }
    let flags = if policy == CompositionPolicy::Strict {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let _ = AppendMenuW(menu, flags, 6, w!("変換状態を取得できない欄では非表示"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, 2, w!("終了"));
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(window);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        None,
        window,
        None,
    )
    .0;
    let _ = DestroyMenu(menu);
    command
}

unsafe extern "system" fn controller_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *const RefCell<State>;
    if !state.is_null() {
        if message == WM_TRAY && matches!(lparam.0 as u32, WM_RBUTTONUP | WM_LBUTTONUP) {
            dispatch_tray_menu(&*state, |window, paused, policy| {
                show_tray_menu(window, paused, policy)
            });
            return LRESULT(0);
        }
        // Native calls can reenter the window procedure. RefCell prevents aliasing
        // mutable state across nested messages; custom events are always posted.
        if let Ok(mut state) = (*state).try_borrow_mut() {
            if state.taskbar_created != 0 && message == state.taskbar_created {
                let _ = Shell_NotifyIconW(NIM_ADD, &state.tray.data);
                return LRESULT(0);
            }
            match message {
                WM_CHANGED => {
                    state.changed();
                    return LRESULT(0);
                }
                WM_SAMPLE => {
                    state.results();
                    return LRESULT(0);
                }
                WM_TIMER => {
                    let _ = KillTimer(Some(window), wparam.0);
                    if wparam.0 == TIMER_SMOKE {
                        if let Some(path) = &state.smoke_report {
                            let report = format!("window_created=true\ntray_installed=true\nkeyboard_hook=true\nwin_event_hooks={}\nsamples_received={}\n", state.hooks.events.len(), state.samples_received);
                            let _ = std::fs::write(path, report);
                        }
                        let _ = PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0));
                    } else {
                        state.refresh();
                    }
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    state.notify.window.store(0, Ordering::Release);
                    state.requests.take();
                    let _ = DestroyWindow(state.overlay);
                    let _ = DestroyWindow(window);
                    return LRESULT(0);
                }
                _ => {}
            }
        }
    }
    if message == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(window, message, wparam, lparam)
}

unsafe extern "system" fn overlay_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(window, &mut paint);
            let mut bounds = RECT::default();
            let _ = GetClientRect(window, &mut bounds);
            let brush = CreateSolidBrush(COLORREF(0x003c3228));
            FillRect(dc, &bounds, brush);
            let font = CreateFontW(
                -(bounds.bottom * 16 / 26).max(8),
                0,
                0,
                0,
                FW_MEDIUM.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                w!("Yu Gothic UI"),
            );
            let previous = SelectObject(dc, font.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00ffffff));
            let mut label = [0u16; 8];
            let count = GetWindowTextW(window, &mut label);
            DrawTextW(
                dc,
                &mut label[..count.max(0) as usize],
                &mut bounds,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            SelectObject(dc, previous);
            let _ = DeleteObject(font.into());
            let _ = DeleteObject(brush.into());
            let _ = EndPaint(window, &paint);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(window, message, wparam, lparam)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub fn report_error(text: &str) {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("Caret Mode"),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn run() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut arguments: Vec<_> = std::env::args().skip(1).collect();
    let composition_policy = if arguments.iter().any(|arg| arg == "--strict-composition") {
        CompositionPolicy::Strict
    } else {
        CompositionPolicy::IdleFallback
    };
    arguments.retain(|arg| arg != "--strict-composition");
    if arguments.first().is_some_and(|arg| arg == "--diagnose") {
        let report = observer::diagnose()?;
        if let Some(path) = arguments.get(1) {
            std::fs::write(path, report)
                .map_err(|e| windows::core::Error::new(E_FAIL, e.to_string()))?;
        } else {
            let text = wide(&report);
            unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    w!("Caret Mode diagnostics"),
                    MB_OK,
                );
            }
        }
        return Ok(());
    }
    if arguments.first().is_some_and(|arg| arg == "--quit") {
        unsafe {
            if let Ok(window) = FindWindowW(CLASS_NAME, None) {
                let _ = PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        return Ok(());
    }
    let idle_ms = if arguments.first().is_some_and(|arg| arg == "--idle-ms") {
        arguments
            .get(1)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| (100..=5000).contains(value))
            .ok_or_else(|| {
                windows::core::Error::new(E_INVALIDARG, "--idle-ms requires 100..5000")
            })?
    } else {
        700
    };
    let smoke_report = if arguments.first().is_some_and(|arg| arg == "--smoke-test") {
        Some(
            arguments
                .get(1)
                .map(std::path::PathBuf::from)
                .ok_or_else(|| {
                    windows::core::Error::new(E_INVALIDARG, "--smoke-test requires an output file")
                })?,
        )
    } else {
        None
    };
    let trace_report = if arguments.first().is_some_and(|arg| arg == "--trace") {
        Some(
            arguments
                .get(1)
                .map(std::path::PathBuf::from)
                .ok_or_else(|| {
                    windows::core::Error::new(E_INVALIDARG, "--trace requires an output file")
                })?,
        )
    } else {
        None
    };
    if !arguments.is_empty()
        && !matches!(
            arguments[0].as_str(),
            "--idle-ms" | "--smoke-test" | "--trace"
        )
    {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            "Supported options: --idle-ms, --diagnose, --smoke-test, --trace, --strict-composition, --quit",
        ));
    }
    unsafe {
        let singleton = CreateMutexW(None, false, w!("Local\\CaretMode.Singleton.v1"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(singleton);
            return Ok(());
        }
        let result = run_windows(idle_ms, smoke_report, trace_report, composition_policy);
        let _ = CloseHandle(singleton);
        result
    }
}

unsafe fn run_windows(
    idle_ms: u64,
    smoke_report: Option<std::path::PathBuf>,
    trace_report: Option<std::path::PathBuf>,
    composition_policy: CompositionPolicy,
) -> windows::core::Result<()> {
    let instance = GetModuleHandleW(None)?;
    for (name, procedure) in [
        (
            CLASS_NAME,
            Some(controller_proc as unsafe extern "system" fn(_, _, _, _) -> _),
        ),
        (
            OVERLAY_CLASS,
            Some(overlay_proc as unsafe extern "system" fn(_, _, _, _) -> _),
        ),
    ] {
        let class = WNDCLASSW {
            hInstance: instance.into(),
            lpszClassName: name,
            lpfnWndProc: procedure,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: LoadIconW(Some(instance.into()), APP_ICON)?,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_thread());
        }
    }
    let window = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        CLASS_NAME,
        w!("Caret Mode"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance.into()),
        None,
    )?;
    let _window_guard = WindowGuard(window);
    let overlay = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
        OVERLAY_CLASS,
        w!(""),
        WS_POPUP,
        0,
        0,
        26,
        26,
        Some(window),
        None,
        Some(instance.into()),
        None,
    )?;
    let _overlay_guard = WindowGuard(overlay);
    SetLayeredWindowAttributes(overlay, COLORREF(0), 245, LWA_ALPHA)?;
    let notify = Arc::new(Notify::new(window.0 as usize));
    let _notification_guard = NotificationGuard(notify.clone());
    let _ = ACTIVE_NOTIFY.set(notify.clone());
    let hooks = Hooks::install()?;
    let tray = Tray::new(window)?;
    let (requests, samples) = observer::spawn(notify.clone())?;
    let state = Box::new(RefCell::new(State {
        window,
        overlay,
        notify,
        requests: Some(requests),
        samples,
        gate: IdleGate::new(idle_ms),
        start: Instant::now(),
        focus: None,
        busy: false,
        redraw: None,
        tray,
        taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        hooks,
        smoke_report,
        samples_received: 0,
        retry: QueryRetry::default(),
        trace_report,
        composition_policy,
        menu_open: false,
        last_input: None,
        finished_focus: None,
        finished_at_ms: 0,
        finished_element: 0,
        observed_element: 0,
        inactive_observed: false,
        last_change_flags: 0,
    }));
    SetWindowLongPtrW(
        window,
        GWLP_USERDATA,
        (&*state as *const RefCell<State>) as isize,
    );
    let userdata_guard = UserDataGuard(window);
    {
        let mut state = state.borrow_mut();
        state.refresh();
        if state.smoke_report.is_some() {
            state.schedule(TIMER_SMOKE, 1200);
        }
    }
    let mut message = MSG::default();
    loop {
        let received = GetMessageW(&mut message, None, 0, 0).0;
        if received == -1 {
            return Err(windows::core::Error::from_thread());
        }
        if received == 0 {
            break;
        }
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    // Rust drops the hooks and tray after the message loop. The COM worker is
    // signalled via channel closure; it never blocks this thread during shutdown.
    drop(userdata_guard);
    drop(state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    static NATIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    const TIMER_END_TEST_MENU: usize = 901;

    #[test]
    fn embedded_app_icon_loads_at_tray_and_explorer_sizes() {
        unsafe {
            for size in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
                let icon = AppIcon::load(size, size).expect("embedded icon is missing or invalid");
                let mut info = ICONINFO::default();
                GetIconInfo(icon.0, &mut info).unwrap();
                let mut bitmap = BITMAP::default();
                let copied = GetObjectW(
                    info.hbmColor.into(),
                    size_of::<BITMAP>() as i32,
                    Some((&mut bitmap as *mut BITMAP).cast()),
                );
                let _ = DeleteObject(info.hbmColor.into());
                let _ = DeleteObject(info.hbmMask.into());
                assert_ne!(copied, 0);
                assert_eq!((bitmap.bmWidth, bitmap.bmHeight), (size, size));
                assert_eq!(bitmap.bmBitsPixel, 32);
            }
        }
    }

    #[test]
    fn japanese_mode_keys_do_not_erase_idle_evidence_as_character_input() {
        for key in 0xf0..=0xf5 {
            assert_eq!(keyboard_change(key, false, true), CHANGE_CARET);
            assert_eq!(
                keyboard_change(key, false, false),
                CHANGE_EDIT | CHANGE_USER_EDIT
            );
        }
        assert_eq!(keyboard_change(0xc0, true, true), CHANGE_CARET);
        assert_eq!(
            keyboard_change(0xc0, false, true),
            CHANGE_EDIT | CHANGE_USER_EDIT
        );
        assert_eq!(
            keyboard_change(0xc0, true, false),
            CHANGE_EDIT | CHANGE_USER_EDIT
        );
        assert_eq!(
            keyboard_change(0x41, false, true),
            CHANGE_EDIT | CHANGE_USER_EDIT
        );
        assert_eq!(
            keyboard_change(0x0d, false, true),
            CHANGE_EDIT | CHANGE_FINISH
        );
    }

    struct TestForegroundGuard {
        previous: HWND,
        host: HWND,
        controller: HWND,
    }
    impl Drop for TestForegroundGuard {
        fn drop(&mut self) {
            unsafe {
                if [self.host, self.controller].contains(&GetForegroundWindow())
                    && IsWindow(Some(self.previous)).as_bool()
                {
                    let _ = SetForegroundWindow(self.previous);
                }
            }
        }
    }

    unsafe extern "system" fn menu_test_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_TIMER && wparam.0 == TIMER_END_TEST_MENU {
            let _ = KillTimer(Some(window), TIMER_END_TEST_MENU);
            let _ = EndMenu();
            return LRESULT(0);
        }
        controller_proc(window, message, wparam, lparam)
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn tray_menu_drains_in_flight_results_and_recovers_after_settings_and_resume() {
        let _lock = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class_name = w!("CaretModeController.MenuRegression.v1");
            let overlay_name = w!("CaretModeOverlay.MenuRegression.v1");
            for (name, procedure) in [
                (
                    class_name,
                    Some(menu_test_proc as unsafe extern "system" fn(_, _, _, _) -> _),
                ),
                (
                    overlay_name,
                    Some(overlay_proc as unsafe extern "system" fn(_, _, _, _) -> _),
                ),
            ] {
                assert_ne!(
                    RegisterClassW(&WNDCLASSW {
                        hInstance: instance.into(),
                        lpszClassName: name,
                        lpfnWndProc: procedure,
                        ..Default::default()
                    }),
                    0
                );
            }
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name,
                w!("Menu regression"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let window_guard = WindowGuard(window);
            let overlay = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                overlay_name,
                w!(""),
                WS_POPUP,
                0,
                0,
                26,
                26,
                Some(window),
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let overlay_guard = WindowGuard(overlay);
            // Give the fixture its own real edit control. A shell/background
            // window can have no keyboard focus, which used to skip the core
            // assertions depending on what the user last clicked.
            let test_host = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("Caret Mode regression test"),
                WS_OVERLAPPEDWINDOW,
                20,
                20,
                360,
                100,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let test_host_guard = WindowGuard(test_host);
            let test_input = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER,
                8,
                8,
                300,
                26,
                Some(test_host),
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let test_input_guard = WindowGuard(test_input);
            let foreground_guard = TestForegroundGuard {
                previous: GetForegroundWindow(),
                host: test_host,
                controller: window,
            };
            let _ = ShowWindow(test_host, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(test_host);
            windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(test_input)).unwrap();
            assert!(
                Focus::current().is_some(),
                "Test input did not receive desktop focus"
            );
            let notify = Arc::new(Notify::new(window.0 as usize));
            let (requests, request_rx) = mpsc::sync_channel(1);
            let (sample_tx, samples) = mpsc::channel();
            let state = Box::new(RefCell::new(State {
                window,
                overlay,
                notify: notify.clone(),
                requests: Some(requests),
                samples,
                gate: IdleGate::new(700),
                start: Instant::now(),
                focus: None,
                busy: false,
                redraw: None,
                tray: Tray::new(window).unwrap(),
                taskbar_created: 0,
                hooks: Hooks::install().unwrap(),
                smoke_report: None,
                samples_received: 0,
                retry: QueryRetry::default(),
                trace_report: None,
                composition_policy: CompositionPolicy::IdleFallback,
                menu_open: false,
                last_input: None,
                finished_focus: None,
                finished_at_ms: 0,
                finished_element: 0,
                observed_element: 0,
                inactive_observed: false,
                last_change_flags: 0,
            }));
            SetWindowLongPtrW(
                window,
                GWLP_USERDATA,
                (&*state as *const RefCell<State>) as isize,
            );
            let userdata_guard = UserDataGuard(window);
            for command in [3, 4, 5, 1, 1, 0, 6, 6] {
                while request_rx.try_recv().is_ok() {}
                state.borrow_mut().busy = true;
                let revision = state.borrow().gate.revision();
                dispatch_tray_menu(&state, |window, paused, policy| {
                    assert!(state.borrow().menu_open);
                    sample_tx
                        .send(Sample {
                            element: 0,
                            control_type: None,
                            request: Request {
                                focus: Focus {
                                    foreground: 0,
                                    input: 0,
                                    thread: 0,
                                },
                                revision,
                            },
                            caret: None,
                            mode: None,
                            composition: Composition::Unknown,
                            source: "test",
                            reason: "uia-focus-unavailable",
                        })
                        .unwrap();
                    notify.post(WM_SAMPLE);
                    notify.changed(CHANGE_FOCUS | CHANGE_EDIT);
                    if command == 3 {
                        // Exercise the real nested TrackPopupMenu loop once; a
                        // native timer cancels the popup without user input.
                        assert_ne!(SetTimer(Some(window), TIMER_END_TEST_MENU, 80, None), 0);
                        let _ = show_tray_menu(window, paused, policy);
                    } else {
                        let mut message = MSG::default();
                        while PeekMessageW(
                            &mut message,
                            Some(window),
                            WM_CHANGED,
                            WM_SAMPLE,
                            PM_REMOVE,
                        )
                        .as_bool()
                        {
                            DispatchMessageW(&message);
                        }
                    }
                    assert!(
                        !state.borrow().busy,
                        "Worker result was dropped inside menu loop"
                    );
                    assert_eq!(
                        notify.pending.load(Ordering::Acquire),
                        0,
                        "Input notification remained latched"
                    );
                    assert!(
                        request_rx.try_recv().is_err(),
                        "Query started while menu owned focus"
                    );
                    command
                });
                assert!(!state.borrow().menu_open);
                // The user may activate another app while this desktop test is
                // running; the hidden controller must not keep foreground focus.
                assert_ne!(GetForegroundWindow(), window);
                match command {
                    3..=5 => {
                        let expected = match command {
                            3 => 300,
                            4 => 700,
                            _ => 1200,
                        };
                        let state = state.borrow();
                        let remaining = state.gate.remaining_ms(state.now());
                        assert!(remaining > 0 && remaining <= expected);
                        assert!(!state.busy);
                    }
                    1 => {
                        if state.borrow().gate.paused {
                            assert!(!state.borrow().busy);
                            assert!(request_rx.try_recv().is_err());
                        } else if Focus::current().is_some() {
                            assert!(state.borrow().busy, "Resume did not request a fresh sample");
                        }
                    }
                    _ => {}
                }
                if !state.borrow().gate.paused && Focus::current().is_some() {
                    // Complete the mock worker's *new* request, including the
                    // real wait timer after each delay change. A drained old
                    // response is insufficient to bring the badge back.
                    let deadline = Instant::now() + std::time::Duration::from_secs(3);
                    let request = loop {
                        if let Ok(request) = request_rx.try_recv() {
                            break request;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "No query after changing tray settings"
                        );
                        let mut message = MSG::default();
                        while PeekMessageW(
                            &mut message,
                            Some(window),
                            WM_TIMER,
                            WM_TIMER,
                            PM_REMOVE,
                        )
                        .as_bool()
                        {
                            DispatchMessageW(&message);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    };
                    let monitor = MonitorFromWindow(
                        observer::hwnd(request.focus.foreground),
                        MONITOR_DEFAULTTOPRIMARY,
                    );
                    let mut info = MONITORINFO {
                        cbSize: size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    assert!(GetMonitorInfoW(monitor, &mut info).as_bool());
                    sample_tx
                        .send(Sample {
                            element: 1,
                            control_type: None,
                            request,
                            caret: Some(Rect {
                                left: info.rcWork.left + 100,
                                right: info.rcWork.left + 101,
                                top: info.rcWork.top + 100,
                                bottom: info.rcWork.top + 120,
                            }),
                            mode: Some(InputMode::Alphanumeric),
                            composition: Composition::Inactive,
                            source: "test",
                            reason: "ready",
                        })
                        .unwrap();
                    notify.post(WM_SAMPLE);
                    let mut message = MSG::default();
                    while PeekMessageW(&mut message, Some(window), WM_SAMPLE, WM_SAMPLE, PM_REMOVE)
                        .as_bool()
                    {
                        DispatchMessageW(&message);
                    }
                    assert!(!state.borrow().busy);
                    assert!(
                        IsWindowVisible(overlay).as_bool(),
                        "Badge did not return after tray operation"
                    );
                }
            }
            assert!(state.borrow().samples_received >= 8);
            assert!(!state.borrow().gate.paused);
            assert_eq!(
                state.borrow().composition_policy,
                CompositionPolicy::IdleFallback
            );
            // A composition-end event can be missing. The real recovery timer
            // must request another sample while the badge remains hidden.
            if let Some(focus) = Focus::current() {
                notify.changed(CHANGE_EDIT | CHANGE_USER_EDIT);
                state.borrow_mut().changed();
                state.borrow_mut().gate = IdleGate::new(700);
                while request_rx.try_recv().is_ok() {}
                let request = Request {
                    focus,
                    revision: state.borrow().gate.revision(),
                };
                let monitor =
                    MonitorFromWindow(observer::hwnd(focus.foreground), MONITOR_DEFAULTTOPRIMARY);
                let mut info = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                assert!(GetMonitorInfoW(monitor, &mut info).as_bool());
                let caret = Some(Rect {
                    left: info.rcWork.left + 100,
                    right: info.rcWork.left + 101,
                    top: info.rcWork.top + 100,
                    bottom: info.rcWork.top + 120,
                });
                sample_tx
                    .send(Sample {
                        element: 1,
                        control_type: None,
                        request,
                        caret,
                        mode: Some(InputMode::Hiragana),
                        composition: Composition::Active,
                        source: "test",
                        reason: "composing",
                    })
                    .unwrap();
                notify.post(WM_SAMPLE);
                let mut message = MSG::default();
                while PeekMessageW(&mut message, Some(window), WM_SAMPLE, WM_SAMPLE, PM_REMOVE)
                    .as_bool()
                {
                    DispatchMessageW(&message);
                }
                assert!(!IsWindowVisible(overlay).as_bool());
                let deadline = Instant::now() + std::time::Duration::from_secs(2);
                let request = loop {
                    if let Ok(request) = request_rx.try_recv() {
                        break request;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "Composition recovery did not query again"
                    );
                    while PeekMessageW(&mut message, Some(window), WM_TIMER, WM_TIMER, PM_REMOVE)
                        .as_bool()
                    {
                        DispatchMessageW(&message);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                };
                // No CHANGE_EDIT / CompositionFinalized notification is sent.
                sample_tx
                    .send(Sample {
                        element: 1,
                        control_type: None,
                        request,
                        caret,
                        mode: Some(InputMode::Hiragana),
                        composition: Composition::Inactive,
                        source: "test",
                        reason: "ready",
                    })
                    .unwrap();
                notify.post(WM_SAMPLE);
                while PeekMessageW(&mut message, Some(window), WM_SAMPLE, WM_SAMPLE, PM_REMOVE)
                    .as_bool()
                {
                    DispatchMessageW(&message);
                }
                assert!(
                    IsWindowVisible(overlay).as_bool(),
                    "Hiragana badge did not recover after composition"
                );
                let mut label = [0u16; 8];
                let length = GetWindowTextW(overlay, &mut label).max(0) as usize;
                assert_eq!(String::from_utf16_lossy(&label[..length]), "あ");

                // Cached ranges must not suppress a mode-only change, but the
                // evidence must expire on typing and on a different UIA field.
                let deliver_active = |element| {
                    let request = Request {
                        focus: Focus::current().unwrap(),
                        revision: state.borrow().gate.revision(),
                    };
                    sample_tx
                        .send(Sample {
                            request,
                            element,
                            control_type: None,
                            caret,
                            mode: Some(InputMode::Hiragana),
                            composition: Composition::Active,
                            source: "test",
                            reason: "composing",
                        })
                        .unwrap();
                    state.borrow_mut().results();
                };
                deliver_active(1);
                assert!(IsWindowVisible(overlay).as_bool(), "Idle evidence was lost");
                notify.changed(keyboard_change(0xf3, false, true));
                state.borrow_mut().changed();
                deliver_active(1);
                assert!(
                    IsWindowVisible(overlay).as_bool(),
                    "IME mode switch erased idle evidence"
                );
                notify.changed(CHANGE_FOCUS);
                state.borrow_mut().changed();
                deliver_active(1);
                assert!(
                    IsWindowVisible(overlay).as_bool(),
                    "Popup focus erased the input identity"
                );
                state.borrow_mut().composition_policy = CompositionPolicy::Strict;
                deliver_active(1);
                assert!(!IsWindowVisible(overlay).as_bool());
                state.borrow_mut().composition_policy = CompositionPolicy::IdleFallback;

                let now = state.borrow().now();
                state.borrow_mut().gate.set_delay(700, now);
                notify.changed(CHANGE_FOCUS);
                state.borrow_mut().changed();
                assert!(
                    state.borrow().gate.remaining_ms(state.borrow().now()) > 0,
                    "Logical popup focus bypassed the edit's idle delay"
                );

                state.borrow_mut().gate = IdleGate::new(1);
                notify.changed(CHANGE_EDIT | CHANGE_USER_EDIT);
                state.borrow_mut().changed();
                std::thread::sleep(std::time::Duration::from_millis(5));
                deliver_active(1);
                assert!(
                    !IsWindowVisible(overlay).as_bool(),
                    "Typing retained idle evidence"
                );

                notify.changed(CHANGE_EDIT | CHANGE_FINISH);
                state.borrow_mut().changed();
                notify.changed(CHANGE_EDIT | CHANGE_COMPOSITION_START);
                state.borrow_mut().changed();
                assert_eq!(state.borrow().finished_focus, Focus::current());
                assert!(!IsWindowVisible(overlay).as_bool());
                std::thread::sleep(std::time::Duration::from_millis(5));
                deliver_active(1);
                assert!(
                    IsWindowVisible(overlay).as_bool(),
                    "Late update erased the finish"
                );
                deliver_active(2);
                assert!(
                    !IsWindowVisible(overlay).as_bool(),
                    "Evidence leaked to another field"
                );
                // A provider can briefly return a read-only/non-input element
                // and never send another focus event. The retry timer alone
                // must acquire the input and restore the badge.
                for reason in ["read-only", "not-text-input", "not-editable-document"] {
                    let _ = KillTimer(Some(window), TIMER_WAIT);
                    let _ = KillTimer(Some(window), TIMER_REFRESH);
                    let mut queued_timer = MSG::default();
                    while PeekMessageW(
                        &mut queued_timer,
                        Some(window),
                        WM_TIMER,
                        WM_TIMER,
                        PM_REMOVE,
                    )
                    .as_bool()
                    {}
                    while request_rx.try_recv().is_ok() {}
                    let request = Request {
                        focus: Focus::current().unwrap(),
                        revision: state.borrow().gate.revision(),
                    };
                    sample_tx
                        .send(Sample {
                            request,
                            element: 0,
                            control_type: None,
                            caret: None,
                            mode: None,
                            composition: Composition::Unknown,
                            source: "test",
                            reason,
                        })
                        .unwrap();
                    state.borrow_mut().results();
                    assert!(!IsWindowVisible(overlay).as_bool());
                    let deadline = Instant::now() + std::time::Duration::from_secs(2);
                    let request = loop {
                        if let Ok(request) = request_rx.try_recv() {
                            break request;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "Focus-transition retry did not run: {reason}"
                        );
                        let mut message = MSG::default();
                        while PeekMessageW(
                            &mut message,
                            Some(window),
                            WM_TIMER,
                            WM_TIMER,
                            PM_REMOVE,
                        )
                        .as_bool()
                        {
                            DispatchMessageW(&message);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    };
                    sample_tx
                        .send(Sample {
                            request,
                            element: 2,
                            control_type: None,
                            caret,
                            mode: Some(InputMode::Hiragana),
                            composition: Composition::Inactive,
                            source: "test",
                            reason: "ready",
                        })
                        .unwrap();
                    state.borrow_mut().results();
                    assert!(
                        IsWindowVisible(overlay).as_bool(),
                        "No recovery from {reason}"
                    );
                }
                notify.changed(CHANGE_EDIT | CHANGE_FINISH);
                notify.changed(CHANGE_EDIT | CHANGE_USER_EDIT);
                state.borrow_mut().changed();
                assert!(
                    state.borrow().finished_focus.is_none(),
                    "Coalescing lost typing after completion"
                );
            }
            assert_eq!(
                state.borrow().observed_element,
                2,
                "Composition regression requires a focused desktop window"
            );
            drop(userdata_guard);
            drop(state);
            drop(overlay_guard);
            drop(foreground_guard);
            drop(test_input_guard);
            drop(test_host_guard);
            drop(window_guard);
            UnregisterClassW(overlay_name, Some(instance.into())).unwrap();
            UnregisterClassW(class_name, Some(instance.into())).unwrap();
        }
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn overlay_renders_without_activating_and_passes_mouse_input() {
        let _lock = NATIVE_TEST_LOCK.lock().unwrap();
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class_name = w!("CaretModeOverlay.NativeTest.v1");
            let class = WNDCLASSW {
                hInstance: instance.into(),
                lpszClassName: class_name,
                lpfnWndProc: Some(overlay_proc),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let window = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT,
                class_name,
                w!("あ"),
                WS_POPUP,
                20,
                20,
                52,
                52,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let guard = WindowGuard(window);
            SetLayeredWindowAttributes(window, COLORREF(0), 245, LWA_ALPHA).unwrap();
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
            assert!(UpdateWindow(window).as_bool());
            assert_ne!(GetForegroundWindow(), window);
            assert_eq!(
                SendMessageW(window, WM_MOUSEACTIVATE, None, None).0,
                MA_NOACTIVATE as isize
            );
            assert_eq!(
                SendMessageW(window, WM_NCHITTEST, None, None).0,
                HTTRANSPARENT as isize
            );
            let styles = GetWindowLongW(window, GWL_EXSTYLE) as u32;
            assert_eq!(styles & WS_EX_TRANSPARENT.0, WS_EX_TRANSPARENT.0);
            assert_eq!(styles & WS_EX_NOACTIVATE.0, WS_EX_NOACTIVATE.0);
            for (label, width) in [(w!("あ"), 52), (w!("全A"), 72)] {
                SetWindowTextW(window, label).unwrap();
                SetWindowPos(
                    window,
                    None,
                    0,
                    0,
                    width,
                    52,
                    SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
                )
                .unwrap();
                assert!(InvalidateRect(Some(window), None, false).as_bool());
                assert!(UpdateWindow(window).as_bool());
                assert_ne!(GetForegroundWindow(), window);
                let dc = GetDC(Some(window));
                let mut text_pixels = 0;
                let (mut left, mut right) = (width, 0);
                for y in 0..52 {
                    for x in 0..width {
                        let pixel = GetPixel(dc, x, y).0;
                        if pixel != u32::MAX
                            && pixel & 255 > 160
                            && (pixel >> 8) & 255 > 160
                            && (pixel >> 16) & 255 > 160
                        {
                            text_pixels += 1;
                            left = left.min(x);
                            right = right.max(x);
                        }
                    }
                }
                ReleaseDC(Some(window), dc);
                assert!(text_pixels > 10, "Label was not rendered");
                assert!(
                    left > 2 && right < width - 3,
                    "Label reaches the window edge"
                );
            }
            drop(guard);
            assert!(!IsWindow(Some(window)).as_bool());
            UnregisterClassW(class_name, Some(instance.into())).unwrap();
        }
    }
}
