/*
 * The date and timestamp functions as batch functions, a date its days
 * since 2000-01-01 and a timestamp its microseconds: a date plus or minus
 * days and the days between two dates, the casts between date and
 * timestamp, date_trunc of a timestamp, a timestamp or a date plus or
 * minus an interval, extract of a date or a timestamp. The calendar is the
 * Rust kernels' (tessera/calendar.h), a batch a call: the core's Julian day
 * routines, the truncations, the months of an interval clamped to the
 * month's last day, the fields; each fails where the core's function
 * raises, at the first row it would, with the core's message: 22008 "date
 * out of range" past the dates, "cannot subtract infinite dates", "date
 * out of range for timestamp", "timestamp out of range", "interval out of
 * range". An infinite value stays infinite where the core keeps it.
 *
 * Here stay what the calendar needs of PostgreSQL: the unit names, parsed
 * once a call by DecodeUnits as the core parses them a row; the session's
 * time zone; the numerics of extract (a small integer from the cache of
 * numeric.c, the others written by the kernels); and the rows the kernels
 * leave (an infinite value extract does not take, a unit it does not know
 * or that is a column, a result out of the zone's reach), which go to the
 * core's function a row, its NULL for an oscillating field of infinity
 * kept.
 *
 * A timestamp with time zone is read in the session's zone: its local
 * time is the instant plus the offset pg_localtime gives, as timestamp2tm
 * reads it, the offset kept for the span of instants up to the zone's
 * next transition that pg_next_dst_boundary names; a local day's midnight
 * is the instant DetermineTimeZoneOffset makes of it, kept for the next
 * rows of that day in a cache of the process for the zone. The kernels
 * truncate and extract the local times, a batch of them at a time.
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
#include "tessera/calendar.h"
#include "tessera/decimal.h"

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

#define DATE_FLAGS \
	(TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | TESS_FUNCTION_ANY_SHAPE)

#define DATE_FUNCTION(oid, code, format, evaluator) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = (format), .flags = DATE_FLAGS, \
	  .evaluate = (evaluator)}, (code)}

/* extract, a numeric, writes decimals where the call asks for them. */
#define EXTRACT_FUNCTION(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = TESS_RESULT_DATUM, .flags = DATE_FLAGS | TESS_FUNCTION_DECIMALS, \
	  .evaluate = extract_evaluate}, (code)}

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
	EXTRACT_FUNCTION(F_EXTRACT_TEXT_DATE, DATE_EXTRACT),
	EXTRACT_FUNCTION(F_EXTRACT_TEXT_TIMESTAMP, TIMESTAMP_EXTRACT),
	DATE_FUNCTION(F_DATE_TRUNC_TEXT_TIMESTAMPTZ, TIMESTAMPTZ_TRUNC, TESS_RESULT_DATUM,
				  trunc_evaluate),
	EXTRACT_FUNCTION(F_EXTRACT_TEXT_TIMESTAMPTZ, TIMESTAMPTZ_EXTRACT),
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

/* An argument for the kernels: its column or its scalar. */
static inline TessCalendarArg
calendar_arg(const TessFunctionArg *arg)
{
	return (TessCalendarArg) {arg->column, arg->scalar};
}

/*
 * date + integer, date - integer, date - date, date(timestamp) and
 * timestamp(date), any argument a column or a scalar, by the kernels.
 */
