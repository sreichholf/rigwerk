//! The measured-loopback calibration: the app's side of the state machine.
//!
//! The audio side (the click, the capture buffer and the correlator) lives in
//! `audio::calibration`; this only decides when a measurement may run, arms
//! it, and turns the correlation into the stored [`MeasuredLatency`].  The
//! measurement needs the bridge live with both devices open: a preview opens
//! no hardware, and a stopped bridge has no callback to duck or to hear the
//! click.
//!
//! Nothing here changes a setting.  The duck and the click live in the mixer,
//! which ramps the mix back as soon as the measurement leaves its running
//! states.  The audio side's shared transitions are compare-and-exchange, and
//! `poll_calibration` clears any measurement state the app is not tracking,
//! so a cancel racing a callback cannot leave the clock output ducked.

use crate::app::LogTone;
use std::time::{Duration, Instant};

use crate::app::latency::{LatencyConditions, LoopbackKind, MeasuredLatency};
use crate::app::RigWerkApp;
use crate::audio::{self, analyze_channels, CalibrationFailure, CalibrationState, EngineState};
/// How long a measurement may take before the app gives up.  The capture
/// window is shorter; this covers the input starting and any callback stall.
pub(crate) const CALIBRATION_TIMEOUT: Duration = Duration::from_secs(4);

/// The calibration panel's state, kept apart from the audio side.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CalibrationPhase {
    /// The panel is closed or showing the last result.
    Idle,
    /// A measurement is armed or running.
    Measuring {
        started: Instant,
        conditions: LatencyConditions,
        /// How the loop was closed when this measurement started; changing
        /// the tab mid-capture must not relabel the result.
        loopback: LoopbackKind,
    },
    /// The last measurement completed; the result is in `measured_latency`.
    Done,
    /// The last attempt failed, with the reason to show and retry from.
    Failed { reason: String },
}

/// The panel's own state.  Not persisted.
pub(crate) struct CalibrationUi {
    pub(crate) open: bool,
    pub(crate) phase: CalibrationPhase,
    /// How the loop is closed, stored with a result.
    pub(crate) loopback: LoopbackKind,
    /// Scratch for the captured loopback, one buffer per channel, reused
    /// between measurements so the UI thread does not allocate every time.
    pub(crate) capture_l: Vec<f32>,
    pub(crate) capture_r: Vec<f32>,
}

impl Default for CalibrationUi {
    fn default() -> Self {
        Self {
            open: false,
            phase: CalibrationPhase::Idle,
            loopback: LoopbackKind::Cable,
            capture_l: Vec::new(),
            capture_r: Vec::new(),
        }
    }
}

impl CalibrationUi {
    pub(crate) fn is_measuring(&self) -> bool {
        matches!(self.phase, CalibrationPhase::Measuring { .. })
    }
}

impl RigWerkApp {
    /// Why a measurement cannot start right now, or `None` when it can.
    pub(crate) fn calibration_blocker(&self) -> Option<String> {
        if self.is_preview() {
            return Some(
                "Preview opens no audio hardware, so there is no loop to measure.".to_owned(),
            );
        }
        if self.engine.state != EngineState::Running {
            return Some(
                "Start the bridge first: the loopback needs the input and output open.".to_owned(),
            );
        }
        if self.snapshot.input_rate == 0 || self.snapshot.output_rate == 0 {
            return Some(
                "The bridge has not reported both rates yet. Wait until it is live and try again."
                    .to_owned(),
            );
        }
        if self.calibration.is_measuring() {
            return Some("A measurement is already running.".to_owned());
        }
        None
    }

