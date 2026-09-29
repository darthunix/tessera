/*
 * A numeric of at most 18 digits read as an int64 of its display scale,
 * straight from its stored form, for the batch functions of numeric and
 * the aggregates that fold numeric values themselves.
 *
 * The stored form is the core's (utils/adt/numeric.c): after the varlena
 * header, a short header word (0x8000 set: 0x2000 the sign, 0x1F80 the
 * display scale, 0x0040 and 0x003F the weight) or a long one (the sign in
 * 0xC000, the display scale in 0x3FFF, then an int16 weight), then the
 * digits of base 10000 from the highest, each at 10000^(weight - i); a
 * header 0xC000 and above is NaN or an infinity. A value of a short
 * varlena, as a table's usually is, is read in place.
 */
#ifndef TESSERA_DECIMAL_H
#define TESSERA_DECIMAL_H

#include "postgres.h"

#include "common/int.h"
#include "varatt.h"

/* A numeric of at most 18 digits: its value at its display scale. */
typedef struct TessDecimal
{
	int64		value;
	int			scale;
} TessDecimal;

#define TESS_DECIMAL_DIGITS 18

static const int64 tess_powers_of_ten[TESS_DECIMAL_DIGITS + 1] = {
	INT64CONST(1), INT64CONST(10), INT64CONST(100), INT64CONST(1000),
	INT64CONST(10000), INT64CONST(100000), INT64CONST(1000000),
	INT64CONST(10000000), INT64CONST(100000000), INT64CONST(1000000000),
	INT64CONST(10000000000), INT64CONST(100000000000),
	INT64CONST(1000000000000), INT64CONST(10000000000000),
	INT64CONST(100000000000000), INT64CONST(1000000000000000),
	INT64CONST(10000000000000000), INT64CONST(100000000000000000),
	INT64CONST(1000000000000000000)
};

/*
 * The decimal of a numeric Datum, read from its stored header (short:
 * sign, display scale and weight in one word; long: sign and scale, then
 * the weight) and its digits of base 10000, each the value's digits at
 * 10000^(weight - i); false for a compressed or external value, NaN, an
 * infinity, a scale past 18 or a value of more than 18 digits.
 */
static inline bool
tess_decimal_of(Datum datum, TessDecimal *result)
{
	struct varlena *pointer = (struct varlena *) DatumGetPointer(datum);
	const char *data;
	int			len;
	uint16		header;
	int			weight;
	int			scale;
	int			offset;
	int			ndigits;
	int			exponent;
	bool		negative;
	int64		value = 0;

	if (VARATT_IS_EXTENDED(pointer) && !VARATT_IS_SHORT(pointer))
		return false;
	data = VARDATA_ANY(pointer);
	len = VARSIZE_ANY_EXHDR(pointer);
	if (len < (int) sizeof(uint16))
		return false;
	memcpy(&header, data, sizeof(uint16));
	if ((header & 0xC000) == 0xC000)
		return false;			/* NaN or an infinity */
	if ((header & 0x8000) != 0)
	{
		/* Short: 0x2000 the sign, 0x1F80 the scale, 0x0040 and 0x003F the weight. */
		negative = (header & 0x2000) != 0;
		scale = (header & 0x1F80) >> 7;
		weight = (header & 0x0040) != 0 ? (int) (~0x003F | (header & 0x003F)) :
			(int) (header & 0x003F);
		offset = sizeof(uint16);
	}
	else
	{
		int16		long_weight;

		if (len < (int) (2 * sizeof(uint16)))
			return false;
		negative = (header & 0xC000) == 0x4000;
		scale = header & 0x3FFF;
		memcpy(&long_weight, data + sizeof(uint16), sizeof(int16));
		weight = long_weight;
		offset = 2 * sizeof(uint16);
	}
	ndigits = (len - offset) / (int) sizeof(int16);
	if (scale > TESS_DECIMAL_DIGITS || ndigits > 5)
		return false;
	for (int at = 0; at < ndigits; at++)
	{
		int16		digit;

		memcpy(&digit, data + offset + at * sizeof(int16), sizeof(int16));
		/* Four digits of base 10000 are 16 decimal ones: the fifth is checked. */
		if (at < 4)
			value = value * 10000 + digit;
		else if (pg_mul_s64_overflow(value, 10000, &value) ||
				 pg_add_s64_overflow(value, digit, &value))
			return false;
	}
	/* The last digit stands at 10000^(weight - ndigits + 1): to the display scale. */
	exponent = 4 * (weight - ndigits + 1) + scale;
	if (ndigits == 0)
		value = 0;
	else if (exponent >= 0)
	{
		if (exponent > TESS_DECIMAL_DIGITS ||
			pg_mul_s64_overflow(value, tess_powers_of_ten[exponent], &value) ||
			value >= tess_powers_of_ten[TESS_DECIMAL_DIGITS])
			return false;
	}
	else
	{
		/* Digits past the display scale: only zeros the scale does not show. */
		if (exponent < -TESS_DECIMAL_DIGITS || value % tess_powers_of_ten[-exponent] != 0)
			return false;
		value /= tess_powers_of_ten[-exponent];
	}
	result->value = negative ? -value : value;
	result->scale = scale;
	return true;
}

#endif							/* TESSERA_DECIMAL_H */
