/*
 * The date and timestamp functions as batch functions, row by row in C
 * over the words of the columns, a date its days since 2000-01-01 and a
 * timestamp its microseconds: a date plus or minus days and the days
 * between two dates, the casts between date and timestamp, and date_trunc
 * of a timestamp, a timestamp or a date plus or minus an interval. Each
 * raises what the core's function raises, at the
 * first row it would: 22008 "date out of range" past the dates, "cannot
 * subtract infinite dates", "timestamp out of range" for a truncation
 * below the first timestamp. An infinite value stays infinite where the
 * core keeps it.
 *
 * An interval adds its months through the calendar, clamped to the
 * month's last day, then its days through the Julian day, then its
 * microseconds, each step checked, as timestamp_pl_interval does; a date
 * becomes its timestamp first, as date_pl_interval makes it.
 *
 * extract of a date or a timestamp, a numeric: the integer fields of a
 * finite value by calendar arithmetic (a numeric of a small integer from
 * the cache of numeric.c), seconds and milliseconds of a timestamp with
 * their fraction; an infinite value, the Julian day and epoch of a
 * timestamp, a unit it does not know or a unit that is a column by the
 * core's function a row, its NULL for an oscillating field of infinity
 * kept.
 *
 * A timestamp with time zone is read in the session's zone: its local
 * time is the instant plus the offset pg_localtime gives, as timestamp2tm
 * reads it, the offset kept for the span of instants up to the zone's
 * next transition that pg_next_dst_boundary names; a local day's midnight is the instant DetermineTimeZoneOffset
 * makes of it, kept for the next rows of that day in a cache of the
 * process for the zone. So date_trunc('month', d), where the date becomes
 * a timestamptz, converts each date once and each month once. Where the
 * Julian day routines do not reach or a result falls out of range, the
 * core's function takes the row and raises its own error.
 *
 * date_trunc parses its unit once a call, as the core does a row, and
 * truncates by calendar arithmetic: the time units by the microseconds of
 * the unit, a week to its Monday, the larger units through the year,
 * month and day of the Julian day. A unit it does not know, or a unit
 * that is a column, goes to the core's function a row, which raises the
 * core's own errors.
 */
#include "postgres.h"

#include "common/int.h"
#include "datatype/timestamp.h"
#include "fmgr.h"
#include "parser/scansup.h"
#include "pgtime.h"
#include "port/pg_bitutils.h"
#include "utils/builtins.h"
#include "utils/date.h"
#include "utils/datetime.h"
#include "utils/fmgroids.h"
#include "utils/memutils.h"
#include "utils/numeric.h"
#include "utils/timestamp.h"
#include "varatt.h"

#include "tessera/bridge.h"

#include "internal.h"

typedef enum DateOp
{
	DATE_PLUS_DAYS,
	DATE_MINUS_DAYS,
	DATE_MINUS_DATE,
	TIMESTAMP_TO_DATE,
	DATE_TO_TIMESTAMP,
	TIMESTAMP_TRUNC,
	TIMESTAMP_PLUS_INTERVAL,
	TIMESTAMP_MINUS_INTERVAL,
	DATE_PLUS_INTERVAL,
	DATE_MINUS_INTERVAL,
	DATE_EXTRACT,
	TIMESTAMP_EXTRACT,
	TIMESTAMPTZ_TRUNC,
	TIMESTAMPTZ_EXTRACT,
	DATE_TO_TIMESTAMPTZ,
	TIMESTAMPTZ_TO_DATE
} DateOp;

typedef struct DateFunction
{
	TessFunction function;
	DateOp		op;
} DateFunction;

static TessStatusCode date_evaluate(TessFunctionCall *call);
static TessStatusCode trunc_evaluate(TessFunctionCall *call);
static TessStatusCode interval_evaluate(TessFunctionCall *call);
static TessStatusCode extract_evaluate(TessFunctionCall *call);
static TessStatusCode zone_cast_evaluate(TessFunctionCall *call);

#define DATE_FUNCTION(oid, code, format, evaluator) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = (format), \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | \
			   TESS_FUNCTION_ANY_SHAPE, \
	  .evaluate = (evaluator)}, (code)}

