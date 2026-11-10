//! A wheel response owns unconsumed displacement, never a scroll position.

use crate::Motion;
use std::time::{Duration, Instant};

// Chromium's inverse-delta duration and retarget suppression calibration.
// Distance conversion and the scrollable range remain the caller's facts.
const WHEEL_GLIDE_MAX_MS: f32 = 150.0;
const WHEEL_GLIDE_MIN_MS: f32 = 60.0;
const WHEEL_GLIDE_NEAR_PX: f32 = 120.0;
const WHEEL_GLIDE_FAR_PX: f32 = 360.0;
const WHEEL_GLIDE_SPEED_FACTOR: f32 = 2.5;

fn wheel_glide_duration(distance_px: f32) -> Duration {
    let travel = ((distance_px.abs() - WHEEL_GLIDE_NEAR_PX)
        / (WHEEL_GLIDE_FAR_PX - WHEEL_GLIDE_NEAR_PX))
        .clamp(0.0, 1.0);
    Duration::from_secs_f32(
        (WHEEL_GLIDE_MAX_MS + (WHEEL_GLIDE_MIN_MS - WHEEL_GLIDE_MAX_MS) * travel) / 1000.0,
    )
}

fn wheel_retarget_duration(distance_px: f32, carried_px_per_s: f32) -> Duration {
    let law = wheel_glide_duration(distance_px);
    if carried_px_per_s > 0.0 {
        law.min(Duration::from_secs_f32(
            distance_px.abs() / carried_px_per_s * WHEEL_GLIDE_SPEED_FACTOR,
        ))
    } else {
        law
    }
}

/// One axis of finite wheel travel, expressed relative to its consumer.
///
/// Native containers consume increments with [`Self::take_delta`]. A layout
/// that already committed the destination can instead use [`Self::remaining`]
/// as its temporary presentation correction. Both consume the same response;
/// neither gives this object ownership of actual scroll position or geometry.
#[derive(Debug, Default, Clone)]
pub struct WheelMotion {
    motion: Option<Motion<f32>>,
}

impl WheelMotion {
    /// Append a same-direction notch, or replace unfinished travel on reversal.
    /// The caller clamps this proposal against its own authoritative range.
    pub fn proposed_remaining(remaining: f32, delta: f32) -> f32 {
        if remaining * delta < 0.0 {
            delta
        } else {
            remaining + delta
        }
    }

    /// The signed displacement not yet presented at `at`.
    pub fn remaining(&self, at: Instant) -> f32 {
        self.motion
            .as_ref()
            .map_or(0.0, |motion| motion.target() - motion.presented(at))
    }

    /// The total displacement still owed to an incremental consumer, including
    /// elapsed travel that has not yet been delivered by [`Self::take_delta`].
    pub fn unconsumed_target(&self) -> f32 {
        self.motion.as_ref().map_or(0.0, Motion::target)
    }

    /// Aim at an accepted remaining displacement from the current sample.
    /// Same-direction changes retain speed; reversal starts from rest. An
    /// unchanged goal does not renew the response's terminal instant. Elapsed
    /// but unconsumed travel stays in the curve until its consumer takes it.
    pub fn retarget_remaining(&mut self, remaining: f32, at: Instant) {
        let previous = self.remaining(at);
        if remaining == previous {
            return;
        }
        let presented = self
            .motion
            .as_ref()
            .map_or(0.0, |motion| motion.presented(at));
        if remaining == 0.0 {
            self.motion =
                (presented != 0.0).then(|| Motion::start(presented, presented, at, Duration::ZERO));
            return;
        }
        match &mut self.motion {
            Some(motion) if previous * remaining > 0.0 => {
                let duration = wheel_retarget_duration(remaining, motion.speed(at));
                motion.reaim(presented, presented + remaining, at, duration);
            }
            _ => {
                self.motion = Some(Motion::start(
                    presented,
                    presented + remaining,
                    at,
                    wheel_glide_duration(remaining),
                ));
            }
        }
    }

    /// Deliver travel since the previous consumption without restarting time.
    /// This must also be called on the terminal frame: an inactive response may
    /// still have its exact final increment to deliver.
    pub fn take_delta(&mut self, at: Instant) -> f32 {
        let Some(motion) = &mut self.motion else {
            return 0.0;
        };
        let delta = motion.presented(at);
        if motion.is_settled(at) {
            self.motion = None;
        } else {
            motion.shift(-delta);
        }
        delta
    }

    /// Discard unfinished travel. The consumer's actual position is untouched.
    pub fn cancel(&mut self) {
        self.motion = None;
    }

    /// Whether this response still needs another presentation sample.
    pub fn is_active(&self, at: Instant) -> bool {
        self.motion
            .as_ref()
            .is_some_and(|motion| !motion.is_settled(at))
    }

