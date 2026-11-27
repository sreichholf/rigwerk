//! The *Tight* engine: a time-domain shifter in the style of a delay-line
//! "whammy" or "drop" pedal, for single notes, power chords and chugs at a
//! few ms (the delay setting, 4 … 40 ms).
//!
//! The input runs through a delay line; up to three read heads read it at
//! `r` samples per sample (four-point Catmull-Rom interpolation), so the
//! pitch moves by `r` with no frame and no resolution limit.  A head that
//! reads slower than the input falls behind (a drop), one that reads faster
//! catches up, so now and then the playing head is replaced by a new one a
//! jump `J` nearer (or further), crossfaded.  The splice is
//! period-synchronous: a YIN period estimate on a decimated copy (about
//! 8 kHz, every hop, 20 Hz … 1.3 kHz; its difference function spread over
//! three quarters of the hop, a few lags a sample, so no block carries the
//! whole of it) makes `J` a whole number of periods, and a normalised
//! correlation of the two heads' last samples at full rate refines it, so
//! the two heads are in phase and a note does not warble.
//! The range reaches the common period of a low power chord (drop A's A5
//! is 27.5 Hz, an eight-string's F#5 23 Hz) and of most barre chords (an
//! octave or two under the root), so those splice in phase too.  A signal
//! without a clear period takes a jump of at least 30 ms while it rings (a
//! longer delay leaves more room, so it splices less often: a few splices a
//! second at 4 ms), and the range about `D` while it falls fast (a
//! palm-muted chug), so a chug's tail is not read late into the gap before
//! the next.  The chorusing left on a chord no period fits is slow at the
//! default and slower with a longer delay.
//!
//! The crossfade's shape follows the correlation `ρ` of the two heads at
//! the splice: a raised cosine whose gains are divided by
//! `sqrt(a² + b² + 2ρab)`, so heads in phase (`ρ = 1`) sum to one and do
//! not dip, and uncorrelated ones (`ρ ≤ 0`) keep their power.
//!
//! Delay.  The playing head's delay sweeps the jump around `D` (the
//! reported latency, [`spec`]: the delay setting), `D ± J/2`, but never
//! nearer the input than 0.25 ms on a drop, or on a rise than 0.25 ms plus
//! what the outgoing head reads through the crossfade (`r − 1` times its
//! length).  A long jump (a low period, the ringing jump) is pushed up by
//! that: a ringing chord an octave up at 4 ms sweeps about 15 … 45 ms.  An onset re-anchors the heads: a new
//! head is placed `D` behind the input and reads the attack at a ratio of 1
//! for 3 ms of input past the onset, then glides to `r` over 5 ms, so every
//! attack is heard at exactly the reported latency and keeps its own
//! length, whatever the shift (a pick's timing matters more than its
//! pitch).  An onset is a fast power envelope jumping four times over a
//! slow one and twice over the loudest of the last 45 ms (so a low note's
//! own peaks, once a period, never count), on the whole signal or on its
//! top (above the decimation's low-pass: the pick, which the last note's
//! tail lacks, so it is called within a millisecond).  No periodic splice
//! starts while the power is rising or until the new head has read the
//! attack: a head nearer the input than `D` would play the pick early.  At
//! a ratio of 1 the playing head sits at exactly `D`: the output is the
//! input, delayed.  Between attacks the head drifts about `D` before it
//! splices, so a longer `D` splices less often: a ringing chord choruses
//! less.
//!
//! Real-time safety: [`TightState::new`] allocates for 192 kHz and the
//! longest delay; `configure`, `set_delay`, `clear` and `tick` never
//! allocate.  The output depends on the samples since the last clear and
//! on where the clear fell on the hop grid (`due`), never on the block
//! size.

use std::f64::consts::PI;

