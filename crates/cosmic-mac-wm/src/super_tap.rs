// SPDX-License-Identifier: GPL-3.0-or-later
//! Super-tap detection: COSMIC opens the launcher when Super is pressed and
//! released on its own (cosmic-comp's `(modifiers: [Super]): System(Launcher)`
//! binding). Super is Command here, as everywhere in cosmicMac.
//!
//! [`SuperTap`] is a pure state machine over the event tap's key and
//! modifier events and their timestamps. A Command release is a tap
//! candidate when:
//!
//! 1. Command went down with no other modifier held (Shift, Control,
//!    Option, Fn; Caps Lock is a lock state and does not count),
//! 2. nothing else happened before it came up: no key down, key up or
//!    autorepeat, no other modifier change, not the other Command key,
//! 3. it was held at most [`MAX_HOLD`].
//!
//! Mouse clicks are not tap events (adding them to the active tap would put
//! every click through the WM). Instead the caller confirms a candidate with
//! [`no_input_during`]: the time since the session's last click, scroll or
//! key press must exceed the hold. That platform counter also sees key
//! presses the tap misses (Secure Event Input), so Cmd+V in a password field
//! never opens the launcher.
//!
//! The machine resynchronises on every full Command release, so a missed
//! event (tap disabled by timeout, WM started with Command held) can at worst
//! drop one tap, never produce one.

use std::time::{Duration, Instant};

use cosmic_mac_platform::HotkeySpec;

/// Longest hold that still counts as a tap.
pub const MAX_HOLD: Duration = Duration::from_millis(400);

/// Extra margin for [`no_input_during`]: the press is timestamped when the
/// tap callback runs, slightly after the hardware event.
pub const INPUT_SLACK: Duration = Duration::from_millis(25);

/// `kVK_Command`, the left Command key.
pub const KEYCODE_COMMAND: u16 = 0x37;
/// `kVK_RightCommand`.
pub const KEYCODE_RIGHT_COMMAND: u16 = 0x36;

/// Modifier bits that count as "another modifier held". Caps Lock
/// (`kCGEventFlagMaskAlphaShift`) is deliberately absent.
const HELD_MODIFIERS: u64 = HotkeySpec::MASK_COMMAND
    | HotkeySpec::MASK_SHIFT
    | HotkeySpec::MASK_CONTROL
    | HotkeySpec::MASK_OPTION
    | HotkeySpec::MASK_FN;

/// One event from the tap, reduced to what the detector needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapEvent {
    /// A modifier change: the modifier key's code and the raw flags after
    /// the change.
    Flags { keycode: u16, flags: u64 },
    /// Any key down, key up or autorepeat.
    Key,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Command is up.
    Idle,
    /// Command went down alone at `since` with key `keycode`.
    Armed { since: Instant, keycode: u16 },
    /// Command is down but this press can no longer be a tap.
    Spoiled,
}

/// The tap detector. See the module docs for the rules.
#[derive(Debug, Clone)]
pub struct SuperTap {
    state: State,
    max_hold: Duration,
}

impl Default for SuperTap {
    fn default() -> Self {
        SuperTap::new(MAX_HOLD)
    }
}

impl SuperTap {
    /// A detector accepting holds up to `max_hold`.
    pub fn new(max_hold: Duration) -> Self {
        SuperTap {
            state: State::Idle,
            max_hold,
        }
    }

    /// Feeds one event observed at `at`. Returns how long Command was held
    /// when this event completes a tap candidate (confirm it with
    /// [`no_input_during`]).
    pub fn observe(&mut self, event: TapEvent, at: Instant) -> Option<Duration> {
        let (next, tap) = match (self.state, event) {
            (State::Armed { .. }, TapEvent::Key) => (State::Spoiled, None),
            (state, TapEvent::Key) => (state, None),
            (state, TapEvent::Flags { keycode, flags }) => {
                let held = flags & HELD_MODIFIERS;
                let command_down = held & HotkeySpec::MASK_COMMAND != 0;
                match state {
                    State::Idle if held == HotkeySpec::MASK_COMMAND && is_command(keycode) => {
                        (State::Armed { since: at, keycode }, None)
                    }
                    // Command together with something else, or a press the
                    // tap only half saw.
                    State::Idle if command_down => (State::Spoiled, None),
                    State::Idle => (State::Idle, None),
                    State::Armed {
                        since,
                        keycode: armed,
                    } if keycode == armed => {
                        if held == 0 {
                            let hold = at.saturating_duration_since(since);
                            (State::Idle, (hold <= self.max_hold).then_some(hold))
                        } else if held == HotkeySpec::MASK_COMMAND {
                            // A repeated report of the same press.
                            (state, None)
                        } else {
                            (State::Spoiled, None)
                        }
                    }
                    // Another modifier (or the other Command key) changed.
                    State::Armed { .. } | State::Spoiled if command_down => (State::Spoiled, None),
                    State::Armed { .. } | State::Spoiled => (State::Idle, None),
                }
            }
        };
        self.state = next;
        tap
    }

    /// Whether Command is down as a tap candidate right now (tests, logs).
    pub fn is_armed(&self) -> bool {
        matches!(self.state, State::Armed { .. })
    }
}

