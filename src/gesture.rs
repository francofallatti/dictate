//! Push-to-talk gesture state machine (pure, no OS dependencies).
//!
//! Mirrors the behaviour of mainstream dictation tools:
//!
//! * hold the chord → dictate while held, release → stop and paste;
//! * quick tap (release within `double_tap`) → wait for a second tap; if it comes, lock
//!   hands-free mode, otherwise cancel;
//! * while locked, pressing the chord again stops (a third quick tap cancels instead);
//! * Escape cancels at any point.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    ChordDown,
    ChordUp,
    Escape,
    /// Periodic tick so time-based transitions can fire.
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Start capturing audio.
    Start,
    /// Hands-free mode engaged; keep capturing until the next press.
    Locked,
    /// Stop capturing and process what was recorded.
    Stop,
    /// Stop capturing and discard the recording.
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// Chord is held down, recording.
    Held { started: Instant },
    /// Chord released quickly; recording continues until the window closes or a second tap arrives.
    AwaitingSecondTap { deadline: Instant },
    /// Hands-free recording.
    Locked { locked_at: Instant, press_pending: bool },
    /// Chord still physically down after a stop/cancel; ignore until released.
    DrainRelease,
}

#[derive(Debug)]
pub struct Gesture {
    state: State,
    double_tap: Duration,
    max_duration: Duration,
    recording_since: Option<Instant>,
}

impl Gesture {
    pub fn new(double_tap: Duration, max_duration: Duration) -> Self {
        Self { state: State::Idle, double_tap, max_duration, recording_since: None }
    }

    /// Returns the instant at which a `Tick` is needed, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        let max_deadline = self.recording_since.map(|t| t + self.max_duration);
        match self.state {
            State::AwaitingSecondTap { deadline } => Some(match max_deadline {
                Some(m) if m < deadline => m,
                _ => deadline,
            }),
            State::Held { .. } | State::Locked { .. } => max_deadline,
            _ => None,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording_since.is_some()
    }

    pub fn handle(&mut self, input: Input, now: Instant) -> Option<Action> {
        // Global guards first.
        if let Some(since) = self.recording_since {
            if input == Input::Tick && now.duration_since(since) >= self.max_duration {
                return self.finish(Action::Stop, true);
            }
        }
        if input == Input::Escape {
            return if self.recording_since.is_some() { self.finish(Action::Cancel, true) } else { None };
        }

        match (self.state, input) {
            (State::Idle, Input::ChordDown) => {
                self.state = State::Held { started: now };
                self.recording_since = Some(now);
                Some(Action::Start)
            }
            (State::Held { started }, Input::ChordUp) => {
                if now.duration_since(started) < self.double_tap {
                    self.state = State::AwaitingSecondTap { deadline: now + self.double_tap };
                    None
                } else {
                    self.finish(Action::Stop, false)
                }
            }
            (State::AwaitingSecondTap { deadline }, Input::ChordDown) => {
                if now <= deadline {
                    self.state = State::Locked { locked_at: now, press_pending: true };
                    Some(Action::Locked)
                } else {
                    // Window already closed but no tick arrived yet: treat as a fresh start.
                    let cancel = self.finish(Action::Cancel, false);
                    debug_assert!(cancel.is_some());
                    self.state = State::Held { started: now };
                    self.recording_since = Some(now);
                    // The caller sees a Cancel first; the next ChordUp/tick drives the new session.
                    cancel
                }
            }
            (State::AwaitingSecondTap { deadline }, Input::Tick) => {
                if now >= deadline {
                    self.finish(Action::Cancel, false)
                } else {
                    None
                }
            }
            (State::Locked { locked_at, press_pending: true }, Input::ChordUp) => {
                self.state = State::Locked { locked_at, press_pending: false };
                None
            }
            (State::Locked { locked_at, press_pending: false }, Input::ChordDown) => {
                let action = if now.duration_since(locked_at) < self.double_tap { Action::Cancel } else { Action::Stop };
                self.finish(action, true)
            }
            (State::DrainRelease, Input::ChordUp) => {
                self.state = State::Idle;
                None
            }
            _ => None,
        }
    }

