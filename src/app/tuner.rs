//! The tuner page's state: persisted settings, the throttled analysis poll and
//! the auto-mute rule.  All DSP lives in [`crate::audio::tuner`]; this module
//! only decides when it runs and with which settings.

use crate::app::LogTone;
use std::time::{Duration, Instant};

use crate::app::settings::Preferences;
use crate::app::{Page, RigWerkApp};
use crate::audio::{
    tuning_index, AudioEngine, EngineState, TunerAnalysis, TunerChannel, TunerFrame,
    TunerTolerance, Tuning, DEFAULT_PRESET_ID, PRESETS, TAP_COPY_FRAMES,
};
/// The A4 reference the page offers.  415 is the bottom of the baroque range
/// and 466 the top of the classical one.
pub(crate) const REFERENCE_MIN_HZ: f32 = 415.0;
pub(crate) const REFERENCE_MAX_HZ: f32 = 466.0;
pub(crate) const DEFAULT_REFERENCE_HZ: f32 = 440.0;

/// The analysis is cheap but not free: one poll per 40 ms keeps the page at
/// 25 Hz and leaves the frame loop alone.
const POLL_INTERVAL: Duration = Duration::from_millis(40);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TunerDisplay {
    Strobe,
    Needle,
}

impl TunerDisplay {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Needle,
            _ => Self::Strobe,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Strobe => 0,
            Self::Needle => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TunerListen {
    Single,
    All,
}

impl TunerListen {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::All,
            _ => Self::Single,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Single => 0,
            Self::All => 1,
        }
    }
}

pub(crate) struct TunerState {
    /// Index into [`PRESETS`]; `None` is chromatic and has no string rows.
    pub(crate) preset: Option<usize>,
    pub(crate) reference_hz: f32,
    pub(crate) channel: TunerChannel,
    pub(crate) display: TunerDisplay,
    pub(crate) listen: TunerListen,
    pub(crate) auto_mute: bool,
    pub(crate) tolerance: TunerTolerance,
    analysis: TunerAnalysis,
    /// Interleaved L/R copy of the latest tap window.
    scratch: Vec<f32>,
    last_poll: Instant,
    started: Instant,
}

impl TunerState {
    /// Reads the persisted page settings.  A screenshot run pins the demo
    /// (Drop C, strobe, all strings, auto-mute on), Sum input and a 440 Hz
    /// reference so the image is reproducible; the tolerance follows the
    /// stored choice.
    pub(crate) fn new(preferences: &Preferences, screenshot: bool) -> Self {
        let preset = if screenshot {
            tuning_index("drop-c")
        } else {
            tuning_index(preferences.tuner_tuning.trim()).filter(|index| *index < PRESETS.len())
        };
        let mut listen = if screenshot {
            TunerListen::All
        } else {
            TunerListen::from_u8(preferences.tuner_listen)
        };
        // All strings has no meaning without a preset; a stale value from a
        // save made in another mode falls back to the single readout.
        if preset.is_none() {
            listen = TunerListen::Single;
        }
        let display = if screenshot {
            TunerDisplay::Strobe
        } else {
            TunerDisplay::from_u8(preferences.tuner_display)
        };
        let mut state = Self {
            preset,
            reference_hz: if screenshot {
                DEFAULT_REFERENCE_HZ
            } else {
                sanitise_reference(preferences.tuner_reference_hz)
            },
            channel: if screenshot {
                TunerChannel::Sum
            } else {
                TunerChannel::from_u8(preferences.tuner_channel)
            },
            display,
            listen,
            auto_mute: if screenshot {
                true
            } else {
                preferences.tuner_auto_mute
            },
            tolerance: TunerTolerance::from_u8(preferences.tuner_tolerance),
            analysis: TunerAnalysis::new(),
            scratch: vec![0.0; TAP_COPY_FRAMES * 2],
            last_poll: Instant::now(),
            started: Instant::now(),
        };
        if screenshot {
            state
                .analysis
                .demo(state.preset(), state.reference_hz, state.tolerance);
        }
        state
    }

