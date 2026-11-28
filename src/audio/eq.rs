//! RBJ Audio EQ Cookbook biquads.  Per sample, so the EQ adds no latency.

use std::f32::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqSlot {
    Pre = 0,
    Post = 1,
}

impl EqSlot {
    pub const ALL: [EqSlot; 2] = [EqSlot::Pre, EqSlot::Post];

    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandKind {
    LowShelf,
    Bell,
    HighShelf,
}

pub const MAX_GAIN_DB: f32 = 12.0;
/// Q: low is wide, high is narrow.
pub const MIN_Q: f32 = 0.3;
pub const MAX_Q: f32 = 6.0;
const DEFAULT_Q: f32 = 1.0;
/// Butterworth shelf: gentlest slope, no resonant bump.
const SHELF_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

pub struct BandSpec {
    pub name: &'static str,
    pub kind: BandKind,
    pub default_hz: f32,
    pub min_hz: f32,
    pub max_hz: f32,
    /// Restored by FLAT and double-click.
    pub default_q: f32,
    /// Guitar role, shown as hover text.
    pub role: &'static str,
}

pub const BANDS: [BandSpec; 5] = [
    BandSpec {
        name: "Low",
        kind: BandKind::LowShelf,
        default_hz: 100.0,
        min_hz: 40.0,
        max_hz: 250.0,
        default_q: SHELF_Q,
        role: "Body and boom. Cut it to tighten the low end.",
    },
    BandSpec {
        name: "Low-mid",
        kind: BandKind::Bell,
        default_hz: 300.0,
        min_hz: 150.0,
        max_hz: 600.0,
        default_q: DEFAULT_Q,
        role: "Warmth, or mud if there is too much.",
    },
    BandSpec {
        name: "Mid",
        kind: BandKind::Bell,
        default_hz: 800.0,
        min_hz: 500.0,
        max_hz: 2_000.0,
        default_q: DEFAULT_Q,
        role: "How much the guitar cuts through the mix.",
    },
    BandSpec {
        name: "High-mid",
        kind: BandKind::Bell,
        default_hz: 2_500.0,
        min_hz: 1_500.0,
        max_hz: 5_000.0,
        default_q: DEFAULT_Q,
        role: "Attack, bite and presence.",
    },
    BandSpec {
        name: "High",
        kind: BandKind::HighShelf,
        default_hz: 6_000.0,
        min_hz: 4_000.0,
        max_hz: 12_000.0,
        default_q: SHELF_Q,
        role: "Air and sparkle, or fizz.",
    },
];

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EqBand {
    pub on: bool,
    pub hz: f32,
    pub gain_db: f32,
    /// Shelf slope as well as bell width.
    pub q: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EqSettings {
    pub enabled: bool,
    pub bands: [EqBand; 5],
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            bands: std::array::from_fn(|index| EqBand {
                on: true,
                hz: BANDS[index].default_hz,
                gain_db: 0.0,
                q: BANDS[index].default_q,
            }),
        }
    }
}

impl EqSettings {
    /// Keeps `enabled`.
    pub fn flat(self) -> Self {
        Self {
            enabled: self.enabled,
            ..Self::default()
        }
    }

    pub fn sanitized(mut self) -> Self {
        for (band, spec) in self.bands.iter_mut().zip(BANDS.iter()) {
            band.hz = finite_or(band.hz, spec.default_hz).clamp(spec.min_hz, spec.max_hz);
            band.gain_db = finite_or(band.gain_db, 0.0).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
            band.q = finite_or(band.q, spec.default_q).clamp(MIN_Q, MAX_Q);
        }
        self
    }

    pub fn is_active(&self) -> bool {
        self.enabled
            && self
                .bands
                .iter()
                .any(|band| band.on && band.gain_db.abs() > 0.01)
    }

    pub fn response_db(&self, hz: f32, sample_rate: u32) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        (0..self.bands.len())
            .map(|index| self.band_response_db(index, hz, sample_rate))
            .sum()
    }

    pub fn band_response_db(&self, index: usize, hz: f32, sample_rate: u32) -> f32 {
        let Some(band) = self.bands.get(index) else {
            return 0.0;
        };
        if !self.enabled || !band.on {
            return 0.0;
        }
        Biquad::design(BANDS[index].kind, band, sample_rate).response_db(hz, sample_rate)
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// Coefficients with `a0` divided out.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    fn design(kind: BandKind, band: &EqBand, sample_rate: u32) -> Self {
        let rate = sample_rate.max(1) as f32;
        // Stay safely below Nyquist on low-rate devices.
        let hz = band.hz.min(rate * 0.45);
        if band.gain_db.abs() < 0.01 {
            return Self::IDENTITY;
        }
        let a = 10f32.powf(band.gain_db / 40.0);
        let w0 = 2.0 * PI * hz / rate;
        let (sin, cos) = w0.sin_cos();
        // Shelves share the bell Q.  ~0.7 stays Butterworth; lower is softer
        // and higher bumps the corner.
        let q = band.q.clamp(MIN_Q, MAX_Q);
        let alpha = sin / (2.0 * q);
        let (b0, b1, b2, a0, a1, a2) = match kind {
            BandKind::Bell => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            BandKind::LowShelf => {
                let root = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + root),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - root),
                    (a + 1.0) + (a - 1.0) * cos + root,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - root,
                )
            }
            BandKind::HighShelf => {
                let root = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + root),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - root),
                    (a + 1.0) - (a - 1.0) * cos + root,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - root,
                )
            }
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    fn response_db(&self, hz: f32, sample_rate: u32) -> f32 {
        let w = 2.0 * PI * hz / sample_rate.max(1) as f32;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let power = (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im);
        10.0 * power.max(1e-12).log10()
    }
}

/// Transposed direct form II state.
#[derive(Clone, Copy, Default)]
struct State {
    z1: f32,
    z2: f32,
}

impl State {
    #[inline]
    fn run(&mut self, c: &Biquad, x: f32) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }
}

pub(crate) struct Equalizer {
    settings: EqSettings,
    sample_rate: u32,
    coefficients: [Biquad; 5],
    left: [State; 5],
    right: [State; 5],
    active: bool,
}

impl Equalizer {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let mut eq = Self {
            settings: EqSettings::default(),
            sample_rate,
            coefficients: [Biquad::IDENTITY; 5],
            left: [State::default(); 5],
            right: [State::default(); 5],
            active: false,
        };
        eq.update(EqSettings::default(), sample_rate);
        eq
    }

    /// Keeps filter memory, so a live tweak does not click.
    pub(crate) fn update(&mut self, settings: EqSettings, sample_rate: u32) {
        if settings == self.settings
            && sample_rate == self.sample_rate
            && self.active == settings.is_active()
        {
            return;
        }
        self.settings = settings;
        self.sample_rate = sample_rate;
        self.active = settings.is_active();
        for (index, spec) in BANDS.iter().enumerate() {
            let band = &settings.bands[index];
            self.coefficients[index] = if settings.enabled && band.on {
                Biquad::design(spec.kind, band, sample_rate)
            } else {
                Biquad::IDENTITY
            };
        }
        if !self.active {
            self.left = [State::default(); 5];
            self.right = [State::default(); 5];
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    #[inline]
    pub(crate) fn process(&mut self, mut left: f32, mut right: f32) -> (f32, f32) {
        for index in 0..5 {
            let c = &self.coefficients[index];
            if *c == Biquad::IDENTITY {
                continue;
            }
            left = self.left[index].run(c, left);
            right = self.right[index].run(c, right);
        }
        (left, right)
    }
}

#[cfg(test)]
mod tests;