    /// Arms a measurement with the click for the running output rate.
    pub(crate) fn start_calibration(&mut self) {
        // The app is not measuring, so any state the audio side still holds
        // is a stray from a cancelled or timed-out attempt.  Clear it before
        // "already running" could block every retry.
        if !self.calibration.is_measuring()
            && self.engine.calibration_state() != CalibrationState::Idle
        {
            self.engine.cancel_calibration();
        }
        if self.calibration.is_measuring() {
            return;
        }
        if let Some(reason) = self.calibration_blocker() {
            self.calibration.phase = CalibrationPhase::Failed { reason };
            return;
        }
        let conditions = self.latency_conditions();
        let loopback = self.calibration.loopback;
        let click = audio::click_waveform(conditions.output_rate);
        match self.engine.arm_calibration(
            &click,
            conditions.input_rate,
            conditions.input_buffer,
            conditions.output_step,
        ) {
            Ok(()) => {
                self.calibration.capture_l.clear();
                self.calibration.capture_r.clear();
                self.calibration.phase = CalibrationPhase::Measuring {
                    started: Instant::now(),
                    conditions,
                    loopback,
                };
                self.push_log(
                    "Measuring the loopback: the mix is muted and a short click plays. Keep the loopback connected.",
                    LogTone::Amber);
            }
            Err(reason) => {
                self.calibration.phase = CalibrationPhase::Failed { reason };
            }
        }
    }

    /// Cancels a running measurement (or closes a failed one).  The last
    /// stored result is kept.
    pub(crate) fn cancel_calibration(&mut self) {
        self.engine.cancel_calibration();
        if self.calibration.is_measuring() {
            self.push_log("Latency measurement cancelled.", LogTone::Muted);
        }
        self.calibration.phase = CalibrationPhase::Idle;
    }

    /// Advances a running measurement.  Called once per tick from `logic`,
    /// after the engine polled the bridge.
    pub(crate) fn poll_calibration(&mut self) {
        let CalibrationPhase::Measuring { started, .. } = self.calibration.phase.clone() else {
            // Nothing of ours is running.  A state left by a race is cleared
            // here too, so it cannot keep the clock output ducked.
            if self.engine.calibration_state() != CalibrationState::Idle {
                self.cancel_calibration();
            }
            return;
        };
        if self.engine.state != EngineState::Running {
            // The session this measurement was armed in is gone; nothing was
            // forced, but the click can never come back.
            self.abort_calibration("The bridge stopped before the click came back.");
            return;
        }
        match self.engine.calibration_state() {
            CalibrationState::Ready => self.finish_calibration(),
            CalibrationState::Idle => {
                self.abort_calibration(
                    "The measurement ended before the capture window filled (the bridge or a device may have stopped).",
                );
            }
            _ if started.elapsed() >= CALIBRATION_TIMEOUT => {
                self.abort_calibration(
                    "No click came back within four seconds. Check that the loopback reaches the chosen input, then try again.",
                );
            }
            _ => {}
        }
    }