fn is_command(keycode: u16) -> bool {
    keycode == KEYCODE_COMMAND || keycode == KEYCODE_RIGHT_COMMAND
}

/// Confirms a candidate held for `held`: true when the most recent click,
/// scroll or key press (`since_input` ago) happened before Command went
/// down, with [`INPUT_SLACK`] to spare.
pub fn no_input_during(held: Duration, since_input: Duration) -> bool {
    since_input > held + INPUT_SLACK
}

/// Whether a Super tap candidate should toggle the launcher: no click,
/// scroll or key during the hold ([`no_input_during`]) and no mouse button
/// held down (Command tapped in the middle of a drag).
pub fn should_toggle(held: Duration, since_input: Duration, button_down: bool) -> bool {
    !button_down && no_input_during(held, since_input)
}

/// `seconds` (from `CGEventSourceSecondsSinceLastEventType`) as a duration;
/// negative, NaN or infinite values (never any input) mean "long ago".
pub fn duration_from_seconds(seconds: f64) -> Duration {
    if seconds.is_finite() && seconds >= 0.0 {
        Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX)
    } else {
        Duration::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: u64 = HotkeySpec::MASK_COMMAND;
    const SHIFT: u64 = HotkeySpec::MASK_SHIFT;
    const OPTION: u64 = HotkeySpec::MASK_OPTION;
    const FN: u64 = HotkeySpec::MASK_FN;
    const CAPS_LOCK: u64 = 1 << 16;
    /// Device-dependent bits macOS adds (left Command, non-coalesced).
    const NOISE: u64 = 0x0000_0108;
    const KEYCODE_SHIFT: u16 = 0x38;
    const KEYCODE_CAPS_LOCK: u16 = 0x39;

    fn flags(keycode: u16, flags: u64) -> TapEvent {
        TapEvent::Flags { keycode, flags }
    }

    /// Replays `(event, ms)` pairs and returns the tap holds produced.
    fn run(detector: &mut SuperTap, events: &[(TapEvent, u64)]) -> Vec<Duration> {
        let start = Instant::now();
        events
            .iter()
            .filter_map(|&(event, ms)| detector.observe(event, start + Duration::from_millis(ms)))
            .collect()
    }

    fn taps(events: &[(TapEvent, u64)]) -> Vec<Duration> {
        run(&mut SuperTap::default(), events)
    }

    #[test]
    fn press_and_release_alone_is_a_tap() {
        let held = taps(&[
            (flags(KEYCODE_COMMAND, CMD | NOISE), 0),
            (flags(KEYCODE_COMMAND, 0x100), 120),
        ]);
        assert_eq!(held, [Duration::from_millis(120)]);
        // The right Command key works the same way.
        let held = taps(&[
            (flags(KEYCODE_RIGHT_COMMAND, CMD), 0),
            (flags(KEYCODE_RIGHT_COMMAND, 0), 80),
        ]);
        assert_eq!(held, [Duration::from_millis(80)]);
    }

    #[test]
    fn holding_too_long_is_not_a_tap() {
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (flags(KEYCODE_COMMAND, 0), 401),
            ])
            .is_empty()
        );
        assert_eq!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (flags(KEYCODE_COMMAND, 0), 400),
            ]),
            [MAX_HOLD]
        );
    }

    #[test]
    fn a_key_in_between_spoils_the_tap() {
        // Cmd+C, released in either order.
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (TapEvent::Key, 50),
                (TapEvent::Key, 90),
                (flags(KEYCODE_COMMAND, 0), 120),
            ])
            .is_empty()
        );
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (TapEvent::Key, 50),
                (flags(KEYCODE_COMMAND, 0), 100),
                (TapEvent::Key, 130),
            ])
            .is_empty()
        );
        // A key released after Command went down was held across the press.
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (TapEvent::Key, 20),
                (flags(KEYCODE_COMMAND, 0), 100),
            ])
            .is_empty()
        );
    }

    #[test]
    fn another_modifier_spoils_the_tap() {
        // Shift added and released while Command is held.
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (flags(KEYCODE_SHIFT, CMD | SHIFT), 30),
                (flags(KEYCODE_SHIFT, CMD), 60),
                (flags(KEYCODE_COMMAND, 0), 100),
            ])
            .is_empty()
        );
        // Shift held first, then Command; Shift released before Command.
        assert!(
            taps(&[
                (flags(KEYCODE_SHIFT, SHIFT), 0),
                (flags(KEYCODE_COMMAND, CMD | SHIFT), 30),
                (flags(KEYCODE_SHIFT, CMD), 60),
                (flags(KEYCODE_COMMAND, 0), 100),
            ])
            .is_empty()
        );
        // Command released while Option is still held.
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (flags(0x3A, CMD | OPTION), 30),
                (flags(KEYCODE_COMMAND, OPTION), 60),
                (flags(0x3A, 0), 90),
            ])
            .is_empty()
        );
        // Fn / Globe held.
        assert!(
            taps(&[
                (flags(0x3F, FN), 0),
                (flags(KEYCODE_COMMAND, CMD | FN), 20),
                (flags(KEYCODE_COMMAND, FN), 80),
            ])
            .is_empty()
        );
    }

    #[test]
    fn both_command_keys_spoil_the_tap() {
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (flags(KEYCODE_RIGHT_COMMAND, CMD), 30),
                (flags(KEYCODE_COMMAND, CMD), 60),
                (flags(KEYCODE_RIGHT_COMMAND, 0), 90),
            ])
            .is_empty()
        );
    }

    #[test]
    fn caps_lock_state_does_not_matter_but_toggling_it_does() {
        let held = taps(&[
            (flags(KEYCODE_COMMAND, CMD | CAPS_LOCK), 0),
            (flags(KEYCODE_COMMAND, CAPS_LOCK), 90),
        ]);
        assert_eq!(held, [Duration::from_millis(90)]);
        assert!(
            taps(&[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (flags(KEYCODE_CAPS_LOCK, CMD | CAPS_LOCK), 40),
                (flags(KEYCODE_COMMAND, CAPS_LOCK), 90),
            ])
            .is_empty()
        );
    }

    #[test]
    fn keys_while_idle_do_not_matter_and_detection_recovers() {
        let mut detector = SuperTap::default();
        // Typing, then a spoiled press, then a clean tap.
        let held = run(
            &mut detector,
            &[
                (TapEvent::Key, 0),
                (TapEvent::Key, 10),
                (flags(KEYCODE_COMMAND, CMD), 100),
                (TapEvent::Key, 120),
                (flags(KEYCODE_COMMAND, 0), 150),
                (TapEvent::Key, 200),
                (flags(KEYCODE_COMMAND, CMD), 300),
                (flags(KEYCODE_COMMAND, 0), 350),
            ],
        );
        assert_eq!(held, [Duration::from_millis(50)]);
        assert!(!detector.is_armed());
    }

    #[test]
    fn a_half_seen_press_never_taps() {
        // The WM started while Command was held: the first event is another
        // modifier with Command already set, or the release itself.
        assert!(
            taps(&[
                (flags(KEYCODE_SHIFT, CMD | SHIFT), 0),
                (flags(KEYCODE_SHIFT, CMD), 20),
                (flags(KEYCODE_COMMAND, 0), 50),
            ])
            .is_empty()
        );
        assert!(taps(&[(flags(KEYCODE_COMMAND, 0), 50)]).is_empty());
        // A Command flag reported for a non-Command key code.
        assert!(
            taps(&[
                (flags(KEYCODE_SHIFT, CMD), 0),
                (flags(KEYCODE_COMMAND, 0), 50),
            ])
            .is_empty()
        );
    }

    #[test]
    fn a_repeated_press_report_keeps_the_original_time() {
        let held = taps(&[
            (flags(KEYCODE_COMMAND, CMD), 0),
            (flags(KEYCODE_COMMAND, CMD), 300),
            (flags(KEYCODE_COMMAND, 0), 450),
        ]);
        assert!(held.is_empty(), "{held:?}");
    }

    #[test]
    fn missed_release_resynchronises() {
        let mut detector = SuperTap::default();
        // Armed, then the release was missed; Shift down/up arrive with
        // Command already gone, which returns to Idle.
        let held = run(
            &mut detector,
            &[
                (flags(KEYCODE_COMMAND, CMD), 0),
                (flags(KEYCODE_SHIFT, SHIFT), 500),
                (flags(KEYCODE_SHIFT, 0), 520),
                (flags(KEYCODE_COMMAND, CMD), 600),
                (flags(KEYCODE_COMMAND, 0), 650),
            ],
        );
        assert_eq!(held, [Duration::from_millis(50)]);
    }

    #[test]
    fn input_check_needs_the_last_click_or_key_before_the_press() {
        let held = Duration::from_millis(120);
        // Last click 2 s ago: fine.
        assert!(no_input_during(held, Duration::from_secs(2)));
        // A click 60 ms ago, i.e. while Command was down.
        assert!(!no_input_during(held, Duration::from_millis(60)));
        // Right at the press (within the slack) also counts as during.
        assert!(!no_input_during(held, held + Duration::from_millis(10)));
        assert!(no_input_during(
            held,
            held + INPUT_SLACK + Duration::from_millis(1)
        ));
        assert!(no_input_during(held, Duration::MAX));
    }

    #[test]
    fn a_held_mouse_button_rejects_the_tap() {
        let held = Duration::from_millis(120);
        assert!(should_toggle(held, Duration::from_secs(2), false));
        // The drag started long before Command went down.
        assert!(!should_toggle(held, Duration::from_secs(2), true));
        assert!(!should_toggle(held, Duration::from_millis(60), false));
    }

    #[test]
    fn seconds_convert_safely() {
        assert_eq!(duration_from_seconds(0.25), Duration::from_millis(250));
        assert_eq!(duration_from_seconds(f64::INFINITY), Duration::MAX);
        assert_eq!(duration_from_seconds(f64::NAN), Duration::MAX);
        assert_eq!(duration_from_seconds(-1.0), Duration::MAX);
        assert_eq!(duration_from_seconds(1e300), Duration::MAX);
    }
}
