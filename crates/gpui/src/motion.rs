//! Finite, time-based motion. Callers own policy, geometry and frame demand.

use std::time::{Duration, Instant};

/// Interpolation and distance in a presented value's own units.
pub trait Lerp: Copy {
    /// Interpolate by the share `u` of the distance from `from` to `to`.
    fn lerp(from: Self, to: Self, u: f32) -> Self;

    /// How far apart two presented values are, in the value's own units. A
    /// scalar's distance is its difference and a point's is its length; motion
    /// reads it to weigh the travel a response still has against the travel a
    /// new one would have.
    fn distance(from: Self, to: Self) -> f32;
}

impl Lerp for f32 {
    fn lerp(from: Self, to: Self, u: f32) -> Self {
        from + (to - from) * u
    }

    fn distance(from: Self, to: Self) -> f32 {
        (to - from).abs()
    }
}

impl Lerp for crate::Point<f32> {
    fn lerp(from: Self, to: Self, u: f32) -> Self {
        crate::point(f32::lerp(from.x, to.x, u), f32::lerp(from.y, to.y, u))
    }

    fn distance(from: Self, to: Self) -> f32 {
        (to.x - from.x).hypot(to.y - from.y)
    }
}

/// The normalized minimum-jerk position profile `s(u)`, read as the share of
/// the move still outstanding at `u`: `s(0) = 0`, `s(1) = 1`, with zero
/// velocity and acceleration at both ends. Duration stays a caller decision.
///
/// Written as `1 - s(u)` in `w = 1 - u`, where it expands to
/// `w^3 * (10 - 15w + 6w^2)`: a product of exact factors, so the last part of
/// the profile keeps its precision instead of cancelling away.
fn minimum_jerk_outstanding(u: f32) -> f32 {
    let w = (1.0 - u).clamp(0.0, 1.0);
    w * w * w * (10.0 - 15.0 * w + 6.0 * w * w)
}

/// The profile's normalized speed `s'(u)`: zero at both ends, peaking at the
/// midpoint.
fn minimum_jerk_slope(u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    let w = 1.0 - u;
    30.0 * u * u * w * w
}

/// A redirect never enters the profile past this phase. The last percent of a
/// response is what keeps a redirected motion in flight: a motion that joined
/// at the very end would already be settled the moment it was redirected, and
/// the visual would be dropped onto a caret it was still supposed to glide to.
const JOIN_PHASE_CEILING: f32 = 0.99;

/// The phase at which the profile's speed, as a multiple of the average speed
/// a fresh motion over the same distance has, is `ratio`.
///
/// That speed rises from zero at the start of the profile to unbounded at its
/// end, so an answer exists for every ratio and is found by bisection. Both
/// halves are written in `w = 1 - u`, where they stay cancellation-free:
/// `s'(u) = 30 u^2 w^2` and `1 - s(u) = w^3 (10 - 15w + 6w^2)`.
fn phase_for_speed(ratio: f32) -> f32 {
    let speed_ratio = |w: f32| 30.0 * (1.0 - w) * (1.0 - w) / (w * (10.0 - 15.0 * w + 6.0 * w * w));
    let (mut slowest, mut fastest) = (1.0 - JOIN_PHASE_CEILING, 1.0);
    for _ in 0..32 {
        let middle = 0.5 * (slowest + fastest);
        if speed_ratio(middle) > ratio {
            slowest = middle;
        } else {
            fastest = middle;
        }
    }
    1.0 - 0.5 * (slowest + fastest)
}

/// One value moving from `from` toward `target` over real time. All reading
/// is time-based through an `Instant` the caller supplies, so motion is
/// refresh-rate independent and deterministic: past its duration the motion
/// reports exactly its target.
///
/// Retargeting continues from the *current presented state* — never from the
/// old target or an earlier origin.
#[derive(Debug, Clone)]
pub struct Motion<T: Lerp> {
    from: T,
    target: T,
    began: Instant,
    duration: Duration,
}

impl<T: Lerp> Motion<T> {
    /// Start a motion from `from` toward `target`.
    pub fn start(from: T, target: T, began: Instant, duration: Duration) -> Self {
        Self {
            from,
            target,
            began,
            duration,
        }
    }

    /// Redirect an in-flight (or finished) motion toward a new target from
    /// wherever it is presented right now. The canonical way to interrupt.
    pub fn retarget(&mut self, target: T, now: Instant, duration: Duration) {
        self.from = self.presented(now);
        self.target = target;
        self.began = now;
        self.duration = duration;
    }

