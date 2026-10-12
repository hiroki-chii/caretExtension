//! All COM interfaces stay on the owning MTA thread. Results contain only IDs,
//! rectangles and mode flags; no input text is read or sent to the UI thread.

use super::app::{
    Notify, CHANGE_CARET, CHANGE_COMPOSITION_START, CHANGE_EDIT, CHANGE_FINISH, CHANGE_FOCUS,
    WM_SAMPLE,
};
use crate::model::{composition_from_endpoints, japanese_mode, Composition, InputMode, Rect};
use std::{
    ffi::c_void,
    sync::{mpsc, Arc},
    thread,
};
use windows::{
    core::{implement, Interface, Ref, BOOL},
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        Graphics::Gdi::MapWindowPoints,
        System::{
            Com::*,
            Ole::*,
            Variant::{VariantToBoolean, VariantToInt32, VARIANT},
        },
        UI::{
            Accessibility::*,
            Input::{Ime::*, KeyboardAndMouse::GetKeyboardLayout},
            WindowsAndMessaging::*,
        },
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Focus {
    pub foreground: usize,
    pub input: usize,
    pub thread: u32,
}

impl Focus {
    pub fn current() -> Option<Self> {
        // SAFETY: GUITHREADINFO is initialized with the required size. HWNDs are
        // borrowed OS identifiers, used only in checked API calls.
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.is_invalid() {
                return None;
            }
            let thread = GetWindowThreadProcessId(foreground, None);
            let mut gui = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            GetGUIThreadInfo(thread, &mut gui).ok()?;
            if gui.hwndFocus.is_invalid() {
                return None;
            }
            Some(Self {
                foreground: foreground.0 as usize,
                input: gui.hwndFocus.0 as usize,
                thread,
            })
        }
    }
}

pub fn hwnd(value: usize) -> HWND {
    HWND(value as *mut c_void)
}

#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub focus: Focus,
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub request: Request,
    /// Worker-local identity of the currently bound editable element.
    pub element: u64,
    pub control_type: Option<i32>,
    pub caret: Option<Rect>,
    pub mode: Option<InputMode>,
    pub composition: Composition,
    pub source: &'static str,
    pub reason: &'static str,
}

impl Sample {
    fn unavailable(request: Request, reason: &'static str) -> Self {
        Self {
            request,
            element: 0,
            control_type: None,
            caret: None,
            mode: None,
            composition: Composition::Unknown,
            source: "none",
            reason,
        }
    }

    fn with_control_type(mut self, control: Option<UIA_CONTROLTYPE_ID>) -> Self {
        self.control_type = control.map(|id| id.0);
        self
    }
}

#[implement(IUIAutomationFocusChangedEventHandler)]
struct FocusHandler {
    notify: Arc<Notify>,
}
impl IUIAutomationFocusChangedEventHandler_Impl for FocusHandler_Impl {
    fn HandleFocusChangedEvent(
        &self,
        _sender: Ref<IUIAutomationElement>,
    ) -> windows::core::Result<()> {
        self.notify.changed(CHANGE_FOCUS);
        Ok(())
    }
}

#[implement(IUIAutomationEventHandler)]
struct TextHandler {
    notify: Arc<Notify>,
}
impl IUIAutomationEventHandler_Impl for TextHandler_Impl {
    fn HandleAutomationEvent(
        &self,
        _sender: Ref<IUIAutomationElement>,
        event: UIA_EVENT_ID,
    ) -> windows::core::Result<()> {
        self.notify
            .changed(if event == UIA_Text_TextChangedEventId {
                CHANGE_EDIT
            } else {
                CHANGE_CARET
            });
        Ok(())
    }
}

#[implement(IUIAutomationTextEditTextChangedEventHandler)]
struct CompositionHandler {
    notify: Arc<Notify>,
}
impl IUIAutomationTextEditTextChangedEventHandler_Impl for CompositionHandler_Impl {
    fn HandleTextEditTextChangedEvent(
        &self,
        _sender: Ref<IUIAutomationElement>,
        kind: TextEditChangeType,
        _strings: *const SAFEARRAY,
    ) -> windows::core::Result<()> {
        // The provider supplies a payload, but we deliberately never dereference it.
        self.notify.changed(
            CHANGE_EDIT
                | if kind == TextEditChangeType_CompositionFinalized {
                    CHANGE_FINISH
                } else {
                    CHANGE_COMPOSITION_START
                },
        );
        Ok(())
    }
}

