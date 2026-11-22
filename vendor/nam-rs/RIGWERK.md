# nam-rs in RigWerk

This is nam-rs 0.4.0 (tag `v0.4.0` of <https://github.com/OpenSauce/nam-rs>,
MIT, see `LICENSE` and `NOTICE`) with faster kernels.  The root `Cargo.toml`
uses it through `[patch.crates-io]`.  Drop the folder and the patch once an
upstream release has the same speed.

Everything in this folder, the changes below included, is under the
crate's MIT license (`LICENSE`), not RigWerk's GPL, so the changes can go
upstream as they are.  Only MIT-compatible code goes in here: no code from
RigWerk's own tree.

What changed, all under `src/`:

- `wavenet/conv.rs`: the history is one linear row per input channel, so a
  block is appended and the taps read straight out of it, with no ring
  staging per block.  The kernel computes 4 output channels by 8 time steps
  at once and keeps the sums in registers: full tiles read the input column
  straight from the row, with no branch and no copy, and the rest of a block
  runs in steps of 4, then 1.  (One loop for full and partial tiles made the
  compiler keep the sums on the stack, on x86 and aarch64 alike.)  Every
  output adds its bias and products in the per-sample order, so the result
  is bit-identical to 0.4.0.  A 1x1 convolution keeps no history and reads
  the block in place.
- `lstm/cell.rs`: the gate matrix is stored transposed, so `W·[x; h]`
  updates all gates from one column at a time.  It vectorizes and keeps the
  summation order, so the matrix product is bit-identical; the gates then
  use the new `tanh` and `sigmoid`.
- `wavenet/activation.rs`: `tanh` and `sigmoid` are built on a branch-free
  `e^x` (range reduction by `ln 2`, Taylor series to `x^7`), so they
  vectorize.  Within 3e-7 of the exact functions, like libm's `tanhf`.  No
  third-party approximation or coefficients: only a Cody–Waite split of
  `ln 2` (355/512 and the rest) and `1/k!`.  Activations run over whole
  rows (`apply_slice`).
- `wavenet/gating.rs`: gated and blended layers run each row in passes,
  with each activation chosen once per row, so they vectorize too.  Each
  sample sees the same operations as before, so they are bit-identical.
- AVX2 at run time (`conv.rs`, `activation.rs`, `gating.rs`): on an x86-64
  CPU with AVX2 the conv tiles (16 time steps instead of 8), the
  activations and the gating run a build compiled for AVX2.  Each call
  checks `is_x86_feature_detected!`, which the standard library answers
  from a cached bit after its first use.  AVX2 only, not FMA: a fused
  multiply-add rounds once where the portable code rounds twice, so without
  it both builds do the same operations in the same order and agree bit
  for bit.  Other CPUs, and every other architecture, run the portable
  build.  The AVX2 functions are `unsafe fn`, so the crate still builds on
  its `rust-version`, 1.74.

Against 0.4.0, a standard WaveNet runs about 2.5 times as fast in the
portable build and about 4.3 times with AVX2 (one x86 machine, whole
model, `opt-level = 3`, which RigWerk uses for this crate in every
profile).  A gated one gains about twice as much again from AVX2.  A
standard LSTM runs about 3.5 times as fast; one 16-wide step per sample,
it gains nothing from AVX2 and has no AVX2 build.  At opt-level 2 the conv
tiles do not vectorize and a WaveNet is slower than 0.4.0.  Only the new
`tanh` and `sigmoid` change the output: by at most 1.2e-5 on a standard
WaveNet driven near 0 dBFS, about −135 dBFS RMS.  The parity fixtures
against NAM Core and the Python trainer still pass at their 1e-5.

## Where the ideas come from

All of it is written for this crate; the ideas are standard ones.

- Run-time dispatch: one function body compiled twice, once with
  `#[target_feature(enable = "avx2")]`, picked after
  `is_x86_feature_detected!` (the Rust standard library's mechanism).
- Conv history: a linear buffer per channel with a write cursor, and the
  history copied back to the front when a block would run past the end.
  This is the scheme of NAM Core's `RingBuffer` (`NAM/ring_buffer.cpp`,
  "rewind"; MIT, already credited in `NOTICE`).  The sizing differs:
  `hist + max(hist, MAX_BLOCK)` columns here, `2·lookback + buffer` there.
- Conv kernel: register blocking, the micro-kernel idea of high-performance
  matrix multiplication (Goto and van de Geijn, "Anatomy of
  High-Performance Matrix Multiplication", ACM TOMS 34(3), 2008).  A
  dilated convolution is one small matrix product per tap; a tile of
  output channels by time steps is accumulated in registers over all taps
  and input channels, then stored once.
- LSTM: the column-oriented ("saxpy" or gaxpy) form of a matrix-vector
  product, `y += x_k · W[:, k]` for each `k` (Golub and Van Loan, *Matrix
  Computations*, §1.1).  Keeping `k` as the outer loop gives each gate the
  same order of additions as the row-wise dot product, hence bit-identical.
- `tanh` and `sigmoid`: Cody and Waite's argument reduction for `e^x`
  (*Software Manual for the Elementary Functions*, 1980) with the Taylor
  series, and the identities `tanh x = (e^2x − 1) / (e^2x + 1)` and
  `σ(x) = 1 / (1 + e^−x)`.

The upstream fixtures are several MB and are not committed here.
`scripts/nam-rs-parity.sh <dir>` clones the tag, puts this `src/` in its
place and runs upstream's checks: rustfmt, clippy, and the whole suite
(unit tests, parity against the fixtures, real-time safety) in the two
profiles RigWerk builds this crate with, dev and release at opt-level 3.
The real-time safety tests' allocator check needs debug assertions, so the
dev run is the one that proves it.