/// How long a shift change glides.
const GLIDE_SECONDS: f32 = 0.020;
/// How often the period is estimated, at 48 kHz.
const HOP: usize = 128;
/// The nearest a head may read to the input, at 48 kHz (0.25 ms).
const MIN_DELAY: usize = 12;
/// The decimation of the period estimate at 48 kHz (to 8 kHz); it scales
/// with the rate like the sizes.
const DECIMATION: usize = 6;
/// YIN on the decimated signal: the window, the lag range (20 Hz …
/// 1.3 kHz) and the dip that counts as a clear period.  20 Hz takes in the
/// common period of a low power chord (drop A's A5 is 27.5 Hz, an
/// eight-string's F#5 23 Hz), so its splices keep both notes in phase.
const YIN_WINDOW: usize = 192;
const YIN_MAX_LAG: usize = 400;
const YIN_MIN_LAG: usize = 6;
const YIN_THRESHOLD: f32 = 0.2;
/// Hops a period is kept after the estimate loses it.
const PERIOD_HOLD: u32 = 4;
/// The decimated ring, a power of two above `YIN_WINDOW + YIN_MAX_LAG`.
const DECIMATED_LEN: usize = 1024;
/// The delay line at 192 kHz (170 ms).  At a 40 ms `D` the furthest read is
/// a drop's head held to `max_delay` (85 ms, past its latest splice point,
/// `D + J/2` ≈ 80 ms) plus its drift through a 20 ms crossfade: about
/// 95 ms.
pub(super) const BUFFER_LEN: usize = 1 << 15;
/// The longest crossfade of a periodic splice, seconds.
const MAX_CROSSFADE_SECONDS: f64 = 0.010;
/// A splice without a period (a ringing chord, noise): the least jump and
/// the longest crossfade, seconds.  The jump grows with the room a longer
/// delay leaves, so a longer delay splices less often.
const CHORD_JUMP_SECONDS: f64 = 0.030;
const CHORD_CROSSFADE_SECONDS: f64 = 0.020;
/// A chord takes the long jump only while it rings: its level falls by less
/// than this, dB per second (a palm-muted chug falls by hundreds).  Faster,
/// it takes the short one, so the sweep stays near `D` and a chug's tail
/// does not reach into the gap before the next.
const RINGING_DB_PER_SECOND: f32 = 60.0;
/// An anchor's crossfade, seconds, and how long before the detection the
/// onset is taken to start.
const ANCHOR_CROSSFADE_SECONDS: f64 = 0.001;
const ANCHOR_LAG_SECONDS: f64 = 0.0005;
/// How far an anchor may move to land in phase with the playing head,
/// seconds.
const ANCHOR_ALIGN_SECONDS: f64 = 0.0005;
/// After an onset: how much of the input is read at a ratio of 1, and how
/// long the ratio then glides to the shift, seconds.
const ATTACK_HOLD_SECONDS: f64 = 0.003;
const ATTACK_GLIDE_SECONDS: f64 = 0.005;
/// After an onset, how much of the input no periodic splice may start in.
const GUARD_SECONDS: f64 = 0.008;
/// The onset detector: the fast and the slow power envelope, the jump of
/// the first over the second that counts, an absolute floor (about
/// −80 dBFS) and the hold-off.
const FAST_SECONDS: f32 = 0.001;
const SLOW_SECONDS: f32 = 0.020;
const ONSET_RATIO: f32 = 4.0;
const ONSET_FLOOR: f32 = 1e-8;
const HOLDOFF_SECONDS: f32 = 0.030;
/// While the fast envelope is over this many times the slow one (and over
/// the window's peak, below) the input is rising (an attack the detector
/// has not yet called): no periodic splice starts, so a new head nearer
/// the input cannot read it early.
const RISING_RATIO: f32 = 2.0;
/// An onset must also be this far over the loudest the fast envelope was
/// in the last `PEAK_WINDOW` blocks of 1 ms but the newest `PEAK_SKIP`: a
/// low note's or a power chord's own peaks, once a period (down to about
/// 22 Hz), never count.
const PEAK_RATIO: f32 = 2.0;
const PEAK_WINDOW: usize = 45;
const PEAK_SKIP: usize = 2;
const PEAK_BLOCKS: usize = 64;
/// The widest a periodic splice's refinement searches.  It only binds on
/// jumps of many short periods, where half a period, all an in-phase point
/// needs, is well inside it; it keeps a long *Delay* from searching
/// thousands of positions in one sample.
const MAX_SEARCH_SECONDS: f64 = 0.002;
/// The correlation segment of a splice's refinement, at 48 kHz (2 ms).
const MATCH_LEN: usize = 96;

/// Tight's sizes at one rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Spec {
    /// The span the period estimate hears.
    pub(super) window: usize,
    /// How often the period is estimated.
    pub(super) hop: usize,
    /// `D`: what the stage reports and delays by, the delay every attack
    /// is heard at and the centre of the sweep.
    pub(super) latency: u32,
}

impl Spec {
    /// The samples a cleared state needs before its output has settled.
    pub(super) fn warm_up(self) -> usize {
        self.window + self.latency as usize + self.hop
    }
}

/// The sizes at `rate` and `delay_ms`: the analysis window and hop are the
/// 48 kHz ones times [`rate_scale`]; the latency is `delay_ms` at `rate`,
/// so the delay setting is exact at every rate.
pub(super) fn spec(rate: u32, delay_ms: f32) -> Spec {
    let scale = rate_scale(rate);
    let latency = (delay_ms * rate.max(1) as f32 / 1000.0).round() as usize;
    Spec {
        window: (YIN_WINDOW + YIN_MAX_LAG + 1) * DECIMATION * scale,
        hop: HOP * scale,
        latency: latency as u32,
    }
}

/// The power of two nearest `rate / 48000` in octaves, clamped to 1 … 4,
/// so the window and hop keep about the same milliseconds; a slower rate
/// takes the 48 kHz sizes, a faster one the 192 kHz sizes.  The latency is
/// not scaled: [`spec`] computes it from the rate.
pub(super) fn rate_scale(rate: u32) -> usize {
    // sqrt(2) · 48 kHz and sqrt(8) · 48 kHz are the octave midpoints.
    if rate < 67_882 {
        1
    } else if rate < 135_765 {
        2
    } else {
        4
    }
}

/// One read head.
#[derive(Clone, Copy, Debug, Default)]
struct Head {
    /// The (fractional) input index it reads next.
    pos: f64,
    gain: f32,
    /// Its gain when the running crossfade started.
    from: f32,
}

