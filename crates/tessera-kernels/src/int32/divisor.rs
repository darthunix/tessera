//! Division by a scalar divisor through multiplication.
//!
//! A divisor of magnitude at least two is turned once into a multiplier and
//! a shift (Granlund and Montgomery's method in libdivide's branch-free
//! form) so that every quotient is one high multiplication, two additions
//! and two shifts, which vector code performs on four lanes at once where
//! NEON has no integer division. The formula neither traps nor overflows:
//! `i32::MIN` divided by a divisor of magnitude at least two fits, and the
//! remainder `n - q * d` is exact modulo 2^32. Zero and ±1 are not prepared:
//! zero is an error and `i32::MIN / -1` overflows, so both stay on the
//! per-lane path with its checks.

/// A divisor of magnitude at least two, prepared for division by
/// multiplication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Divisor {
    /// `floor(2^(32 + shift) / |d|) + 1`, which has bit 31 set and so reads
    /// as negative; zero for a power of two.
    pub(crate) magic: i32,
    /// `floor(log2 |d|)`: the final arithmetic right shift.
    pub(crate) shift: u32,
    /// What a negative intermediate quotient gets before the shift so that
    /// the result truncates toward zero: `2^shift`, or `2^shift - 1` for a
    /// power of two.
    pub(crate) bias: i32,
    /// `-1` for a negative divisor, else `0`: the quotient by the magnitude
    /// is negated as `(q ^ sign) - sign`.
    pub(crate) sign: i32,
    /// The divisor itself, for the remainder.
    pub(crate) value: i32,
}

impl Divisor {
    /// Prepare `d`; `None` for 0, 1 and -1.
    pub(crate) fn new(d: i32) -> Option<Self> {
        let magnitude = d.unsigned_abs();
        if magnitude < 2 {
            return None;
        }
        let shift = 31 - magnitude.leading_zeros();
        let power_of_two = magnitude.is_power_of_two();
        let magic = if power_of_two {
            0
        } else {
            // 2^(32 + shift) / |d| lies strictly between 2^31 and 2^32 - 1
            // because 2^shift < |d| < 2^(shift + 1); rounding it up makes
            // the truncated quotient exact for every 32-bit dividend.
            let multiplier = (1u64 << (32 + shift)) / u64::from(magnitude) + 1;
            (multiplier as u32) as i32
        };
        Some(Self {
            magic,
            shift,
            bias: ((1u32 << shift) - u32::from(power_of_two)) as i32,
            sign: -i32::from(d < 0),
            value: d,
        })
    }

    /// `n / d`, truncating toward zero: the scalar form of what the vector
    /// code computes, kept as the specification the tests check against.
    ///
    /// The high half of the product is `floor(magic * n / 2^32) - n` when
    /// `magic` reads as negative and `0` for a power of two; adding `n`
    /// gives `floor(2^(32 + shift) / |d| * n / 2^32)` or `n`. That is one
    /// below the truncated quotient's shifted form for negative dividends,
    /// which the bias corrects before the shift.
    #[cfg(test)]
    pub(crate) fn quotient(self, n: i32) -> i32 {
        let high = ((i64::from(self.magic) * i64::from(n)) >> 32) as i32;
        let mut q = high.wrapping_add(n);
        q = q.wrapping_add((q >> 31) & self.bias);
        q >>= self.shift;
        (q ^ self.sign).wrapping_sub(self.sign)
    }

    /// `n % d`, with the dividend's sign; the scalar form, for the tests.
    #[cfg(test)]
    pub(crate) fn remainder(self, n: i32) -> i32 {
        n.wrapping_sub(self.quotient(n).wrapping_mul(self.value))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use anyhow::Result;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use tessera_testing::{integer, property};

    use super::Divisor;

    /// Divisors of every shape: small, around powers of two, large, and of
    /// both signs.
    pub(crate) fn divisors() -> Vec<i32> {
        let mut divisors: Vec<i32> = (2..=64).collect();
        divisors.extend([
            641,
            1000,
            65_535,
            65_536,
            65_537,
            (1 << 20) + 1,
            (1 << 30) - 1,
            1 << 30,
            (1 << 30) + 1,
            3 << 29,
            i32::MAX,
        ]);
        let negatives: Vec<i32> = divisors.iter().map(|d| -d).collect();
        divisors.extend(negatives);
        divisors.push(i32::MIN);
        divisors
    }

    /// Dividends where a wrong multiplier or bias shows: a window around
    /// zero, the ends of the range, the neighbours of multiples of `d`
    /// including the largest ones; any dividend is the property below.
    pub(crate) fn dividends(d: i32) -> Vec<i32> {
        let wide = i64::from(d);
        let mut dividends: Vec<i32> = (-(1 << 15)..=1 << 15).collect();
        dividends.extend(i32::MIN..=i32::MIN + (1 << 12));
        dividends.extend(i32::MAX - (1 << 12)..=i32::MAX);
        let extreme = i64::from(i32::MAX) / wide.abs();
        for k in [1, 2, 3, 7, 1000, 123_456, extreme - 1, extreme, extreme + 1] {
            for multiple in [k * wide, -k * wide] {
                for n in multiple - 1..=multiple + 1 {
                    if let Ok(n) = i32::try_from(n) {
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
        // Hacker's Delight's multiplier for 7: 0x9249_2493.
        assert_eq!(seven.magic as u32, 0x9249_2493);
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
        let min = Divisor::new(i32::MIN).unwrap();
        assert_eq!(
            (min.magic, min.shift, min.bias, min.sign),
            (0, 31, i32::MAX, -1)
        );
    }

    /// A divisor a word divides by, any but 0 and ±1: a power of two or
    /// its neighbour of either sign, or a value leaning to the ends of the
    /// range.
    pub(crate) fn any_divisor() -> impl Strategy<Value = i32> {
        let near_power =
            (1..31_u32, -1_i32..=1, any::<bool>()).prop_map(|(shift, step, negative)| {
                let d = (1_i32 << shift) + step;
                if negative { -d } else { d }
            });
        prop_oneof![near_power, integer::<i32>()]
            .prop_filter("a prepared divisor", |&d| Divisor::new(d).is_some())
    }

    #[test]
    fn any_dividend_divides_as_the_operators() {
        let cases = (any_divisor(), vec(integer::<i32>(), 0..256));
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

    /// Every dividend for a dozen divisors: minutes in release, so it runs
    /// on request (`cargo test --release -- --ignored`).
    #[test]
    #[ignore = "exhaustive; run in release on request"]
    fn every_dividend_matches_the_operators() {
        for d in [
            2,
            3,
            7,
            -7,
            641,
            65_535,
            65_537,
            (1 << 20) + 1,
            (1 << 30) + 1,
            3 << 29,
            i32::MAX,
            i32::MIN,
        ] {
            let divisor = Divisor::new(d).unwrap();
            for n in i32::MIN..=i32::MAX {
                let (q, r) = (divisor.quotient(n), divisor.remainder(n));
                if q != n / d || r != n % d {
                    panic!("{n} / {d}: got {q} rem {r}, want {} rem {}", n / d, n % d);
                }
            }
        }
    }
}
