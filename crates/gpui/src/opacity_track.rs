use std::time::Instant;

/// An immutable opacity program in application monotonic time.
/// A renderer may discard and recompile it without changing its phase.
#[derive(Clone, Debug, PartialEq)]
pub struct OpacityTrack {
    started_at: Instant,
    segments: Vec<OpacityCubic>,
    ending: OpacityEnding,
}

/// A cubic starting at `offset_s`, evaluated in seconds from that offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OpacityCubic {
    /// Seconds from the track's absolute start.
    pub offset_s: f64,
    /// Constant, linear, quadratic and cubic coefficients.
    pub coefficients: [f32; 4],
}

/// The finite endpoint or final repeating interval of an opacity program.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpacityEnding {
    /// Hold `value` after `at_s`.
    Hold {
        /// Seconds from the program start.
        at_s: f64,
        /// Exact final opacity.
        value: f32,
    },
    /// Repeat the interval immediately before `at_s` indefinitely.
    Repeat {
        /// Seconds from the program start to the repeat instruction.
        at_s: f64,
        /// Length of the interval immediately preceding that instruction.
        duration_s: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn finite_cubic_holds_its_exact_endpoint() {
        let start = Instant::now();
        let track = OpacityTrack::new(
            start,
            vec![OpacityCubic {
                offset_s: 0.0,
                coefficients: [0.0, 0.0, 3.0, -2.0],
            }],
            OpacityEnding::Hold {
                at_s: 1.0,
                value: 1.0,
            },
        );
        assert_eq!(track.value_at(start), 0.0);
        assert_eq!(track.value_at(start + Duration::from_millis(500)), 0.5);
        assert_eq!(track.value_at(start + Duration::from_secs(20)), 1.0);
    }

    #[test]
    fn delayed_repeat_and_recompilation_keep_absolute_phase() {
        let start = Instant::now();
        let track = OpacityTrack::new(
            start,
            vec![
                OpacityCubic {
                    offset_s: 0.0,
                    coefficients: [1.0, 0.0, 0.0, 0.0],
                },
                OpacityCubic {
                    offset_s: 2.0,
                    coefficients: [0.0, 0.5, 0.0, 0.0],
                },
            ],
            OpacityEnding::Repeat {
                at_s: 4.0,
                duration_s: 2.0,
            },
        );
        assert_eq!(track.value_at(start + Duration::from_secs(1)), 1.0);
        assert_eq!(track.value_at(start + Duration::from_secs(3)), 0.5);
        assert_eq!(track.clone().value_at(start + Duration::from_secs(7)), 0.5);
    }
}

impl OpacityTrack {
    /// Construct a program. Segments must start at zero, be strictly ordered,
    /// and end before the finite endpoint/repeat instruction.
    pub fn new(started_at: Instant, segments: Vec<OpacityCubic>, ending: OpacityEnding) -> Self {
        assert!(
            segments
                .first()
                .is_some_and(|segment| segment.offset_s == 0.0)
        );
        assert!(
            segments
                .windows(2)
                .all(|pair| pair[0].offset_s < pair[1].offset_s)
        );
        assert!(segments.iter().all(|segment| segment.offset_s.is_finite()
            && segment.coefficients.iter().all(|value| value.is_finite())));
        let end = match ending {
            OpacityEnding::Hold { at_s, value } => {
                assert!(value.is_finite() && (0.0..=1.0).contains(&value));
                at_s
            }
            OpacityEnding::Repeat { at_s, duration_s } => {
                assert!(duration_s.is_finite() && duration_s > 0.0 && duration_s <= at_s);
                at_s
            }
        };
        assert!(end.is_finite() && end >= segments.last().expect("nonempty track").offset_s);
        Self {
            started_at,
            segments,
            ending,
        }
    }

    /// A constant is the same program with a finite endpoint at zero.
    pub fn constant(started_at: Instant, value: f32) -> Self {
        Self::new(
            started_at,
            vec![OpacityCubic {
                offset_s: 0.0,
                coefficients: [value, 0.0, 0.0, 0.0],
            }],
            OpacityEnding::Hold { at_s: 0.0, value },
        )
    }

    /// The absolute application-time origin.
    pub fn started_at(&self) -> Instant {
        self.started_at
    }
    /// Ordered native-independent cubic instructions.
    pub fn segments(&self) -> &[OpacityCubic] {
        &self.segments
    }
    /// The final instruction.
    pub fn ending(&self) -> OpacityEnding {
        self.ending
    }

    /// CPU evaluation, also used by headless renderers and interruption policy.
    pub fn value_at(&self, at: Instant) -> f32 {
        let mut elapsed = at.saturating_duration_since(self.started_at).as_secs_f64();
        match self.ending {
            OpacityEnding::Hold { at_s, value } if elapsed >= at_s => return value,
            OpacityEnding::Repeat { at_s, duration_s } if elapsed >= at_s => {
                elapsed = at_s - duration_s + (elapsed - at_s) % duration_s;
            }
            _ => {}
        }
        let index = self
            .segments
            .partition_point(|segment| segment.offset_s <= elapsed)
            - 1;
        let segment = &self.segments[index];
        let t = elapsed - segment.offset_s;
        let [a, b, c, d] = segment.coefficients.map(f64::from);
        (a + t * (b + t * (c + t * d))).clamp(0.0, 1.0) as f32
    }
}