pub(super) struct TightState {
    pub(super) spec: Spec,
    /// Set when a non-finite value cleared the state; the shifter fades in.
    pub(super) faulted: bool,
    /// The sizes at the current rate, in samples.
    latency: f64,
    min_delay: f64,
    max_delay: f64,
    decimation: usize,
    max_crossfade: usize,
    chord_jump: f64,
    chord_crossfade: usize,
    hop_seconds: f32,
    anchor_crossfade: usize,
    anchor_lag: f64,
    anchor_align: usize,
    attack_hold: f64,
    attack_glide: f64,
    guard: f64,
    match_len: usize,
    holdoff_len: u32,
    glide_samples: f32,
    /// The delay line and the index of the newest sample.
    buf: Vec<f32>,
    t: i64,
    /// The decimated copy: two one-pole low-passes, the phase, the ring and
    /// its count.
    coefficient: f32,
    z1: f32,
    z2: f32,
    phase: usize,
    decimated: Vec<f32>,
    count: usize,
    difference: Vec<f32>,
    /// The estimate in progress: the span it hears (a linear copy of the
    /// ring), the next lag to compute (0: none), its lag range, the
    /// samples since it started and the samples it is spread over.
    yin: Vec<f32>,
    yin_next: usize,
    yin_lags: usize,
    yin_tick: usize,
    yin_spread: usize,
    /// The decimated rate, Hz, and the rate, Hz.
    decimated_rate: f64,
    fs: f64,
    /// The period in input samples, 0 for none, and hops it is still kept.
    period: f64,
    hold: u32,
    heads: [Head; 3],
    /// The head that plays (or fades in).
    current: usize,
    /// The running crossfade: samples done, its length and the correlation
    /// of its heads.
    fade_pos: usize,
    fade_len: usize,
    fade_rho: f32,
    /// No periodic splice until the playing head reads past this index.
    guard_until: f64,
    /// The attack after an onset: whether it is being read, the index the
    /// playing head reads it at a ratio of 1 to, and the glide's step.
    attacking: bool,
    attack_until: f64,
    attack_step: f64,
    /// The slow envelope at the last hop, and how fast it falls, dB per
    /// second (smoothed; negative while it rises).
    last_slow: f32,
    decay: f32,
    /// The onset detector: the whole signal, and what the decimation's
    /// low-pass leaves out (above about 2 kHz: a pick, not the tail of the
    /// last note), their coefficients, the 1 ms block and the hold-off.
    broad: Envelope,
    high: Envelope,
    fast_coeff: f32,
    slow_coeff: f32,
    peak_len: usize,
    holdoff: u32,
    /// The shift in octaves, its target and its step per sample, the ratio,
    /// and the ratio the heads read at (the ratio but in an attack).
    octaves: f32,
    octaves_target: f32,
    octaves_step: f32,
    ratio: f64,
    rate: f64,
}

impl TightState {
    /// Off the audio thread: allocates for 192 kHz, configures for `rate`
    /// and `delay_ms`.
    pub(super) fn new(rate: u32, delay_ms: f32) -> Self {
        let mut state = Self {
            spec: spec(rate, delay_ms),
            faulted: false,
            latency: 0.0,
            min_delay: 0.0,
            max_delay: 0.0,
            decimation: 1,
            max_crossfade: 1,
            chord_jump: 0.0,
            chord_crossfade: 1,
            hop_seconds: 0.0,
            anchor_crossfade: 1,
            anchor_lag: 0.0,
            anchor_align: 0,
            attack_hold: 0.0,
            attack_glide: 1.0,
            guard: 0.0,
            match_len: 1,
            holdoff_len: 0,
            glide_samples: 1.0,
            buf: vec![0.0; BUFFER_LEN],
            t: 0,
            coefficient: 0.0,
            z1: 0.0,
            z2: 0.0,
            phase: 0,
            decimated: vec![0.0; DECIMATED_LEN],
            count: 0,
            difference: vec![0.0; YIN_MAX_LAG + 2],
            yin: vec![0.0; YIN_WINDOW + YIN_MAX_LAG],
            yin_next: 0,
            yin_lags: YIN_MAX_LAG,
            yin_tick: 0,
            yin_spread: 1,
            decimated_rate: 8000.0,
            fs: 48_000.0,
            period: 0.0,
            hold: 0,
            heads: [Head::default(); 3],
            current: 0,
            fade_pos: 0,
            fade_len: 0,
            fade_rho: 1.0,
            guard_until: 0.0,
            attacking: false,
            attack_until: 0.0,
            attack_step: 0.0,
            last_slow: 0.0,
            decay: 0.0,
            broad: Envelope::default(),
            high: Envelope::default(),
            fast_coeff: 0.0,
            slow_coeff: 0.0,
            peak_len: 1,
            holdoff: 0,
            octaves: 0.0,
            octaves_target: 0.0,
            octaves_step: 0.0,
            ratio: 1.0,
            rate: 1.0,
        };
        state.configure(rate, delay_ms);
        state
    }