static const DateFunction date_functions[] = {
	DATE_FUNCTION(F_DATE_PLI, DATE_PLUS_DAYS, TESS_RESULT_INT32, date_evaluate),
	DATE_FUNCTION(F_DATE_MII, DATE_MINUS_DAYS, TESS_RESULT_INT32, date_evaluate),
	DATE_FUNCTION(F_DATE_MI, DATE_MINUS_DATE, TESS_RESULT_INT32, date_evaluate),
	DATE_FUNCTION(F_DATE_TIMESTAMP, TIMESTAMP_TO_DATE, TESS_RESULT_INT32, date_evaluate),
	DATE_FUNCTION(F_TIMESTAMP_DATE, DATE_TO_TIMESTAMP, TESS_RESULT_DATUM, date_evaluate),
	DATE_FUNCTION(F_DATE_TRUNC_TEXT_TIMESTAMP, TIMESTAMP_TRUNC, TESS_RESULT_DATUM,
				  trunc_evaluate),
	DATE_FUNCTION(F_TIMESTAMP_PL_INTERVAL, TIMESTAMP_PLUS_INTERVAL, TESS_RESULT_DATUM,
				  interval_evaluate),
	DATE_FUNCTION(F_TIMESTAMP_MI_INTERVAL, TIMESTAMP_MINUS_INTERVAL, TESS_RESULT_DATUM,
				  interval_evaluate),
	DATE_FUNCTION(F_DATE_PL_INTERVAL, DATE_PLUS_INTERVAL, TESS_RESULT_DATUM,
				  interval_evaluate),
	DATE_FUNCTION(F_DATE_MI_INTERVAL, DATE_MINUS_INTERVAL, TESS_RESULT_DATUM,
				  interval_evaluate),
	DATE_FUNCTION(F_EXTRACT_TEXT_DATE, DATE_EXTRACT, TESS_RESULT_DATUM, extract_evaluate),
	DATE_FUNCTION(F_EXTRACT_TEXT_TIMESTAMP, TIMESTAMP_EXTRACT, TESS_RESULT_DATUM,
				  extract_evaluate),
	DATE_FUNCTION(F_DATE_TRUNC_TEXT_TIMESTAMPTZ, TIMESTAMPTZ_TRUNC, TESS_RESULT_DATUM,
				  trunc_evaluate),
	DATE_FUNCTION(F_EXTRACT_TEXT_TIMESTAMPTZ, TIMESTAMPTZ_EXTRACT, TESS_RESULT_DATUM,
				  extract_evaluate),
	DATE_FUNCTION(F_TIMESTAMPTZ_DATE, DATE_TO_TIMESTAMPTZ, TESS_RESULT_DATUM,
				  zone_cast_evaluate),
	DATE_FUNCTION(F_DATE_TIMESTAMPTZ, TIMESTAMPTZ_TO_DATE, TESS_RESULT_INT32,
				  zone_cast_evaluate),
};

static DateOp
date_op(const TessFunctionCall *call)
{
	return ((const DateFunction *) ((const char *) call->function -
									offsetof(DateFunction, function)))->op;
}

static TessStatusCode
date_fail(TessFunctionCall *call, TessStatusCode code, const char *sqlstate,
		  const char *message)
{
	if (call != NULL && call->status != NULL &&
		call->status->struct_size >= TESS_STATUS_MIN_SIZE)
	{
		call->status->code = code;
		strlcpy(call->status->sqlstate, sqlstate, sizeof(call->status->sqlstate));
		strlcpy(call->status->message, message, sizeof(call->status->message));
	}
	return code;
}

static TessStatusCode
date_invalid(TessFunctionCall *call, const char *message)
{
	return date_fail(call, TESS_ERROR_INVALID_ARGUMENT, "XX000", message);
}

static TessStatusCode
date_out_of_range(TessFunctionCall *call, const char *message)
{
	return date_fail(call, TESS_ERROR_DATA_EXCEPTION, "22008", message);
}

static bool
date_call_valid(const TessFunctionCall *call, int nargs)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != nargs || call->args == NULL || call->rows == NULL ||
		call->values == NULL || call->non_nulls == NULL)
		return false;
	for (int arg = 0; arg < nargs; arg++)
		if (call->args[arg].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
			return false;
	return true;
}

/* An argument's Datum on a row: the column's or the scalar. */
static inline Datum
arg_datum(const TessFunctionArg *arg, int row)
{
	return arg->column != NULL ? arg->column->values[row] : arg->scalar;
}

static inline bool
arg_null(const TessFunctionArg *arg, int row)
{
	return arg->column != NULL && arg->column->isnull[row];
}

/* The day and the microseconds into it of a finite timestamp, rounded down. */
static inline int64
timestamp_day(Timestamp timestamp, int64 *time)
{
	int64		day = timestamp / USECS_PER_DAY;

	*time = timestamp - day * USECS_PER_DAY;
	if (*time < 0)
	{
		*time += USECS_PER_DAY;
		day--;
	}
	return day;
}

/* A date plus days, as date_pli: an infinite date stays, a finite one checked. */
static inline bool
date_plus(DateADT date, int32 days, DateADT *result)
{
	if (DATE_NOT_FINITE(date))
	{
		*result = date;
		return true;
	}
	if (pg_add_s32_overflow(date, days, result) || !IS_VALID_DATE(*result))
		return false;
	return true;
}

/*
 * date + integer, date - integer, date - date, date(timestamp) and
 * timestamp(date), any argument a column or a scalar.
 */
