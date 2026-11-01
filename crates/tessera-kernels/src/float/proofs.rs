//! Kani proofs of the casts of a float to an integer: for every float of
//! either width and every integer type, the cast accepts exactly the
//! values whose nearest integer, a tie to the even one, lies in the type's
//! range, and gives that integer. The reference finds the nearest integer
//! by truncation and the fraction left, in integers, not by the rounding
//! the code calls.

use super::{FloatError, Integer, Rounded};

/// The integer nearest to `x`, a tie to the even one; `None` for NaN, an
/// infinity, or a magnitude past every integer type.
fn nearest_even(x: f64) -> Option<i128> {
    if !x.is_finite() || x.abs() >= 1.0e30 {
        return None;
    }
    // Toward zero; exact below 2^127. The fraction left is exact too: below
    // 2^52 both are close enough, and above x is an integer.
    let truncated = x as i128;
    let rest = x - truncated as f64;
    let odd = truncated % 2 != 0;
    let step = if rest > 0.5 || (rest == 0.5 && odd) {
        1
    } else if rest < -0.5 || (rest == -0.5 && odd) {
        -1
    } else {
        0
    };
    Some(truncated + step)
}

fn range(to: Integer) -> (i128, i128) {
    match to {
        Integer::Int2 => (i128::from(i16::MIN), i128::from(i16::MAX)),
        Integer::Int4 => (i128::from(i32::MIN), i128::from(i32::MAX)),
        Integer::Int8 => (i128::from(i64::MIN), i128::from(i64::MAX)),
    }
}

fn holds(x: f64, to: Integer, cast: Result<i64, FloatError>) {
    let (least, most) = range(to);
    let want = nearest_even(x).filter(|n| (least..=most).contains(n));
    match (cast, want) {
        (Ok(value), Some(nearest)) => assert!(i128::from(value) == nearest),
        (Err(error), None) => assert!(error == to.out_of_range()),
        _ => assert!(false, "the cast and the reference disagree"),
    }
    kani::cover!(cast.is_ok());
    kani::cover!(cast.is_err() && x.is_finite());
}

macro_rules! proofs {
    ($($name:ident: $float:ty => $to:expr;)*) => {
        $(
            #[kani::proof]
            fn $name() {
                let x: $float = kani::any();
                holds(f64::from(x), $to, x.integer($to));
            }
        )*
    };
}

proofs! {
    float8_to_int2: f64 => Integer::Int2;
    float8_to_int4: f64 => Integer::Int4;
    float8_to_int8: f64 => Integer::Int8;
    float4_to_int2: f32 => Integer::Int2;
    float4_to_int4: f32 => Integer::Int4;
    float4_to_int8: f32 => Integer::Int8;
}