    pub(crate) fn preset(&self) -> Option<&'static Tuning> {
        self.preset.and_then(|index| PRESETS.get(index))
    }

    pub(crate) fn preset_id(&self) -> &'static str {
        self.preset()
            .map(|tuning| tuning.id)
            .unwrap_or(DEFAULT_PRESET_ID)
    }

    /// Picks a preset (or chromatic).  The analysis starts over so a reading
    /// taken against the old string table cannot leak into the new one.
    pub(crate) fn set_preset(&mut self, index: Option<usize>) {
        let index = index.filter(|index| *index < PRESETS.len());
        if self.preset == index {
            return;
        }
        self.preset = index;
        if index.is_none() {
            self.listen = TunerListen::Single;
        }
        self.analysis.clear();
    }

    /// The A4 reference, kept inside the range the page can show.
    pub(crate) fn set_reference(&mut self, hz: f32) {
        self.reference_hz = sanitise_reference(hz);
    }

    /// The mute layer the page owns.  It never touches the master mute, and
    /// it releases by itself when the bridge leaves Running/Preview or the
    /// page is left.
    pub(crate) fn auto_muted(&self, page: Page, state: EngineState) -> bool {
        page == Page::Tuner
            && self.auto_mute
            && matches!(state, EngineState::Running | EngineState::Preview)
    }

    /// One poll of the current page.  Only the Tuner page pays for analysis;
    /// anywhere else clears the frame so a stale needle is never painted.
    pub(crate) fn poll(
        &mut self,
        engine: &AudioEngine,
        app_rate: u32,
        page: Page,
        screenshot: bool,
        elapsed: f32,
    ) {
        if screenshot {
            self.analysis
                .demo(self.preset(), self.reference_hz, self.tolerance);
            return;
        }
        if page != Page::Tuner {
            self.analysis.clear();
            return;
        }
        if self.last_poll.elapsed() < POLL_INTERVAL {
            return;
        }
        self.last_poll = Instant::now();
        let preset = self.preset();
        let channel = self.channel;
        let reference_hz = self.reference_hz;
        let tolerance = self.tolerance;
        match engine.state {
            EngineState::Running => {
                let frames = engine.copy_tap(&mut self.scratch, TAP_COPY_FRAMES);
                let rate = engine.active_rate();
                let rate = if rate == 0 { app_rate } else { rate };
                self.analysis.analyse(
                    &self.scratch,
                    frames,
                    rate,
                    preset,
                    channel,
                    reference_hz,
                    tolerance,
                );
            }
            EngineState::Preview => {
                // The wobble just needs a monotonic clock; the page's own
                // start keeps it moving if the caller's clock is reset.
                let elapsed = elapsed.max(self.started.elapsed().as_secs_f32());
                self.analysis
                    .preview(preset, reference_hz, tolerance, elapsed);
            }
            EngineState::Idle
            | EngineState::Starting
            | EngineState::Stopping
            | EngineState::Error => self.analysis.clear(),
        }
    }

    pub(crate) fn frame(&self) -> &TunerFrame {
        self.analysis.frame()
    }
}

pub(crate) fn sanitise_reference(hz: f32) -> f32 {
    if hz.is_finite() {
        hz.clamp(REFERENCE_MIN_HZ, REFERENCE_MAX_HZ)
    } else {
        DEFAULT_REFERENCE_HZ
    }
}

impl RigWerkApp {
    /// Logs the tuner's mute layer opening and closing.  Runs after `meters`
    /// has refreshed the engine state.
    pub(crate) fn log_tuner_page(&mut self) {
        let page = self.page;
        if page == self.last_page {
            return;
        }
        if page == Page::Tuner {
            if self.tuner.auto_muted(page, self.engine.state) {
                self.push_log("Tuner open — output muted by auto-mute.", LogTone::Amber);
            } else {
                self.push_log("Tuner open.", LogTone::Teal);
            }
        } else if self.last_page == Page::Tuner
            && self.tuner.auto_muted(self.last_page, self.engine.state)
        {
            // Auto-mute is layered on the master mute.  Leaving the page
            // releases only this layer, so a master mute that is still on
            // keeps the output silent.
            if self.mute {
                self.push_log("Tuner closed — output still muted.", LogTone::Muted);
            } else {
                self.push_log("Tuner closed — output restored.", LogTone::Muted);
            }
        }
        self.last_page = page;
    }
}

#[cfg(test)]
mod tests;