    /// Current speed magnitude, in displacement units per second.
    pub fn speed(&self, at: Instant) -> f32 {
        self.motion.as_ref().map_or(0.0, |motion| motion.speed(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(duration: Duration) -> u128 {
        (duration.as_secs_f32() * 1000.0).round() as u128
    }

    #[test]
    fn the_wheel_glide_duration_falls_linearly_over_the_ramp() {
        assert_eq!(
            ms(wheel_glide_duration(WHEEL_GLIDE_NEAR_PX)),
            WHEEL_GLIDE_MAX_MS as u128
        );
        assert_eq!(
            ms(wheel_glide_duration(WHEEL_GLIDE_FAR_PX)),
            WHEEL_GLIDE_MIN_MS as u128
        );
        let mid_px = (WHEEL_GLIDE_NEAR_PX + WHEEL_GLIDE_FAR_PX) / 2.0;
        let mid_ms = wheel_glide_duration(mid_px).as_secs_f32() * 1000.0;
        let expected = (WHEEL_GLIDE_MAX_MS + WHEEL_GLIDE_MIN_MS) / 2.0;
        assert!((mid_ms - expected).abs() < 0.01);
    }

    #[test]
    fn the_wheel_glide_duration_clamps_outside_the_ramp() {
        assert_eq!(ms(wheel_glide_duration(0.0)), WHEEL_GLIDE_MAX_MS as u128);
        assert_eq!(
            ms(wheel_glide_duration(WHEEL_GLIDE_NEAR_PX - 1.0)),
            WHEEL_GLIDE_MAX_MS as u128
        );
        assert_eq!(
            ms(wheel_glide_duration(WHEEL_GLIDE_FAR_PX + 1000.0)),
            WHEEL_GLIDE_MIN_MS as u128
        );
        assert_eq!(wheel_glide_duration(-300.0), wheel_glide_duration(300.0));
    }

    #[test]
    fn the_wheel_glide_duration_shrinks_as_the_distance_grows() {
        let mut previous = wheel_glide_duration(WHEEL_GLIDE_NEAR_PX);
        for step in 1..=12 {
            let distance = WHEEL_GLIDE_NEAR_PX
                + (WHEEL_GLIDE_FAR_PX - WHEEL_GLIDE_NEAR_PX) * (step as f32 / 12.0);
            let duration = wheel_glide_duration(distance);
            assert!(duration < previous);
            previous = duration;
        }
    }

    #[test]
    fn the_rebound_suppression_caps_only_what_the_speed_cannot_carry() {
        assert_eq!(
            wheel_retarget_duration(120.0, 300.0).as_millis(),
            WHEEL_GLIDE_MAX_MS as u128
        );
        let capped_ms = wheel_retarget_duration(120.0, 3000.0).as_secs_f32() * 1000.0;
        let cap = 120.0 / 3000.0 * WHEEL_GLIDE_SPEED_FACTOR * 1000.0;
        assert!((capped_ms - cap).abs() < 0.01);
        assert_eq!(
            wheel_retarget_duration(300.0, 0.0),
            wheel_glide_duration(300.0)
        );
    }

    #[test]
    fn reversal_replaces_even_a_larger_unfinished_goal() {
        assert_eq!(WheelMotion::proposed_remaining(200.0, 30.0), 230.0);
        assert_eq!(WheelMotion::proposed_remaining(-200.0, -30.0), -230.0);
        assert_eq!(WheelMotion::proposed_remaining(200.0, -30.0), -30.0);
        assert_eq!(WheelMotion::proposed_remaining(-200.0, 30.0), 30.0);
        assert_eq!(WheelMotion::proposed_remaining(0.0, -30.0), -30.0);
        assert_eq!(WheelMotion::proposed_remaining(200.0, -0.0), 200.0);
    }

    #[test]
    fn consuming_increments_preserves_the_curve_and_delivers_the_terminal_value() {
        let base = Instant::now();
        for direction in [-1.0, 1.0] {
            let mut motion = WheelMotion::default();
            motion.retarget_remaining(120.0 * direction, base);
            let end = motion.motion.as_ref().unwrap().ends_at();
            let mut delivered = 0.0;
            for tick in 0..15 {
                let at = base + Duration::from_millis(tick * 10);
                delivered += motion.take_delta(at);
                let remaining = motion.remaining(at);
                assert!((delivered + remaining - 120.0 * direction).abs() < 0.001);
                assert_eq!(motion.motion.as_ref().unwrap().ends_at(), end);
            }
            assert!(!motion.is_active(end));
            delivered += motion.take_delta(end);
            assert!((delivered - 120.0 * direction).abs() < 0.001);
            assert_eq!(motion.remaining(end), 0.0);
            assert_eq!(motion.take_delta(end), 0.0);
            assert!(motion.motion.is_none());
        }
    }

    #[test]
    fn an_unchanged_goal_keeps_its_speed_and_terminal_instant() {
        let base = Instant::now();
        let at = base + Duration::from_millis(30);
        let mut motion = WheelMotion::default();
        motion.retarget_remaining(120.0, base);
        let end = motion.motion.as_ref().unwrap().ends_at();
        let speed = motion.speed(at);
        motion.retarget_remaining(motion.remaining(at), at);
        assert_eq!(motion.motion.as_ref().unwrap().ends_at(), end);
        assert_eq!(motion.speed(at), speed);
        let mut idle = WheelMotion::default();
        idle.retarget_remaining(0.0, base);
        assert!(!idle.is_active(base));
        assert_eq!(idle.take_delta(at), 0.0);
    }

    #[test]
    fn a_same_direction_retarget_keeps_speed_in_local_coordinates() {
        let base = Instant::now();
        let at = base + Duration::from_millis(30);
        let mut motion = WheelMotion::default();
        motion.retarget_remaining(120.0, base);
        let delivered = motion.take_delta(at);
        let speed = motion.speed(at);
        let remaining = motion.remaining(at) + 60.0;
        motion.retarget_remaining(remaining, at);
        assert!((motion.remaining(at) - remaining).abs() < 0.001);
        assert!((motion.speed(at) - speed).abs() < speed * 0.01);
        assert!((motion.take_delta(at)).abs() < 0.001);
        let terminal = at + wheel_glide_duration(0.0);
        assert!((delivered + motion.take_delta(terminal) - 180.0).abs() < 0.001);
        assert!(!motion.is_active(terminal));
    }

    #[test]
    fn a_reversal_discards_old_travel_and_starts_from_rest() {
        let base = Instant::now();
        let at = base + Duration::from_millis(20);
        let mut motion = WheelMotion::default();
        motion.retarget_remaining(200.0, base);
        let delivered = motion.take_delta(at);
        assert!(delivered > 0.0 && motion.speed(at) > 0.0);
        let proposed = WheelMotion::proposed_remaining(motion.remaining(at), -30.0);
        motion.retarget_remaining(proposed, at);
        assert_eq!(motion.remaining(at), -30.0);
        assert_eq!(motion.speed(at), 0.0);
        assert_eq!(motion.take_delta(at), 0.0);
        let terminal = motion.motion.as_ref().unwrap().ends_at();
        assert!(motion.take_delta(at + Duration::from_millis(10)) < 0.0);
        let last = motion.take_delta(terminal);
        assert!(last < 0.0);
        assert!(!motion.is_active(terminal));
    }

    #[test]
    fn repeated_notches_share_one_short_tail_without_accumulating_global_position() {
        let base = Instant::now();
        let mut motion = WheelMotion::default();
        let mut delivered = 0.0;
        for tick in 0..100 {
            let at = base + Duration::from_millis(tick * 16);
            delivered += motion.take_delta(at);
            let proposed = WheelMotion::proposed_remaining(motion.remaining(at), 120.0);
            motion.retarget_remaining(proposed, at);
            assert!(motion.motion.as_ref().unwrap().ends_at() <= at + wheel_glide_duration(0.0));
            assert!((delivered + motion.remaining(at) - (tick + 1) as f32 * 120.0).abs() < 0.02);
        }
        let terminal = base + Duration::from_millis(99 * 16) + wheel_glide_duration(0.0);
        delivered += motion.take_delta(terminal);
        assert!((delivered - 12_000.0).abs() < 0.02);
        assert!(!motion.is_active(terminal));
        assert_eq!(motion.remaining(terminal), 0.0);
    }

    #[test]
    fn cancellation_does_not_deliver_the_discarded_target() {
        let base = Instant::now();
        let mut motion = WheelMotion::default();
        motion.retarget_remaining(120.0, base);
        motion.take_delta(base + Duration::from_millis(30));
        motion.cancel();
        assert_eq!(motion.take_delta(base + Duration::from_secs(1)), 0.0);
        assert_eq!(motion.remaining(base), 0.0);
        assert!(!motion.is_active(base));
    }

    #[test]
    fn retargeting_before_a_frame_keeps_elapsed_but_unconsumed_travel() {
        let base = Instant::now();
        let at = base + Duration::from_millis(30);
        let mut motion = WheelMotion::default();
        motion.retarget_remaining(120.0, base);
        let remaining = motion.remaining(at);
        let elapsed = motion.unconsumed_target() - remaining;
        let speed = motion.speed(at);
        motion.retarget_remaining(remaining + 60.0, at);
        assert!((motion.unconsumed_target() - 180.0).abs() < 0.001);
        assert!((motion.speed(at) - speed).abs() < speed * 0.01);
        let delivered = motion.take_delta(at);
        assert!((delivered - elapsed).abs() < 0.001);
        let last = motion.take_delta(at + wheel_glide_duration(0.0));
        assert!((delivered + last - 180.0).abs() < 0.001);
    }

    #[test]
    fn accepting_zero_remaining_still_delivers_already_elapsed_travel() {
        let base = Instant::now();
        let at = base + Duration::from_millis(30);
        let mut motion = WheelMotion::default();
        motion.retarget_remaining(120.0, base);
        let elapsed = motion.unconsumed_target() - motion.remaining(at);
        motion.retarget_remaining(0.0, at);
        assert!(!motion.is_active(at));
        assert_eq!(motion.remaining(at), 0.0);
        assert!((motion.take_delta(at) - elapsed).abs() < 0.001);
        assert_eq!(motion.take_delta(at), 0.0);
    }
}
