/*
 * The calendar of dates and timestamps: the C entry points of the Rust
 * kernels (tessera_kernels::calendar) for the date and timestamp functions
 * of kernels/date.c. A date is an int4 Datum (days since 2000-01-01), a
 * timestamp an int8 Datum (microseconds since then), both with an infinity
 * at each end, and an interval a pointer to the core's Interval.
 *
 * The Julian day routines are the core's own arithmetic (date2j, j2date,
 * j2day, date2isoweek, date2isoyear), so a day maps as the core maps it.
 * Each call fails where the core's function raises, with
 * TESS_ERROR_DATA_EXCEPTION and its message and SQLSTATE 22008 in the
 * status, at the first row the core would; an infinite value stays
 * infinite where the core keeps it. Time zones, unit names and the rows
 * left to the core stay with the caller.
 *
 * The calls follow tessera/kernels.h: they never raise ERROR nor call
 * PostgreSQL, and return a status. They read the selected rows alone,
 * whose NULL flags and non-NULL values must be initialized; every mask
 * has the selection's row count, and so has every array. A result mask
 * gets every word written: the selected rows without NULL.
 */
#ifndef TESSERA_CALENDAR_H
#define TESSERA_CALENDAR_H

#include "postgres.h"

#include "tessera/kernels.h"

/* An argument: a column, or a scalar Datum without one. */
typedef struct TessCalendarArg
{
	const TessDatumColumn *column;
	Datum		scalar;
} TessCalendarArg;

/* An operation of tess_date_arith. */
typedef enum TessDateOp
{
	TESS_DATE_PLUS_DAYS = 0,
	TESS_DATE_MINUS_DAYS = 1,
	TESS_DATE_MINUS_DATE = 2
} TessDateOp;

/* A unit of date_trunc. */
typedef enum TessCalendarUnit
{
	TESS_UNIT_MICROSECOND = 0,
	TESS_UNIT_MILLISECOND = 1,
	TESS_UNIT_SECOND = 2,
	TESS_UNIT_MINUTE = 3,
	TESS_UNIT_HOUR = 4,
	TESS_UNIT_DAY = 5,
	TESS_UNIT_WEEK = 6,
	TESS_UNIT_MONTH = 7,
	TESS_UNIT_QUARTER = 8,
	TESS_UNIT_YEAR = 9,
	TESS_UNIT_DECADE = 10,
	TESS_UNIT_CENTURY = 11,
	TESS_UNIT_MILLENNIUM = 12
} TessCalendarUnit;

/*
 * A field of extract: the microseconds, milliseconds (scale 3) and seconds
 * (scale 6) of the minute, the minute, the hour, the day of the month, the
 * month, the quarter, the ISO week, the year (no year 0), the decade,
 * century and millennium, the ISO year, the day of the week (Sunday 0),
 * the ISO day of the week (Sunday 7), the day of the year, a date's Julian
 * day and a date's epoch.
 */
typedef enum TessCalendarField
{
	TESS_FIELD_MICROSECOND = 0,
	TESS_FIELD_MILLISECOND = 1,
	TESS_FIELD_SECOND = 2,
	TESS_FIELD_MINUTE = 3,
	TESS_FIELD_HOUR = 4,
	TESS_FIELD_DAY = 5,
	TESS_FIELD_MONTH = 6,
	TESS_FIELD_QUARTER = 7,
	TESS_FIELD_WEEK = 8,
	TESS_FIELD_YEAR = 9,
	TESS_FIELD_DECADE = 10,
	TESS_FIELD_CENTURY = 11,
	TESS_FIELD_MILLENNIUM = 12,
	TESS_FIELD_ISOYEAR = 13,
	TESS_FIELD_DOW = 14,
	TESS_FIELD_ISODOW = 15,
	TESS_FIELD_DOY = 16,
	TESS_FIELD_JULIAN = 17,
	TESS_FIELD_EPOCH = 18
} TessCalendarField;

/*
 * date + integer, date - integer ("date out of range" past the dates, an
 * infinite date kept) and date - date ("cannot subtract infinite dates"),
 * as date_pli, date_mii and date_mi, into int32 values.
 */
extern TessStatusCode tess_date_arith(TessDateOp op,
									  const TessCalendarArg *left,
									  const TessCalendarArg *right,
									  const TessRowMask *rows,
									  int32 *values,
									  TessRowMask *non_nulls,
									  TessStatus *status);

