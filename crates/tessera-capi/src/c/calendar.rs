//! The calendar entry points, declared in `include/tessera/calendar.h`:
//! dates (an int4 Datum), timestamps (an int8 Datum) and intervals (a
//! pointer to PostgreSQL's 16-byte `Interval`) computed by
//! [`tessera_kernels::calendar`].
//!
//! # Arguments
//!
//! A `TessCalendarArg` points to a column or holds a scalar Datum. A column
//! must be a valid `TessDatumColumn` of the call's row count whose `values`
//! and `isnull` hold that many elements, unchanged for the call; a selected
//! row's flag must be initialized and, when not NULL, its value: a date, a
//! timestamp, or a pointer to an interval. A scalar interval must point to
//! an interval.

use std::ffi::c_int;
use std::marker::PhantomData;

use anyhow::{Context, Result, bail, ensure};
use tessera_kernels::calendar::{self, DateOp, Field, Fields, Interval, Source, Unit};

use super::column::DatumColumn;
use super::mask::Mask;
use super::source::{self, Constant, output, selection, slots, with_source, with_sources};
use super::status::{Code, Status, guard};

/// `TessCalendarArg`: a column, or a scalar Datum when it is null.
#[repr(C)]
#[derive(Debug)]
pub struct CalendarArg {
    /// The column, or null.
    pub column: *const DatumColumn,
    /// The scalar when `column` is null.
    pub scalar: u64,
}

/// A value read from its Datum.
trait FromDatum: Copy {
    fn from_datum(datum: u64) -> Self;
}

impl FromDatum for i32 {
    #[inline(always)]
    fn from_datum(datum: u64) -> Self {
        datum as i32
    }
}

impl FromDatum for i64 {
    #[inline(always)]
    fn from_datum(datum: u64) -> Self {
        datum as i64
    }
}

impl FromDatum for Interval {
    /// The interval the Datum points to.
    #[inline(always)]
    fn from_datum(datum: u64) -> Self {
        // SAFETY: the constructor's caller guarantees a pointer to an
        // interval for every value read as one.
        unsafe { (datum as usize as *const Interval).read_unaligned() }
    }
}

/// A column's values and flags, read through raw pointers behind one check
/// of the row against the row count.
struct Values<'a, T> {
    values: *const u64,
    isnull: *const u8,
    nrows: usize,
    borrow: PhantomData<(&'a (), T)>,
}

impl<T: FromDatum> Source<T> for Values<'_, T> {
    #[inline(always)]
    fn get(&self, row: usize) -> Option<T> {
        assert!(row < self.nrows, "a calendar row past its column");
        // SAFETY: the row is within the arrays, and the calls read selected
        // rows only, whose flags and non-NULL values the constructor's
        // caller guarantees; a flag is read as its byte.
        unsafe {
            if *self.isnull.add(row) != 0 {
                return None;
            }
            Some(T::from_datum(*self.values.add(row)))
        }
    }
}

impl<T: Copy> Source<T> for Constant<T> {
    #[inline(always)]
    fn get(&self, _row: usize) -> Option<T> {
        Some(self.0)
    }
}

/// A call's argument: a column or a scalar, a loop for each.
type Input<'a, T> = source::Input<Values<'a, T>, T>;

/// Borrow a column of `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of a column.
#[inline]
unsafe fn column<'a, T>(column: *const DatumColumn, nrows: usize) -> Result<Values<'a, T>> {
    // SAFETY: the caller's contract.
    let column = unsafe { column.as_ref() }.context("a null column")?;
    ensure!(
        column.struct_size >= DatumColumn::MIN_SIZE,
        "a Datum column is smaller than its required fields"
    );
    ensure!(
        usize::try_from(column.nrows).ok() == Some(nrows),
        "a calendar column has another row count than its rows"
    );
    ensure!(
        nrows == 0 || (!column.values.is_null() && !column.isnull.is_null()),
        "a column has null buffers"
    );
    Ok(Values {
        values: column.values,
        isnull: column.isnull.cast(),
        nrows,
        borrow: PhantomData,
    })
}

/// The argument of a call with `nrows` rows.
///
/// # Safety
///
/// As the module documentation says of an argument.
#[inline]
unsafe fn input<'a, T: FromDatum>(arg: *const CalendarArg, nrows: usize) -> Result<Input<'a, T>> {
    // SAFETY: the caller's contract.
    unsafe {
        let arg = arg.as_ref().context("a null calendar argument")?;
        Ok(if arg.column.is_null() {
            Input::Scalar(T::from_datum(arg.scalar))
        } else {
            Input::Column(column(arg.column, nrows)?)
        })
    }
}