    /// Takes the sizes of `rate` and `delay_ms` and clears the state.  No
    /// allocation.
    pub(super) fn configure(&mut self, rate: u32, delay_ms: f32) {
        let scale = rate_scale(rate);
        let fs = rate.max(1) as f64;
        self.spec = spec(rate, delay_ms);
        self.latency = self.spec.latency as f64;
        self.min_delay = (MIN_DELAY * scale) as f64;
        self.max_delay = (BUFFER_LEN / 2) as f64;
        self.decimation = DECIMATION * scale;
        self.max_crossfade = (MAX_CROSSFADE_SECONDS * fs) as usize;
        self.fs = fs;
        self.decimated_rate = fs / self.decimation as f64;
        self.chord_jump = (CHORD_JUMP_SECONDS * fs).round();
        self.chord_crossfade = (CHORD_CROSSFADE_SECONDS * fs) as usize;
        self.hop_seconds = (HOP * scale) as f32 / fs as f32;
        self.yin_spread = (HOP * scale * 3 / 4).max(1);
        self.anchor_crossfade = (ANCHOR_CROSSFADE_SECONDS * fs) as usize;
        self.anchor_lag = (ANCHOR_LAG_SECONDS * fs).round();
        self.anchor_align = (ANCHOR_ALIGN_SECONDS * fs) as usize;
        self.attack_hold = (ATTACK_HOLD_SECONDS * fs).round();
        self.attack_glide = (ATTACK_GLIDE_SECONDS * fs).max(1.0);
        self.guard = GUARD_SECONDS * fs;
        self.match_len = MATCH_LEN * scale;
        self.holdoff_len = (HOLDOFF_SECONDS * rate as f32) as u32;
        self.peak_len = (rate as usize / 1000).max(1);
        self.glide_samples = (GLIDE_SECONDS * rate as f32).max(1.0);
        let corner = 0.25 * fs / self.decimation as f64;
        self.coefficient = (1.0 - (-2.0 * PI * corner / fs).exp()) as f32;
        self.fast_coeff = (1.0 / (FAST_SECONDS * rate as f32)).min(1.0);
        self.slow_coeff = (1.0 / (SLOW_SECONDS * rate as f32)).min(1.0);
        self.clear();
    }

    /// Takes `delay_ms` at the same rate: one head at the new `D`, no
    /// splice running.  The delay line, the estimate and the envelopes are
    /// kept.  No allocation.
    pub(super) fn set_delay(&mut self, delay_ms: f32) {
        self.spec.latency = spec(self.fs as u32, delay_ms).latency;
        self.latency = self.spec.latency as f64;
        self.place_head();
    }

    /// Empties the delay line and the estimate and puts one head at `D`.
    /// No allocation.
    pub(super) fn clear(&mut self) {
        self.buf.fill(0.0);
        self.decimated.fill(0.0);
        self.t = 0;
        self.z1 = 0.0;
        self.z2 = 0.0;
        self.phase = 0;
        self.count = 0;
        self.period = 0.0;
        self.hold = 0;
        self.yin_next = 0;
        self.place_head();
        self.broad = Envelope::default();
        self.high = Envelope::default();
        self.last_slow = 0.0;
        self.decay = 0.0;
        self.holdoff = 0;
    }

    /// One head `D` behind the next sample, no crossfade, no attack.
    fn place_head(&mut self) {
        self.heads = [Head::default(); 3];
        self.current = 0;
        self.heads[0] = Head {
            pos: (self.t + 1) as f64 - self.latency,
            gain: 1.0,
            from: 1.0,
        };
        self.fade_pos = 0;
        self.fade_len = 0;
        self.fade_rho = 1.0;
        self.guard_until = f64::NEG_INFINITY;
        self.attacking = false;
        self.rate = self.ratio;
    }