    fn finish(&mut self, action: Action, chord_may_be_down: bool) -> Option<Action> {
        self.recording_since = None;
        self.state = if chord_may_be_down && matches!(self.state, State::Held { .. } | State::Locked { .. }) {
            State::DrainRelease
        } else {
            State::Idle
        };
        Some(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> (Gesture, Instant) {
        (Gesture::new(Duration::from_millis(500), Duration::from_secs(600)), Instant::now())
    }

    #[test]
    fn hold_and_release_stops() {
        let (mut g, t0) = g();
        assert_eq!(g.handle(Input::ChordDown, t0), Some(Action::Start));
        assert_eq!(g.handle(Input::ChordUp, t0 + Duration::from_secs(2)), Some(Action::Stop));
        assert!(!g.is_recording());
    }

    #[test]
    fn quick_tap_without_second_tap_cancels() {
        let (mut g, t0) = g();
        assert_eq!(g.handle(Input::ChordDown, t0), Some(Action::Start));
        assert_eq!(g.handle(Input::ChordUp, t0 + Duration::from_millis(100)), None);
        assert!(g.is_recording());
        assert_eq!(g.handle(Input::Tick, t0 + Duration::from_millis(300)), None);
        assert_eq!(g.handle(Input::Tick, t0 + Duration::from_millis(700)), Some(Action::Cancel));
        assert!(!g.is_recording());
    }

    #[test]
    fn double_tap_locks_then_press_stops() {
        let (mut g, t0) = g();
        assert_eq!(g.handle(Input::ChordDown, t0), Some(Action::Start));
        assert_eq!(g.handle(Input::ChordUp, t0 + Duration::from_millis(100)), None);
        assert_eq!(g.handle(Input::ChordDown, t0 + Duration::from_millis(300)), Some(Action::Locked));
        assert_eq!(g.handle(Input::ChordUp, t0 + Duration::from_millis(400)), None);
        assert!(g.is_recording());
        assert_eq!(g.handle(Input::ChordDown, t0 + Duration::from_secs(5)), Some(Action::Stop));
        // Release of that press is swallowed.
        assert_eq!(g.handle(Input::ChordUp, t0 + Duration::from_secs(5)), None);
        // Next press starts fresh.
        assert_eq!(g.handle(Input::ChordDown, t0 + Duration::from_secs(6)), Some(Action::Start));
    }

    #[test]
    fn triple_tap_cancels() {
        let (mut g, t0) = g();
        g.handle(Input::ChordDown, t0);
        g.handle(Input::ChordUp, t0 + Duration::from_millis(100));
        assert_eq!(g.handle(Input::ChordDown, t0 + Duration::from_millis(200)), Some(Action::Locked));
        g.handle(Input::ChordUp, t0 + Duration::from_millis(250));
        assert_eq!(g.handle(Input::ChordDown, t0 + Duration::from_millis(400)), Some(Action::Cancel));
    }

    #[test]
    fn escape_cancels_while_recording_only() {
        let (mut g, t0) = g();
        assert_eq!(g.handle(Input::Escape, t0), None);
        g.handle(Input::ChordDown, t0);
        assert_eq!(g.handle(Input::Escape, t0 + Duration::from_secs(1)), Some(Action::Cancel));
        assert_eq!(g.handle(Input::ChordUp, t0 + Duration::from_secs(1)), None);
        assert_eq!(g.handle(Input::ChordDown, t0 + Duration::from_secs(2)), Some(Action::Start));
    }

    #[test]
    fn max_duration_forces_stop() {
        let mut g = Gesture::new(Duration::from_millis(500), Duration::from_secs(10));
        let t0 = Instant::now();
        g.handle(Input::ChordDown, t0);
        assert_eq!(g.handle(Input::Tick, t0 + Duration::from_secs(11)), Some(Action::Stop));
    }
}