static TessStatusCode
date_evaluate(TessFunctionCall *call)
{
	DateOp		op;
	int			nargs;
	TessCalendarArg left;
	TessCalendarArg right;

	op = call != NULL && call->function != NULL ? date_op(call) : DATE_PLUS_DAYS;
	nargs = op == TIMESTAMP_TO_DATE || op == DATE_TO_TIMESTAMP ? 1 : 2;
	if (!date_call_valid(call, nargs))
		return tess_call_invalid(call, "a date function takes its arguments");
	left = calendar_arg(&call->args[0]);
	right = nargs == 2 ? calendar_arg(&call->args[1]) : left;
	switch (op)
	{
		case DATE_PLUS_DAYS:
			return tess_date_arith(TESS_DATE_PLUS_DAYS, &left, &right, call->rows,
								   (int32 *) call->values, call->non_nulls, call->status);
		case DATE_MINUS_DAYS:
			return tess_date_arith(TESS_DATE_MINUS_DAYS, &left, &right, call->rows,
								   (int32 *) call->values, call->non_nulls, call->status);
		case DATE_MINUS_DATE:
			return tess_date_arith(TESS_DATE_MINUS_DATE, &left, &right, call->rows,
								   (int32 *) call->values, call->non_nulls, call->status);
		case TIMESTAMP_TO_DATE:
			return tess_timestamp_to_date(&left, call->rows, (int32 *) call->values,
										  call->non_nulls, call->status);
		case DATE_TO_TIMESTAMP:
			return tess_date_to_timestamp(&left, call->rows, (int64 *) call->values,
										  call->non_nulls, call->status);
		default:
			return tess_call_invalid(call, "not a date function");
	}
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
 * The instant of a day's 00:00 in the zone, the day a Julian day, as
 * date2timestamptz and timestamptz_trunc make it: false out of range,
 * where the caller leaves the row to the core. A day in the cache needs
 * no calendar; a new one its fields for DetermineTimeZoneOffset.
 */
static bool
local_midnight(int julian, pg_tz *zone, TimestampTz *result)
{
	int64		day = (int64) julian - POSTGRES_EPOCH_JDATE;
	Midnight   *slot;
	struct pg_tm tm;
	int			offset;

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
	if (julian < 0)
		return false;
	memset(&tm, 0, sizeof(tm));
	j2date(julian, &tm.tm_year, &tm.tm_mon, &tm.tm_mday);
	if (!IS_VALID_JULIAN(tm.tm_year, tm.tm_mon, tm.tm_mday))
		return false;
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
 * Scratch of a call: the local times of a batch's timestamptz column, and
 * the masks and scales the kernels write, on the stack for a batch of up
 * to this many rows, palloc'd beyond.
 */
#define SCRATCH_ROWS 1024

typedef struct DateScratch
{
	int64		locals[SCRATCH_ROWS];
	uint64		words[3][SCRATCH_ROWS / 64];
	uint8		scales[SCRATCH_ROWS];
} DateScratch;

/*
 * The session zone's local times of a timestamptz column's selected
 * non-NULL rows into locals, an infinity kept: a column of them with the
 * column's NULL flags.
 */
static TessDatumColumn
local_column(const TessDatumColumn *column, const TessRowMask *rows, int64 *locals)
{
	TessDatumColumn result = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			nwords = tess_row_mask_word_count(rows->nrows);

	for (int word = 0; word < nwords; word++)
	{
		for (uint64 look = rows->bits[word]; look != 0; look &= look - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(look);
			TimestampTz instant = DatumGetTimestampTz(column->values[row]);

			if (!column->isnull[row])
				locals[row] = TIMESTAMP_NOT_FINITE(instant) ? instant :
					local_timestamp(instant, session_timezone);
		}
	}
	result.values = (Datum *) locals;
	result.isnull = column->isnull;
	result.nrows = column->nrows;
	return result;
}

/* The kernels' unit of a DecodeUnits unit date_trunc truncates, or -1. */
static int
trunc_unit(int val)
{
	switch (val)
	{
		case DTK_MICROSEC:
			return TESS_UNIT_MICROSECOND;
		case DTK_MILLISEC:
			return TESS_UNIT_MILLISECOND;
		case DTK_SECOND:
			return TESS_UNIT_SECOND;
		case DTK_MINUTE:
			return TESS_UNIT_MINUTE;
		case DTK_HOUR:
			return TESS_UNIT_HOUR;
		case DTK_DAY:
			return TESS_UNIT_DAY;
		case DTK_WEEK:
			return TESS_UNIT_WEEK;
		case DTK_MONTH:
			return TESS_UNIT_MONTH;
		case DTK_QUARTER:
			return TESS_UNIT_QUARTER;
		case DTK_YEAR:
			return TESS_UNIT_YEAR;
		case DTK_DECADE:
			return TESS_UNIT_DECADE;
		case DTK_CENTURY:
			return TESS_UNIT_CENTURY;
		case DTK_MILLENNIUM:
			return TESS_UNIT_MILLENNIUM;
		default:
			return -1;
	}
}

/*
 * date_trunc(unit, timestamptz) of a column in the session's zone, as
 * timestamptz_trunc_internal: the kernels truncate the local times, a unit
 * below a day kept at the instant's offset, a day and above at its first
 * day's midnight in the zone; the rest by the core's function a row.
 */
static TessStatusCode
trunc_zoned(TessFunctionCall *call, TessCalendarUnit unit)
{
	const TessDatumColumn *column = call->args[1].column;
	Datum	   *values = (Datum *) call->values;
	int			nrows = call->rows->nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	DateScratch space;
	int64	   *locals = tess_scratch_alloc(sizeof(int64) * nrows, space.locals, sizeof(space.locals));
	TessRowMask days = tess_scratch_mask(space.words[0], sizeof(space.words[0]), nrows);
	TessRowMask rest = tess_scratch_mask(space.words[1], sizeof(space.words[1]), nrows);
	TessDatumColumn local = local_column(column, call->rows, locals);
	TessStatusCode code;

	code = tess_timestamp_trunc_local(unit, &local, call->rows, (int64 *) values, &days, &rest,
									  call->status);
	for (int word = 0; code == TESS_OK && word < nwords; word++)
	{
		uint64		present = 0;

		for (uint64 look = call->rows->bits[word]; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			TimestampTz instant = DatumGetTimestampTz(column->values[row]);
			TimestampTz result = 0;
			bool		done = false;

			if (column->isnull[row])
				continue;
			if ((days.bits[word] >> bit) & 1)
				done = local_midnight((int) DatumGetInt64(values[row]), session_timezone,
									  &result);
			else if (((rest.bits[word] >> bit) & 1) == 0)
			{
				result = DatumGetInt64(values[row]) - (locals[row] - instant);
				done = IS_VALID_TIMESTAMP(result);
			}
			if (!done)
				result = DatumGetTimestampTz(DirectFunctionCall2(timestamptz_trunc,
																 arg_datum(&call->args[0], row),
																 TimestampTzGetDatum(instant)));
			values[row] = TimestampTzGetDatum(result);
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	tess_scratch_release(rest.bits, space.words[1]);
	tess_scratch_release(days.bits, space.words[0]);
	tess_scratch_release(locals, space.locals);
	return code;
}

/*
 * date_trunc(unit, timestamp) and date_trunc(unit, timestamptz): a known
 * unit constant by the kernels, anything else by the core's function a row.
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
		return tess_call_invalid(call, "date_trunc takes a unit and a timestamp");
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
			unit = trunc_unit(val);
		pfree(lowunits);
		if ((Pointer) text_units != DatumGetPointer(units->scalar))
			pfree(text_units);
	}
	if (unit >= 0 && !zoned)
	{
		TessCalendarArg arg = calendar_arg(stamps);

		return tess_timestamp_trunc(unit, &arg, call->rows, (int64 *) values, call->non_nulls,
									call->status);
	}
	if (unit >= 0 && stamps->column != NULL)
		return trunc_zoned(call, unit);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;

			if (arg_null(units, row) || arg_null(stamps, row))
				continue;
			values[row] = DirectFunctionCall2(zoned ? timestamptz_trunc : timestamp_trunc,
											  arg_datum(units, row), arg_datum(stamps, row));
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/*
 * A timestamp or a date plus or minus an interval, a timestamp: either
 * argument a column or a scalar, by the kernels.
 */
static TessStatusCode
interval_evaluate(TessFunctionCall *call)
{
	DateOp		op;
	bool		minus;
	TessCalendarArg left;
	TessCalendarArg right;

	if (!date_call_valid(call, 2))
		return tess_call_invalid(call, "an interval sum takes two arguments");
	op = date_op(call);
	minus = op == TIMESTAMP_MINUS_INTERVAL || op == DATE_MINUS_INTERVAL;
	left = calendar_arg(&call->args[0]);
	right = calendar_arg(&call->args[1]);
	if (op == DATE_PLUS_INTERVAL || op == DATE_MINUS_INTERVAL)
		return tess_date_add_interval(minus, &left, &right, call->rows, (int64 *) call->values,
									  call->non_nulls, call->status);
	return tess_timestamp_add_interval(minus, &left, &right, call->rows, (int64 *) call->values,
									   call->non_nulls, call->status);
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

/* The kernels' field of a unit extract computes of a finite value, or -1. */
static int
extract_field_of(bool timestamp, int type, int val)
{
	if (type == RESERV)
		return !timestamp && val == DTK_EPOCH ? TESS_FIELD_EPOCH : -1;
	if (type != UNITS)
		return -1;
	switch (val)
	{
		case DTK_DAY:
			return TESS_FIELD_DAY;
		case DTK_MONTH:
			return TESS_FIELD_MONTH;
		case DTK_QUARTER:
			return TESS_FIELD_QUARTER;
		case DTK_WEEK:
			return TESS_FIELD_WEEK;
		case DTK_YEAR:
			return TESS_FIELD_YEAR;
		case DTK_DECADE:
			return TESS_FIELD_DECADE;
		case DTK_CENTURY:
			return TESS_FIELD_CENTURY;
		case DTK_MILLENNIUM:
			return TESS_FIELD_MILLENNIUM;
		case DTK_ISOYEAR:
			return TESS_FIELD_ISOYEAR;
		case DTK_DOW:
			return TESS_FIELD_DOW;
		case DTK_ISODOW:
			return TESS_FIELD_ISODOW;
		case DTK_DOY:
			return TESS_FIELD_DOY;
		case DTK_JULIAN:
			return timestamp ? -1 : TESS_FIELD_JULIAN;
		case DTK_MICROSEC:
			return timestamp ? TESS_FIELD_MICROSECOND : -1;
		case DTK_MILLISEC:
			return timestamp ? TESS_FIELD_MILLISECOND : -1;
		case DTK_SECOND:
			return timestamp ? TESS_FIELD_SECOND : -1;
		case DTK_MINUTE:
			return timestamp ? TESS_FIELD_MINUTE : -1;
		case DTK_HOUR:
			return timestamp ? TESS_FIELD_HOUR : -1;
		default:
			return -1;
	}
}

/*
 * extract by the core's function over the rows of only, in the call's
 * context: a row with a NULL argument or a NULL result leaves non_nulls.
 */
static void
extract_core(TessFunctionCall *call, PGFunction core, const TessRowMask *only)
{
	Datum	   *values = (Datum *) call->values;
	int			nwords = tess_row_mask_word_count(only->nrows);

	for (int word = 0; word < nwords; word++)
	{
		for (uint64 look = only->bits[word]; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			bool		isnull = true;

			if (!arg_null(&call->args[0], row) && !arg_null(&call->args[1], row))
				values[row] = call_core(core, arg_datum(&call->args[0], row),
										arg_datum(&call->args[1], row), &isnull);
			if (isnull)
				call->non_nulls->bits[word] &= ~(UINT64CONST(1) << bit);
			else
				call->non_nulls->bits[word] |= UINT64CONST(1) << bit;
		}
	}
}

/*
 * extract(unit from date), extract(unit from timestamp) and extract(unit
 * from timestamptz), a numeric: a known unit constant by the kernels over
 * the finite values (a timestamptz's local times), their numerics made
 * here; anything else by the core's function a row, in the call's context
 * either way.
 */
static TessStatusCode
extract_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *units;
	const TessFunctionArg *stamps;
	bool		timestamp;
	bool		zoned;
	PGFunction	core;
	int			field = -1;
	int			nrows;
	int			nwords;
	DateScratch space;
	TessRowMask rest;
	TessRowMask write;
	uint8	   *scales;
	int64	   *locals;
	TessDatumColumn local_stamps;
	TessCalendarArg arg;
	TessStatusCode code;
	MemoryContext old;

	if (!date_call_valid(call, 2) || call->context == NULL)
		return tess_call_invalid(call, "extract takes a unit and a date or a timestamp");
	units = &call->args[0];
	stamps = &call->args[1];
	zoned = date_op(call) == TIMESTAMPTZ_EXTRACT;
	timestamp = zoned || date_op(call) == TIMESTAMP_EXTRACT;
	core = zoned ? extract_timestamptz : timestamp ? extract_timestamp : extract_date;
	nrows = call->rows->nrows;
	nwords = tess_row_mask_word_count(nrows);
	old = MemoryContextSwitchTo(call->context);
	if (units->column == NULL)
	{
		text	   *text_units = DatumGetTextPP(units->scalar);
		char	   *lowunits;
		int			type;
		int			val = 0;

		lowunits = downcase_truncate_identifier(VARDATA_ANY(text_units),
												VARSIZE_ANY_EXHDR(text_units), false);
		type = DecodeUnits(0, lowunits, &val);
		if (type == UNKNOWN_FIELD)
			type = DecodeSpecial(0, lowunits, &val);
		field = extract_field_of(timestamp, type, val);
	}
	if (field < 0)
	{
		for (int word = 0; word < nwords; word++)
			call->non_nulls->bits[word] = 0;
		extract_core(call, core, call->rows);
		MemoryContextSwitchTo(old);
		return TESS_OK;
	}
	scales = tess_scratch_alloc(nrows, space.scales, sizeof(space.scales));
	locals = NULL;
	rest = tess_scratch_mask(space.words[0], sizeof(space.words[0]), nrows);
	write = tess_scratch_mask(space.words[1], sizeof(space.words[1]), nrows);
	arg = calendar_arg(stamps);
	if (zoned && stamps->column != NULL)
	{
		locals = tess_scratch_alloc(sizeof(int64) * nrows, space.locals, sizeof(space.locals));
		local_stamps = local_column(stamps->column, call->rows, locals);
		arg.column = &local_stamps;
	}
	else if (zoned && !TIMESTAMP_NOT_FINITE(DatumGetTimestampTz(stamps->scalar)))
		arg.scalar = TimestampGetDatum(local_timestamp(DatumGetTimestampTz(stamps->scalar),
													   session_timezone));
	code = timestamp ?
		tess_timestamp_extract(field, &arg, call->rows, (int64 *) call->values, scales,
							   call->non_nulls, &rest, call->status) :
		tess_date_extract(field, &arg, call->rows, (int64 *) call->values, scales,
						  call->non_nulls, &rest, call->status);
	if (code == TESS_OK)
	{
		/* A call that asks for decimals takes the fields of its scale as they are. */
		bool		asks = call->struct_size >= TESS_FUNCTION_CALL_DECIMALS_SIZE &&
			call->decimal_rows != NULL;

		for (int word = 0; word < nwords; word++)
		{
			uint64		done = call->non_nulls->bits[word] & ~rest.bits[word];
			uint64		decimals = 0;

			for (uint64 look = asks ? done : 0; look != 0; look &= look - 1)
			{
				int			bit = pg_rightmost_one_pos64(look);

				if (scales[word * 64 + bit] == call->result_scale)
					decimals |= UINT64CONST(1) << bit;
			}
			if (asks)
				call->decimal_rows->bits[word] = decimals;
			write.bits[word] = done & ~decimals;
		}
		code = tess_numeric_results(call->context, (Datum *) call->values, scales, &write,
									call->status);
	}
	if (code == TESS_OK)
		extract_core(call, core, &rest);
	if (locals != NULL)
		tess_scratch_release(locals, space.locals);
	tess_scratch_release(write.bits, space.words[1]);
	tess_scratch_release(rest.bits, space.words[0]);
	tess_scratch_release(scales, space.scales);
	MemoryContextSwitchTo(old);
	return code;
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
		return tess_call_invalid(call, "a zone cast takes one argument");
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

				if (DATE_IS_NOBEGIN(date))
					TIMESTAMP_NOBEGIN(instant);
				else if (DATE_IS_NOEND(date))
					TIMESTAMP_NOEND(instant);
				else
				{
					bool		done = false;

					if (date < (TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE))
					{
						done = local_midnight(date + POSTGRES_EPOCH_JDATE, session_timezone,
											  &instant);
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