pub struct Observer {
    automation: IUIAutomation,
    automation3: Option<IUIAutomation3>,
    focus_handler: IUIAutomationFocusChangedEventHandler,
    text_handler: IUIAutomationEventHandler,
    composition_handler: IUIAutomationTextEditTextChangedEventHandler,
    bound: Option<IUIAutomationElement>,
    bound_token: u64,
    bound_focus: Option<Focus>,
}

impl Observer {
    pub fn new(notify: Arc<Notify>) -> windows::core::Result<Self> {
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            if let Ok(a2) = automation.cast::<IUIAutomation2>() {
                let _ = a2.SetConnectionTimeout(200);
                let _ = a2.SetTransactionTimeout(200);
            }
            let focus_handler = FocusHandler {
                notify: notify.clone(),
            }
            .into();
            let text_handler = TextHandler {
                notify: notify.clone(),
            }
            .into();
            let composition_handler = CompositionHandler { notify }.into();
            automation.AddFocusChangedEventHandler(None, &focus_handler)?;
            Ok(Self {
                automation3: automation.cast().ok(),
                automation,
                focus_handler,
                text_handler,
                composition_handler,
                bound: None,
                bound_token: 0,
                bound_focus: None,
            })
        }
    }

    unsafe fn unbind(&mut self) {
        self.bound_focus = None;
        if let Some(element) = self.bound.take() {
            for event in [
                UIA_Text_TextChangedEventId,
                UIA_Text_TextSelectionChangedEventId,
            ] {
                let _ = self.automation.RemoveAutomationEventHandler(
                    event,
                    &element,
                    &self.text_handler,
                );
            }
            if let Some(a3) = &self.automation3 {
                let _ =
                    a3.RemoveTextEditTextChangedEventHandler(&element, &self.composition_handler);
            }
        }
    }

    unsafe fn bind(&mut self, element: &IUIAutomationElement, focus: Focus) {
        if self.bound.as_ref().is_some_and(|previous| {
            self.automation
                .CompareElements(previous, element)
                .map(|same| same.as_bool())
                .unwrap_or(false)
        }) {
            self.bound_focus = Some(focus);
            return;
        }
        self.unbind();
        self.bound_token = self.bound_token.wrapping_add(1).max(1);
        for event in [
            UIA_Text_TextChangedEventId,
            UIA_Text_TextSelectionChangedEventId,
        ] {
            let _ = self.automation.AddAutomationEventHandler(
                event,
                element,
                TreeScope_Element,
                None,
                &self.text_handler,
            );
        }
        if let Some(a3) = &self.automation3 {
            for kind in [
                TextEditChangeType_Composition,
                TextEditChangeType_CompositionFinalized,
            ] {
                let _ = a3.AddTextEditTextChangedEventHandler(
                    element,
                    TreeScope_Element,
                    kind,
                    None,
                    &self.composition_handler,
                );
            }
        }
        self.bound = Some(element.clone());
        self.bound_focus = Some(focus);
    }

    unsafe fn autocomplete_owner(
        &self,
        candidate: &IUIAutomationElement,
        focus: Focus,
    ) -> Option<IUIAutomationElement> {
        // aria-activedescendant moves accessibility focus to a suggestion while
        // the editable combo box can still own the keyboard caret. Resolve only
        // a related popup of our currently bound combo, never an arbitrary list.
        if self.bound_focus != Some(focus)
            || ![UIA_ListItemControlTypeId, UIA_ListControlTypeId]
                .contains(&candidate.CurrentControlType().ok()?)
        {
            return None;
        }
        let owner = self.bound.as_ref()?;
        if owner.CurrentControlType().ok()? != UIA_ComboBoxControlTypeId
            || !owner.CurrentIsEnabled().ok()?.as_bool()
            || owner.CurrentIsOffscreen().ok()?.as_bool()
        {
            return None;
        }
        let controllers = owner.CurrentControllerFor().ok();
        let length = controllers
            .as_ref()
            .and_then(|items| items.Length().ok())
            .unwrap_or(0)
            .clamp(0, 4);
        let walker = self.automation.RawViewWalker().ok()?;
        let mut node = candidate.clone();
        let mut related = false;
        for _ in 0..8 {
            if self
                .automation
                .CompareElements(owner, &node)
                .ok()?
                .as_bool()
            {
                related = true;
                break;
            }
            if let Some(controllers) = &controllers {
                for index in 0..length {
                    if let Ok(target) = controllers.GetElement(index) {
                        if self
                            .automation
                            .CompareElements(&target, &node)
                            .map(|same| same.as_bool())
                            .unwrap_or(false)
                        {
                            related = true;
                            break;
                        }
                    }
                }
            }
            if related {
                break;
            }
            node = walker.GetParentElement(&node).ok()?;
        }
        if !related {
            return None;
        }
        let bounds = owner.CurrentBoundingRectangle().ok()?;
        let caret_inside = |caret: Rect| {
            caret.left >= bounds.left - 2
                && caret.right <= bounds.right + 2
                && caret.top >= bounds.top - 2
                && caret.bottom <= bounds.bottom + 2
        };
        // A cached rectangle alone is insufficient: requery the native/MSAA
        // caret and require it to remain inside this related editing control.
        let caret_focused = owner
            .CurrentHasKeyboardFocus()
            .map(|value| value.as_bool())
            .unwrap_or(false)
            || native_caret(focus)
                .filter(|caret| caret_inside(*caret))
                .is_some()
            || accessible_caret(focus)
                .filter(|caret| caret_inside(*caret))
                .is_some();
        caret_focused.then(|| owner.clone())
    }

    pub fn sample(&mut self, request: Request) -> Sample {
        // SAFETY: this method is only invoked on the initialized, dedicated MTA.
        // Every external window or element may disappear; failures hide the badge.
        unsafe {
            if Focus::current() != Some(request.focus) {
                return Sample::unavailable(request, "focus-changed");
            }
            let Ok(element) = self.automation.GetFocusedElement() else {
                self.unbind();
                return Sample::unavailable(request, "uia-focus-unavailable");
            };
            let popup_owner = self.autocomplete_owner(&element, request.focus);
            let via_popup = popup_owner.is_some();
            let element = popup_owner.unwrap_or(element);
            if element
                .CurrentIsPassword()
                .map(|v| v.as_bool())
                .unwrap_or(true)
            {
                self.unbind();
                return Sample::unavailable(request, "password-or-protected");
            }
            let element_has_focus = element
                .CurrentHasKeyboardFocus()
                .map(|value| value.as_bool())
                .unwrap_or(false);
            if !via_popup && !element_has_focus {
                return Sample::unavailable(request, "element-not-focused");
            }
            let control = element.CurrentControlType().ok();
            if !control.is_some_and(|control| {
                [
                    UIA_EditControlTypeId,
                    UIA_DocumentControlTypeId,
                    UIA_CustomControlTypeId,
                    UIA_ComboBoxControlTypeId,
                ]
                .contains(&control)
            }) {
                self.unbind();
                return Sample::unavailable(request, "not-text-input").with_control_type(control);
            }
            let mut writable_value = false;
            if let Ok(value) =
                element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            {
                if value
                    .CurrentIsReadOnly()
                    .map(|v| v.as_bool())
                    .unwrap_or(true)
                {
                    self.unbind();
                    return Sample::unavailable(request, "read-only").with_control_type(control);
                }
                writable_value = true;
            }
            if control != Some(UIA_EditControlTypeId) && !writable_value {
                // A focused document/container is not sufficient evidence of an
                // editable input. Its cached MSAA caret may belong to a field
                // that has already lost focus.
                let editable = element
                    .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                    .and_then(|pattern| pattern.DocumentRange())
                    .and_then(|range| range.GetAttributeValue(UIA_IsReadOnlyAttributeId))
                    .and_then(|value| VariantToBoolean(&value))
                    .map(|read_only| !read_only.as_bool())
                    .unwrap_or(false);
                if !editable {
                    self.unbind();
                    return Sample::unavailable(request, "not-editable-document")
                        .with_control_type(control);
                }
            }
            self.bind(&element, request.focus);
            let bounds = element.CurrentBoundingRectangle().ok();
            let inside = |rect: Rect| {
                bounds.is_some_and(|bounds| {
                    rect.left >= bounds.left - 2
                        && rect.right <= bounds.right + 2
                        && rect.top >= bounds.top - 2
                        && rect.bottom <= bounds.bottom + 2
                })
            };
            let (caret, source) = if let Some(rect) =
                native_caret(request.focus).filter(|rect| inside(*rect))
            {
                (Some(rect), "win32")
            } else if let Some(rect) = automation_caret(&element) {
                (Some(rect), "uia")
            } else if let Some(rect) = accessible_caret(request.focus).filter(|rect| inside(*rect))
            {
                (Some(rect), "msaa")
            } else if let Some((rect, source)) = automation_caret_fallback(&element, request.focus)
            {
                (Some(rect), source)
            } else {
                (None, "none")
            };
            let (mode, ime_open) = input_mode(request.focus);
            let source = if via_popup {
                match source {
                    "win32" => "win32-popup",
                    "uia" => "uia-popup",
                    "msaa" => "msaa-popup",
                    "uia-character-edge" => "uia-character-edge-popup",
                    "uia-input-edge" => "uia-input-edge-popup",
                    _ => source,
                }
            } else {
                source
            };
            let composition = if ime_open == Some(false) {
                Composition::Inactive
            } else if via_popup && !element_has_focus {
                // A provider may return no active range solely because logical
                // focus is on its option. That is not proof of IME completion.
                Composition::Unknown
            } else if let Ok(pattern) =
                element.GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId)
            {
                active_composition(&pattern)
            } else {
                Composition::Unknown
            };
            if Focus::current() != Some(request.focus) {
                return Sample::unavailable(request, "focus-changed-during-query");
            }
            let reason = if caret.is_none() {
                "caret-unavailable"
            } else if mode.is_none() {
                "ime-mode-unavailable"
            } else if composition == Composition::Unknown {
                "composition-unavailable"
            } else if composition == Composition::Active {
                "composing"
            } else {
                "ready"
            };
            Sample {
                request,
                element: self.bound_token,
                control_type: control.map(|id| id.0),
                caret,
                mode,
                composition,
                source,
                reason,
            }
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        unsafe {
            self.unbind();
            let _ = self
                .automation
                .RemoveFocusChangedEventHandler(&self.focus_handler);
        }
    }
}

