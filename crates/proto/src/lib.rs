//! Streetlight wire protocol, shared by host and clients.

pub mod control;
pub mod cursor;
pub mod crypto;
pub mod fec;
pub mod media;
#[cfg(feature = "net")]
pub mod net;
pub mod pair;

/// Microseconds on CLOCK_MONOTONIC (host timestamps in media packets).
pub fn now_us() -> u64 {
    let mut ts = std::mem::MaybeUninit::<[i64; 2]>::uninit();
    // SAFETY: clock_gettime writes a timespec (two longs on 64-bit targets).
    unsafe extern "C" {
        fn clock_gettime(clk: i32, ts: *mut [i64; 2]) -> i32;
    }
    unsafe {
        clock_gettime(1, ts.as_mut_ptr());
        let [s, ns] = ts.assume_init();
        (s as u64) * 1_000_000 + (ns as u64) / 1000
    }
}