/* timestamp(date): "date out of range for timestamp" past the timestamps. */
extern TessStatusCode tess_date_to_timestamp(const TessCalendarArg *arg,
											 const TessRowMask *rows,
											 int64 *values,
											 TessRowMask *non_nulls,
											 TessStatus *status);

/*
 * The selected rows of a date column whose non-NULL date satisfies
 * date op timestamp, as the core compares them (date_cmp_timestamp): rows
 * is narrowed to them. The timestamp becomes once an operator and a bound
 * in days, and the dates are filtered as int4 values: an infinite
 * timestamp is the infinite date of its sign, a finite one its day,
 * rounded down, and the time into it; a finite date past the last
 * timestamp stays above every finite bound and below infinity.
 */
extern TessStatusCode tess_date_timestamp_filter(const TessDatumColumn *column,
												 const TessRowMask *prepared,
												 TessRowMask *rows,
												 TessCompareOp op,
												 int64 timestamp,
												 TessStatus *status);

/* date(timestamp): the day, rounded down. */
extern TessStatusCode tess_timestamp_to_date(const TessCalendarArg *arg,
											 const TessRowMask *rows,
											 int32 *values,
											 TessRowMask *non_nulls,
											 TessStatus *status);

/*
 * date_trunc(unit, timestamp), as timestamp_trunc: "timestamp out of
 * range" below the first timestamp, an infinity kept.
 */
extern TessStatusCode tess_timestamp_trunc(TessCalendarUnit unit,
										   const TessCalendarArg *arg,
										   const TessRowMask *rows,
										   int64 *values,
										   TessRowMask *non_nulls,
										   TessStatus *status);

/*
 * Local times (a timestamptz's in its zone) truncated to a unit, as
 * timestamptz_trunc_internal truncates their fields: a unit below a day
 * gives the truncated local time in values; a day and above the Julian
 * day of the period's first day in values, its bit set in days, for the
 * caller to find its midnight in the zone. An infinite time, or one before
 * the Julian days, is set in rest for the core; NULL rows in no mask.
 */
extern TessStatusCode tess_timestamp_trunc_local(TessCalendarUnit unit,
												 const TessDatumColumn *locals,
												 const TessRowMask *rows,
												 int64 *values,
												 TessRowMask *days,
												 TessRowMask *rest,
												 TessStatus *status);

/*
 * A timestamp plus, or minus, an interval, as timestamp_pl_interval and
 * timestamp_mi_interval: months through the calendar clamped to the
 * month's last day, then days, then microseconds, "timestamp out of range"
 * past the timestamps, "interval out of range" when the negation
 * overflows; an infinite interval makes its infinity.
 */
extern TessStatusCode tess_timestamp_add_interval(bool minus,
												  const TessCalendarArg *left,
												  const TessCalendarArg *right,
												  const TessRowMask *rows,
												  int64 *values,
												  TessRowMask *non_nulls,
												  TessStatus *status);

/*
 * A date plus, or minus, an interval, as date_pl_interval and
 * date_mi_interval: the date's midnight ("date out of range for
 * timestamp" past the timestamps) plus the interval as above, each row's
 * error in the rows' order.
 */
extern TessStatusCode tess_date_add_interval(bool minus,
											 const TessCalendarArg *left,
											 const TessCalendarArg *right,
											 const TessRowMask *rows,
											 int64 *values,
											 TessRowMask *non_nulls,
											 TessStatus *status);

/*
 * extract(field from date) over the selected rows: a finite date's field
 * as a value at the scale in scales, an infinite date set in rest.
 */
extern TessStatusCode tess_date_extract(TessCalendarField field,
										const TessCalendarArg *arg,
										const TessRowMask *rows,
										int64 *values,
										uint8 *scales,
										TessRowMask *non_nulls,
										TessRowMask *rest,
										TessStatus *status);

/*
 * extract(field from timestamp), of timestamps or local times, as
 * tess_date_extract: an infinite value or one before the Julian days set
 * in rest.
 */
extern TessStatusCode tess_timestamp_extract(TessCalendarField field,
											 const TessCalendarArg *arg,
											 const TessRowMask *rows,
											 int64 *values,
											 uint8 *scales,
											 TessRowMask *non_nulls,
											 TessRowMask *rest,
											 TessStatus *status);