fn unit_of(unit: c_int) -> Result<Unit> {
    Ok(match unit {
        0 => Unit::Microsecond,
        1 => Unit::Millisecond,
        2 => Unit::Second,
        3 => Unit::Minute,
        4 => Unit::Hour,
        5 => Unit::Day,
        6 => Unit::Week,
        7 => Unit::Month,
        8 => Unit::Quarter,
        9 => Unit::Year,
        10 => Unit::Decade,
        11 => Unit::Century,
        12 => Unit::Millennium,
        _ => bail!("an unknown date_trunc unit {unit}"),
    })
}

fn field_of(field: c_int) -> Result<Field> {
    Ok(match field {
        0 => Field::Microsecond,
        1 => Field::Millisecond,
        2 => Field::Second,
        3 => Field::Minute,
        4 => Field::Hour,
        5 => Field::Day,
        6 => Field::Month,
        7 => Field::Quarter,
        8 => Field::Week,
        9 => Field::Year,
        10 => Field::Decade,
        11 => Field::Century,
        12 => Field::Millennium,
        13 => Field::IsoYear,
        14 => Field::Dow,
        15 => Field::IsoDow,
        16 => Field::Doy,
        17 => Field::Julian,
        18 => Field::Epoch,
        _ => bail!("an unknown extract field {field}"),
    })
}

/// `tess_date_arith`: date + integer (op 0), date - integer (1) or date -
/// date (2) over the selected rows.
///
/// # Safety
///
/// `left` and `right` must be valid arguments (see the module
/// documentation) of `rows`' row count, `values` point to as many writable
/// int32 slots and `non_nulls` to a valid mask, none accessed by anything
/// else for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_date_arith(
    op: c_int,
    left: *const CalendarArg,
    right: *const CalendarArg,
    rows: *const Mask,
    values: *mut i32,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let op = match op {
                0 => DateOp::PlusDays,
                1 => DateOp::MinusDays,
                2 => DateOp::MinusDate,
                _ => bail!("an unknown date operation {op}"),
            };
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let (left, right) = (input::<i32>(left, nrows)?, input::<i32>(right, nrows)?);
            let values = slots(values, nrows)?;
            let mut non_nulls = output(non_nulls)?;
            with_sources!(left, right, |left, right| calendar::date_arith(
                op,
                left,
                right,
                rows,
                values,
                &mut non_nulls
            ))
        })
    }
}

/// `tess_date_to_timestamp`: `timestamp(date)` over the selected rows.
///
/// # Safety
///
/// As for [`tess_date_arith`], one argument, `values` of int8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_date_to_timestamp(
    arg: *const CalendarArg,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let arg = input::<i32>(arg, rows.nrows())?;
            let values = slots(values, rows.nrows())?;
            let mut non_nulls = output(non_nulls)?;
            with_source!(arg, |source| calendar::dates_to_timestamps(
                source,
                rows,
                values,
                &mut non_nulls
            ))
        })
    }
}

/// `tess_timestamp_to_date`: `date(timestamp)` over the selected rows.
///
/// # Safety
///
/// As for [`tess_date_arith`], one argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_timestamp_to_date(
    arg: *const CalendarArg,
    rows: *const Mask,
    values: *mut i32,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let arg = input::<i64>(arg, rows.nrows())?;
            let values = slots(values, rows.nrows())?;
            let mut non_nulls = output(non_nulls)?;
            with_source!(arg, |source| calendar::timestamps_to_dates(
                source,
                rows,
                values,
                &mut non_nulls
            ))
        })
    }
}

/// `tess_timestamp_trunc`: `date_trunc(unit, timestamp)` over the selected
/// rows, `unit` a `TessCalendarUnit`.
///
/// # Safety
///
/// As for [`tess_date_to_timestamp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_timestamp_trunc(
    unit: c_int,
    arg: *const CalendarArg,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let unit = unit_of(unit)?;
            let rows = selection(rows)?;
            let arg = input::<i64>(arg, rows.nrows())?;
            let values = slots(values, rows.nrows())?;
            let mut non_nulls = output(non_nulls)?;
            with_source!(arg, |source| calendar::truncate_timestamps(
                unit,
                source,
                rows,
                values,
                &mut non_nulls
            ))
        })
    }
}

/// `tess_timestamp_trunc_local`: local times truncated to a unit, a time or
/// a first day's Julian day (set in `days`), the rest left in `rest`.
///
/// # Safety
///
/// `locals` must be a valid column (see the module documentation) of
/// `rows`' row count, `values` point to as many writable int8 slots,
/// `days` and `rest` to valid masks, none accessed by anything else for
/// the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_timestamp_trunc_local(
    unit: c_int,
    locals: *const DatumColumn,
    rows: *const Mask,
    values: *mut i64,
    days: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let unit = unit_of(unit)?;
            let rows = selection(rows)?;
            let locals = column::<i64>(locals, rows.nrows())?;
            calendar::truncate_locals(
                unit,
                &locals,
                rows,
                slots(values, rows.nrows())?,
                &mut output(days)?,
                &mut output(rest)?,
            )
        })
    }
}