unsafe fn active_composition(pattern: &IUIAutomationTextEditPattern) -> Composition {
    // Call the ABI directly: S_OK + null means "not composing", whereas the
    // generated non-optional return wrapper turns a null interface into an error.
    let mut range = std::ptr::null_mut();
    let status = (pattern.vtable().GetActiveComposition)(pattern.as_raw(), &mut range);
    let range = (!range.is_null()).then(|| IUIAutomationTextRange::from_raw(range));
    if status.is_err() {
        Composition::Unknown
    } else if let Some(range) = range {
        composition_from_endpoints(
            range
                .CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    &range,
                    TextPatternRangeEndpoint_End,
                )
                .ok(),
        )
    } else {
        Composition::Inactive
    }
}

unsafe fn native_caret(focus: Focus) -> Option<Rect> {
    let mut gui = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let thread = GetWindowThreadProcessId(hwnd(focus.input), None);
    GetGUIThreadInfo(thread, &mut gui).ok()?;
    if gui.hwndCaret.is_invalid() || gui.hwndFocus != hwnd(focus.input) {
        return None;
    }
    let rect = gui.rcCaret;
    let mut points = [
        POINT {
            x: rect.left,
            y: rect.top,
        },
        POINT {
            x: rect.right,
            y: rect.bottom,
        },
    ];
    // A zero return also means "no coordinate displacement"; do not treat it as failure.
    MapWindowPoints(Some(gui.hwndCaret), None, &mut points);
    let caret = Rect {
        left: points[0].x,
        top: points[0].y,
        right: points[1].x,
        bottom: points[1].y,
    };
    caret.valid_caret().then_some(caret)
}

