/*
 * Strings as the text functions read them: the C entry points of the Rust
 * kernels (tessera_kernels::text) for kernels/text.c. A string is read in
 * place behind its Datum, a varlena of either header; a compressed or
 * external one is left in rest, for the caller to unpack and hand back as
 * a column of its own. Characters count as the database encoding counts
 * them: a byte each or UTF-8 (TessTextChars); other multibyte encodings
 * stay with the core's functions.
 *
 * The calls follow tessera/kernels.h: they never raise ERROR nor call
 * PostgreSQL, and return a status. They read the selected rows alone,
 * whose NULL flags and non-NULL values must be initialized; a scalar
 * argument must be read in place (unpacked by the caller). Every mask has
 * the selection's row count, and so has every array.
 */
#ifndef TESSERA_TEXT_H
#define TESSERA_TEXT_H

#include "postgres.h"

#include "tessera/kernels.h"

/* An argument: a column, or a scalar Datum without one. */
typedef struct TessTextArg
{
	const TessDatumColumn *column;
	Datum		scalar;
} TessTextArg;

/* How the database encoding counts characters. */
typedef enum TessTextChars
{
	TESS_CHARS_BYTES = 0,
	TESS_CHARS_UTF8 = 1
} TessTextChars;

/* A length of tess_text_lengths. */
typedef enum TessTextLength
{
	TESS_LENGTH_CHARS = 0,
	/* A bpchar's characters without its trailing spaces. */
	TESS_LENGTH_BPCHAR_CHARS = 1,
	TESS_LENGTH_OCTETS = 2
} TessTextLength;

/* A piece of tess_text_pieces. */
typedef enum TessTextPiece
{
	/* substring(s, first[, second]), the length when has_second */
	TESS_PIECE_SUBSTRING = 0,
	/* left(s, first) */
	TESS_PIECE_LEFT = 1,
	/* right(s, first) */
	TESS_PIECE_RIGHT = 2,
	/* rtrim, ltrim and btrim of spaces */
	TESS_PIECE_RTRIM = 3,
	TESS_PIECE_LTRIM = 4,
	TESS_PIECE_BTRIM = 5
} TessTextPiece;

/*
 * Narrow rows to the rows where two strings are equal (equal) or not, a
 * bpchar's without its trailing spaces; a row with a NULL leaves, a row
 * with a string not in place leaves too and is set in rest, whose other
 * bits are cleared.
 */
extern TessStatusCode tess_text_compare(bool equal, bool bpchar,
										const TessTextArg *left,
										const TessTextArg *right,
										TessRowMask *rows,
										TessRowMask *rest,
										TessStatus *status);

/* starts_with of a column and len bytes of prefix, rest as above. */
extern TessStatusCode tess_text_starts_with(const TessDatumColumn *column,
											const char *prefix,
											Size len,
											TessRowMask *rows,
											TessRowMask *rest,
											TessStatus *status);

/*
 * LIKE (or, negate, NOT LIKE) of a column and len bytes of a pattern of
 * literals and %, matched by its pieces, rest as above; *simple false, and
 * nothing done, for a pattern with _ or an escape, or of too many pieces,
 * which the core's function matches.
 */
extern TessStatusCode tess_text_like(const TessDatumColumn *column,
									 const char *pattern,
									 Size len,
									 bool negate,
									 TessRowMask *rows,
									 TessRowMask *rest,
									 bool *simple,
									 TessStatus *status);

/*
 * The lengths of the selected rows' strings into values: non_nulls gets
 * the rows without NULL, rest those of them whose string is not in place.
 */
extern TessStatusCode tess_text_lengths(TessTextLength length,
										TessTextChars chars,
										const TessDatumColumn *column,
										const TessRowMask *rows,
										int32 *values,
										TessRowMask *non_nulls,
										TessRowMask *rest,
										TessStatus *status);

/*
 * The bounds of a piece of the selected rows' strings, as the core's
 * text_substring, text_left, text_right and trims take them: the first
 * byte in starts and the bytes in lengths, the masks as above; a
 * substring of negative length fails with TESS_ERROR_DATA_EXCEPTION,
 * 22011 "negative substring length not allowed", at the first row
 * without NULL.
 */
extern TessStatusCode tess_text_pieces(TessTextPiece piece,
									   int32 first,
									   int32 second,
									   bool has_second,
									   TessTextChars chars,
									   const TessDatumColumn *column,
									   const TessRowMask *rows,
									   int32 *starts,
									   int32 *lengths,
									   TessRowMask *non_nulls,
									   TessRowMask *rest,
									   TessStatus *status);

#endif							/* TESSERA_TEXT_H */