    /// What the state preallocates, bytes (for the budget test).
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        (self.buf.capacity()
            + self.decimated.capacity()
            + self.difference.capacity()
            + self.yin.capacity())
            * 4
    }

    /// The samples fed since the last clear.
    #[cfg(test)]
    pub(super) fn samples(&self) -> i64 {
        self.t
    }

    /// The nearest to the input and the furthest back an audible head
    /// read on the last tick, samples; `None` when none played.
    #[cfg(test)]
    pub(super) fn read_delay(&self) -> Option<(f64, f64)> {
        let t = self.t as f64;
        self.heads
            .iter()
            .filter(|head| head.gain != 0.0)
            .map(|head| t - (head.pos - self.rate))
            .fold(None, |span, delay| match span {
                None => Some((delay, delay)),
                Some((near, far)) => Some((f64::min(near, delay), f64::max(far, delay))),
            })
    }

    /// The period estimate, input samples (0: none).
    #[cfg(test)]
    pub(super) fn period(&self) -> f64 {
        self.period
    }

    /// Whether a crossfade is running.
    #[cfg(test)]
    pub(super) fn fading(&self) -> bool {
        !self.idle()
    }

    fn fault(&mut self) {
        self.clear();
        self.faulted = true;
    }

    /// Points the shift at `octaves`, gliding per sample, or jumping there
    /// when `snap`.
    pub(super) fn set_octaves(&mut self, octaves: f32, snap: bool) {
        if snap {
            self.octaves = octaves;
            self.octaves_target = octaves;
            self.octaves_step = 0.0;
            self.ratio = (octaves as f64).exp2();
            self.attacking = false;
            self.rate = self.ratio;
        } else if octaves != self.octaves_target {
            self.octaves_target = octaves;
            self.octaves_step = (octaves - self.octaves).abs() / self.glide_samples;
        }
    }

    /// One input sample.  Returns the shifted output and the dry input,
    /// both `D` samples late.  `due` estimates the period on this sample.
    #[inline]
    pub(super) fn tick(&mut self, x: f32, due: bool) -> (f32, f32) {
        let x = if x.is_finite() {
            x
        } else {
            self.fault();
            0.0
        };
        let mask = (BUFFER_LEN - 1) as i64;
        self.t += 1;
        self.buf[(self.t & mask) as usize] = x;
        self.decimate(x);
        if due {
            self.start_estimate();
            self.track_decay();
        }
        self.step_estimate();
        if self.octaves != self.octaves_target {
            let diff = self.octaves_target - self.octaves;
            if diff.abs() <= self.octaves_step {
                self.octaves = self.octaves_target;
            } else {
                self.octaves += self.octaves_step.copysign(diff);
            }
            self.ratio = (self.octaves as f64).exp2();
        }
        let onset = self.detect(x);
        // A finite input can still square to infinity.
        if !(self.broad.slow + self.high.slow).is_finite() {
            self.fault();
            return (0.0, 0.0);
        }
        self.steer(onset);
        self.follow_rate();

        // The crossfade, then the heads.
        if self.fade_pos < self.fade_len {
            self.fade_pos += 1;
            let (incoming, outgoing) = if self.fade_pos >= self.fade_len {
                (1.0, 0.0)
            } else {
                let c = 0.5 - 0.5 * (PI * self.fade_pos as f64 / self.fade_len as f64).cos();
                let (a, b) = (c as f32, 1.0 - c as f32);
                let norm = (a * a + b * b + 2.0 * self.fade_rho * a * b).sqrt();
                (a / norm, b / norm)
            };
            for (i, head) in self.heads.iter_mut().enumerate() {
                head.gain = if i == self.current {
                    incoming
                } else {
                    head.from * outgoing
                };
            }
        }
        let mut wet = 0.0f32;
        for head in &mut self.heads {
            if head.gain != 0.0 {
                wet += head.gain * read(&self.buf, head.pos);
            }
            head.pos += self.rate;
        }
        let dry = self.buf[((self.t - self.latency as i64) & mask) as usize];
        if !wet.is_finite() {
            self.fault();
            return (0.0, 0.0);
        }
        (wet, dry)
    }

    /// The ratio the heads read at: the shift's, but in an attack 1 until
    /// the playing head has read `attack_hold` past the onset, then a glide
    /// to the shift's.
    #[inline]
    fn follow_rate(&mut self) {
        if !self.attacking {
            self.rate = self.ratio;
            return;
        }
        if self.heads[self.current].pos < self.attack_until {
            self.rate = 1.0;
            return;
        }
        let diff = self.ratio - self.rate;
        if diff.abs() <= self.attack_step {
            self.rate = self.ratio;
            self.attacking = false;
        } else {
            self.rate += self.attack_step.copysign(diff);
        }
    }

    /// Feeds the decimated copy.
    #[inline]
    fn decimate(&mut self, x: f32) {
        self.z1 += self.coefficient * (x - self.z1);
        self.z2 += self.coefficient * (self.z1 - self.z2);
        self.phase += 1;
        if self.phase >= self.decimation {
            self.phase = 0;
            self.decimated[self.count & (DECIMATED_LEN - 1)] = self.z2;
            self.count += 1;
        }
    }

    /// How fast the slow envelope falls, once a hop.
    fn track_decay(&mut self) {
        let now = self.broad.slow.max(1e-20);
        let last = self.last_slow.max(1e-20);
        let rate = 10.0 * (last / now).log10() / self.hop_seconds;
        self.last_slow = self.broad.slow;
        self.decay += 0.5 * (rate - self.decay);
    }

    /// The onset detector: on the whole signal or its top, a fast power
    /// envelope jumping over a slow one and over the recent peaks.
    #[inline]
    fn detect(&mut self, x: f32) -> bool {
        let top = x - self.z2;
        let (fast, slow, len) = (self.fast_coeff, self.slow_coeff, self.peak_len);
        self.broad.feed(x * x, fast, slow, len);
        self.high.feed(top * top, fast, slow, len);
        if self.holdoff > 0 {
            self.holdoff -= 1;
            return false;
        }
        if self.broad.onset() || self.high.onset() {
            self.holdoff = self.holdoff_len;
            return true;
        }
        false
    }

    /// Starts YIN on the newest decimated samples (a chord, noise or
    /// silence has no clear period): copies the span it hears and leaves
    /// the difference function to [`Self::step_estimate`], a few lags a
    /// sample, so no block carries the whole of it.
    fn start_estimate(&mut self) {
        // One still running (not on the schedule, which spreads each over
        // three quarters of a hop) finishes first.
        while self.yin_next != 0 {
            self.yin_tick = self.yin_spread;
            self.step_estimate();
        }
        let max_lag = YIN_MAX_LAG;
        let span = YIN_WINDOW + max_lag;
        if self.count <= span {
            return self.conclude(None);
        }
        let base = self.count - span;
        for (i, slot) in self.yin[..span].iter_mut().enumerate() {
            *slot = self.decimated[(base + i) & (DECIMATED_LEN - 1)];
        }
        let window = &self.yin[max_lag..span];
        if window.iter().map(|x| x * x).sum::<f32>() < 1e-9 {
            return self.conclude(None);
        }
        self.yin_next = 1;
        self.yin_lags = max_lag;
        self.yin_tick = 0;
    }

    /// The lags of the running estimate due by this sample; the period
    /// once the last is in.
    #[inline]
    fn step_estimate(&mut self) {
        if self.yin_next == 0 {
            return;
        }
        self.yin_tick += 1;
        let lags = self.yin_lags;
        let due = (lags * self.yin_tick).div_ceil(self.yin_spread).min(lags);
        let window = &self.yin[lags..lags + YIN_WINDOW];
        for tau in self.yin_next..=due {
            let older = lags - tau;
            self.difference[tau] = squared_difference(window, &self.yin[older..older + YIN_WINDOW]);
        }
        self.yin_next = due + 1;
        if self.yin_next > lags {
            self.yin_next = 0;
            let found = self.first_dip(lags);
            self.conclude(found);
        }
    }

    /// Takes an estimate's period, or keeps the last for a few hops.
    fn conclude(&mut self, found: Option<f64>) {
        match found {
            Some(lag) => {
                self.period = lag * self.decimation as f64;
                self.hold = PERIOD_HOLD;
            }
            None if self.hold > 0 => self.hold -= 1,
            None => self.period = 0.0,
        }
    }

    /// The first dip of the cumulative mean normalised difference under the
    /// threshold, walked to its bottom and refined by a parabola on the
    /// difference, in decimated samples.
    fn first_dip(&self, max_lag: usize) -> Option<f64> {
        let d = &self.difference;
        let mut running = 0.0f32;
        let mut values = [1.0f32; YIN_MAX_LAG + 1];
        for (tau, value) in values.iter_mut().enumerate().take(max_lag + 1).skip(1) {
            running += d[tau];
            if running > 0.0 {
                *value = d[tau] * tau as f32 / running;
            }
        }
        let mut tau = (YIN_MIN_LAG..=max_lag).find(|&tau| values[tau] < YIN_THRESHOLD)?;
        while tau < max_lag && values[tau + 1] < values[tau] {
            tau += 1;
        }
        let mut lag = tau as f64;
        if tau < max_lag {
            let (a, b, c) = (d[tau - 1] as f64, d[tau] as f64, d[tau + 1] as f64);
            let denom = a - 2.0 * b + c;
            if denom > 0.0 {
                lag += (0.5 * (a - c) / denom).clamp(-0.5, 0.5);
            }
        }
        Some(lag)
    }

    /// Starts an anchor on an onset, or a periodic splice when the playing
    /// head has drifted to the edge of its range.
    #[inline]
    fn steer(&mut self, onset: bool) {
        let r = self.ratio;
        let t = self.t as f64;
        let delay = t - self.heads[self.current].pos;
        if r == 1.0 {
            // At a ratio of 1 the playing head belongs at exactly D.
            if self.idle() && delay != self.latency {
                let target = t - self.latency;
                let (_, rho) = self.align(self.heads[self.current].pos, target, 0);
                self.start_fade(target, self.max_crossfade, rho);
            }
            return;
        }
        if onset {
            self.anchor(t);
            return;
        }
        if !self.idle() || self.attacking {
            return;
        }
        let rising = self.broad.rising() || self.high.rising();
        let range = self.latency - self.min_delay;
        let synced = self.period > 0.0;
        // Without a period the refinement may move the new head a
        // decimation step either way, so the sweep leaves that much room.
        let search = if synced {
            0.0
        } else {
            (self.decimation + 2) as f64
        };
        let jump = if synced {
            let limit = if r < 1.0 { 2.0 * range } else { range };
            let n = (limit / self.period).floor().max(1.0);
            n * self.period
        } else if self.decay < RINGING_DB_PER_SECOND {
            // A ringing chord has no period to scale the jump with, so the
            // room a longer delay leaves sets it: a longer delay splices
            // less often.  At the 4 ms default this is the 30 ms jump.
            self.chord_jump.max(if r < 1.0 {
                2.0 * range - search
            } else {
                range - search
            })
        } else if r < 1.0 {
            // The jump and its search span the range.
            2.0 * range - search
        } else {
            range - search
        };
        let interval = jump / (1.0 - r).abs();
        let longest = if synced {
            self.max_crossfade
        } else {
            self.chord_crossfade
        };
        let guarded = self.heads[self.current].pos < self.guard_until;
        let pos = self.heads[self.current].pos;
        if r < 1.0 {
            let high = (self.latency + 0.5 * jump).max(self.min_delay + jump + search);
            if delay < high || ((guarded || rising) && delay < self.max_delay) {
                return;
            }
            // The new head is `jump` nearer the input, refined so it is in
            // phase, and never nearer than the minimum.  Without a period it
            // lands at the bottom of the sweep, however far the head had
            // got (a chord that stops ringing comes back at once).
            let jump = if synced {
                jump
            } else {
                (delay - self.min_delay - search).floor().max(jump)
            };
            let fade = (jump.min(0.5 * interval) as usize).clamp(1, longest);
            let room = (delay - jump - self.min_delay).floor().max(0.0) as usize;
            let radius = if synced {
                self.radius(jump)
            } else {
                search as usize
            };
            let (target, rho) = self.align(pos, pos + jump, radius.min(room));
            self.start_fade(target, fade, rho);
        } else {
            let fade = (jump.min(0.5 * interval) as usize).clamp(1, longest);
            let low = (self.latency - 0.5 * jump).max(self.min_delay + (r - 1.0) * fade as f64);
            let forced = delay <= self.min_delay + (r - 1.0) * fade as f64;
            if delay > low || ((guarded || rising) && !forced) {
                return;
            }
            // An attack's glide can leave the head below `low`: the fade
            // ends before the outgoing head, still reading at `r`, passes
            // the minimum.
            let fade = fade.min(((delay - self.min_delay) / (r - 1.0)).floor().max(1.0) as usize);
            let room = (self.max_delay - delay - jump).floor().max(0.0) as usize;
            let radius = if synced {
                self.radius(jump)
            } else {
                search as usize
            };
            let (target, rho) = self.align(pos, pos - jump, radius.min(room));
            self.start_fade(target, fade, rho);
        }
    }

    /// How far a periodic splice's refinement searches: the period
    /// estimate's uncertainty, at most [`MAX_SEARCH_SECONDS`].
    fn radius(&self, jump: f64) -> usize {
        let n = (jump / self.period).round().max(1.0);
        let uncertainty = (n * self.decimation as f64 * 0.5).ceil() as usize + 2;
        uncertainty.min((MAX_SEARCH_SECONDS * self.fs) as usize)
    }

    /// Places a head `D` behind the input, moved by at most `anchor_align`
    /// to be in phase with the playing head, to read the attack at a ratio
    /// of 1; and keeps the periodic splices off the attack.
    fn anchor(&mut self, t: f64) {
        let lag = self.anchor_lag;
        let ideal = t - self.latency;
        let pos = self.heads[self.current].pos;
        let radius = if self.period > 0.0 {
            ((self.period * 0.5) as usize).min(self.anchor_align)
        } else {
            self.anchor_align
        };
        // Within the delay limits.
        let radius = radius.min((self.latency - self.min_delay).max(0.0) as usize);
        let (target, rho) = self.align(pos, ideal, radius);
        // The crossfade ends before the new head reaches the onset.
        let reach = (t - lag - target).floor() as usize;
        let fade = self.anchor_crossfade.min(reach.saturating_sub(1)).max(1);
        self.start_fade(target, fade, rho);
        self.guard_until = t - lag + self.guard;
        self.attacking = true;
        self.attack_until = t - lag + self.attack_hold;
        self.attack_step = (1.0 - self.ratio).abs() / self.attack_glide;
        self.rate = 1.0;
    }

    /// The position within `radius` samples of `target` whose last
    /// `match_len` samples best match those before `pos` (normalised
    /// correlation), with `pos`'s fraction, and that correlation; `target`
    /// itself when `radius` is 0, and 1 when the line is silent there.
    fn align(&self, pos: f64, target: f64, radius: usize) -> (f64, f32) {
        let mask = (BUFFER_LEN - 1) as i64;
        let a = pos.floor() as i64;
        let frac = pos - a as f64;
        let centre = (target - frac).round() as i64;
        let len = self.match_len as i64;
        let at = |i: i64| self.buf[(i & mask) as usize];
        let energy = |end: i64| (0..len).map(|i| at(end - i).powi(2)).sum::<f32>();
        let ea = energy(a);
        if ea < 1e-12 {
            return (target, 1.0);
        }
        // The `len` samples up to `end` as one slice, unless the ring
        // wraps inside them.
        let segment = |end: i64| {
            let (start, end) = ((end - len + 1) & mask, end & mask);
            (start <= end).then(|| &self.buf[start as usize..=end as usize])
        };
        let newest = segment(a);
        let radius = radius as i64;
        let mut best = (0i64, f32::NEG_INFINITY);
        let mut previous = f32::NEG_INFINITY;
        let mut before_best = f32::NEG_INFINITY;
        let mut after_best = f32::NEG_INFINITY;
        let mut eb = energy(centre - radius);
        for j in -radius..=radius {
            let c = centre + j;
            if j > -radius {
                // Slide the candidate's energy by one sample.
                eb += at(c).powi(2) - at(c - len).powi(2);
            }
            let cross = match (newest, segment(c)) {
                (Some(x), Some(y)) => dot(x, y),
                _ => (0..len).map(|i| at(a - i) * at(c - i)).sum(),
            };
            let score = cross / (ea * eb.max(1e-12)).sqrt();
            if score > best.1 {
                best = (j, score);
                before_best = previous;
                after_best = f32::NEG_INFINITY;
            } else if j == best.0 + 1 {
                after_best = score;
            }
            previous = score;
        }
        if radius == 0 {
            return (target, best.1.clamp(-1.0, 1.0));
        }
        let mut offset = 0.0;
        if before_best.is_finite() && after_best.is_finite() {
            let denom = before_best - 2.0 * best.1 + after_best;
            if denom < 0.0 {
                offset = (0.5 * (before_best - after_best) / denom).clamp(-0.5, 0.5) as f64;
            }
        }
        (
            (centre + best.0) as f64 + offset + frac,
            best.1.clamp(-1.0, 1.0),
        )
    }

    fn idle(&self) -> bool {
        self.fade_pos >= self.fade_len
    }

    /// Starts a head at `pos` and fades the others out over `len` samples,
    /// shaped for heads that correlate by `rho`.  A free head is used; with
    /// none free, the quietest one that does not play is taken over.
    fn start_fade(&mut self, pos: f64, len: usize, rho: f32) {
        let next = (0..self.heads.len())
            .filter(|&i| i != self.current)
            .min_by(|&a, &b| self.heads[a].gain.total_cmp(&self.heads[b].gain))
            .unwrap_or(0);
        for head in &mut self.heads {
            head.from = head.gain;
        }
        self.heads[next] = Head {
            pos,
            gain: 0.0,
            from: 0.0,
        };
        self.current = next;
        self.fade_pos = 0;
        self.fade_len = len.max(1);
        self.fade_rho = rho.clamp(0.0, 1.0);
    }
}

