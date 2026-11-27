//! Terminal output shared by the commands and the dashboard: colours, number formats, tables, bars.

pub mod fmt;
pub mod glossary;
pub mod mask;
pub mod style;
pub mod table;

/// A cheap deterministic hash: per-cell randomness of the boot animation, and the made-up world
/// of `swarmy demo`.
pub fn hash(a: u64, b: u64, c: u64) -> u64 {
    let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ c.wrapping_mul(0x1656_67B1_9E37_79F9);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x
}
