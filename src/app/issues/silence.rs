//! The watch behind "Output went silent under low latency": RigWerk sends a
//! loud signal to the clock sink while PipeWire's monitor of that sink stays
//! quiet.  Pure and platform-neutral; the sink monitor that feeds it is
//! `audio::linux::sink_monitor`, and the time is passed in for tests.
//!
//! Guitar rests are common, so only a tick that is loud on the sending side
//! counts as evidence: a rest (both quiet) proves nothing either way, and
//! [`SILENCE_REST_RESET`] of rest drops the evidence gathered before it, so
//! loud moments minutes apart do not add up.  The
//! sent peak is the clock device's meter, after the gate, the map, the level
//! and the mutes, so a closed gate reads as a rest.  Master or device mute,
//! or no unmuted path reaching the clock device, disarms it outright.
//! Without monitor data (not Linux, no PipeWire sink, monitor not linked)
//! the watch is inert and never guesses.
//!
//! The sent peak is RigWerk's meter, before PipeWire's own stream volume:
//! a RigWerk stream muted or turned down in the desktop mixer looks the
//! same as a starved sink.  Recent underruns are what tell them apart: with
//! them the silence has to last [`SILENCE_RAISE_WITH_UNDERRUNS`], without
//! them [`SILENCE_RAISE_ALONE`], and the issue built on it is only an error
//! with them.  Any monitor signal resets that evidence, and
//! [`SILENCE_CLEAR_AFTER`] of monitor signal clears a raised watch.

use std::time::{Duration, Instant};

/// RigWerk counts as sending above this peak.
pub(crate) const SILENCE_SENT_DBFS: f32 = -40.0;
/// The sink's monitor counts as silent below this peak.
pub(crate) const SILENCE_MONITOR_DBFS: f32 = -70.0;
/// Loud-but-unheard time that raises the watch with recent underruns.
pub(crate) const SILENCE_RAISE_WITH_UNDERRUNS: Duration = Duration::from_millis(1500);
/// The same without underruns: rarer, so it needs more evidence.
pub(crate) const SILENCE_RAISE_ALONE: Duration = Duration::from_secs(3);
/// Monitor signal that clears a raised watch.
pub(crate) const SILENCE_CLEAR_AFTER: Duration = Duration::from_secs(1);
/// A rest (neither side loud) this long drops the gathered evidence.
pub(crate) const SILENCE_REST_RESET: Duration = Duration::from_secs(2);
/// The most one tick counts for, so a stalled frame does not raise it alone.
const MAX_STEP: Duration = Duration::from_millis(250);

/// One tick's readings.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SilenceInput {
    /// Low latency is engaged and the bridge runs.
    pub(crate) armed: bool,
    /// The peak RigWerk sends to the clock device (linear).
    pub(crate) sent: f32,
    /// The peak on the sink's monitor (linear), `None` without monitor data.
    pub(crate) monitor: Option<f32>,
    /// Master mute, the clock device muted, or no unmuted path reaching it.
    pub(crate) parked: bool,
    /// The clock output has a run of underruns that has not gone quiet.
    pub(crate) underruns: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SilenceWatch {
    silent: bool,
    /// Loud-but-unheard time gathered toward raising.
    unheard: Duration,
    /// Monitor-signal time gathered toward clearing.
    heard: Duration,
    /// Rest time since either side was last loud.
    rest: Duration,
    last: Option<Instant>,
}

impl SilenceWatch {
    pub(crate) fn update(&mut self, input: SilenceInput, now: Instant) {
        let step = self
            .last
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last))
            .min(MAX_STEP);
        self.last = Some(now);
        let Some(monitor) = input.monitor.filter(|_| input.armed && !input.parked) else {
            *self = Self {
                last: Some(now),
                ..Self::default()
            };
            return;
        };
        let loud = input.sent >= amplitude(SILENCE_SENT_DBFS);
        if monitor >= amplitude(SILENCE_MONITOR_DBFS) {
            self.unheard = Duration::ZERO;
            self.rest = Duration::ZERO;
            if self.silent {
                self.heard += step;
                if self.heard >= SILENCE_CLEAR_AFTER {
                    self.silent = false;
                    self.heard = Duration::ZERO;
                }
            }
        } else if loud {
            self.heard = Duration::ZERO;
            self.rest = Duration::ZERO;
            if !self.silent {
                self.unheard += step;
                let need = if input.underruns {
                    SILENCE_RAISE_WITH_UNDERRUNS
                } else {
                    SILENCE_RAISE_ALONE
                };
                if self.unheard >= need {
                    self.silent = true;
                    self.unheard = Duration::ZERO;
                }
            }
        } else {
            self.rest += step;
            if self.rest >= SILENCE_REST_RESET {
                self.unheard = Duration::ZERO;
            }
        }
    }

    /// Whether the sink is judged silent while RigWerk plays into it.
    pub(crate) fn silent(&self) -> bool {
        self.silent
    }
}

fn amplitude(dbfs: f32) -> f32 {
    10.0f32.powf(dbfs / 20.0)
}

#[cfg(test)]
mod tests;