/// One band of the onset detector: the fast and the slow power envelope,
/// each 1 ms block's loudest fast envelope (a ring), the running block's
/// and its position, and the loudest of the window.
#[derive(Clone, Copy, Debug)]
struct Envelope {
    fast: f32,
    slow: f32,
    peaks: [f32; PEAK_BLOCKS],
    block: f32,
    pos: usize,
    count: usize,
    held: f32,
}

impl Default for Envelope {
    fn default() -> Self {
        Self {
            fast: 0.0,
            slow: 0.0,
            peaks: [0.0; PEAK_BLOCKS],
            block: 0.0,
            pos: 0,
            count: 0,
            held: 0.0,
        }
    }
}

impl Envelope {
    /// One sample's power `p`; `len` samples make a block.
    #[inline]
    fn feed(&mut self, p: f32, fast: f32, slow: f32, len: usize) {
        self.fast += (p - self.fast) * fast;
        self.slow += (p - self.slow) * slow;
        self.block = self.block.max(self.fast);
        self.pos += 1;
        if self.pos >= len {
            self.push();
        }
    }

    /// Closes a block and takes the window's loudest, the newest
    /// `PEAK_SKIP` blocks left out.
    fn push(&mut self) {
        self.peaks[self.count % PEAK_BLOCKS] = self.block;
        self.count += 1;
        self.block = 0.0;
        self.pos = 0;
        let from = self.count.saturating_sub(PEAK_WINDOW);
        let to = self.count.saturating_sub(PEAK_SKIP);
        self.held = (from..to)
            .map(|i| self.peaks[i % PEAK_BLOCKS])
            .fold(0.0, f32::max);
    }