    /// Redirect toward `target` from `presented` — the value this redirect
    /// puts the motion at — **keeping the speed the motion is already
    /// travelling at**.
    ///
    /// A fresh motion over the same distance starts at rest. This one joins
    /// that same curve at the phase which already carries the incoming speed,
    /// so neither the presented value nor its speed jumps across the redirect.
    ///
    /// That is the redirect a *repeated* intent needs. `retarget` restarts the
    /// curve from rest, so a stream of redirects arriving faster than one
    /// response lasts would each begin a fresh ease-in; the value would never
    /// reach the speed its target is moving at, and the gap would grow with
    /// every repeat. Joining spends the progress already made instead of
    /// clearing it. A motion at rest has no speed to keep and joins at phase
    /// zero, which is exactly `Motion::start`.
    ///
    /// `presented` is supplied rather than sampled because a caller that has
    /// just re-derived the value against a moved origin already knows it.
    ///
    /// Returns the phase the new curve was entered at: it is what tells a
    /// continued redirect from a restarted one, and nothing about the motion
    /// itself depends on it.
    pub fn reaim(&mut self, presented: T, target: T, now: Instant, duration: Duration) -> f32 {
        let seconds = duration.as_secs_f32();
        let outstanding = T::distance(presented, target);
        let carried = self.speed(now);
        let join = if seconds > 0.0 && outstanding > 0.0 && carried > 0.0 {
            phase_for_speed(carried * seconds / outstanding)
        } else {
            0.0
        };
        let left = minimum_jerk_outstanding(join);
        self.from = if left > 0.0 {
            T::lerp(target, presented, left.recip())
        } else {
            presented
        };
        self.target = target;
        self.began = now
            .checked_sub(Duration::from_secs_f32(join * seconds))
            .unwrap_or(now);
        self.duration = duration;
        join
    }

    /// The presented value at `now`. Always inside `from..=target`: the
    /// profile cannot overshoot, so a wrong frame can never fling the value
    /// past its destination.
    ///
    /// Read through the travel still outstanding rather than the travel done:
    /// near the end of the profile the two are the same number algebraically,
    /// but the outstanding form keeps its precision where `s(u)` has already
    /// rounded to one — which a redirect deep into the profile depends on.
    pub fn presented(&self, now: Instant) -> T {
        let outstanding = minimum_jerk_outstanding(self.phase(now));
        T::lerp(self.target, self.from, outstanding)
    }

    /// How fast the presented value is moving at `now`, in value units per
    /// second. Zero at both ends of the profile, fastest at its midpoint.
    pub fn speed(&self, now: Instant) -> f32 {
        let seconds = self.duration.as_secs_f32();
        if seconds <= 0.0 {
            return 0.0;
        }
        T::distance(self.from, self.target) * minimum_jerk_slope(self.phase(now)) / seconds
    }

    /// How far through its own profile the motion is: zero at the start, one
    /// at the end.
    fn phase(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            1.0
        } else {
            let elapsed = now.saturating_duration_since(self.began).as_secs_f32();
            (elapsed / self.duration.as_secs_f32()).clamp(0.0, 1.0)
        }
    }

    /// The value this motion is moving toward.
    pub fn target(&self) -> T {
        self.target
    }

    /// Whether the motion has deterministically reached its target: every
    /// `now` at or past the end presents exactly the target.
    pub fn is_settled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.began) >= self.duration
    }

    /// The exact terminal instant, for deterministic motion tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn ends_at(&self) -> Instant {
        self.began + self.duration
    }

    /// Complete the motion immediately at `now`: the presented value becomes
    /// the target and the motion reports settled from this instant on. The
    /// explicit cancel/settle half of the motion contract, without faking it
    /// through a zero-duration `retarget`. A later `retarget` simply starts a
    /// fresh motion from the settled value.
    pub fn finish(&mut self, now: Instant) {
        self.from = self.target;
        self.began = now;
        self.duration = Duration::ZERO;
    }
}

