//! Pure state and placement logic. No text or keystroke history is retained.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Hiragana,
    Alphanumeric,
    FullAlphanumeric,
    Katakana,
    HalfKatakana,
}

impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hiragana => "あ",
            Self::Alphanumeric => "A",
            Self::FullAlphanumeric => "全A",
            Self::Katakana => "カ",
            Self::HalfKatakana => "ｶ",
        }
    }
}

/// Conversion flags for Japanese IMM-compatible input methods.
/// None means unknown; an unsuccessful read must never become "A".
pub fn japanese_mode(open: bool, conversion: Option<u32>) -> Option<InputMode> {
    if !open {
        return Some(InputMode::Alphanumeric);
    }
    let flags = conversion?;
    // Character-code, EUDC, no-conversion and symbol modes are not ordinary input.
    if flags & (0x20 | 0x40 | 0x100 | 0x400) != 0 {
        return None;
    }
    let native = flags & 1 != 0;
    let katakana = flags & 2 != 0;
    let full = flags & 8 != 0;
    match (native, katakana, full) {
        (false, _, false) => Some(InputMode::Alphanumeric),
        (false, _, true) => Some(InputMode::FullAlphanumeric),
        (true, true, true) => Some(InputMode::Katakana),
        (true, true, false) => Some(InputMode::HalfKatakana),
        (true, false, true) => Some(InputMode::Hiragana),
        (true, false, false) => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Composition {
    Inactive,
    Active,
    Unknown,
}

/// A non-null UIA range can be collapsed after composition has ended.
/// Inspect endpoint ordering only; no input text needs to be retrieved.
pub fn composition_from_endpoints(endpoints: Option<i32>) -> Composition {
    match endpoints {
        Some(0) => Composition::Inactive,
        Some(_) => Composition::Active,
        None => Composition::Unknown,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositionPolicy {
    Strict,
    IdleFallback,
}

/// Some providers retain an active range after completion or an IME-off sample.
/// Evidence is scoped to one input field and expires on a new edit/composition.
/// Compatibility mode treats the conflicting range as unknown, not inactive.
pub fn composition_with_idle_evidence(
    composition: Composition,
    idle_evidence: bool,
    policy: CompositionPolicy,
) -> Composition {
    if idle_evidence
        && composition == Composition::Active
        && policy == CompositionPolicy::IdleFallback
    {
        Composition::Unknown
    } else {
        composition
    }
}

/// Some accessibility providers start returning caret bounds only after their
/// first query. Retry a short burst, without polling unsupported controls forever.
#[derive(Default)]
pub struct QueryRetry {
    revision: Option<u64>,
    attempts: u8,
}

impl QueryRetry {
    pub fn take(&mut self, revision: u64) -> bool {
        if self.revision != Some(revision) {
            self.revision = Some(revision);
            self.attempts = 0;
        }
        if self.attempts >= 6 {
            return false;
        }
        self.attempts += 1;
        true
    }

    pub fn recovered(&mut self) {
        self.attempts = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn valid_caret(self) -> bool {
        self.right >= self.left && self.bottom > self.top
    }
}

/// Physical coordinates in, physical coordinates out. DPI only scales the badge.
pub fn place_badge(caret: Rect, work: Rect, dpi: u32, mode: InputMode) -> Rect {
    let scale = |dip: i32| ((dip as i64 * dpi.max(96) as i64 + 48) / 96) as i32;
    let width = scale(if mode == InputMode::FullAlphanumeric {
        36
    } else {
        26
    })
    .min((work.right - work.left).max(1));
    let height = scale(26).min((work.bottom - work.top).max(1));
    let gap = scale(6);
    let x = (caret.left - width / 2).clamp(work.left, work.right - width);
    let above = caret.top.saturating_sub(gap).saturating_sub(height);
    let y = if above >= work.top {
        above
    } else {
        caret.bottom.saturating_add(gap)
    }
    .clamp(work.top, work.bottom - height);
    Rect {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    }
}

#[derive(Debug)]
pub struct IdleGate {
    delay_ms: u64,
    last_edit_ms: Option<u64>,
    pub paused: bool,
    revision: u64,
}

impl IdleGate {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            last_edit_ms: None,
            paused: false,
            revision: 0,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn delay_ms(&self) -> u64 {
        self.delay_ms
    }

    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn focus_changed(&mut self) {
        self.last_edit_ms = None;
        self.invalidate();
    }

    pub fn set_delay(&mut self, delay_ms: u64, now_ms: u64) {
        self.delay_ms = delay_ms;
        self.edited(now_ms);
    }

    pub fn edited(&mut self, now_ms: u64) {
        self.last_edit_ms = Some(now_ms);
        self.invalidate();
    }

    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.last_edit_ms
            .map(|last| self.delay_ms.saturating_sub(now_ms.saturating_sub(last)))
            .unwrap_or(0)
    }

    pub fn allows(&self, now_ms: u64, revision: u64, composition: Composition) -> bool {
        self.allows_with_policy(now_ms, revision, composition, CompositionPolicy::Strict)
    }

    pub fn allows_with_policy(
        &self,
        now_ms: u64,
        revision: u64,
        composition: Composition,
        policy: CompositionPolicy,
    ) -> bool {
        !self.paused
            && revision == self.revision
            && self.remaining_ms(now_ms) == 0
            && (composition == Composition::Inactive
                || (composition == Composition::Unknown
                    && policy == CompositionPolicy::IdleFallback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_composition_range_is_inactive_and_failed_query_is_unknown() {
        assert_eq!(composition_from_endpoints(Some(0)), Composition::Inactive);
        assert_eq!(composition_from_endpoints(Some(-1)), Composition::Active);
        assert_eq!(composition_from_endpoints(None), Composition::Unknown);
    }

    #[test]
    fn stale_composition_after_finish_uses_only_the_compatible_idle_policy() {
        let mut gate = IdleGate::new(700);
        gate.edited(100);
        let effective = composition_with_idle_evidence(
            Composition::Active,
            true,
            CompositionPolicy::IdleFallback,
        );
        assert_eq!(effective, Composition::Unknown);
        assert!(!gate.allows_with_policy(
            799,
            gate.revision(),
            effective,
            CompositionPolicy::IdleFallback
        ));
        assert!(gate.allows_with_policy(
            800,
            gate.revision(),
            effective,
            CompositionPolicy::IdleFallback
        ));
        assert_eq!(
            composition_with_idle_evidence(
                Composition::Active,
                false,
                CompositionPolicy::IdleFallback
            ),
            Composition::Active
        );
        assert_eq!(
            composition_with_idle_evidence(Composition::Active, true, CompositionPolicy::Strict),
            Composition::Active
        );
    }

    #[test]
    fn unknown_composition_fallback_still_requires_idle_and_a_fresh_result() {
        let mut gate = IdleGate::new(700);
        let policy = CompositionPolicy::IdleFallback;
        gate.edited(100);
        let previous = gate.revision();
        assert!(!gate.allows_with_policy(799, previous, Composition::Unknown, policy));
        assert!(gate.allows_with_policy(800, previous, Composition::Unknown, policy));
        assert!(!gate.allows(800, previous, Composition::Unknown));
        assert!(!gate.allows_with_policy(60000, previous, Composition::Active, policy));
        gate.edited(1000);
        assert!(!gate.allows_with_policy(1700, previous, Composition::Unknown, policy));
        gate.paused = true;
        assert!(!gate.allows_with_policy(1700, gate.revision(), Composition::Unknown, policy));
    }

    #[test]
    fn transient_provider_failure_retries_but_permanent_failure_stops() {
        let mut retry = QueryRetry::default();
        for _ in 0..6 {
            assert!(retry.take(1));
        }
        assert!(!retry.take(1));
        assert!(!retry.take(1));
        assert!(retry.take(2)); // A new input field gets its own retry budget.
        retry.recovered();
        for _ in 0..6 {
            assert!(retry.take(2));
        }
        assert!(!retry.take(2));
    }

    #[test]
    fn maps_japanese_modes_and_preserves_failures() {
        assert_eq!(japanese_mode(false, None), Some(InputMode::Alphanumeric));
        assert_eq!(japanese_mode(true, None), None);
        for (flags, mode) in [
            (9, InputMode::Hiragana),
            (0, InputMode::Alphanumeric),
            (8, InputMode::FullAlphanumeric),
            (11, InputMode::Katakana),
            (3, InputMode::HalfKatakana),
        ] {
            assert_eq!(japanese_mode(true, Some(flags)), Some(mode));
        }
        assert_eq!(japanese_mode(true, Some(1)), None);
        assert_eq!(japanese_mode(true, Some(0x109)), None);
    }

    #[test]
    fn first_focus_shows_but_edit_hides_until_exact_deadline() {
        let mut gate = IdleGate::new(700);
        assert!(gate.allows(0, gate.revision(), Composition::Inactive));
        gate.edited(100);
        assert!(!gate.allows(799, gate.revision(), Composition::Inactive));
        assert!(gate.allows(800, gate.revision(), Composition::Inactive));
    }

    #[test]
    fn repeated_input_restarts_delay_and_rejects_stale_samples() {
        let mut gate = IdleGate::new(700);
        gate.edited(100);
        let stale = gate.revision();
        gate.edited(700);
        assert!(!gate.allows(1399, gate.revision(), Composition::Inactive));
        assert!(!gate.allows(2000, stale, Composition::Inactive));
        assert!(gate.allows(1400, gate.revision(), Composition::Inactive));
    }

    #[test]
    fn long_pause_during_composition_never_shows() {
        let mut gate = IdleGate::new(700);
        gate.edited(0);
        assert!(!gate.allows(60000, gate.revision(), Composition::Active));
        assert!(!gate.allows(60000, gate.revision(), Composition::Unknown));
        gate.edited(60000); // Finalization or cancellation restarts the idle delay.
        assert!(!gate.allows(60699, gate.revision(), Composition::Inactive));
        assert!(gate.allows(60700, gate.revision(), Composition::Inactive));
    }

    #[test]
    fn focus_change_and_pause_invalidate_previous_results() {
        let mut gate = IdleGate::new(700);
        let previous = gate.revision();
        gate.focus_changed();
        assert!(!gate.allows(1000, previous, Composition::Inactive));
        gate.paused = true;
        assert!(!gate.allows(1000, gate.revision(), Composition::Inactive));
    }

    #[test]
    fn changing_delay_preserves_generation_and_rejects_in_flight_result() {
        let mut gate = IdleGate::new(700);
        gate.edited(10);
        let previous = gate.revision();
        gate.set_delay(300, 20);
        assert_ne!(gate.revision(), previous);
        assert!(!gate.allows(1000, previous, Composition::Inactive));
        assert!(!gate.allows(319, gate.revision(), Composition::Inactive));
        assert!(gate.allows(320, gate.revision(), Composition::Inactive));
    }

    #[test]
    fn badge_respects_negative_coordinates_dpi_and_screen_edges() {
        let work = Rect {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let caret = Rect {
            left: -1918,
            top: 2,
            right: -1918,
            bottom: 22,
        };
        let badge = place_badge(caret, work, 144, InputMode::Alphanumeric);
        assert_eq!(badge.left, -1920);
        assert_eq!(badge.top, 31);
        assert_eq!(badge.right - badge.left, 39);
        let edge = place_badge(
            Rect {
                left: -1,
                top: 1070,
                right: -1,
                bottom: 1090,
            },
            work,
            192,
            InputMode::FullAlphanumeric,
        );
        assert_eq!(edge.right, 0);
        assert_eq!(edge.right - edge.left, 72);
        assert_eq!(edge.bottom - edge.top, 52);
        assert!(edge.bottom <= 1080);
    }
}
