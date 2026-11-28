//! A control change keeps its slope across blocks.  A step smaller than
//! one ulp snaps.

use super::route_frame;

/// Long enough that a dropout edge does not click, short enough not to
/// smear the note.
pub(crate) const DROPOUT_FADE_SECONDS: f32 = 0.0025;

/// Long enough to hide a control step, short enough to feel immediate.
pub(crate) const CONTROL_RAMP_SECONDS: f32 = 0.006;

/// The remaining step is kept across callbacks, so a ramp does not restart
/// at a block boundary.
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

    /// The first block adopts the live value instead of fading to it.
    pub(crate) fn snap(&mut self, target: f32) {
        self.current = target;
        self.target = target;
        self.step = 0.0;
    }

    /// A target that did not change leaves a ramp that is still moving.
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

    pub(crate) fn is_settled_at(&self, value: f32) -> bool {
        self.current == value && self.target == value
    }

    /// Lands on the target exactly, so a settled ramp adds no noise.
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

/// Route, enable and mono.  Mute and the master live on the mixer.
pub(super) struct PathRamps {
    route: [[Ramp; 4]; 2],
    output_enabled: [Ramp; 2],
    mono: Ramp,
}

impl PathRamps {
    pub(super) fn new() -> Self {
        Self {
            route: [[Ramp::new(0.0); 4]; 2],
            output_enabled: [Ramp::new(1.0); 2],
            mono: Ramp::new(0.0),
        }
    }

    /// The first block adopts the live settings instead of fading to them.
    pub(super) fn snap(&mut self, routes: &[[f32; 4]; 2], enabled: [f32; 2], mono: f32) {
        for (row, targets) in self.route.iter_mut().zip(routes) {
            for (ramp, target) in row.iter_mut().zip(targets) {
                ramp.snap(*target);
            }
        }
        for (ramp, target) in self.output_enabled.iter_mut().zip(enabled) {
            ramp.snap(target);
        }
        self.mono.snap(mono);
    }

    pub(super) fn set_targets(
        &mut self,
        routes: &[[f32; 4]; 2],
        enabled: [f32; 2],
        mono: f32,
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
    }

    /// Routing, enable and mono for one frame, before the EQ and the chain.
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
}

pub(crate) fn fade_down(gain: f32, step: f32) -> f32 {
    let next = gain - step;
    if next < step * 0.5 {
        0.0
    } else {
        next
    }
}

pub(crate) fn fade_up(gain: f32, step: f32) -> f32 {
    let next = gain + step;
    if next > 1.0 - step * 0.5 {
        1.0
    } else {
        next
    }
}
