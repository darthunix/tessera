//! The failures a plan names: the ways one of its values may still fail.

use core::fmt;

use crate::error::ErrorKind;

/// The ways a value of a plan may fail, a set of bits.
///
/// A plan names only the failures a value of its source type can meet: a
/// plan of integers widened names none, a plan of days in 32 bits names
/// [`Failures::OUT_OF_RANGE`], a plan of checked bytes into `text` names
/// [`Failures::NOT_UTF8`], [`Failures::ZERO_BYTE`] and
/// [`Failures::TOO_LARGE`]. A conversion of values through the plan fails
/// in no other way, save for a call of a broken shape
/// ([`ErrorKind::BrokenShape`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Failures(u8);

impl Failures {
    /// No failure.
    pub const NONE: Failures = Failures(0);
    /// A value outside the range of its target type:
    /// [`ErrorKind::OutOfRange`].
    pub const OUT_OF_RANGE: Failures = Failures(1);
    /// Bytes that are not UTF-8: [`ErrorKind::InvalidText`].
    pub const NOT_UTF8: Failures = Failures(1 << 1);
    /// A zero byte in a text: [`ErrorKind::InvalidText`].
    pub const ZERO_BYTE: Failures = Failures(1 << 2);
    /// A string longer than a `varchar`'s length: [`ErrorKind::TooLong`].
    pub const TOO_LONG: Failures = Failures(1 << 3);
    /// A value larger than PostgreSQL holds, or values of one call past
    /// what Arrow's offsets hold: [`ErrorKind::TooLarge`].
    pub const TOO_LARGE: Failures = Failures(1 << 4);

    const ALL: [(Failures, &'static str, ErrorKind); 5] = [
        (
            Failures::OUT_OF_RANGE,
            "OUT_OF_RANGE",
            ErrorKind::OutOfRange,
        ),
        (Failures::NOT_UTF8, "NOT_UTF8", ErrorKind::InvalidText),
        (Failures::ZERO_BYTE, "ZERO_BYTE", ErrorKind::InvalidText),
        (Failures::TOO_LONG, "TOO_LONG", ErrorKind::TooLong),
        (Failures::TOO_LARGE, "TOO_LARGE", ErrorKind::TooLarge),
    ];

    /// Whether the set holds every failure of `other`.
    pub const fn contains(self, other: Failures) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the set is empty: no value of the plan can fail.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The failures of both sets.
    pub const fn union(self, other: Failures) -> Failures {
        Failures(self.0 | other.0)
    }

    /// The kinds of error the set's failures give, each once.
    pub fn kinds(self) -> impl Iterator<Item = ErrorKind> {
        let mut seen: [Option<ErrorKind>; 5] = [None; 5];
        let mut count = 0;
        for (failure, _, kind) in Failures::ALL {
            if self.contains(failure) && !seen[..count].contains(&Some(kind)) {
                seen[count] = Some(kind);
                count += 1;
            }
        }
        seen.into_iter().take(count).flatten()
    }
}

impl core::ops::BitOr for Failures {
    type Output = Failures;

    fn bitor(self, other: Failures) -> Failures {
        self.union(other)
    }
}

impl fmt::Debug for Failures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut set = f.debug_set();
        for (failure, name, _) in Failures::ALL {
            if self.contains(failure) {
                set.entry(&format_args!("{name}"));
            }
        }
        set.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_of_failures() {
        let set = Failures::NOT_UTF8 | Failures::ZERO_BYTE | Failures::TOO_LARGE;
        assert!(set.contains(Failures::ZERO_BYTE));
        assert!(set.contains(Failures::NOT_UTF8 | Failures::TOO_LARGE));
        assert!(!set.contains(Failures::OUT_OF_RANGE));
        assert!(!set.is_empty() && Failures::NONE.is_empty());
        assert!(set.contains(Failures::NONE));
        assert_eq!(
            set.kinds().collect::<Vec<_>>(),
            [ErrorKind::InvalidText, ErrorKind::TooLarge]
        );
        assert_eq!(format!("{set:?}"), "{NOT_UTF8, ZERO_BYTE, TOO_LARGE}");
        assert_eq!(Failures::default(), Failures::NONE);
    }
}