unsafe fn automation_caret(element: &IUIAutomationElement) -> Option<Rect> {
    // A provider can advertise TextPattern2 before GetCaretRange works. Fall
    // back to the collapsed selection rather than returning immediately.
    let caret_range = element
        .GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id)
        .ok()
        .and_then(|pattern| {
            let mut active = BOOL(0);
            let range = pattern.GetCaretRange(&mut active).ok()?;
            active.as_bool().then_some(range)
        });
    let range = if let Some(range) = caret_range {
        range
    } else {
        let pattern: IUIAutomationTextPattern =
            element.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
        let ranges = pattern.GetSelection().ok()?;
        if ranges.Length().ok()? != 1 {
            return None;
        }
        let range = ranges.GetElement(0).ok()?;
        if range
            .CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &range,
                TextPatternRangeEndpoint_End,
            )
            .ok()?
            != 0
        {
            let attribute = range
                .GetAttributeValue(UIA_SelectionActiveEndAttributeId)
                .ok()?;
            let active_end = VariantToInt32(&attribute).ok()?;
            match active_end {
                1 => {
                    range
                        .MoveEndpointByRange(
                            TextPatternRangeEndpoint_End,
                            &range,
                            TextPatternRangeEndpoint_Start,
                        )
                        .ok()?;
                }
                2 => {
                    range
                        .MoveEndpointByRange(
                            TextPatternRangeEndpoint_Start,
                            &range,
                            TextPatternRangeEndpoint_End,
                        )
                        .ok()?;
                }
                _ => return None,
            }
        }
        range
    };
    range_bounds(&range, 4.0)
}

