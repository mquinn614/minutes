//! Pure cost model + config picker for live-transcript auto-tuning.
//!
//! Live buzzword scoring (Minutes Madness) re-transcribes a growing audio
//! buffer repeatedly. On a slow/CPU-only host that work can exceed real time,
//! so whisper backs up and the bounded audio queue drops speech. The fix is to
//! pick a config — partial cadence + utterance cap (and, later, model) — whose
//! *sustained* whisper load stays comfortably under real time on the actual
//! machine.
//!
//! This module is the math behind that decision, kept dependency-free (no
//! whisper, no audio) so it is unit-testable anywhere and is the single source
//! of truth shared by the `whisper_rtf` benchmark example and (later) a runtime
//! probe. Measuring the timing points is the caller's job; turning them into a
//! cost model, projecting a config's load, and ranking candidates lives here.

/// Sustained load below this ratio keeps up comfortably (room for jitter/VAD).
pub const OK_THRESHOLD: f64 = 0.85;
/// At/above this ratio whisper cannot keep up and will back up + drop audio.
pub const TIGHT_THRESHOLD: f64 = 1.0;

/// One measured datapoint: transcribing `audio_secs` of audio took
/// `elapsed_secs` of wall-clock time on this machine/model/backend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtfPoint {
    pub audio_secs: f64,
    pub elapsed_secs: f64,
}

impl RtfPoint {
    pub fn new(audio_secs: f64, elapsed_secs: f64) -> Self {
        Self {
            audio_secs,
            elapsed_secs,
        }
    }
}

/// Linear cost model `elapsed ≈ overhead + marginal_rtf · audio_secs`.
///
/// `overhead_secs` captures the fixed per-call cost (state alloc, encoder warm)
/// that dominates short partial buffers; `marginal_rtf` is the per-second
/// transcription cost once that overhead is paid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostModel {
    pub overhead_secs: f64,
    pub marginal_rtf: f64,
}

impl CostModel {
    /// Fit from measured points using the first and last samples — identical to
    /// the long-standing `whisper_rtf` example so the projection is unchanged.
    /// Returns `None` with fewer than two distinct-length points to fit through.
    pub fn fit(points: &[RtfPoint]) -> Option<CostModel> {
        if points.len() < 2 {
            return None;
        }
        let first = points[0];
        let last = *points.last().unwrap();
        let span = last.audio_secs - first.audio_secs;
        if span.abs() < f64::EPSILON {
            return None;
        }
        let slope = (last.elapsed_secs - first.elapsed_secs) / span;
        let overhead = (first.elapsed_secs - slope * first.audio_secs).max(0.0);
        Some(CostModel {
            overhead_secs: overhead,
            marginal_rtf: slope,
        })
    }

    /// Predicted wall-clock seconds to transcribe a `buffer_secs` buffer.
    pub fn predict_secs(&self, buffer_secs: f64) -> f64 {
        self.overhead_secs + self.marginal_rtf * buffer_secs
    }
}

/// A candidate live-scoring config to evaluate against a [`CostModel`].
///
/// `partial_interval_secs == None` means partials are off: exactly one
/// transcription per utterance (the finalize at the cap).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveCandidate {
    pub partial_interval_secs: Option<f64>,
    pub cap_secs: f64,
}

impl LiveCandidate {
    pub fn partials_off(cap_secs: f64) -> Self {
        Self {
            partial_interval_secs: None,
            cap_secs,
        }
    }

    pub fn partials(interval_secs: f64, cap_secs: f64) -> Self {
        Self {
            partial_interval_secs: Some(interval_secs),
            cap_secs,
        }
    }

    /// Sustained whisper work per utterance divided by the wall-clock cap.
    ///
    /// With partials on, every partial re-transcribes the whole growing buffer
    /// (`predict(interval), predict(2·interval), …`), then a finalize pass over
    /// the full cap. With partials off it is just the single finalize pass.
    /// `< OK_THRESHOLD` keeps up; `>= TIGHT_THRESHOLD` falls behind.
    pub fn projected_load(&self, cost: &CostModel) -> f64 {
        if self.cap_secs <= 0.0 {
            return f64::INFINITY;
        }
        let mut work = 0.0;
        if let Some(interval) = self.partial_interval_secs {
            if interval > 0.0 {
                let mut t = interval;
                while t < self.cap_secs - 0.001 {
                    work += cost.predict_secs(t);
                    t += interval;
                }
            }
        }
        work += cost.predict_secs(self.cap_secs); // finalization pass
        work / self.cap_secs
    }
}