impl Motion<f32> {
    pub(crate) fn shift(&mut self, displacement: f32) {
        self.from += displacement;
        self.target += displacement;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One base reading per test; every instant is derived from it by pure
    // arithmetic, so the assertions hold no matter how the scheduler
    // interleaves the run.
    fn base() -> Instant {
        Instant::now()
    }

    // --- Minimum-jerk motion primitives ----------------------------------

    #[test]
    fn scalar_motion_is_time_based_and_never_overshoots() {
        let base = base();
        let t = |ms: u64| base + Duration::from_millis(ms);
        let duration = Duration::from_millis(100);
        let motion = Motion::start(0.0, 10.0, t(0), duration);
        // Early frames are near the start; mid-flight is partway; the end is
        // exactly the target, at any refresh rate.
        let early = motion.presented(t(5));
        let mid = motion.presented(t(50));
        // The value runs 0..=10: early stays near the start, mid is halfway.
        assert!((0.0..5.0).contains(&early), "early={early}");
        assert!((4.0..6.0).contains(&mid), "mid={mid}");
        assert_eq!(motion.presented(t(100)), 10.0);
        assert_eq!(motion.presented(t(1_000)), 10.0);
        assert!(motion.is_settled(t(100)));
        assert!(!motion.is_settled(t(99)));
        // Zero duration settles immediately instead of dividing by zero.
        let instant = Motion::start(0.0, 10.0, t(0), Duration::ZERO);
        assert_eq!(instant.presented(t(0)), 10.0);
    }

    #[test]
    fn retarget_continues_from_the_current_presented_state() {
        let base = base();
        let t = |ms: u64| base + Duration::from_millis(ms);
        let duration = Duration::from_millis(100);
        let mut motion = Motion::start(0.0, 10.0, t(0), duration);
        let mid_time = t(50);
        let mid = motion.presented(mid_time);
        // A ..= mid ..= B hold mid-flight: the presented state is between.
        assert!((0.0..10.0).contains(&mid));
        motion.retarget(20.0, mid_time, duration);
        // The new motion starts exactly where the old one was presented —
        // not at the old start, not at the old target.
        assert_eq!(motion.presented(mid_time), mid);
        assert_eq!(motion.target(), 20.0);
        let later = motion.presented(t(75));
        assert!(
            later > mid,
            "motion continues forward from the presented state"
        );
        assert!(
            (mid..=20.0).contains(&later),
            "no overshoot past the new target"
        );
        assert_eq!(motion.presented(t(150)), 20.0);
        // A retarget after the end just starts a fresh motion from the
        // settled value.
        motion.retarget(30.0, t(200), duration);
        assert_eq!(motion.presented(t(200)), 20.0);
        assert_eq!(motion.presented(t(300)), 30.0);
    }

    // --- Redirecting on speed rather than from rest ------------------------

    #[test]
    fn the_profile_is_the_same_curve_read_from_the_other_end() {
        // The profile is written as `1 - s(u)` so that the end of a response
        // keeps its precision and a redirect deep into the profile stays exact.
        // It is the same curve and the same motion law: expanding
        // `w^3 (10 - 15w + 6w^2)` in `w = 1 - u` gives exactly
        // `1 - (10u^3 - 15u^4 + 6u^5)`. This is that identity across the whole
        // profile, to within the direct polynomial's own f32 evaluation error —
        // its terms reach 15 where the value it produces is under one, which is
        // exactly the cancellation the complement form avoids.
        let mut worst = 0.0_f32;
        for step in 0..=1000 {
            let u = step as f32 / 1000.0;
            let minimum_jerk = 10.0 * u * u * u - 15.0 * u * u * u * u + 6.0 * u * u * u * u * u;
            worst = worst.max((minimum_jerk_outstanding(u) - (1.0 - minimum_jerk)).abs());
        }
        assert!(
            worst < 1e-5,
            "the two forms of the profile differ by {worst}"
        );
    }

    #[test]
    fn a_redirect_keeps_the_value_and_the_speed_the_motion_already_has() {
        // BLOCKER (repeat starvation): a redirect that restarts the curve from
        // rest throws away all the speed the motion has built up, so a stream
        // of redirects that arrive faster than one response lasts would each
        // begin a fresh ease-in and the value would never catch its target.
        // The redirect joins the curve where it already carries that speed.
        let base = base();
        let t = |ms: u64| base + Duration::from_millis(ms);
        let mut motion = Motion::start(0.0, 400.0, t(0), Duration::from_millis(160));
        let at = t(60);
        let presented = motion.presented(at);
        let speed = motion.speed(at);
        assert!(speed > 0.0, "an in-flight motion is moving");

        let join = motion.reaim(presented + 20.0, 0.0, at, Duration::from_millis(100));
        assert!(
            (motion.presented(at) - (presented + 20.0)).abs() < 0.1,
            "the redirect puts the value exactly where the caller says: {} vs {}",
            motion.presented(at),
            presented + 20.0
        );
        assert!(
            (motion.speed(at) - speed).abs() < speed * 0.01,
            "the speed is continuous across the redirect: {speed} -> {}",
            motion.speed(at)
        );
        assert!(
            join > 0.0,
            "a moving motion joins past the start of the curve, got {join}"
        );
        // The join is only ever a redirect: the profile is monotone, so the
        // value still cannot pass the new target.
        for tick in 0..12 {
            let now = t(60 + tick * 10);
            assert!(
                (0.0..=presented + 20.0).contains(&motion.presented(now)),
                "no overshoot past the new target: {:?}",
                motion.presented(now)
            );
        }
    }

    #[test]
    fn a_redirect_of_a_motion_at_rest_is_exactly_a_fresh_start() {
        // A single move is unaffected by the join: with no speed to keep, the
        // redirect enters at phase zero, which is the profile's own start.
        let base = base();
        let t = |ms: u64| base + Duration::from_millis(ms);
        let mut restarted = Motion::start(0.0, 0.0, t(0), Duration::from_millis(50));
        let join = restarted.reaim(30.0, 0.0, t(10), Duration::from_millis(100));
        assert_eq!(join, 0.0);
        let fresh = Motion::start(30.0, 0.0, t(10), Duration::from_millis(100));
        for tick in 0..=12 {
            assert_eq!(
                restarted.presented(t(10 + tick * 10)),
                fresh.presented(t(10 + tick * 10)),
                "tick {tick}"
            );
        }
        // The same holds for a motion that has already arrived.
        let mut settled = Motion::start(0.0, 10.0, t(0), Duration::from_millis(50));
        settled.finish(t(50));
        settled.reaim(40.0, 0.0, t(60), Duration::from_millis(100));
        assert_eq!(
            settled.presented(t(60)),
            40.0,
            "the redirect is where the caller says"
        );
        assert!(!settled.is_settled(t(60)), "and it is travelling again");
    }

    #[test]
    fn a_redirect_never_spends_the_last_of_its_response() {
        // A fast motion redirected onto a very short remaining distance asks
        // for the end of the profile. Joining there would leave the motion
        // settled the instant it was redirected — the visual dropped onto a
        // caret it was meant to glide to, with no frame left to draw it — and
        // the virtual origin behind the value would run away. The ceiling is
        // what keeps both bounded.
        let base = base();
        let t = |ms: u64| base + Duration::from_millis(ms);
        for remaining in [1_000.0_f32, 100.0, 10.0, 1.0, 0.1, 0.01] {
            // Mid-profile of a fast move: the largest speed it can carry.
            let mut motion = Motion::start(0.0, 400.0, t(0), Duration::from_millis(60));
            let at = t(30);
            let join = motion.reaim(remaining, 0.0, at, Duration::from_millis(100));
            assert!(
                join <= JOIN_PHASE_CEILING,
                "a redirect never enters past the ceiling: {join} at {remaining}px"
            );
            assert!(
                !motion.is_settled(at),
                "a redirected motion is still moving: {remaining}px"
            );
            assert!(
                (motion.presented(at) - remaining).abs() <= remaining * 0.001 + 0.001,
                "the value is where the caller says: {} vs {remaining}",
                motion.presented(at)
            );
            assert!(motion.speed(at).is_finite(), "speed stays finite");
        }
    }

    #[test]
    fn finish_settles_immediately_and_retargets_from_there() {
        let base = base();
        let t = |ms: u64| base + Duration::from_millis(ms);
        let duration = Duration::from_millis(100);
        let mut motion = Motion::start(0.0, 10.0, t(0), duration);
        // Cancel/settle mid-flight: the presented value snaps to the target
        // and stays there deterministically.
        motion.finish(t(30));
        assert_eq!(motion.presented(t(30)), 10.0);
        assert!(motion.is_settled(t(30)));
        assert_eq!(motion.presented(t(1_000)), 10.0);
        // A later retarget rebases from the settled value, not from 0.
        motion.retarget(20.0, t(1_000), duration);
        assert_eq!(motion.presented(t(1_000)), 10.0);
        assert_eq!(motion.presented(t(1_100)), 20.0);
    }

    #[test]
    fn point_motion_interpolates_both_axes() {
        use crate::point;
        let base = base();
        let t = |ms: u64| base + Duration::from_millis(ms);
        let from = point(0.0, 0.0);
        let to = point(100.0, 50.0);
        let motion = Motion::start(from, to, t(0), Duration::from_millis(100));
        let presented = motion.presented(t(100));
        assert_eq!(presented, to);
        let quarter = motion.presented(t(20));
        assert!(quarter.x > 0.0 && quarter.x < 100.0);
        assert!(quarter.y > 0.0 && quarter.y < 50.0);
    }
}