unsafe fn range_bounds(range: &IUIAutomationTextRange, max_width: f64) -> Option<Rect> {
    let array = range.GetBoundingRectangles().ok()?;
    if array.is_null() {
        return None;
    }
    let result = (|| {
        if SafeArrayGetDim(array) != 1 || SafeArrayGetElemsize(array) != 8 {
            return None;
        }
        let lower = SafeArrayGetLBound(array, 1).ok()?;
        let upper = SafeArrayGetUBound(array, 1).ok()?;
        // Only one narrow caret rectangle qualifies; ordinary text bounds do not.
        if upper - lower + 1 != 4 {
            return None;
        }
        let mut values = [0.0f64; 4];
        for (offset, value) in values.iter_mut().enumerate() {
            SafeArrayGetElement(array, &(lower + offset as i32), (value as *mut f64).cast())
                .ok()?;
        }
        if !values.iter().all(|value| value.is_finite())
            || !(0.0..=max_width).contains(&values[2])
            || values[3] <= 0.0
        {
            return None;
        }
        Some(Rect {
            left: values[0].round() as i32,
            top: values[1].round() as i32,
            right: (values[0] + values[2]).round() as i32,
            bottom: (values[1] + values[3]).round() as i32,
        })
    })();
    let _ = SafeArrayDestroy(array);
    result.filter(|rect| rect.valid_caret())
}

unsafe fn automation_caret_fallback(
    element: &IUIAutomationElement,
    focus: Focus,
) -> Option<(Rect, &'static str)> {
    // Empty/degenerate UIA ranges often have no bounding rectangle in WinUI.
    // Neighbouring glyphs can supply a boundary without reading any text.
    if let Ok(pattern) =
        element.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id)
    {
        let mut active = BOOL(0);
        if let Ok(caret) = pattern.GetCaretRange(&mut active) {
            if active.as_bool() {
                let expanded = caret.Clone().ok()?;
                expanded.ExpandToEnclosingUnit(TextUnit_Character).ok()?;
                if let Some(bounds) = range_bounds(&expanded, 4096.0) {
                    let at_start = expanded
                        .CompareEndpoints(
                            TextPatternRangeEndpoint_Start,
                            &caret,
                            TextPatternRangeEndpoint_Start,
                        )
                        .ok()?
                        == 0;
                    let at_end = expanded
                        .CompareEndpoints(
                            TextPatternRangeEndpoint_End,
                            &caret,
                            TextPatternRangeEndpoint_Start,
                        )
                        .ok()?
                        == 0;
                    if at_start || at_end {
                        let rtl = expanded
                            .GetAttributeValue(UIA_TextFlowDirectionsAttributeId)
                            .and_then(|value| VariantToInt32(&value))
                            .ok()
                            .is_some_and(|flags| flags & 1 != 0);
                        let x = if at_start != rtl {
                            bounds.left
                        } else {
                            bounds.right
                        };
                        return Some((
                            Rect {
                                left: x,
                                right: x,
                                ..bounds
                            },
                            "uia-character-edge",
                        ));
                    }
                }
            }
        }
    }
    // Value-only input controls (including Explorer's WinUI fields) can expose
    // neither a system caret nor a TextPattern. Use the field's leading edge as
    // an explicitly approximate anchor; never reuse another field's rectangle.
    if ![UIA_EditControlTypeId, UIA_ComboBoxControlTypeId]
        .contains(&element.CurrentControlType().ok()?)
    {
        return None;
    }
    let bounds = element.CurrentBoundingRectangle().ok()?;
    if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
        return None;
    }
    let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd(focus.input)).max(96);
    let padding = (6 * dpi / 96) as i32;
    let x = (bounds.left + padding).min(bounds.right);
    let y = (bounds.top + padding).min(bounds.bottom - 1);
    let rect = Rect {
        left: x,
        right: x,
        top: y,
        bottom: (y + (24 * dpi / 96) as i32).min(bounds.bottom),
    };
    rect.valid_caret().then_some((rect, "uia-input-edge"))
}