static TessStatusCode
date_evaluate(TessFunctionCall *call)
{
	DateOp		op;
	int			nargs;
	int32	   *ints;
	Datum	   *datums;
	int			nwords;

	op = call != NULL && call->function != NULL ? date_op(call) : DATE_PLUS_DAYS;
	nargs = op == TIMESTAMP_TO_DATE || op == DATE_TO_TIMESTAMP ? 1 : 2;
	if (!date_call_valid(call, nargs))
		return date_invalid(call, "a date function takes its arguments");
	ints = (int32 *) call->values;
	datums = (Datum *) call->values;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			Datum		left;
			DateADT		date;

			if (arg_null(&call->args[0], row) ||
				(nargs == 2 && arg_null(&call->args[1], row)))
				continue;
			left = arg_datum(&call->args[0], row);
			switch (op)
			{
				case DATE_PLUS_DAYS:
				case DATE_MINUS_DAYS:
					{
						int32		days = DatumGetInt32(arg_datum(&call->args[1], row));

						/* date - n is date + -n, but -INT_MIN overflows: add it in two. */
						if (op == DATE_MINUS_DAYS && days == PG_INT32_MIN)
						{
							if (!date_plus(DatumGetDateADT(left), PG_INT32_MAX, &date) ||
								!date_plus(date, 1, &date))
								goto out_of_range;
						}
						else if (!date_plus(DatumGetDateADT(left),
											op == DATE_MINUS_DAYS ? -days : days, &date))
							goto out_of_range;
						ints[row] = date;
						break;
					}
				case DATE_MINUS_DATE:
					{
						DateADT		right = DatumGetDateADT(arg_datum(&call->args[1], row));

						if (DATE_NOT_FINITE(DatumGetDateADT(left)) || DATE_NOT_FINITE(right))
						{
							call->non_nulls->bits[word] = present;
							return date_out_of_range(call, "cannot subtract infinite dates");
						}
						ints[row] = (int32) (DatumGetDateADT(left) - right);
						break;
					}
				case TIMESTAMP_TO_DATE:
					{
						Timestamp	timestamp = DatumGetTimestamp(left);
						int64		time;

						if (TIMESTAMP_IS_NOBEGIN(timestamp))
							DATE_NOBEGIN(date);
						else if (TIMESTAMP_IS_NOEND(timestamp))
							DATE_NOEND(date);
						else
							date = (DateADT) timestamp_day(timestamp, &time);
						ints[row] = date;
						break;
					}
				case DATE_TO_TIMESTAMP:
					{
						Timestamp	timestamp;

						date = DatumGetDateADT(left);
						if (DATE_IS_NOBEGIN(date))
							TIMESTAMP_NOBEGIN(timestamp);
						else if (DATE_IS_NOEND(date))
							TIMESTAMP_NOEND(timestamp);
						else if (date >= (TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE))
						{
							call->non_nulls->bits[word] = present;
							return date_out_of_range(call, "date out of range for timestamp");
						}
						else
							timestamp = (Timestamp) date * USECS_PER_DAY;
						datums[row] = TimestampGetDatum(timestamp);
						break;
					}
				default:
					return date_invalid(call, "not a date function");
			}
			present |= UINT64CONST(1) << bit;
			continue;
	out_of_range:
			call->non_nulls->bits[word] = present;
			return date_out_of_range(call, "date out of range");
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/*
 * The year and month that start the period of unit (DTK_MONTH and up)
 * holding them, as timestamp_trunc rounds years, 1 BC being year 0.
 */
static void
truncate_fields(int unit, int *year, int *month)
{
	if (unit == DTK_MILLENNIUM)
		*year = *year > 0 ? ((*year + 999) / 1000) * 1000 - 999 :
			-((999 - (*year - 1)) / 1000) * 1000 + 1;
	else if (unit == DTK_CENTURY)
		*year = *year > 0 ? ((*year + 99) / 100) * 100 - 99 :
			-((99 - (*year - 1)) / 100) * 100 + 1;
	else if (unit == DTK_DECADE)
		*year = *year > 0 ? (*year / 10) * 10 : -((8 - (*year - 1)) / 10) * 10;
	if (unit != DTK_QUARTER && unit != DTK_MONTH)
		*month = 1;
	else if (unit == DTK_QUARTER)
		*month = 3 * ((*month - 1) / 3) + 1;
}

/*
 * A finite timestamp truncated to unit (a DTK_* of DecodeUnits), as
 * timestamp_trunc truncates its fields: false below the first timestamp.
 */
static bool
truncate_timestamp(Timestamp timestamp, int unit, Timestamp *result)
{
	int64		time;
	int64		day = timestamp_day(timestamp, &time);
	int			year;
	int			month;
	int			mday;

	switch (unit)
	{
		case DTK_MICROSEC:
			*result = timestamp;
			return true;
		case DTK_MILLISEC:
			*result = day * USECS_PER_DAY + (time / 1000) * 1000;
			return true;
		case DTK_SECOND:
			*result = day * USECS_PER_DAY + (time / USECS_PER_SEC) * USECS_PER_SEC;
			return true;
		case DTK_MINUTE:
			*result = day * USECS_PER_DAY + (time / USECS_PER_MINUTE) * USECS_PER_MINUTE;
			return true;
		case DTK_HOUR:
			*result = day * USECS_PER_DAY + (time / USECS_PER_HOUR) * USECS_PER_HOUR;
			return true;
		case DTK_DAY:
			break;
		case DTK_WEEK:
			/* The Monday of the ISO week: 2000-01-01 was a Saturday, 5 days past one. */
			day -= ((day + 5) % 7 + 7) % 7;
			break;
		default:
			j2date((int) (day + POSTGRES_EPOCH_JDATE), &year, &month, &mday);
			truncate_fields(unit, &year, &month);
			if (!IS_VALID_JULIAN(year, month, 1))
				return false;
			day = date2j(year, month, 1) - POSTGRES_EPOCH_JDATE;
			break;
	}
	*result = day * USECS_PER_DAY;
	return IS_VALID_TIMESTAMP(*result);
}

/*
 * Spans of instants [start, end) over which the zone keeps one offset:
 * pg_next_dst_boundary gives the offset at an instant, the one localsub
 * picks for pg_localtime, and the next transition, where the span ends;
 * an earlier instant with the same next transition and offset lies in the
 * same segment and moves the span's start down.
 */
#define ZONE_SPANS 32

typedef struct ZoneSpan
{
	pg_time_t	start;
	pg_time_t	end;
	long int	offset;
} ZoneSpan;

static const pg_tz *span_zone;
static ZoneSpan spans[ZONE_SPANS];
static int	nspans;
static int	last_span;
static int	next_victim;

static long int
zone_offset(pg_time_t instant, const pg_tz *zone)
{
	long int	before;
	long int	after;
	int			before_isdst;
	int			after_isdst;
	pg_time_t	boundary;
	pg_time_t	end;
	int			found;
	int			slot;

	if (zone != span_zone)
	{
		nspans = 0;
		last_span = 0;
		span_zone = zone;
	}
	if (last_span < nspans && instant >= spans[last_span].start &&
		instant < spans[last_span].end)
		return spans[last_span].offset;
	for (int span = 0; span < nspans; span++)
		if (instant >= spans[span].start && instant < spans[span].end)
		{
			last_span = span;
			return spans[span].offset;
		}
	found = pg_next_dst_boundary(&instant, &before, &before_isdst, &boundary,
								 &after, &after_isdst, zone);
	if (found < 0)
		return pg_localtime(&instant, zone)->tm_gmtoff;
	end = found == 0 ? PG_INT64_MAX : boundary;
	for (int span = 0; span < nspans; span++)
		if (spans[span].end == end && spans[span].offset == before)
		{
			spans[span].start = Min(spans[span].start, instant);
			last_span = span;
			return before;
		}
	slot = nspans < ZONE_SPANS ? nspans++ : next_victim++ % ZONE_SPANS;
	spans[slot].start = instant;
	spans[slot].end = end;
	spans[slot].offset = before;
	last_span = slot;
	return before;
}

/*
 * A finite timestamptz in the zone's local time, as timestamp2tm reads it:
 * the instant plus the zone's offset at its second.
 */
static inline Timestamp
local_timestamp(TimestampTz timestamp, const pg_tz *zone)
{
	pg_time_t	utime;
	int64		seconds = timestamp / USECS_PER_SEC;

	if (timestamp % USECS_PER_SEC < 0)
		seconds--;
	utime = (pg_time_t) (seconds + (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * SECS_PER_DAY);
	return timestamp + (int64) zone_offset(utime, zone) * USECS_PER_SEC;
}

/* The local midnights met in the zone, by day. */
#define MIDNIGHTS 1024

typedef struct Midnight
{
	int64		day;
	TimestampTz instant;
	bool		valid;
} Midnight;

static pg_tz *midnight_zone;
static Midnight midnights[MIDNIGHTS];

/*
 * The instant of a day's 00:00 in the zone, as date2timestamptz and
 * timestamptz_trunc make it: false out of range, where the caller leaves
 * the row to the core.
 */
static bool
local_midnight(int year, int month, int mday, pg_tz *zone, TimestampTz *result)
{
	int64		day;
	Midnight   *slot;
	struct pg_tm tm;
	int			offset;

	if (!IS_VALID_JULIAN(year, month, mday))
		return false;
	day = date2j(year, month, mday) - POSTGRES_EPOCH_JDATE;
	if (zone != midnight_zone)
	{
		memset(midnights, 0, sizeof(midnights));
		midnight_zone = zone;
	}
	slot = &midnights[(uint64) day % MIDNIGHTS];
	if (slot->valid && slot->day == day)
	{
		*result = slot->instant;
		return true;
	}
	memset(&tm, 0, sizeof(tm));
	tm.tm_year = year;
	tm.tm_mon = month;
	tm.tm_mday = mday;
	offset = DetermineTimeZoneOffset(&tm, zone);
	if (pg_mul_s64_overflow(day, USECS_PER_DAY, result) ||
		pg_add_s64_overflow(*result, (int64) offset * USECS_PER_SEC, result) ||
		!IS_VALID_TIMESTAMP(*result))
		return false;
	slot->day = day;
	slot->instant = *result;
	slot->valid = true;
	return true;
}

/*
 * A finite timestamptz truncated to unit in the zone, as
 * timestamptz_trunc_internal: the local fields truncated, a unit below a
 * day kept at the instant's offset, a day and above at its first day's
 * midnight. False where the core must take the row.
 */
static bool
truncate_zoned(TimestampTz timestamp, int unit, pg_tz *zone, TimestampTz *result)
{
	Timestamp	local = local_timestamp(timestamp, zone);
	int64		time;
	int64		day = timestamp_day(local, &time);
	int			year;
	int			month;
	int			mday;

	if (day + POSTGRES_EPOCH_JDATE < 0)
		return false;
	switch (unit)
	{
		case DTK_MICROSEC:
		case DTK_MILLISEC:
		case DTK_SECOND:
		case DTK_MINUTE:
		case DTK_HOUR:
			{
				int64		size = unit == DTK_MICROSEC ? 1 : unit == DTK_MILLISEC ? 1000 :
					unit == DTK_SECOND ? USECS_PER_SEC :
					unit == DTK_MINUTE ? USECS_PER_MINUTE : USECS_PER_HOUR;

				*result = day * USECS_PER_DAY + (time / size) * size - (local - timestamp);
				return IS_VALID_TIMESTAMP(*result);
			}
		case DTK_WEEK:
			day -= ((day + 5) % 7 + 7) % 7;
			if (day + POSTGRES_EPOCH_JDATE < 0)
				return false;
			j2date((int) (day + POSTGRES_EPOCH_JDATE), &year, &month, &mday);
			break;
		default:
			j2date((int) (day + POSTGRES_EPOCH_JDATE), &year, &month, &mday);
			if (unit != DTK_DAY)
			{
				truncate_fields(unit, &year, &month);
				mday = 1;
			}
			break;
	}
	return local_midnight(year, month, mday, zone, result);
}

/*
 * date_trunc(unit, timestamp) and date_trunc(unit, timestamptz): a known
 * unit constant by calendar arithmetic, anything else by the core's
 * function a row.
 */
static TessStatusCode
trunc_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *units;
	const TessFunctionArg *stamps;
	Datum	   *values;
	bool		zoned;
	int			unit = -1;
	int			nwords;

	if (!date_call_valid(call, 2))
		return date_invalid(call, "date_trunc takes a unit and a timestamp");
	units = &call->args[0];
	stamps = &call->args[1];
	values = (Datum *) call->values;
	zoned = date_op(call) == TIMESTAMPTZ_TRUNC;
	if (units->column == NULL)
	{
		text	   *text_units = DatumGetTextPP(units->scalar);
		char	   *lowunits;
		int			val;

		lowunits = downcase_truncate_identifier(VARDATA_ANY(text_units),
												VARSIZE_ANY_EXHDR(text_units), false);
		if (DecodeUnits(0, lowunits, &val) == UNITS)
		{
			switch (val)
			{
				case DTK_WEEK:
				case DTK_MILLENNIUM:
				case DTK_CENTURY:
				case DTK_DECADE:
				case DTK_YEAR:
				case DTK_QUARTER:
				case DTK_MONTH:
				case DTK_DAY:
				case DTK_HOUR:
				case DTK_MINUTE:
				case DTK_SECOND:
				case DTK_MILLISEC:
				case DTK_MICROSEC:
					unit = val;
					break;
				default:
					break;
			}
		}
		pfree(lowunits);
		if ((Pointer) text_units != DatumGetPointer(units->scalar))
			pfree(text_units);
	}
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			Timestamp	timestamp;
			Timestamp	result;

			if (arg_null(units, row) || arg_null(stamps, row))
				continue;
			timestamp = DatumGetTimestamp(arg_datum(stamps, row));
			if (zoned)
			{
				if (unit < 0 || TIMESTAMP_NOT_FINITE(timestamp) ||
					!truncate_zoned(timestamp, unit, session_timezone, &result))
					result = DatumGetTimestampTz(DirectFunctionCall2(timestamptz_trunc,
																	 arg_datum(units, row),
																	 TimestampTzGetDatum(timestamp)));
			}
			else if (unit < 0)
				result = DatumGetTimestamp(DirectFunctionCall2(timestamp_trunc,
															   arg_datum(units, row),
															   TimestampGetDatum(timestamp)));
			else if (TIMESTAMP_NOT_FINITE(timestamp))
				result = timestamp;
			else if (!truncate_timestamp(timestamp, unit, &result))
			{
				call->non_nulls->bits[word] = present;
				return date_out_of_range(call, "timestamp out of range");
			}
			values[row] = TimestampGetDatum(result);
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/* span negated, as interval_um_internal: false when a field overflows. */
static bool
negate_interval(const Interval *span, Interval *result)
{
	if (INTERVAL_IS_NOBEGIN(span))
		INTERVAL_NOEND(result);
	else if (INTERVAL_IS_NOEND(span))
		INTERVAL_NOBEGIN(result);
	else if (pg_sub_s64_overflow(INT64CONST(0), span->time, &result->time) ||
			 pg_sub_s32_overflow(0, span->day, &result->day) ||
			 pg_sub_s32_overflow(0, span->month, &result->month) ||
			 INTERVAL_NOT_FINITE(result))
		return false;
	return true;
}

/*
 * timestamp + span, as timestamp_pl_interval: an infinite interval makes
 * its infinity, an infinite timestamp stays; false out of range.
 */
static bool
timestamp_plus(Timestamp timestamp, const Interval *span, Timestamp *result)
{
	if (INTERVAL_IS_NOBEGIN(span))
	{
		if (TIMESTAMP_IS_NOEND(timestamp))
			return false;
		TIMESTAMP_NOBEGIN(*result);
		return true;
	}
	if (INTERVAL_IS_NOEND(span))
	{
		if (TIMESTAMP_IS_NOBEGIN(timestamp))
			return false;
		TIMESTAMP_NOEND(*result);
		return true;
	}
	if (TIMESTAMP_NOT_FINITE(timestamp))
	{
		*result = timestamp;
		return true;
	}
	if (span->month != 0)
	{
		int64		time;
		int64		day = timestamp_day(timestamp, &time);
		int			year;
		int			month;
		int			mday;

		/* The date's fields as timestamp2tm gives them, the time kept. */
		j2date((int) (day + POSTGRES_EPOCH_JDATE), &year, &month, &mday);
		if (pg_add_s32_overflow(month, span->month, &month))
			return false;
		if (month > MONTHS_PER_YEAR)
		{
			year += (month - 1) / MONTHS_PER_YEAR;
			month = ((month - 1) % MONTHS_PER_YEAR) + 1;
		}
		else if (month < 1)
		{
			year += month / MONTHS_PER_YEAR - 1;
			month = month % MONTHS_PER_YEAR + MONTHS_PER_YEAR;
		}
		if (mday > day_tab[isleap(year)][month - 1])
			mday = day_tab[isleap(year)][month - 1];
		/* tm2timestamp's checks. */
		if (!IS_VALID_JULIAN(year, month, mday) ||
			pg_mul_s64_overflow((int64) date2j(year, month, mday) - POSTGRES_EPOCH_JDATE,
								USECS_PER_DAY, &timestamp) ||
			pg_add_s64_overflow(timestamp, time, &timestamp) ||
			!IS_VALID_TIMESTAMP(timestamp))
			return false;
	}
	if (span->day != 0)
	{
		int64		time;
		int64		day = timestamp_day(timestamp, &time);
		int32		julian;

		/* A Julian day from 0 on, as j2date takes; its timestamp in range. */
		if (pg_add_s32_overflow((int32) (day + POSTGRES_EPOCH_JDATE), span->day, &julian) ||
			julian < 0 ||
			pg_mul_s64_overflow((int64) julian - POSTGRES_EPOCH_JDATE, USECS_PER_DAY,
								&timestamp) ||
			pg_add_s64_overflow(timestamp, time, &timestamp) ||
			!IS_VALID_TIMESTAMP(timestamp))
			return false;
	}
	if (pg_add_s64_overflow(timestamp, span->time, &timestamp) ||
		!IS_VALID_TIMESTAMP(timestamp))
		return false;
	*result = timestamp;
	return true;
}

/*
 * A timestamp or a date plus or minus an interval, a timestamp: either
 * argument a column or a scalar.
 */
static TessStatusCode
interval_evaluate(TessFunctionCall *call)
{
	DateOp		op;
	bool		from_date;
	bool		minus;
	Datum	   *values;
	int			nwords;

	if (!date_call_valid(call, 2))
		return date_invalid(call, "an interval sum takes two arguments");
	op = date_op(call);
	from_date = op == DATE_PLUS_INTERVAL || op == DATE_MINUS_INTERVAL;
	minus = op == TIMESTAMP_MINUS_INTERVAL || op == DATE_MINUS_INTERVAL;
	values = (Datum *) call->values;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			const Interval *span;
			Interval	negated;
			Timestamp	timestamp;
			Timestamp	result;

			if (arg_null(&call->args[0], row) || arg_null(&call->args[1], row))
				continue;
			if (from_date)
			{
				DateADT		date = DatumGetDateADT(arg_datum(&call->args[0], row));

				if (DATE_IS_NOBEGIN(date))
					TIMESTAMP_NOBEGIN(timestamp);
				else if (DATE_IS_NOEND(date))
					TIMESTAMP_NOEND(timestamp);
				else if (date >= (TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE))
				{
					call->non_nulls->bits[word] = present;
					return date_out_of_range(call, "date out of range for timestamp");
				}
				else
					timestamp = (Timestamp) date * USECS_PER_DAY;
			}
			else
				timestamp = DatumGetTimestamp(arg_datum(&call->args[0], row));
			span = DatumGetIntervalP(arg_datum(&call->args[1], row));
			if (minus)
			{
				if (!negate_interval(span, &negated))
				{
					call->non_nulls->bits[word] = present;
					return date_out_of_range(call, "interval out of range");
				}
				span = &negated;
			}
			if (!timestamp_plus(timestamp, span, &result))
			{
				call->non_nulls->bits[word] = present;
				return date_out_of_range(call, "timestamp out of range");
			}
			values[row] = TimestampGetDatum(result);
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/* A core function of two arguments on one row, its NULL result allowed. */
static Datum
call_core(PGFunction function, Datum first, Datum second, bool *isnull)
{
	LOCAL_FCINFO(fcinfo, 2);
	Datum		result;

	InitFunctionCallInfoData(*fcinfo, NULL, 2, InvalidOid, NULL, NULL);
	fcinfo->args[0].value = first;
	fcinfo->args[0].isnull = false;
	fcinfo->args[1].value = second;
	fcinfo->args[1].isnull = false;
	result = (*function) (fcinfo);
	*isnull = fcinfo->isnull;
	return result;
}

/* Whether extract computes the field of a finite value itself. */
static bool
extract_native(bool timestamp, int type, int val)
{
	if (type == RESERV)
		return !timestamp && val == DTK_EPOCH;
	if (type != UNITS)
		return false;
	switch (val)
	{
		case DTK_DAY:
		case DTK_MONTH:
		case DTK_QUARTER:
		case DTK_WEEK:
		case DTK_YEAR:
		case DTK_DECADE:
		case DTK_CENTURY:
		case DTK_MILLENNIUM:
		case DTK_ISOYEAR:
		case DTK_DOW:
		case DTK_ISODOW:
		case DTK_DOY:
			return true;
		case DTK_JULIAN:
			return !timestamp;
		case DTK_MICROSEC:
		case DTK_MILLISEC:
		case DTK_SECOND:
		case DTK_MINUTE:
		case DTK_HOUR:
			return timestamp;
		default:
			return false;
	}
}

/* The last day whose calendar fields were computed, for the next row. */
typedef struct DayFields
{
	int64		day;
	bool		valid;
	int			year;
	int			month;
	int			mday;
} DayFields;

/*
 * The field of a finite day (days since 2000-01-01) and the microseconds
 * into it, as extract_date and timestamp_part_common compute it.
 */
static Numeric
extract_field(int type, int val, int64 day, int64 time, DayFields *fields,
			  MemoryContext context)
{
	int			julian = (int) (day + POSTGRES_EPOCH_JDATE);
	int64		result;

	if (type == RESERV)
		return tess_numeric_from_int64((day + POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) *
									   SECS_PER_DAY, context);
	switch (val)
	{
		case DTK_MICROSEC:
			return tess_numeric_from_int64(time % USECS_PER_MINUTE, context);
		case DTK_MILLISEC:
		case DTK_SECOND:
			{
				MemoryContext old = MemoryContextSwitchTo(context);
				Numeric		fraction;

				fraction = int64_div_fast_to_numeric(time % USECS_PER_MINUTE,
													 val == DTK_SECOND ? 6 : 3);
				MemoryContextSwitchTo(old);
				return fraction;
			}
		case DTK_MINUTE:
			return tess_numeric_from_int64((time / USECS_PER_MINUTE) % MINS_PER_HOUR, context);
		case DTK_HOUR:
			return tess_numeric_from_int64(time / USECS_PER_HOUR, context);
		case DTK_JULIAN:
			return tess_numeric_from_int64(julian, context);
		case DTK_DOW:
		case DTK_ISODOW:
			result = j2day(julian);
			if (val == DTK_ISODOW && result == 0)
				result = 7;
			return tess_numeric_from_int64(result, context);
		default:
			break;
	}
	if (!fields->valid || fields->day != day)
	{
		j2date(julian, &fields->year, &fields->month, &fields->mday);
		fields->day = day;
		fields->valid = true;
	}
	switch (val)
	{
		case DTK_DAY:
			result = fields->mday;
			break;
		case DTK_MONTH:
			result = fields->month;
			break;
		case DTK_QUARTER:
			result = (fields->month - 1) / 3 + 1;
			break;
		case DTK_WEEK:
			result = date2isoweek(fields->year, fields->month, fields->mday);
			break;
		case DTK_YEAR:
			/* There is no year 0, just 1 BC and 1 AD. */
			result = fields->year > 0 ? fields->year : fields->year - 1;
			break;
		case DTK_DECADE:
			result = fields->year >= 0 ? fields->year / 10 :
				-((8 - (fields->year - 1)) / 10);
			break;
		case DTK_CENTURY:
			result = fields->year > 0 ? (fields->year + 99) / 100 :
				-((99 - (fields->year - 1)) / 100);
			break;
		case DTK_MILLENNIUM:
			result = fields->year > 0 ? (fields->year + 999) / 1000 :
				-((999 - (fields->year - 1)) / 1000);
			break;
		case DTK_ISOYEAR:
			result = date2isoyear(fields->year, fields->month, fields->mday);
			if (result <= 0)
				result -= 1;
			break;
		default:
			/* DTK_DOY */
			result = julian - date2j(fields->year, 1, 1) + 1;
			break;
	}
	return tess_numeric_from_int64(result, context);
}

/*
 * extract(unit from date) and extract(unit from timestamp): a known unit
 * constant over a finite value by extract_field, anything else by the
 * core's function a row, in the call's context either way.
 */
static TessStatusCode
extract_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *units;
	const TessFunctionArg *stamps;
	bool		timestamp;
	bool		zoned;
	PGFunction	core;
	Datum	   *values;
	int			type = UNKNOWN_FIELD;
	int			val = 0;
	bool		native = false;
	DayFields	fields = {0};
	MemoryContext old;
	int			nwords;

	if (!date_call_valid(call, 2) || call->context == NULL)
		return date_invalid(call, "extract takes a unit and a date or a timestamp");
	units = &call->args[0];
	stamps = &call->args[1];
	zoned = date_op(call) == TIMESTAMPTZ_EXTRACT;
	timestamp = zoned || date_op(call) == TIMESTAMP_EXTRACT;
	core = zoned ? extract_timestamptz : timestamp ? extract_timestamp : extract_date;
	values = (Datum *) call->values;
	old = MemoryContextSwitchTo(call->context);
	if (units->column == NULL)
	{
		text	   *text_units = DatumGetTextPP(units->scalar);
		char	   *lowunits;

		lowunits = downcase_truncate_identifier(VARDATA_ANY(text_units),
												VARSIZE_ANY_EXHDR(text_units), false);
		type = DecodeUnits(0, lowunits, &val);
		if (type == UNKNOWN_FIELD)
			type = DecodeSpecial(0, lowunits, &val);
		native = extract_native(timestamp, type, val);
	}
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			Datum		value;
			int64		day;
			int64		time = 0;
			bool		finite;

			if (arg_null(units, row) || arg_null(stamps, row))
				continue;
			value = arg_datum(stamps, row);
			if (timestamp)
			{
				Timestamp	local = DatumGetTimestamp(value);

				finite = !TIMESTAMP_NOT_FINITE(local);
				/* A timestamptz's fields are its local time's. */
				if (finite && zoned)
					local = local_timestamp(local, session_timezone);
				day = finite ? timestamp_day(local, &time) : 0;
				finite = finite && day + POSTGRES_EPOCH_JDATE >= 0;
			}
			else
			{
				finite = !DATE_NOT_FINITE(DatumGetDateADT(value));
				day = DatumGetDateADT(value);
			}
			if (native && finite)
				values[row] = NumericGetDatum(extract_field(type, val, day, time, &fields,
															call->context));
			else
			{
				bool		isnull;

				values[row] = call_core(core, arg_datum(units, row), value, &isnull);
				if (isnull)
					continue;
			}
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	MemoryContextSwitchTo(old);
	return TESS_OK;
}

/*
 * timestamptz(date), a date's local midnight, and date(timestamptz), an
 * instant's local day; an infinity kept, the rest by the core's function.
 */
static TessStatusCode
zone_cast_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *arg;
	bool		to_instant;
	int			nwords;

	if (!date_call_valid(call, 1))
		return date_invalid(call, "a zone cast takes one argument");
	arg = &call->args[0];
	to_instant = date_op(call) == DATE_TO_TIMESTAMPTZ;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			Datum		value;

			if (arg_null(arg, row))
				continue;
			value = arg_datum(arg, row);
			if (to_instant)
			{
				DateADT		date = DatumGetDateADT(value);
				TimestampTz instant;
				int			year;
				int			month;
				int			mday;

				if (DATE_IS_NOBEGIN(date))
					TIMESTAMP_NOBEGIN(instant);
				else if (DATE_IS_NOEND(date))
					TIMESTAMP_NOEND(instant);
				else
				{
					bool		done = false;

					if (date < (TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE))
					{
						j2date(date + POSTGRES_EPOCH_JDATE, &year, &month, &mday);
						done = local_midnight(year, month, mday, session_timezone, &instant);
					}
					if (!done)
						instant = DatumGetTimestampTz(DirectFunctionCall1(date_timestamptz, value));
				}
				((Datum *) call->values)[row] = TimestampTzGetDatum(instant);
			}
			else
			{
				TimestampTz instant = DatumGetTimestampTz(value);
				int64		time;
				int64		day = 0;
				DateADT		date;

				if (TIMESTAMP_IS_NOBEGIN(instant))
					DATE_NOBEGIN(date);
				else if (TIMESTAMP_IS_NOEND(instant))
					DATE_NOEND(date);
				else if ((day = timestamp_day(local_timestamp(instant, session_timezone),
											  &time)) + POSTGRES_EPOCH_JDATE >= 0)
					date = (DateADT) day;
				else
					date = DatumGetDateADT(DirectFunctionCall1(timestamptz_date, value));
				((int32 *) call->values)[row] = date;
			}
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/* Register the date functions. */
void
tess_register_date_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(date_functions); i++)
		functions->add(&date_functions[i].function);
}
