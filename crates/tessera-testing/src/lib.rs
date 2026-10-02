//! Strategies for the property tests of the Tessera crates, over
//! [`proptest`].
//!
//! Integer code breaks at the ends of its type and around zero, and a batch
//! breaks at the borders of its 64-row words and in the rows nobody reads: a
//! NULL row's slot and an unselected row still hold a value, which a kernel
//! must neither let fail nor let through. The strategies lean that way:
//! values take the edges of their type often, row counts lie around word
//! borders, each word of a mask has a density of its own, and the rows a
//! check must not read hold edge values.
//!
//! [`property`] runs a check over a strategy as `proptest!` does, for checks
//! written once over a generic width. Each run draws new cases (a fixed seed
//! with `PROPTEST_RNG_SEED`, more cases with `PROPTEST_CASES`); a failing
//! case shrinks to a small one, is kept in `<test file>.proptest-regressions`
//! beside the test, which is committed, and is replayed first ever after.

use std::fmt::Debug;
use std::panic::Location;

use proptest::prelude::*;
use proptest::sample::select;
use proptest::test_runner::{FileFailurePersistence, TestCaseError, TestRunner};

/// An integer type a property draws values of.
pub trait Int: Copy + Debug + PartialEq + 'static {
    /// Where integer code breaks: both ends of the type and one step inside
    /// them, -1, 0 and 1, and for a wider type the ends of the narrower one.
    const EDGES: &'static [Self];

    /// A small value, the same in every width.
    fn small(value: i16) -> Self;

    /// Any value of the type, uniformly.
    fn any() -> BoxedStrategy<Self>;
}

impl Int for i32 {
    const EDGES: &'static [Self] = &[i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];

    fn small(value: i16) -> Self {
        value.into()
    }

    fn any() -> BoxedStrategy<Self> {
        proptest::num::i32::ANY.boxed()
    }
}

impl Int for i64 {
    const EDGES: &'static [Self] = &[
        i64::MIN,
        i64::MIN + 1,
        i32::MIN as i64 - 1,
        i32::MIN as i64,
        -1,
        0,
        1,
        i32::MAX as i64,
        i32::MAX as i64 + 1,
        i64::MAX - 1,
        i64::MAX,
    ];

    fn small(value: i16) -> Self {
        value.into()
    }

    fn any() -> BoxedStrategy<Self> {
        proptest::num::i64::ANY.boxed()
    }
}

/// One of the type's [`Int::EDGES`].
pub fn edge<T: Int>() -> BoxedStrategy<T> {
    select(T::EDGES).boxed()
}

/// A value from -1000 to 1000, which no sum, difference or product of two
/// such values takes out of any width.
pub fn small<T: Int>() -> BoxedStrategy<T> {
    (-1000_i16..=1000).prop_map(T::small).boxed()
}

/// A value of the type leaning to its edges: an edge one time in four, a
/// small value one time in two, any value otherwise.
pub fn integer<T: Int>() -> BoxedStrategy<T> {
    prop_oneof![1 => edge::<T>(), 2 => small::<T>(), 1 => T::any()].boxed()
}

/// The value and scale of a decimal of at most 18 digits: values at the
/// digit edges (0, ±1, ±10^k, ±(10^k − 1)) one time in two, otherwise any
/// value of a random digit count; scales of 0, of `max_scale` and of
/// anything up to it.
pub fn decimal_parts(max_scale: u32) -> BoxedStrategy<(i64, u32)> {
    let power = |digits: u32| 10_i64.pow(digits);
    let edge = (0..=18_u32, -1_i64..=0, any::<bool>()).prop_map(move |(digits, step, negative)| {
        let magnitude = (power(digits) + step).min(power(18) - 1);
        if negative { -magnitude } else { magnitude }
    });
    let spread =
        (0..=18_u32).prop_flat_map(move |digits| -(power(digits) - 1)..power(digits).max(1));
    let scale = prop_oneof![Just(0), Just(max_scale), 0..=max_scale];
    (prop_oneof![edge, spread], scale).boxed()
}

/// A batch's row count: none or a few, around one word and around two, or
/// anything up to three words and a tail.
pub fn nrows() -> BoxedStrategy<usize> {
    prop_oneof![0..=3_usize, 60..=70_usize, 125..=131_usize, 0..=200_usize].boxed()
}

/// How many rows of a 64-row word a mask holds.
#[derive(Clone, Copy, Debug)]
enum Density {
    None,
    Few,
    Half,
    Most,
    All,
}

impl Density {
    fn flag(self) -> BoxedStrategy<bool> {
        match self {
            Self::None => Just(false).boxed(),
            Self::Few => proptest::bool::weighted(1.0 / 16.0).boxed(),
            Self::Half => proptest::bool::weighted(0.5).boxed(),
            Self::Most => proptest::bool::weighted(15.0 / 16.0).boxed(),
            Self::All => Just(true).boxed(),
        }
    }
}

/// A flag for each of `nrows` rows, each 64-row word at a density of its
/// own: none, a few, half, most or all of its rows, so that whole words,
/// empty words, single rows and gaps all occur in one batch.
pub fn flags(nrows: usize) -> BoxedStrategy<Vec<bool>> {
    const DENSITIES: &[Density] = &[
        Density::None,
        Density::Few,
        Density::Half,
        Density::Most,
        Density::All,
    ];
    proptest::collection::vec(select(DENSITIES), nrows.div_ceil(64))
        .prop_flat_map(move |densities| {
            densities
                .into_iter()
                .enumerate()
                .map(|(word, density)| {
                    let len = (nrows - 64 * word).min(64);
                    proptest::collection::vec(density.flag(), len)
                })
                .collect::<Vec<_>>()
        })
        .prop_map(|words| words.concat())
        .boxed()
}

/// A value for each row: drawn from `live` where `live_rows` holds, one of
/// the type's edges elsewhere — the NULL and unselected rows, whose values a
/// kernel must not read into its result or its errors.
pub fn values<T: Int>(live_rows: &[bool], live: &BoxedStrategy<T>) -> BoxedStrategy<Vec<T>> {
    live_rows
        .iter()
        .map(
            |&live_row| {
                if live_row { live.clone() } else { edge::<T>() }
            },
        )
        .collect::<Vec<_>>()
        .boxed()
}

/// The words of a mask of `flags`: row `r` at bit `r % 64` of word `r / 64`.
pub fn words(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        words[row / 64] |= u64::from(flag) << (row % 64);
    }
    words
}

/// Run `test` on the cases of `strategy` as `proptest!` does, and panic
/// with the smallest failing case. The caller's file keeps the failing cases,
/// in `<file>.proptest-regressions` beside it.
#[track_caller]
#[allow(
    clippy::panic,
    reason = "a failed property fails its test, as an assertion does"
)]
pub fn property<S: Strategy, E: Debug>(strategy: S, test: impl Fn(S::Value) -> Result<(), E>) {
    let config = ProptestConfig {
        source_file: Some(Location::caller().file()),
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource(
            "proptest-regressions",
        ))),
        ..ProptestConfig::default()
    };
    let outcome = TestRunner::new(config).run(&strategy, |case| {
        test(case).map_err(|error| TestCaseError::fail(format!("{error:?}")))
    });
    if let Err(failure) = outcome {
        panic!("{failure}");
    }
}
