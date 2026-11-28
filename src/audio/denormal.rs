//! FTZ/DAZ for one audio block.  A decaying IIR or envelope sits in the
//! x86 subnormal microcode path long enough to miss a short buffer.
//!
//! The asm is not `nomem`, so float work cannot move across the mode
//! switch.  32-bit x86 sets FTZ only: DAZ faults on early SSE.

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
    /// MXCSR bit 15 (FTZ).
    const FLUSH_TO_ZERO: u32 = 1 << 15;
    /// MXCSR bit 6 (DAZ).
    #[cfg(target_arch = "x86_64")]
    const DENORMALS_ARE_ZERO: u32 = 1 << 6;
    #[cfg(not(target_arch = "x86_64"))]
    const DENORMALS_ARE_ZERO: u32 = 0;
    /// MXCSR bits 0-5: sticky flags, not mode.
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
        // Flags raised during the block stay.  Only mode bits are restored.
        if (current ^ previous) & !EXCEPTION_FLAGS != 0 {
            write((previous & !EXCEPTION_FLAGS) | (current & EXCEPTION_FLAGS));
        }
    }

    fn read() -> u32 {
        let mut value = 0u32;
        // SAFETY: `stmxcsr` only stores MXCSR into this 4-byte slot.
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
        // SAFETY: `ldmxcsr` reloads MXCSR with only FTZ/DAZ changed, so no
        // reserved bit is set.
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
    /// FPCR bit 24 (FZ).
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
        // SAFETY: `mrs fpcr` only reads.  Not `nomem`, so float work stays
        // on its side of the switch.
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
    // No flush-to-zero mode on this target.
    pub(super) fn enable() -> usize {
        0
    }

    pub(super) fn restore(_previous: usize) {}
}

impl DenormalGuard {
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