/// `tess_timestamp_add_interval`: a timestamp plus (or, `minus`, minus) an
/// interval over the selected rows.
///
/// # Safety
///
/// As for [`tess_date_arith`], `left` of timestamps, `right` of intervals,
/// `values` of int8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_timestamp_add_interval(
    minus: bool,
    left: *const CalendarArg,
    right: *const CalendarArg,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let left = input::<i64>(left, nrows)?;
            let right = input::<Interval>(right, nrows)?;
            let values = slots(values, nrows)?;
            let mut non_nulls = output(non_nulls)?;
            with_sources!(left, right, |left, right| calendar::add_intervals(
                minus,
                left,
                right,
                rows,
                values,
                &mut non_nulls
            ))
        })
    }
}

/// `tess_date_add_interval`: a date plus (or, `minus`, minus) an interval
/// over the selected rows, a timestamp.
///
/// # Safety
///
/// As for [`tess_date_arith`], `left` of dates, `right` of intervals,
/// `values` of int8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_date_add_interval(
    minus: bool,
    left: *const CalendarArg,
    right: *const CalendarArg,
    rows: *const Mask,
    values: *mut i64,
    non_nulls: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = selection(rows)?;
            let nrows = rows.nrows();
            let left = input::<i32>(left, nrows)?;
            let right = input::<Interval>(right, nrows)?;
            let values = slots(values, nrows)?;
            let mut non_nulls = output(non_nulls)?;
            with_sources!(left, right, |left, right| calendar::add_intervals_to_dates(
                minus,
                left,
                right,
                rows,
                values,
                &mut non_nulls
            ))
        })
    }
}

/// The outputs of the extract entry points.
///
/// # Safety
///
/// `values` and `scales` must point to `nrows` writable slots, `non_nulls`
/// and `rest` to valid masks, none accessed by anything else for `'a`.
unsafe fn fields<'a>(
    nrows: usize,
    values: *mut i64,
    scales: *mut u8,
    non_nulls: *mut Mask,
    rest: *mut Mask,
) -> Result<Fields<'a>> {
    // SAFETY: the caller's contract.
    unsafe {
        Ok(Fields {
            values: slots(values, nrows)?,
            scales: slots(scales, nrows)?,
            non_nulls: output(non_nulls)?,
            rest: output(rest)?,
        })
    }
}

/// `tess_date_extract`: `extract(field from date)` over the selected rows,
/// `field` a `TessCalendarField`; an infinite date left in `rest`.
///
/// # Safety
///
/// `arg` must be a valid argument (see the module documentation) of
/// `rows`' row count, `values` and `scales` point to as many writable
/// slots, `non_nulls` and `rest` to valid masks, none accessed by anything
/// else for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_date_extract(
    field: c_int,
    arg: *const CalendarArg,
    rows: *const Mask,
    values: *mut i64,
    scales: *mut u8,
    non_nulls: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let field = field_of(field)?;
            let rows = selection(rows)?;
            let arg = input::<i32>(arg, rows.nrows())?;
            let mut out = fields(rows.nrows(), values, scales, non_nulls, rest)?;
            with_source!(arg, |source| calendar::extract_dates(
                field, source, rows, &mut out
            ))
        })
    }
}

/// `tess_timestamp_extract`: `extract(field from timestamp)` over the
/// selected rows of timestamps or local times; an infinite one or one
/// before the Julian days left in `rest`.
///
/// # Safety
///
/// As for [`tess_date_extract`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_timestamp_extract(
    field: c_int,
    arg: *const CalendarArg,
    rows: *const Mask,
    values: *mut i64,
    scales: *mut u8,
    non_nulls: *mut Mask,
    rest: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let field = field_of(field)?;
            let rows = selection(rows)?;
            let arg = input::<i64>(arg, rows.nrows())?;
            let mut out = fields(rows.nrows(), values, scales, non_nulls, rest)?;
            with_source!(arg, |source| calendar::extract_timestamps(
                field, source, rows, &mut out
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::CalendarArg;
    use tessera_kernels::calendar::Interval;

    #[test]
    fn layout_matches_the_header() {
        assert_eq!(size_of::<CalendarArg>(), 16);
        assert_eq!(size_of::<Interval>(), 16);
        assert_eq!(std::mem::offset_of!(Interval, month), 12);
    }
}
