//! The whole-word operations the vertical probe needs, with the vector
//! code of [`crate::simd`] where it exists and plain loops elsewhere (other
//! targets and Miri), in the manner of the hash kernels' `bulk_op`.

#[cfg(all(target_arch = "aarch64", not(miri)))]
pub(super) use crate::simd::{eq_mask_i64, eq_mask_u32, prefetch};

/// A prefetch hint where none is implemented: nothing.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(always)]
pub(super) fn prefetch(_address: *const u8) {}

/// Bit `i` set where `a[i] == b[i]`.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(always)]
pub(super) fn eq_mask_u32(a: &[u32; 64], b: &[u32; 64]) -> u64 {
    let mut mask = 0;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        mask |= u64::from(x == y) << i;
    }
    mask
}

/// Bit `i` set where `a[i] == b[i]`.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(always)]
pub(super) fn eq_mask_i64(a: &[i64; 64], b: &[i64; 64]) -> u64 {
    let mut mask = 0;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        mask |= u64::from(x == y) << i;
    }
    mask
}

/// The operations against a scalar model, whichever implementation the
/// target compiles: the NEON one here, the AVX2 one when it exists.
#[cfg(test)]
mod tests {
    use super::{eq_mask_i64, eq_mask_u32};

    fn random(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn model<T: PartialEq>(a: &[T; 64], b: &[T; 64]) -> u64 {
        (0..64).fold(0, |bits, i| bits | u64::from(a[i] == b[i]) << i)
    }

    #[test]
    fn equality_masks_match_the_model() {
        let mut state = 0x5eed_1234_abcd_0001;
        for round in 0..200 {
            let a32: [u32; 64] = core::array::from_fn(|_| random(&mut state) as u32 % 4);
            let mut b32 = a32;
            let a64: [i64; 64] = core::array::from_fn(|_| random(&mut state) as i64 % 3 - 1);
            let mut b64 = a64;
            // Rounds of none, all, and some lanes differing, including in
            // the high halves that a narrowing could lose.
            for i in 0..64 {
                let flip = match round % 4 {
                    0 => false,
                    1 => true,
                    _ => random(&mut state).is_multiple_of(2),
                };
                if flip {
                    b32[i] ^= 1 << (random(&mut state) % 32);
                    b64[i] ^= 1 << (random(&mut state) % 64);
                }
            }
            assert_eq!(eq_mask_u32(&a32, &b32), model(&a32, &b32), "round {round}");
            assert_eq!(eq_mask_i64(&a64, &b64), model(&a64, &b64), "round {round}");
        }
        assert_eq!(eq_mask_u32(&[7; 64], &[7; 64]), u64::MAX);
        assert_eq!(eq_mask_i64(&[-1; 64], &[i64::MAX; 64]), 0);
    }
}
