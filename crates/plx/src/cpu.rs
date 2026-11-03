//! plx is built for x86-64-v3 (AVX2): checked first thing
//! in `main`, so an older CPU gets a message rather than an illegal
//! instruction somewhere later.
//!
//! The check itself must not use what it checks for. It tests bits against
//! constant masks (`test` with an immediate, no BMI shifts) and reports
//! through plain `write(2)` calls of static strings (no formatting, whose
//! copies may vectorise); `main`'s disassembly confirms it.

use core::arch::x86_64::{__cpuid, __cpuid_count, _xgetbv};

/// The first x86-64-v3 feature this CPU (or the OS) lacks, if any.
#[must_use]
pub fn first_missing_or_none() -> Option<&'static str> {
    let basic = __cpuid(0).eax;
    let extended = __cpuid(0x8000_0000).eax;
    if basic < 7 || extended < 0x8000_0001 {
        return Some("CPUID leaves 7 and 0x8000_0001");
    }
    let leaf1 = __cpuid(1).ecx;
    let leaf7 = __cpuid_count(7, 0).ebx;
    let ext = __cpuid(0x8000_0001).ecx;
    // Intel SDM vol. 2A, CPUID: leaf 1 ECX, leaf 7 EBX, 0x8000_0001 ECX.
    if leaf1 & (1 << 0) == 0 {
        return Some("SSE3");
    }
    if leaf1 & (1 << 9) == 0 {
        return Some("SSSE3");
    }
    if leaf1 & (1 << 12) == 0 {
        return Some("FMA");
    }
    if leaf1 & (1 << 13) == 0 {
        return Some("CMPXCHG16B");
    }
    if leaf1 & (1 << 19) == 0 {
        return Some("SSE4.1");
    }
    if leaf1 & (1 << 20) == 0 {
        return Some("SSE4.2");
    }
    if leaf1 & (1 << 22) == 0 {
        return Some("MOVBE");
    }
    if leaf1 & (1 << 23) == 0 {
        return Some("POPCNT");
    }
    if leaf1 & (1 << 27) == 0 {
        return Some("OSXSAVE");
    }
    if leaf1 & (1 << 28) == 0 {
        return Some("AVX");
    }
    if leaf1 & (1 << 29) == 0 {
        return Some("F16C");
    }
    if leaf7 & (1 << 3) == 0 {
        return Some("BMI1");
    }
    if leaf7 & (1 << 5) == 0 {
        return Some("AVX2");
    }
    if leaf7 & (1 << 8) == 0 {
        return Some("BMI2");
    }
    if ext & (1 << 0) == 0 {
        return Some("LAHF/SAHF");
    }
    if ext & (1 << 5) == 0 {
        return Some("LZCNT");
    }
    // The OS must save the SSE and AVX registers: XCR0 bits 1 and 2.
    #[expect(unsafe_code, reason = "XGETBV, once CPUID says it is enabled")]
    // SAFETY: XGETBV needs the `xsave` feature, which the build enables,
    // and the OS to have enabled it (CR4.OSXSAVE), which CPUID leaf 1
    // reported in OSXSAVE just above. XCR0 (register 0) always exists.
    let xcr0 = unsafe { _xgetbv(0) };
    if xcr0 & 0b110 != 0b110 {
        return Some("AVX register saving by the OS");
    }
    None
}

/// Tells the user, on stderr, which feature is missing.
pub fn report(missing: &'static str) {
    // Three writes rather than an array of parts: building one may use
    // vector stores.
    write_all(b"plx needs an x86-64-v3 CPU (AVX2, from 2013 on); this one lacks ");
    write_all(missing.as_bytes());
    write_all(b"\n");
}

fn write_all(mut bytes: &[u8]) {
    while let [_, ..] = bytes {
        match rustix::io::write(rustix::stdio::stderr(), bytes) {
            Ok(n) => match bytes.get(n..) {
                Some(rest) if n > 0 => bytes = rest,
                // Nothing written, or more than asked: stderr is broken.
                Some(_) | None => return,
            },
            Err(rustix::io::Errno::INTR) => {}
            // Nowhere else to say it; the exit status still fails.
            Err(_) => return,
        }
    }
}