    /// Correlates the finished capture and stores the result.
    fn finish_calibration(&mut self) {
        let CalibrationPhase::Measuring {
            conditions,
            loopback,
            ..
        } = self.calibration.phase.clone()
        else {
            return;
        };
        // A setting change restarts the bridge (and would have tripped the
        // state check), but compare the rig anyway so a result is never
        // stored for hardware it was not measured on.  A device rename is not
        // a different rig.
        if !conditions.same_rig(&self.latency_conditions()) {
            self.abort_calibration(
                "The bridge settings changed during the measurement, so the result was discarded. Try again.",
            );
            return;
        }
        let captured = self.engine.calibration_capture(
            &mut self.calibration.capture_l,
            &mut self.calibration.capture_r,
        );
        let capture_start_ns = self.engine.calibration_capture_start_ns();
        let capture_end_ns = self.engine.calibration_capture_end_ns();
        // A dropped input block leaves fewer frames than the wall time says,
        // and the delay would silently come out short.
        let expected_ns = captured as f64 / conditions.input_rate as f64 * 1e9;
        let elapsed_ns = capture_end_ns.saturating_sub(capture_start_ns) as f64;
        let tolerance_ns =
            2.0 * conditions.input_buffer as f64 / conditions.input_rate as f64 * 1e9;
        if capture_start_ns == 0
            || capture_end_ns == 0
            || (elapsed_ns - expected_ns).abs() > tolerance_ns
        {
            self.fail_calibration(CalibrationFailure::Timing);
            return;
        }
        let template = audio::click_waveform(conditions.input_rate);
        let analysis = analyze_channels(
            &self.calibration.capture_l[..captured],
            &self.calibration.capture_r[..captured],
            &template,
            conditions.input_rate,
        );
        let emitted_ns = self.engine.calibration_emitted_ns();
        self.engine.cancel_calibration();
        match analysis {
            Ok(analysis) => {
                let Some((latency_ms, samples)) = audio::round_trip_ms(
                    analysis.peak,
                    capture_start_ns,
                    emitted_ns,
                    conditions.input_rate,
                ) else {
                    self.fail_calibration(CalibrationFailure::OutOfWindow);
                    return;
                };
                let measured = MeasuredLatency {
                    latency_ms,
                    samples,
                    input_id: conditions.input_id.clone(),
                    input_name: conditions.input_name.clone(),
                    output_id: conditions.output_id.clone(),
                    output_name: conditions.output_name.clone(),
                    input_rate: conditions.input_rate,
                    output_rate: conditions.output_rate,
                    input_buffer: conditions.input_buffer,
                    output_step: conditions.output_step,
                    output_mode: conditions.output_mode.clone(),
                    measured_at: unix_seconds(),
                    confidence: analysis.score,
                    loopback,
                };
                self.push_log(
                    format!(
                        "Measured {:.1} ms loopback ({} samples at {:.1} kHz, {} loopback, {:.0}% confidence).",
                        measured.latency_ms,
                        measured.samples,
                        measured.input_rate as f32 / 1000.0,
                        measured.loopback.label(),
                        measured.confidence * 100.0
                    ),
                    LogTone::Teal);
                self.measured_latency = Some(measured);
                self.calibration.phase = CalibrationPhase::Done;
            }
            Err(failure) => self.fail_calibration(failure),
        }
    }

    fn fail_calibration(&mut self, failure: CalibrationFailure) {
        // Never leave the audio side holding the measurement (and the duck)
        // after a failure it caused.
        self.engine.cancel_calibration();
        let reason = match failure {
            CalibrationFailure::Clipped => "The click came back clipped at full scale. Lower the level at the interface's output (or the microphone preamp) and try again; RigWerk's own faders do not change the click.".to_owned(),
            CalibrationFailure::TooQuiet { level_db } => format!(
                "The capture never rose above {level_db:.0} dBFS. The loopback may not reach the chosen input: check the cable or microphone and its gain."
            ),
            CalibrationFailure::NoPeak { score } => format!(
                "The click did not come back as itself (correlation {:.0}%). Check that the cable or microphone reaches the chosen input, not another device.",
                score * 100.0
            ),
            CalibrationFailure::TooShort => "The capture window was too short for the click. Try again.".to_owned(),
            CalibrationFailure::OutOfWindow => "The click came back outside the 1.2 second window, or the timing did not make sense. A longer loop needs a direct path.".to_owned(),
            CalibrationFailure::Timing => "The capture lost input blocks (its elapsed time does not match the frames it holds), so the delay would be understated. Try again with a lighter load or a larger input buffer.".to_owned(),
        };
        self.push_log(
            format!("Latency measurement failed: {reason}"),
            LogTone::Amber,
        );
        self.calibration.phase = CalibrationPhase::Failed { reason };
    }

    /// Stops a measurement the app cannot finish and says why.
    fn abort_calibration(&mut self, reason: &str) {
        self.engine.cancel_calibration();
        self.push_log(
            format!("Latency measurement stopped: {reason}"),
            LogTone::Amber,
        );
        self.calibration.phase = CalibrationPhase::Failed {
            reason: reason.to_owned(),
        };
    }

    /// How far the capture has filled, 0..1, for the panel's progress line.
    pub(crate) fn calibration_progress(&self) -> f32 {
        let (captured, limit) = self.engine.calibration_progress();
        if limit == 0 {
            0.0
        } else {
            (captured as f32 / limit as f32).clamp(0.0, 1.0)
        }
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
