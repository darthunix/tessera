//! Division by a scalar int8 divisor through multiplication.
//!
//! The int32 divisor's construction with every width doubled: a divisor of
//! magnitude at least two becomes a multiplier and a shift (Granlund and
//! Montgomery, in libdivide's branch-free form), so that every quotient is
//! one high multiplication, two additions and two shifts. NEON has no
//! 64-bit high multiply, so the lanes of a word are divided one by one
//! with the scalar formula, whose high multiplication is one `smulh`
//! instruction; that still beats a hardware division per lane. Zero and ±1
//! are not prepared: zero is an error and `i64::MIN / -1` overflows, so
//! both stay on the per-lane path with its checks.

/// A divisor of magnitude at least two, prepared for division by
/// multiplication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Divisor {
    /// `floor(2^(64 + shift) / |d|) + 1`, which has bit 63 set and so reads
    /// as negative; zero for a power of two.
    pub(crate) magic: i64,
    /// `floor(log2 |d|)`: the final arithmetic right shift.
    pub(crate) shift: u32,
    /// What a negative intermediate quotient gets before the shift so that
    /// the result truncates toward zero: `2^shift`, or `2^shift - 1` for a
    /// power of two.
    pub(crate) bias: i64,
    /// `-1` for a negative divisor, else `0`: the quotient by the magnitude
    /// is negated as `(q ^ sign) - sign`.
    pub(crate) sign: i64,
    /// The divisor itself, for the remainder.
    pub(crate) value: i64,
}

impl Divisor {
    /// Prepare `d`; `None` for 0, 1 and -1.
    pub(crate) fn new(d: i64) -> Option<Self> {
        let magnitude = d.unsigned_abs();
        if magnitude < 2 {
            return None;
        }
        let shift = 63 - magnitude.leading_zeros();
        let power_of_two = magnitude.is_power_of_two();
        let magic = if power_of_two {
            0
        } else {
            // 2^(64 + shift) / |d| lies strictly between 2^63 and 2^64 - 1
            // because 2^shift < |d| < 2^(shift + 1); rounding it up makes
            // the truncated quotient exact for every 64-bit dividend.
            let multiplier = (1u128 << (64 + shift)) / u128::from(magnitude) + 1;
            (multiplier as u64) as i64
        };
        Some(Self {
            magic,
            shift,
            bias: ((1u64 << shift) - u64::from(power_of_two)) as i64,
            sign: -i64::from(d < 0),
            value: d,
        })
    }

    /// `n / d`, truncating toward zero: the scalar form of the int32 vector
    /// code, and here the form the lane loop runs.
    ///
    /// The high half of the product is `floor(magic * n / 2^64) - n` when
    /// `magic` reads as negative and `0` for a power of two; adding `n`
    /// gives `floor(2^(64 + shift) / |d| * n / 2^64)` or `n`. That is one
    /// below the truncated quotient's shifted form for negative dividends,
    /// which the bias corrects before the shift.
    #[inline(always)]
    #[cfg_attr(not(all(target_arch = "aarch64", not(miri))), allow(dead_code))]
    pub(crate) fn quotient(self, n: i64) -> i64 {
        let high = ((i128::from(self.magic) * i128::from(n)) >> 64) as i64;
        let mut q = high.wrapping_add(n);
        q = q.wrapping_add((q >> 63) & self.bias);
        q >>= self.shift;
        (q ^ self.sign).wrapping_sub(self.sign)
    }

