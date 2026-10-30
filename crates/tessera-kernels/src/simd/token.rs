//! The tokens of the vector sets.
//!
//! A token is a type without data that stands for one vector set, and a
//! value of it proves that the processor runs that set: code compiled for
//! the set may run once a token is in hand. Each token is defined on every
//! target but has values only where its set can run. Elsewhere, and under
//! Miri, it is an empty `enum`, so code that takes one cannot be reached,
//! and the compiler knows it.
//!
//! The tokens are `pub` so that the public trait of the sets,
//! [`crate::set::SetValue`], can take them; `simd` is private, so nothing
//! outside the crate can name or make one.

/// A token of a vector set.
///
/// A driver asks for its token once a call and decides on whole words as
/// it did before tokens: a `bool` of the same chain of conditions, with
/// the token tested in it (`is_some`, or a `match` without a closure where
/// a condition needs the token), and the token taken in the branch,
/// `if let Some(neon) = neon && bulk`. An `Option` combinator or a let
/// chain in the decision itself changed the machine code of the NEON
/// paths (`tessera-bench --disasm --target aarch64-apple-darwin`).
pub(crate) trait Token: Copy {
    /// The token, where the processor runs its set; `None` elsewhere.
    fn get() -> Option<Self>;
}

/// NEON, part of every AArch64 processor.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[derive(Clone, Copy)]
pub struct Neon(());

/// NEON, which this target does not run. Only [`Simd`] names it, off
/// x86-64.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Neon {}

impl Token for Neon {
    #[inline(always)]
    fn get() -> Option<Self> {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            // The code is compiled with NEON, so every processor it runs on
            // has it.
            const { assert!(cfg!(target_feature = "neon")) }
            Some(Self(()))
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        None
    }
}

/// AVX2 with POPCNT, which an x86-64 processor may lack. Every processor
/// with AVX2 has POPCNT, but the compiler does not assume it, so the token
/// asks for both.
#[cfg(all(target_arch = "x86_64", not(miri)))]
#[derive(Clone, Copy)]
pub struct Avx2(());

/// AVX2, which this target does not run. Only [`Simd`] names it, on
/// x86-64 under Miri.
#[cfg(not(all(target_arch = "x86_64", not(miri))))]
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Avx2 {}

impl Token for Avx2 {
    #[inline(always)]
    fn get() -> Option<Self> {
        // The standard library keeps the answers, so asking costs a load for
        // each; a build with both features enabled gets a constant.
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        return (std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("popcnt"))
            .then_some(Self(()));
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        None
    }
}

/// The vector set of the target: NEON on AArch64 and AVX2 on x86-64.
/// Elsewhere it is NEON, which has no values there.
#[cfg(target_arch = "x86_64")]
pub type Simd = Avx2;
#[cfg(not(target_arch = "x86_64"))]
pub type Simd = Neon;

#[cfg(test)]
mod tests {
    use super::{Simd, Token};

    /// CI sets `TESSERA_REQUIRE_SIMD=1` on runners that have the vector set
    /// of their target, so that the tests of the vector code cannot pass
    /// on the row path unseen.
    #[test]
    fn the_vector_set_runs_where_it_is_required() {
        if std::env::var_os("TESSERA_REQUIRE_SIMD").is_some_and(|value| value == "1") {
            assert!(
                Simd::get().is_some(),
                "TESSERA_REQUIRE_SIMD=1, but this processor runs no vector set"
            );
        }
    }
}