/*
 * The local time of a timestamp with time zone in the session's zone
 * (tessera_kernels::calendar::zone). Only PostgreSQL's time zone library
 * knows the zone's transitions, and the kernels never call PostgreSQL: a
 * call answers what its cache knows, sorts each selected non-NULL row into
 * ready (its value written), core (for the core's function: an infinity
 * the operation does not keep, a day before the Julian day 0, a midnight
 * the zone cannot give, a result out of range) or unknown, and lists the
 * distinct questions of the unknown rows, sorted: the spans first, by
 * second, then the midnights, by day. The caller asks the library, adds
 * the answers to those it has, and calls again over the unknown rows with
 * all of them; a call never asks a question that an answer it was given
 * covers, so the rounds end.
 *
 * The cache is the caller's: 32 spans of the zone and 1024 local midnights
 * by day, its zero bytes an empty cache. The caller keeps the zone's
 * identity and resets the cache when the session's zone changes.
 */
#define TESS_ZONE_CACHE_WORDS (3 + 32 * 3 + 1024 * 3)

typedef struct TessZoneCache
{
	uint64		words[TESS_ZONE_CACHE_WORDS];
} TessZoneCache;

/* What a question asks, and an answer tells. */
typedef enum TessZoneKind
{
	/* the offset at a Unix second and the next transition: pg_next_dst_boundary */
	TESS_ZONE_SPAN = 0,
	/* the offset west of UTC of a Julian day's local midnight: DetermineTimeZoneOffset */
	TESS_ZONE_MIDNIGHT = 1
} TessZoneKind;

typedef struct TessZoneQuestion
{
	int64		key;			/* a Unix second, or a Julian day */
	int32		kind;			/* TessZoneKind */
} TessZoneQuestion;

typedef struct TessZoneAnswer
{
	int64		key;			/* the asked second, or the Julian day */
	int64		offset;			/* seconds east of UTC at the second, or west
								 * of UTC of the midnight */
	int64		boundary;		/* the next transition, when found is 1 */
	int32		kind;			/* TessZoneKind */
	int32		found;			/* a span: pg_next_dst_boundary's result, 1 a
								 * transition at boundary, 0 none, -1 a
								 * failure, offset then pg_localtime's */
} TessZoneAnswer;

/*
 * One call of a round. ready and core gain the rows the call settles and
 * keep their other bits; unknown gets every word of rows written;
 * questions, room for rows' row count, gets the questions, nquestions
 * their count.
 */
typedef struct TessZoneRound
{
	size_t		struct_size;
	TessZoneCache *cache;
	const TessZoneAnswer *answers;	/* every answer of the batch so far */
	int			nanswers;
	const TessRowMask *rows;	/* the rows to settle */
	TessRowMask *ready;
	TessRowMask *core;
	TessRowMask *unknown;
	TessZoneQuestion *questions;
	int			nquestions;		/* out */
} TessZoneRound;

/* Forget everything the cache keeps: the zone changed. */
extern TessStatusCode tess_zone_reset(TessZoneCache *cache, TessStatus *status);

/* The local times of a timestamptz argument into locals, for extract. */
extern TessStatusCode tess_zone_locals(TessZoneRound *round,
									   const TessCalendarArg *instants,
									   int64 *locals,
									   TessStatus *status);

/*
 * date_trunc(unit, timestamptz), as timestamptz_trunc_internal: a unit
 * below a day keeps the instant's offset, a day or more starts at the
 * local midnight of its first day.
 */
extern TessStatusCode tess_zone_truncate(TessZoneRound *round,
										 TessCalendarUnit unit,
										 const TessCalendarArg *instants,
										 int64 *values,
										 TessStatus *status);

/* timestamptz(date), a date's local midnight. */
extern TessStatusCode tess_zone_midnights(TessZoneRound *round,
										  const TessCalendarArg *dates,
										  int64 *values,
										  TessStatus *status);

/* date(timestamptz), the day of an instant's local time. */
extern TessStatusCode tess_zone_dates(TessZoneRound *round,
									  const TessCalendarArg *instants,
									  int32 *values,
									  TessStatus *status);

#endif							/* TESSERA_CALENDAR_H */