    /// `n % d`, with the dividend's sign.
    #[inline(always)]
    #[cfg_attr(not(all(target_arch = "aarch64", not(miri))), allow(dead_code))]
    pub(crate) fn remainder(self, n: i64) -> i64 {
        n.wrapping_sub(self.quotient(n).wrapping_mul(self.value))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use tessera_testing::{integer, property};

    use super::Divisor;

    /// Divisors of every shape: small, around powers of two, past the int4
    /// range, large, and of both signs.
    fn divisors() -> Vec<i64> {
        let mut divisors: Vec<i64> = (2..=64).collect();
        divisors.extend([
            641,
            1000,
            65_535,
            65_536,
            65_537,
            (1 << 20) + 1,
            (1 << 31) - 1,
            1 << 31,
            (1 << 31) + 1,
            (1 << 40) + 3,
            (1 << 62) - 1,
            1 << 62,
            (1 << 62) + 1,
            3 << 61,
            i64::MAX,
        ]);
        let negatives: Vec<i64> = divisors.iter().map(|d| -d).collect();
        divisors.extend(negatives);
        divisors.push(i64::MIN);
        divisors
    }

    /// Dividends where a wrong multiplier or bias shows: a window around
    /// zero, the ends of the range, the neighbours of multiples of `d`
    /// including the largest ones; any dividend is the property below.
    fn dividends(d: i64) -> Vec<i64> {
        let wide = i128::from(d);
        let mut dividends: Vec<i64> = (-(1 << 15)..=1 << 15).collect();
        dividends.extend(i64::MIN..=i64::MIN + (1 << 12));
        dividends.extend(i64::MAX - (1 << 12)..=i64::MAX);
        let extreme = i128::from(i64::MAX) / wide.abs();
        for k in [1, 2, 3, 7, 1000, 123_456, extreme - 1, extreme, extreme + 1] {
            for multiple in [k * wide, -k * wide] {
                for n in multiple - 1..=multiple + 1 {
                    if let Ok(n) = i64::try_from(n) {
                        dividends.push(n);
                    }
                }
            }
        }
        dividends
    }

    #[test]
    fn zero_and_units_are_not_prepared() {
        assert_eq!(Divisor::new(0), None);
        assert_eq!(Divisor::new(1), None);
        assert_eq!(Divisor::new(-1), None);
        assert!(Divisor::new(2).is_some());
        assert!(Divisor::new(-2).is_some());
    }

    #[test]
    fn constants_follow_the_construction() {
        let seven = Divisor::new(7).unwrap();
        // Hacker's Delight's 64-bit multiplier for 7: 0x9249_2492_4924_9249
        // rounded up.
        assert_eq!(seven.magic as u64, 0x9249_2492_4924_924A);
        assert_eq!((seven.shift, seven.bias, seven.sign), (2, 4, 0));
        let minus_eight = Divisor::new(-8).unwrap();
        assert_eq!(
            (
                minus_eight.magic,
                minus_eight.shift,
                minus_eight.bias,
                minus_eight.sign
            ),
            (0, 3, 7, -1)
        );
        let min = Divisor::new(i64::MIN).unwrap();
        assert_eq!(
            (min.magic, min.shift, min.bias, min.sign),
            (0, 63, i64::MAX, -1)
        );
    }

    /// A divisor a word divides by, any but 0 and ±1: a power of two or
    /// its neighbour of either sign, or a value leaning to the ends of the
    /// range.
    fn any_divisor() -> impl Strategy<Value = i64> {
        let near_power =
            (1..63_u32, -1_i64..=1, any::<bool>()).prop_map(|(shift, step, negative)| {
                let d = (1_i64 << shift) + step;
                if negative { -d } else { d }
            });
        prop_oneof![near_power, integer::<i64>()]
            .prop_filter("a prepared divisor", |&d| Divisor::new(d).is_some())
    }

    #[test]
    fn any_dividend_divides_as_the_operators() {
        let cases = (any_divisor(), vec(integer::<i64>(), 0..256));
        property(cases, |(d, dividends)| -> Result<()> {
            let divisor = Divisor::new(d).unwrap();
            for n in dividends {
                assert_eq!(divisor.quotient(n), n / d, "{n} / {d}");
                assert_eq!(divisor.remainder(n), n % d, "{n} % {d}");
            }
            Ok(())
        });
    }

    #[test]
    fn quotients_and_remainders_match_the_operators() {
        for d in divisors() {
            let divisor = Divisor::new(d).unwrap();
            for n in dividends(d) {
                assert_eq!(divisor.quotient(n), n / d, "{n} / {d}");
                assert_eq!(divisor.remainder(n), n % d, "{n} % {d}");
            }
        }
    }
}
