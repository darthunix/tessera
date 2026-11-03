//! The proof by Kani of the prepared int64 divisor by a power of two:
//! for every power of two from 2 to 2^63, of either sign that fits an
//! i64, and every dividend, [`Divisor::new`] as it is gives a divisor
//! whose quotient and remainder are the truncating ones. They are checked
//! with shifts alone, against `|n| >> k` signed and `n - q * d` as a
//! shift: Kani cannot finish a division or a multiplication of two
//! unknowns. This is the path of the bias, the shift and the sign, with
//! no multiplier; the divisors by a multiplier keep their tests.

use super::Divisor;

#[kani::proof]
fn a_power_of_two_divides_as_the_operators() {
    let shift: u32 = kani::any();
    kani::assume((1..64).contains(&shift));
    let negative: bool = kani::any();
    let magnitude: i128 = 1 << shift;
    let divisor = if negative { -magnitude } else { magnitude };
    // 2^63 fits only negative.
    kani::assume(i64::try_from(divisor).is_ok());
    let d = divisor as i64;
    let Some(prepared) = Divisor::new(d) else {
        panic!()
    };
    let n: i64 = kani::any();
    // The truncating quotient: |n| shifted, its sign the product of the
    // signs; the remainder: n less the quotient times d, as a shift.
    let magnitude_quotient = i128::from(n).unsigned_abs() >> shift;
    let quotient = if (n < 0) != negative {
        -(magnitude_quotient as i128)
    } else {
        magnitude_quotient as i128
    };
    let product = if negative {
        -(quotient << shift)
    } else {
        quotient << shift
    };
    let remainder = i128::from(n) - product;
    assert!(i128::from(prepared.quotient(n)) == quotient);
    assert!(i128::from(prepared.remainder(n)) == remainder);
    kani::cover!(negative && n > 0 && remainder != 0);
    kani::cover!(!negative && n < 0 && remainder != 0);
    kani::cover!(d == i64::MIN && n == i64::MIN);
}
