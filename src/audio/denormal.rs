//! Flushing subnormal floats to zero on the audio threads.
//!
//! When a signal decays into silence, IIR filter states (the EQ biquads) and
//! envelope followers (the gate) pass through the subnormal range on their way
//! to zero.  On x86 every operation on such a value takes a slow microcode
//! path, often a hundred times slower than a normal one, and a decay can stay
//! there for seconds.  At small buffers that is enough to miss a deadline.
//!
//! [`DenormalGuard`] switches the current thread to flush-to-zero (and, on
//! x86, denormals-are-zero) for as long as it lives and restores the previous
//! mode when it is dropped.  The audio callbacks hold one for the length of a
//! block, so the plugin inserts processed inside that block see the same mode
//! a DAW would give them, while the driver's own code on that thread runs
//! with whatever mode it had before.  Reading and writing the control register
//! is a couple of instructions and never enters the kernel.
//!
//! Changing the floating-point mode is formally outside Rust's model, which
//! assumes the default environment.  The asm blocks below are not `nomem` or
//! `readonly`, so the compiler cannot move memory accesses (and the float work
//! that feeds them) across the switch; arithmetic between the switches is what
//! the mode is meant to affect, and nothing here depends on subnormal results.
//!
//! On 32-bit x86 only FTZ is set: DAZ is missing on the earliest SSE CPUs,
//! where setting it faults.  Every x86_64 CPU has it.

/// Flush-to-zero (and denormals-are-zero on x86_64) for the life of the guard.
/// On targets without such a mode this is a no-op.
#[must_use = "the mode is restored when the guard is dropped"]
pub(crate) struct DenormalGuard {
    #[cfg_attr(
        not(any(
            target_arch = "x86_64",
            all(target_arch = "x86", target_feature = "sse"),
            target_arch = "aarch64"
        )),
        allow(dead_code)
    )]
    previous: usize,
}

#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse")
))]
mod arch {
    /// MXCSR bit 15: results that would be subnormal become zero.
    const FLUSH_TO_ZERO: u32 = 1 << 15;
    /// MXCSR bit 6: subnormal inputs are read as zero.  Only on x86_64; see
    /// the module docs.
    #[cfg(target_arch = "x86_64")]
    const DENORMALS_ARE_ZERO: u32 = 1 << 6;
    #[cfg(not(target_arch = "x86_64"))]
    const DENORMALS_ARE_ZERO: u32 = 0;
    /// MXCSR bits 0-5 are sticky exception flags, not part of the mode.
    const EXCEPTION_FLAGS: u32 = 0x3F;

    pub(super) fn enable() -> usize {
        let previous = read();
        let wanted = previous | FLUSH_TO_ZERO | DENORMALS_ARE_ZERO;
        if wanted != previous {
            write(wanted);
        }
        previous as usize
    }

    pub(super) fn restore(previous: usize) {
        let previous = previous as u32;
        let current = read();
        // Only the mode bits count; flags raised during the block are kept.
        if (current ^ previous) & !EXCEPTION_FLAGS != 0 {
            write((previous & !EXCEPTION_FLAGS) | (current & EXCEPTION_FLAGS));
        }
    }

    fn read() -> u32 {
        let mut value = 0u32;
        // SAFETY: `stmxcsr` only stores the SSE control/status register into
        // the given 4-byte location.
        unsafe {
            std::arch::asm!(
                "stmxcsr [{}]",
                in(reg) &mut value,
                options(nostack, preserves_flags)
            );
        }
        value
    }

    fn write(value: u32) {
        // SAFETY: `ldmxcsr` loads a value read back from the same register
        // with only the FTZ/DAZ bits changed, so no reserved bit is set.
        unsafe {
            std::arch::asm!(
                "ldmxcsr [{}]",
                in(reg) &value,
                options(nostack, preserves_flags)
            );
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    /// FPCR bit 24 (FZ): subnormal inputs and results are flushed to zero.
    const FLUSH_TO_ZERO: u64 = 1 << 24;

    pub(super) fn enable() -> usize {
        let previous = read();
        let wanted = previous | FLUSH_TO_ZERO;
        if wanted != previous {
            write(wanted);
        }
        previous as usize
    }

    pub(super) fn restore(previous: usize) {
        let previous = previous as u64;
        if read() != previous {
            write(previous);
        }
    }

    fn read() -> u64 {
        let value: u64;
        // SAFETY: reading FPCR has no side effects.  Not `nomem`, so memory
        // accesses stay on their side of the mode switch.
        unsafe {
            std::arch::asm!("mrs {}, fpcr", out(reg) value, options(nostack));
        }
        value
    }

    fn write(value: u64) {
        // SAFETY: the value is FPCR as read, with only the FZ bit changed.
        unsafe {
            std::arch::asm!("msr fpcr, {}", in(reg) value, options(nostack));
        }
    }
}

#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse"),
    target_arch = "aarch64"
)))]
mod arch {
    pub(super) fn enable() -> usize {
        0
    }

    pub(super) fn restore(_previous: usize) {}
}

impl DenormalGuard {
    /// Switches this thread to flush-to-zero until the guard is dropped.
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            previous: arch::enable(),
        }
    }
}

impl Drop for DenormalGuard {
    #[inline]
    fn drop(&mut self) {
        arch::restore(self.previous);
    }
}

#[cfg(test)]
mod tests;