unsafe fn accessible_caret(focus: Focus) -> Option<Rect> {
    // Query only the focused window's caret object. No client-tree traversal,
    // text value, name, or global object from a different window is used.
    let mut raw = std::ptr::null_mut();
    AccessibleObjectFromWindow(
        hwnd(focus.input),
        OBJID_CARET.0 as u32,
        &IAccessible::IID,
        &mut raw,
    )
    .ok()?;
    if raw.is_null() {
        return None;
    }
    let object = IAccessible::from_raw(raw);
    let (mut left, mut top, mut width, mut height) = (0, 0, 0, 0);
    object
        .accLocation(
            &mut left,
            &mut top,
            &mut width,
            &mut height,
            &VARIANT::from(0i32),
        )
        .ok()?;
    if !(0..=4).contains(&width) || height <= 0 {
        return None;
    }
    let rect = Rect {
        left,
        top,
        right: left.checked_add(width)?,
        bottom: top.checked_add(height)?,
    };
    rect.valid_caret().then_some(rect)
}

unsafe fn ime_query(window: HWND, command: usize) -> Option<usize> {
    let mut result = 0;
    let sent = SendMessageTimeoutW(
        window,
        WM_IME_CONTROL,
        WPARAM(command),
        LPARAM(0),
        SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
        40,
        Some(&mut result),
    );
    (sent.0 != 0).then_some(result)
}

unsafe fn input_mode(focus: Focus) -> (Option<InputMode>, Option<bool>) {
    let thread = GetWindowThreadProcessId(hwnd(focus.input), None);
    let layout = GetKeyboardLayout(thread).0 as usize;
    let language = layout & 0x3ff;
    if language == 9 {
        return (Some(InputMode::Alphanumeric), Some(false));
    }
    if language != 0x11 {
        return (None, None);
    }
    let window = ImmGetDefaultIMEWnd(hwnd(focus.input));
    if window.is_invalid() {
        return (None, None);
    }
    let Some(open) = ime_query(window, 0x0005) else {
        return (None, None);
    };
    if open > 1 {
        return (None, None);
    }
    let conversion = if open != 0 {
        ime_query(window, 0x0001).map(|v| v as u32)
    } else {
        None
    };
    (japanese_mode(open != 0, conversion), Some(open != 0))
}

pub fn spawn(
    notify: Arc<Notify>,
) -> windows::core::Result<(mpsc::SyncSender<Request>, mpsc::Receiver<Sample>)> {
    let (requests, incoming) = mpsc::sync_channel::<Request>(1);
    let (outgoing, samples) = mpsc::channel();
    let (ready, initialized) = mpsc::sync_channel(1);
    thread::spawn(move || unsafe {
        let com = CoInitializeEx(None, COINIT_MULTITHREADED);
        if com.is_err() {
            let _ = ready.send(Err(format!("COM initialization: {com:?}")));
            return;
        }
        match Observer::new(notify.clone()) {
            Ok(mut observer) => {
                if ready.send(Ok(())).is_ok() {
                    while let Ok(request) = incoming.recv() {
                        let sample = observer.sample(request);
                        if outgoing.send(sample).is_err() {
                            break;
                        }
                        notify.post(WM_SAMPLE);
                    }
                }
            }
            Err(error) => {
                let _ = ready.send(Err(format!("UI Automation initialization: {error}")));
            }
        }
        CoUninitialize();
    });
    match initialized.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(())) => Ok((requests, samples)),
        Ok(Err(message)) => Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            message,
        )),
        Err(error) => Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("UI Automation startup: {error}"),
        )),
    }
}

pub fn diagnose() -> windows::core::Result<String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let result = (|| {
            let Some(focus) = Focus::current() else {
                return Ok("No focused input window.".to_owned());
            };
            let mut observer = Observer::new(Arc::new(Notify::new(0)))?;
            let sample = observer.sample(Request { focus, revision: 0 });
            let mut process = 0;
            GetWindowThreadProcessId(hwnd(focus.foreground), Some(&mut process));
            let element = observer.automation.GetFocusedElement().ok();
            let patterns = element.map(|element| {
                (
                    element.CurrentControlType().ok(),
                    element
                        .GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id)
                        .is_ok(),
                    element
                        .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                        .is_ok(),
                    element
                        .GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId)
                        .is_ok(),
                )
            });
            Ok(format!(
                "{sample:#?}\nsource={}\nreason={}\nfocused_process={}\npatterns=(control_type,text2,text,text_edit) {patterns:?}",
                sample.source,
                sample.reason,
                process
            ))
        })();
        CoUninitialize();
        result
    }
}
