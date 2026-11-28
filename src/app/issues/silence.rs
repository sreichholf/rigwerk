//! Only a tick that is loud on the send side counts.  A rest drops the
//! evidence after [`SILENCE_REST_RESET`], so loud moments minutes apart
//! do not add.  The sent peak is the clock-output meter, before
//! PipeWire's stream volume, so a desktop mute looks like a starved sink.
//! Recent dropouts tell them apart.  Without monitor data the watch
//! never guesses.

use std::time::{Duration, Instant};

pub(crate) const SILENCE_SENT_DBFS: f32 = -40.0;
pub(crate) const SILENCE_MONITOR_DBFS: f32 = -70.0;
pub(crate) const SILENCE_RAISE_WITH_DROPOUTS: Duration = Duration::from_millis(1500);
/// Rarer than a dropout, so it needs more evidence.
pub(crate) const SILENCE_RAISE_ALONE: Duration = Duration::from_secs(3);
pub(crate) const SILENCE_CLEAR_AFTER: Duration = Duration::from_secs(1);
/// A rest this long drops the evidence, so loud moments minutes
/// apart do not add.
pub(crate) const SILENCE_REST_RESET: Duration = Duration::from_secs(2);
/// A stalled frame must not raise the watch by itself.
const MAX_STEP: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SilenceInput {
    /// Low latency is on and the bridge runs.
    pub(crate) armed: bool,
    /// Linear peak sent to the clock output.
    pub(crate) sent: f32,
    /// Linear peak on the sink monitor.  `None` without monitor data.
    pub(crate) monitor: Option<f32>,
    /// Master mute, the clock output muted, or no unmuted path reaching it.
    pub(crate) parked: bool,
    /// A dropout run that has not gone quiet.
    pub(crate) dropouts: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SilenceWatch {
    silent: bool,
    /// Loud-but-unheard time toward raising.
    unheard: Duration,
    /// Monitor-signal time toward clearing.
    heard: Duration,
    /// Since either side was last loud.
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
                let need = if input.dropouts {
                    SILENCE_RAISE_WITH_DROPOUTS
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

    pub(crate) fn silent(&self) -> bool {
        self.silent
    }
}

fn amplitude(dbfs: f32) -> f32 {
    10.0f32.powf(dbfs / 20.0)
}

#[cfg(test)]
mod tests;
