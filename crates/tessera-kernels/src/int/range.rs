//! The range of an arithmetic result. An int4 or int8 result has its
//! type's own, which the width's operations check. A smallint lives in an
//! int4 lane, sign-extended, so its arithmetic is the int4 one, and its
//! result must also fall within -32768 to 32767: PostgreSQL's smallint
//! operators fail with 22003 "smallint out of range" past it. The range is
//! a type parameter of the driver, so that the int4 and int8 code, whose
//! range checks nothing, stays the code it was.

use crate::ops::ArithmeticError;

/// The range a result of the width `T` must fall in.
pub(crate) trait Range<T>: 'static {
    /// Narrower than the width: a whole word's vector code must flag a
    /// lane outside it, as it flags an overflow.
    const NARROW: bool;

    /// One row's result, or the range's error.
    fn check(value: T) -> Result<T, ArithmeticError>;
}

/// The width's own range: nothing more to check.
pub(crate) struct Own;

impl<T> Range<T> for Own {
    const NARROW: bool = false;

    #[inline(always)]
    fn check(value: T) -> Result<T, ArithmeticError> {
        Ok(value)
    }
}

/// The smallint range, of int4 lanes.
pub(crate) struct Smallint;

impl Range<i32> for Smallint {
    const NARROW: bool = true;

    #[inline(always)]
    fn check(value: i32) -> Result<i32, ArithmeticError> {
        if i16::try_from(value).is_ok() {
            Ok(value)
        } else {
            Err(ArithmeticError::SmallintOutOfRange)
        }
    }
}
