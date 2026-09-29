/*
 * The date and timestamp functions as batch functions, row by row in C
 * over the words of the columns, a date its days since 2000-01-01 and a
 * timestamp its microseconds: a date plus or minus days and the days
 * between two dates, the casts between date and timestamp, and date_trunc
 * of a timestamp. Each raises what the core's function raises, at the
 * first row it would: 22008 "date out of range" past the dates, "cannot
 * subtract infinite dates", "timestamp out of range" for a truncation
 * below the first timestamp. An infinite value stays infinite where the
 * core keeps it.
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
#include "port/pg_bitutils.h"
#include "utils/builtins.h"
#include "utils/date.h"
#include "utils/datetime.h"
#include "utils/fmgroids.h"
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
	TIMESTAMP_TRUNC
} DateOp;

typedef struct DateFunction
{
	TessFunction function;
	DateOp		op;
} DateFunction;

static TessStatusCode date_evaluate(TessFunctionCall *call);
static TessStatusCode trunc_evaluate(TessFunctionCall *call);

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
			/* The years as timestamp_trunc rounds them, 1 BC being year 0. */
			if (unit == DTK_MILLENNIUM)
				year = year > 0 ? ((year + 999) / 1000) * 1000 - 999 :
					-((999 - (year - 1)) / 1000) * 1000 + 1;
			else if (unit == DTK_CENTURY)
				year = year > 0 ? ((year + 99) / 100) * 100 - 99 :
					-((99 - (year - 1)) / 100) * 100 + 1;
			else if (unit == DTK_DECADE)
				year = year > 0 ? (year / 10) * 10 : -((8 - (year - 1)) / 10) * 10;
			if (unit != DTK_QUARTER && unit != DTK_MONTH)
				month = 1;
			else if (unit == DTK_QUARTER)
				month = 3 * ((month - 1) / 3) + 1;
			if (!IS_VALID_JULIAN(year, month, 1))
				return false;
			day = date2j(year, month, 1) - POSTGRES_EPOCH_JDATE;
			break;
	}
	*result = day * USECS_PER_DAY;
	return IS_VALID_TIMESTAMP(*result);
}

/*
 * date_trunc(unit, timestamp): a known unit constant by calendar
 * arithmetic, anything else by the core's timestamp_trunc a row.
 */
static TessStatusCode
trunc_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *units;
	const TessFunctionArg *stamps;
	Datum	   *values;
	int			unit = -1;
	int			nwords;

	if (!date_call_valid(call, 2))
		return date_invalid(call, "date_trunc takes a unit and a timestamp");
	units = &call->args[0];
	stamps = &call->args[1];
	values = (Datum *) call->values;
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
			if (unit < 0)
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

/* Register the date functions. */
void
tess_register_date_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(date_functions); i++)
		functions->add(&date_functions[i].function);
}