    #[inline]
    fn onset(&self) -> bool {
        self.fast > ONSET_RATIO * self.slow + ONSET_FLOOR && self.fast > PEAK_RATIO * self.held
    }

    #[inline]
    fn rising(&self) -> bool {
        self.fast > RISING_RATIO * self.slow + ONSET_FLOOR && self.fast > self.held
    }
}

/// `Σ a·b` over two slices of one length, in eight running sums like
/// [`squared_difference`].
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let (a8, a_tail) = a.split_at(a.len() / 8 * 8);
    let (b8, b_tail) = b[..a.len()].split_at(a8.len());
    let mut sums = [0.0f32; 8];
    for (x, y) in a8.chunks_exact(8).zip(b8.chunks_exact(8)) {
        for i in 0..8 {
            sums[i] += x[i] * y[i];
        }
    }
    let tail: f32 = a_tail.iter().zip(b_tail).map(|(x, y)| x * y).sum();
    sums.iter().sum::<f32>() + tail
}

/// `Σ (a − b)²` over two slices of one length, in eight running sums so
/// the compiler vectorises it on any target (no `std::arch`; the scalar
/// tail adds what is left).
#[inline]
fn squared_difference(a: &[f32], b: &[f32]) -> f32 {
    let (a8, a_tail) = a.split_at(a.len() / 8 * 8);
    let (b8, b_tail) = b[..a.len()].split_at(a8.len());
    let mut sums = [0.0f32; 8];
    for (x, y) in a8.chunks_exact(8).zip(b8.chunks_exact(8)) {
        for i in 0..8 {
            let d = x[i] - y[i];
            sums[i] += d * d;
        }
    }
    let tail: f32 = a_tail
        .iter()
        .zip(b_tail)
        .map(|(x, y)| (x - y) * (x - y))
        .sum();
    sums.iter().sum::<f32>() + tail
}

/// Four-point Catmull-Rom interpolation of `buf` at `pos`; exact at a whole
/// index.
#[inline]
fn read(buf: &[f32], pos: f64) -> f32 {
    let mask = (buf.len() - 1) as i64;
    let i = pos.floor() as i64;
    let f = (pos - i as f64) as f32;
    let at = |k: i64| buf[(k & mask) as usize];
    let (ym, y0, y1, y2) = (at(i - 1), at(i), at(i + 1), at(i + 2));
    if f == 0.0 {
        return y0;
    }
    y0 + 0.5
        * f
        * (y1 - ym + f * (2.0 * ym - 5.0 * y0 + 4.0 * y1 - y2 + f * (3.0 * (y0 - y1) + y2 - ym)))
}
