//! Per-sample control ramps and underrun fade helpers for the output mixer.

use super::route_frame;

/// How long an underrun takes to fade the held frame to silence, and how long
/// playback takes to fade back in once the cushion has refilled.  Long enough
/// that neither edge clicks, short enough not to smear the note around it.
pub(crate) const DROPOUT_FADE_SECONDS: f32 = 0.0025;

/// A control change is ramped over this long, whatever block size the host
/// hands over: long enough to hide the step, short enough to feel immediate.
pub(crate) const CONTROL_RAMP_SECONDS: f32 = 0.006;

/// One control value moving linearly to its target, one sample at a time.
/// The remaining step is kept between callbacks, so a ramp that spans several
/// blocks keeps its slope instead of restarting at every block boundary.
#[derive(Clone, Copy)]
pub(crate) struct Ramp {
    current: f32,
    target: f32,
    step: f32,
}

impl Ramp {
    pub(crate) fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
            step: 0.0,
        }
    }

    /// Adopts a value without moving: used for the first block of a mixer.
    pub(crate) fn snap(&mut self, target: f32) {
        self.current = target;
        self.target = target;
        self.step = 0.0;
    }

    /// Points the ramp at `target`; the move takes `samples` samples.  A
    /// target that did not change leaves a ramp that is still moving alone.
    pub(crate) fn set_target(&mut self, target: f32, samples: f32) {
        if target == self.target {
            return;
        }
        self.target = target;
        let step = if samples > 0.0 {
            (target - self.current) / samples
        } else {
            0.0
        };
        // A step below one representable unit would never arrive.  Snap that
        // (inaudible) remainder instead of moving forever.
        if target == self.current || step == 0.0 || self.current + step == self.current {
            self.current = target;
            self.step = 0.0;
        } else {
            self.step = step;
        }
    }

    /// Moves one sample and returns the value to apply.  Once the target is
    /// reached it returns exactly the target for the rest of the block, so a
    /// settled ramp adds no noise to the audio.
    pub(crate) fn next(&mut self) -> f32 {
        if self.current != self.target {
            let next = self.current + self.step;
            if (self.step > 0.0 && next >= self.target) || (self.step < 0.0 && next <= self.target)
            {
                self.current = self.target;
                self.step = 0.0;
            } else {
                self.current = next;
            }
        }
        self.current
    }
}

/// All per-sample control ramps of the output stage.  Routing is linear, so a
/// route change is eight gains moving between 0 and 1; channel enable and mono
/// work the same way, and mute, monitor and the master level fold into one
/// effective output gain so they compose without a step.
pub(super) struct MixRamps {
    route: [[Ramp; 4]; 2],
    output_enabled: [Ramp; 2],
    mono: Ramp,
    gain: Ramp,
}

impl MixRamps {
    pub(super) fn new() -> Self {
        Self {
            route: [[Ramp::new(0.0); 4]; 2],
            output_enabled: [Ramp::new(1.0); 2],
            mono: Ramp::new(0.0),
            gain: Ramp::new(1.0),
        }
    }

    /// First block: adopt the live settings directly, so starting a bridge
    /// neither fades in nor jumps to them.
    pub(super) fn snap(&mut self, routes: &[[f32; 4]; 2], enabled: [f32; 2], mono: f32, gain: f32) {
        for (row, targets) in self.route.iter_mut().zip(routes) {
            for (ramp, target) in row.iter_mut().zip(targets) {
                ramp.snap(*target);
            }
        }
        for (ramp, target) in self.output_enabled.iter_mut().zip(enabled) {
            ramp.snap(target);
        }
        self.mono.snap(mono);
        self.gain.snap(gain);
    }

    pub(super) fn set_targets(
        &mut self,
        routes: &[[f32; 4]; 2],
        enabled: [f32; 2],
        mono: f32,
        gain: f32,
        samples: f32,
    ) {
        for (row, targets) in self.route.iter_mut().zip(routes) {
            for (ramp, target) in row.iter_mut().zip(targets) {
                ramp.set_target(*target, samples);
            }
        }
        for (ramp, target) in self.output_enabled.iter_mut().zip(enabled) {
            ramp.set_target(target, samples);
        }
        self.mono.set_target(mono, samples);
        self.gain.set_target(gain, samples);
    }

    /// Routing, channel enable and mono for one input frame.  This is applied
    /// before the EQ and the plugin chain, where those controls always acted.
    pub(super) fn next_input(&mut self, left: f32, right: f32) -> [f32; 4] {
        let mut routes = [[0.0f32; 4]; 2];
        for (input, row) in routes.iter_mut().enumerate() {
            for (tap, value) in row.iter_mut().enumerate() {
                *value = self.route[input][tap].next();
            }
        }
        let mut out = route_frame(left, right, &routes);
        out[0] *= self.output_enabled[0].next();
        out[1] *= self.output_enabled[1].next();
        let mono = self.mono.next();
        if mono != 0.0 {
            let sum = (out[0] + out[1]) * 0.5;
            for value in out.iter_mut() {
                *value += (sum - *value) * mono;
            }
        }
        out
    }

    /// The effective master gain for one output frame.  Mute, monitor and the
    /// level slider all fold into this value, so any combination of them can
    /// change without a discontinuity.
    pub(super) fn next_output_gain(&mut self) -> f32 {
        self.gain.next()
    }
}

/// One step of the underrun fade-out, snapping to 0 instead of stopping a
/// rounding error above it.
pub(crate) fn fade_down(gain: f32, step: f32) -> f32 {
    let next = gain - step;
    if next < step * 0.5 {
        0.0
    } else {
        next
    }
}

/// One step of the fade-in, snapping to 1 the same way.
pub(crate) fn fade_up(gain: f32, step: f32) -> f32 {
    let next = gain + step;
    if next > 1.0 - step * 0.5 {
        1.0
    } else {
        next
    }
}