/// How a projected load ranks against real time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Comfortably real-time (`< OK_THRESHOLD`).
    Ok,
    /// Real-time but with little margin (`OK_THRESHOLD..TIGHT_THRESHOLD`).
    Tight,
    /// Slower than real time — will back up and drop audio (`>= TIGHT_THRESHOLD`).
    FallsBehind,
}

/// Classify a projected-load ratio into a [`Verdict`].
pub fn verdict(ratio: f64) -> Verdict {
    if ratio < OK_THRESHOLD {
        Verdict::Ok
    } else if ratio < TIGHT_THRESHOLD {
        Verdict::Tight
    } else {
        Verdict::FallsBehind
    }
}

/// The picked config plus why it was picked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Choice {
    pub candidate: LiveCandidate,
    pub projected_load: f64,
    pub verdict: Verdict,
}

/// Pick a config from `ladder` for the measured `cost`.
///
/// The caller orders `ladder` snappiest → safest. The first candidate that
/// projects comfortably real-time (`< OK_THRESHOLD`) wins, giving the snappiest
/// experience the machine can sustain. If none clear that bar (a weak host),
/// fall back to the lowest-load candidate as a best effort and report its
/// `Tight`/`FallsBehind` verdict so the caller can escalate (e.g. a smaller
/// model). Returns `None` only for an empty ladder.
pub fn choose(cost: &CostModel, ladder: &[LiveCandidate]) -> Option<Choice> {
    if ladder.is_empty() {
        return None;
    }
    let scored = ladder.iter().map(|c| (c, c.projected_load(cost)));

    let mut best_overall: Option<(&LiveCandidate, f64)> = None;
    for (cand, load) in scored {
        if load < OK_THRESHOLD {
            return Some(Choice {
                candidate: *cand,
                projected_load: load,
                verdict: Verdict::Ok,
            });
        }
        match best_overall {
            Some((_, best_load)) if best_load <= load => {}
            _ => best_overall = Some((cand, load)),
        }
    }

    let (cand, load) = best_overall.expect("ladder non-empty");
    Some(Choice {
        candidate: *cand,
        projected_load: load,
        verdict: verdict(load),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn fit_recovers_overhead_and_slope() {
        // elapsed = 0.2 + 0.5 * audio
        let pts = [
            RtfPoint::new(1.0, 0.7),
            RtfPoint::new(4.0, 2.2),
            RtfPoint::new(10.0, 5.2),
        ];
        let m = CostModel::fit(&pts).unwrap();
        approx(m.overhead_secs, 0.2);
        approx(m.marginal_rtf, 0.5);
        approx(m.predict_secs(2.0), 1.2);
    }

    #[test]
    fn fit_needs_two_distinct_points() {
        assert!(CostModel::fit(&[]).is_none());
        assert!(CostModel::fit(&[RtfPoint::new(1.0, 0.5)]).is_none());
        // Same buffer length twice → zero span → cannot fit a slope.
        assert!(CostModel::fit(&[RtfPoint::new(2.0, 1.0), RtfPoint::new(2.0, 1.1)]).is_none());
    }

    #[test]
    fn fit_clamps_negative_overhead_to_zero() {
        // A super-linear pair would imply negative intercept; clamp to 0.
        let m = CostModel::fit(&[RtfPoint::new(1.0, 0.1), RtfPoint::new(2.0, 1.0)]).unwrap();
        assert_eq!(m.overhead_secs, 0.0);
    }

    #[test]
    fn partials_off_is_single_finalize_pass() {
        let m = CostModel {
            overhead_secs: 0.5,
            marginal_rtf: 0.3,
        };
        let cand = LiveCandidate::partials_off(3.0);
        // work = predict(3) = 0.5 + 0.9 = 1.4; load = 1.4 / 3
        approx(cand.projected_load(&m), 1.4 / 3.0);
    }

    #[test]
    fn partials_on_accumulates_each_pass() {
        let m = CostModel {
            overhead_secs: 0.5,
            marginal_rtf: 0.3,
        };
        // interval 2.5, cap 5: partials at t=2.5 (t=5.0 is not < 5-0.001),
        // plus the finalize at 5.0.
        let cand = LiveCandidate::partials(2.5, 5.0);
        let expected = (m.predict_secs(2.5) + m.predict_secs(5.0)) / 5.0;
        approx(cand.projected_load(&m), expected);
    }

    #[test]
    fn projected_load_matches_example_for_failed_original() {
        // Sanity that the formula reproduces the example's accumulation shape:
        // 1.5s partials to a 10s cap = passes at 1.5,3,4.5,6,7.5,9 (six) + finalize.
        let m = CostModel {
            overhead_secs: 0.1,
            marginal_rtf: 0.2,
        };
        let cand = LiveCandidate::partials(1.5, 10.0);
        let mut expected = 0.0;
        let mut t = 1.5;
        while t < 10.0 - 0.001 {
            expected += m.predict_secs(t);
            t += 1.5;
        }
        expected += m.predict_secs(10.0);
        approx(cand.projected_load(&m), expected / 10.0);
    }

    #[test]
    fn verdict_bands() {
        assert_eq!(verdict(0.5), Verdict::Ok);
        assert_eq!(verdict(0.84), Verdict::Ok);
        assert_eq!(verdict(0.85), Verdict::Tight);
        assert_eq!(verdict(0.99), Verdict::Tight);
        assert_eq!(verdict(1.0), Verdict::FallsBehind);
        assert_eq!(verdict(2.3), Verdict::FallsBehind);
    }

    #[test]
    fn choose_picks_snappiest_passing() {
        // Fast machine: even the snappy config clears OK.
        let m = CostModel {
            overhead_secs: 0.02,
            marginal_rtf: 0.05,
        };
        let ladder = [
            LiveCandidate::partials(1.5, 10.0),
            LiveCandidate::partials(2.5, 5.0),
            LiveCandidate::partials_off(3.0),
        ];
        let choice = choose(&m, &ladder).unwrap();
        assert_eq!(choice.candidate, ladder[0]);
        assert_eq!(choice.verdict, Verdict::Ok);
    }

    #[test]
    fn choose_skips_to_first_config_under_threshold() {
        // Mid machine: the snappiest config falls behind, so choose skips it and
        // returns the next one in ladder order that clears OK — not necessarily
        // the safest, just the snappiest that keeps up.
        let m = CostModel {
            overhead_secs: 0.3,
            marginal_rtf: 0.45,
        };
        let ladder = [
            LiveCandidate::partials(1.5, 10.0),
            LiveCandidate::partials(2.5, 5.0),
            LiveCandidate::partials_off(3.0),
        ];
        // Precondition: index 0 must not keep up, index 1 must.
        assert!(ladder[0].projected_load(&m) >= OK_THRESHOLD);
        assert!(ladder[1].projected_load(&m) < OK_THRESHOLD);

        let choice = choose(&m, &ladder).unwrap();
        assert_eq!(choice.candidate, ladder[1]);
        assert_eq!(choice.verdict, Verdict::Ok);
        assert!(choice.projected_load < OK_THRESHOLD);
    }

    #[test]
    fn choose_falls_back_to_lowest_load_when_none_pass() {
        // Weak machine: nothing clears OK; pick the lowest-load candidate and
        // surface its (non-Ok) verdict so the caller knows to escalate.
        let m = CostModel {
            overhead_secs: 1.0,
            marginal_rtf: 0.9,
        };
        let ladder = [
            LiveCandidate::partials(1.5, 10.0),
            LiveCandidate::partials_off(5.0),
            LiveCandidate::partials_off(3.0),
        ];
        let choice = choose(&m, &ladder).unwrap();
        let min_load = ladder
            .iter()
            .map(|c| c.projected_load(&m))
            .fold(f64::INFINITY, f64::min);
        approx(choice.projected_load, min_load);
        assert_ne!(choice.verdict, Verdict::Ok);
    }

    #[test]
    fn choose_empty_ladder_is_none() {
        let m = CostModel {
            overhead_secs: 0.1,
            marginal_rtf: 0.2,
        };
        assert!(choose(&m, &[]).is_none());
    }
}
