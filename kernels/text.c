/*
 * The text functions as batch functions, over the Datums of a column, row
 * by row in C: equality and inequality of text (varchar through it) and of
 * bpchar, whose trailing spaces do not count, starts_with, LIKE and NOT
 * LIKE of a constant pattern, and the lengths, registered with
 * TESS_FUNCTION_DETERMINISTIC_COLLATION: under a deterministic collation
 * equal strings are equal bytes and LIKE matches bytes, which is all these
 * compare; a consumer leaves any other collation to the executor. And the
 * pieces of a string, substring, left, right and the trims of spaces, text
 * values allocated in the call's context, which no collation changes.
 *
 * A compressed or external value is detoasted for its row and freed after
 * it; the others are read in place. A pattern of literals and % is matched by its
 * pieces: the first against the start, the last against the end, the
 * others found in order between; any other pattern (_ or an escape) goes
 * to the core's function a row, which the batch saves the interpreter of.
 */
#include "postgres.h"

#include "access/detoast.h"
#include "common/int.h"
#include "fmgr.h"
#include "mb/pg_wchar.h"
#include "port/pg_bitutils.h"
#include "utils/builtins.h"
#include "utils/fmgroids.h"
#include "utils/fmgrprotos.h"
#include "varatt.h"

#include "tessera/bridge.h"

#include "internal.h"

typedef enum TextOp
{
	TEXT_EQ,
	TEXT_NE,
	BPCHAR_EQ,
	BPCHAR_NE,
	TEXT_STARTS_WITH,
	TEXT_LIKE,
	TEXT_NOT_LIKE,
	TEXT_CHARS,
	BPCHAR_CHARS,
	TEXT_OCTETS,
	TEXT_SUBSTR,
	TEXT_LEFT,
	TEXT_RIGHT,
	TEXT_RTRIM,
	TEXT_LTRIM,
	TEXT_BTRIM
} TextOp;

typedef struct TextFunction
{
	TessFunction function;
	TextOp		op;
} TextFunction;

static TessStatusCode text_compare_evaluate(TessFunctionCall *call);
static TessStatusCode text_pattern_evaluate(TessFunctionCall *call);
static TessStatusCode text_length_evaluate(TessFunctionCall *call);
static TessStatusCode text_piece_evaluate(TessFunctionCall *call);

#define TEXT_FLAGS (TESS_FUNCTION_STRICT | TESS_FUNCTION_DETERMINISTIC_COLLATION)

/* A comparison of two strings, a column on either side or both. */
#define TEXT_COMPARE(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TEXT_FLAGS | TESS_FUNCTION_ANY_SHAPE, \
	  .evaluate = text_compare_evaluate}, (code)}

/* A string column against a constant: a prefix or a pattern. */
#define TEXT_PATTERN(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TEXT_FLAGS, \
	  .evaluate = text_pattern_evaluate}, (code)}

/* A length, an int4 of the string column. */
#define TEXT_LENGTH(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = TESS_RESULT_INT32, \
	  .flags = TEXT_FLAGS, \
	  .evaluate = text_length_evaluate}, (code)}

/* A piece of the string column, a text; the other arguments constants. */
#define TEXT_PIECE(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE, \
	  .evaluate = text_piece_evaluate}, (code)}

static const TextFunction text_functions[] = {
	TEXT_COMPARE(F_TEXTEQ, TEXT_EQ),
	TEXT_COMPARE(F_TEXTNE, TEXT_NE),
	TEXT_COMPARE(F_BPCHAREQ, BPCHAR_EQ),
	TEXT_COMPARE(F_BPCHARNE, BPCHAR_NE),
	TEXT_PATTERN(F_STARTS_WITH, TEXT_STARTS_WITH),
	TEXT_PATTERN(F_TEXTLIKE, TEXT_LIKE),
	TEXT_PATTERN(F_TEXTNLIKE, TEXT_NOT_LIKE),
	TEXT_PATTERN(F_BPCHARLIKE, TEXT_LIKE),
	TEXT_PATTERN(F_BPCHARNLIKE, TEXT_NOT_LIKE),
	TEXT_LENGTH(F_LENGTH_TEXT, TEXT_CHARS),
	TEXT_LENGTH(F_CHAR_LENGTH_TEXT, TEXT_CHARS),
	TEXT_LENGTH(F_CHARACTER_LENGTH_TEXT, TEXT_CHARS),
	TEXT_LENGTH(F_LENGTH_BPCHAR, BPCHAR_CHARS),
	TEXT_LENGTH(F_CHAR_LENGTH_BPCHAR, BPCHAR_CHARS),
	TEXT_LENGTH(F_OCTET_LENGTH_TEXT, TEXT_OCTETS),
	TEXT_LENGTH(F_OCTET_LENGTH_BPCHAR, TEXT_OCTETS),
	TEXT_PIECE(F_SUBSTRING_TEXT_INT4_INT4, TEXT_SUBSTR),
	TEXT_PIECE(F_SUBSTRING_TEXT_INT4, TEXT_SUBSTR),
	TEXT_PIECE(F_SUBSTR_TEXT_INT4_INT4, TEXT_SUBSTR),
	TEXT_PIECE(F_SUBSTR_TEXT_INT4, TEXT_SUBSTR),
	TEXT_PIECE(F_LEFT, TEXT_LEFT),
	TEXT_PIECE(F_RIGHT, TEXT_RIGHT),
	TEXT_PIECE(F_TEXT_BPCHAR, TEXT_RTRIM),
	TEXT_PIECE(F_RTRIM_TEXT, TEXT_RTRIM),
	TEXT_PIECE(F_LTRIM_TEXT, TEXT_LTRIM),
	TEXT_PIECE(F_BTRIM_TEXT, TEXT_BTRIM),
};

static TextOp
text_op(const TessFunctionCall *call)
{
	return ((const TextFunction *) ((const char *) call->function -
									offsetof(TextFunction, function)))->op;
}

static TessStatusCode
text_invalid(TessFunctionCall *call, const char *message)
{
	if (call->status != NULL && call->status->struct_size >= TESS_STATUS_MIN_SIZE)
	{
		call->status->code = TESS_ERROR_INVALID_ARGUMENT;
		strlcpy(call->status->sqlstate, "XX000", sizeof(call->status->sqlstate));
		strlcpy(call->status->message, message, sizeof(call->status->message));
	}
	return TESS_ERROR_INVALID_ARGUMENT;
}

static bool
text_call_valid(const TessFunctionCall *call, int nargs)
{
	return call != NULL && call->struct_size >= TESS_FUNCTION_CALL_MIN_SIZE &&
		call->nargs == nargs && call->args != NULL && call->rows != NULL &&
		call->args[0].struct_size >= TESS_FUNCTION_ARG_MIN_SIZE &&
		(nargs < 2 || call->args[1].struct_size >= TESS_FUNCTION_ARG_MIN_SIZE);
}

/*
 * A string's bytes: in place, or of a detoasted copy when compressed or
 * external, which the caller frees after the row (string_free): the
 * call's context lives as long as the expression.
 */
static inline struct varlena *
string_bytes(Datum value, const char **data, int *len)
{
	struct varlena *string = (struct varlena *) DatumGetPointer(value);
	struct varlena *copy = NULL;

	if (VARATT_IS_COMPRESSED(string) || VARATT_IS_EXTERNAL(string))
		string = copy = pg_detoast_datum_packed(string);
	*data = VARDATA_ANY(string);
	*len = VARSIZE_ANY_EXHDR(string);
	return copy;
}

static inline void
string_free(struct varlena *copy)
{
	if (copy != NULL)
		pfree(copy);
}

/* A bpchar's length without its trailing spaces, as bcTruelen counts. */
static inline int
trimmed(const char *data, int len)
{
	while (len > 0 && data[len - 1] == ' ')
		len--;
	return len;
}

/*
 * How the database encoding counts characters: a byte each, UTF-8 by its
 * lead bytes without a call per character, or the encoding's pg_mblen.
 */
typedef enum CharCount
{
	CHARS_BYTES,
	CHARS_UTF8,
	CHARS_MBLEN
} CharCount;

static CharCount
char_count(void)
{
	if (pg_database_encoding_max_length() == 1)
		return CHARS_BYTES;
	return GetDatabaseEncoding() == PG_UTF8 ? CHARS_UTF8 : CHARS_MBLEN;
}

/* The characters of the bytes: in UTF-8 the bytes that do not continue one. */
static inline int
string_chars(const char *data, int len, CharCount count)
{
	int			chars = 0;

	if (count == CHARS_BYTES)
		return len;
	if (count == CHARS_MBLEN)
		return pg_mbstrlen_with_len(data, len);
	for (int at = 0; at < len; at++)
		chars += ((unsigned char) data[at] & 0xC0) != 0x80;
	return chars;
}

/* A UTF-8 character's length by its lead byte, as pg_utf_mblen. */
static inline int
utf8_length(unsigned char lead)
{
	if ((lead & 0x80) == 0)
		return 1;
	if ((lead & 0xE0) == 0xC0)
		return 2;
	if ((lead & 0xF0) == 0xE0)
		return 3;
	if ((lead & 0xF8) == 0xF0)
		return 4;
	return 1;
}

/* The byte offset past the first chars characters of the bytes, at most len. */
static inline int
char_offset(const char *data, int len, int64 chars, CharCount count)
{
	int			offset = 0;

	if (count == CHARS_BYTES)
		return (int) Max(Min((int64) len, chars), 0);
	for (; chars > 0 && offset < len; chars--)
		offset += count == CHARS_UTF8 ? utf8_length((unsigned char) data[offset]) :
			pg_mblen(data + offset);
	return Min(offset, len);
}

/* One side of a comparison: a column, or a scalar's bytes read once. */
typedef struct TextSide
{
	const TessDatumColumn *column;
	const char *data;
	int			len;
	struct varlena *copy;
} TextSide;

static void
text_side(const TessFunctionArg *arg, TextSide *side)
{
	side->column = arg->column;
	side->copy = NULL;
	if (arg->column == NULL)
		side->copy = string_bytes(arg->scalar, &side->data, &side->len);
}

/* A row's bytes of a side; the copy to free, a scalar's being the side's. */
static inline struct varlena *
side_bytes(const TextSide *side, int row, const char **data, int *len)
{
	if (side->column == NULL)
	{
		*data = side->data;
		*len = side->len;
		return NULL;
	}
	return string_bytes(side->column->values[row], data, len);
}

/*
 * Equality and inequality: equal lengths and equal bytes, a bpchar's
 * without trailing spaces; a NULL on a column's row drops it, as a strict
 * predicate does.
 */
static TessStatusCode
text_compare_evaluate(TessFunctionCall *call)
{
	TextSide	left;
	TextSide	right;
	TextOp		op;
	bool		bpchar;
	bool		equal_wanted;
	int			nwords;

	if (!text_call_valid(call, 2))
		return text_invalid(call, "a string comparison takes two arguments");
	if (call->args[0].column == NULL && call->args[1].column == NULL)
		return text_invalid(call, "a string comparison needs a column argument");
	text_side(&call->args[0], &left);
	text_side(&call->args[1], &right);
	op = text_op(call);
	bpchar = op == BPCHAR_EQ || op == BPCHAR_NE;
	equal_wanted = op == TEXT_EQ || op == BPCHAR_EQ;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		keep = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			const char *a;
			const char *b;
			int			alen;
			int			blen;
			struct varlena *acopy;
			struct varlena *bcopy;
			bool		equal;

			if ((left.column != NULL && left.column->isnull[row]) ||
				(right.column != NULL && right.column->isnull[row]))
				continue;
			acopy = side_bytes(&left, row, &a, &alen);
			bcopy = side_bytes(&right, row, &b, &blen);
			if (bpchar)
			{
				alen = trimmed(a, alen);
				blen = trimmed(b, blen);
			}
			equal = alen == blen && memcmp(a, b, alen) == 0;
			string_free(acopy);
			string_free(bcopy);
			if (equal == equal_wanted)
				keep |= UINT64CONST(1) << bit;
		}
		call->rows->bits[word] = keep;
	}
	string_free(left.copy);
	string_free(right.copy);
	return TESS_OK;
}

/* The first place of needle in the bytes, or NULL. */
static const char *
find_bytes(const char *bytes, int len, const char *needle, int nlen)
{
	const char *end = bytes + len - nlen;

	if (nlen == 0)
		return bytes;
	if (nlen > len)
		return NULL;
	for (const char *at = bytes; at <= end; at++)
	{
		at = memchr(at, needle[0], end - at + 1);
		if (at == NULL)
			return NULL;
		if (memcmp(at, needle, nlen) == 0)
			return at;
	}
	return NULL;
}

/*
 * A LIKE pattern of literals and %: its pieces between the %s, whether
 * the first is anchored at the start and the last at the end; simple is
 * false for a pattern with _ or an escape, which the core's function
 * matches instead, and in a multibyte encoding other than UTF-8, where a
 * piece's bytes may start inside a character.
 */
typedef struct LikePieces
{
	bool		simple;
	bool		exact;
	bool		start;
	bool		end;
	int			npieces;
	const char **pieces;
	int		   *lens;
} LikePieces;

static void
like_pieces(const char *pattern, int plen, LikePieces *like)
{
	int			begin = 0;

	like->simple = GetDatabaseEncoding() == PG_UTF8 || pg_database_encoding_max_length() == 1;
	like->exact = true;
	if (!like->simple)
		return;
	like->npieces = 0;
	like->pieces = palloc(sizeof(char *) * (plen + 1));
	like->lens = palloc(sizeof(int) * (plen + 1));
	for (int at = 0; at <= plen; at++)
	{
		if (at < plen && (pattern[at] == '_' || pattern[at] == '\\'))
		{
			like->simple = false;
			return;
		}
		if (at == plen || pattern[at] == '%')
		{
			like->pieces[like->npieces] = pattern + begin;
			like->lens[like->npieces++] = at - begin;
			begin = at + 1;
			if (at < plen)
				like->exact = false;
		}
	}
	like->start = plen == 0 || pattern[0] != '%';
	like->end = plen == 0 || pattern[plen - 1] != '%';
}

/* Whether the string matches the pieces. */
static bool
like_matches(const LikePieces *like, const char *string, int len)
{
	int			first = 0;
	int			last = like->npieces - 1;
	int			from = 0;
	int			to = len;

	if (like->exact)
		return len == like->lens[0] && memcmp(string, like->pieces[0], len) == 0;
	if (like->start)
	{
		if (len < like->lens[0] || memcmp(string, like->pieces[0], like->lens[0]) != 0)
			return false;
		from = like->lens[0];
		first = 1;
	}
	if (like->end)
	{
		int			tail = like->lens[last];

		if (to - from < tail || memcmp(string + len - tail, like->pieces[last], tail) != 0)
			return false;
		to = len - tail;
		last--;
	}
	for (int piece = first; piece <= last; piece++)
	{
		const char *found;

		if (like->lens[piece] == 0)
			continue;
		found = find_bytes(string + from, to - from, like->pieces[piece], like->lens[piece]);
		if (found == NULL)
			return false;
		from = (found - string) + like->lens[piece];
	}
	return true;
}

/*
 * starts_with, LIKE and NOT LIKE of a string column against a constant:
 * the prefix's bytes, the pattern's pieces once a call, or the core's
 * function a row for another pattern.
 */
static TessStatusCode
text_pattern_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *column;
	TextOp		op;
	const char *pattern;
	int			plen;
	struct varlena *pattern_copy;
	Datum		pattern_datum;
	LikePieces	like = {0};
	int			nwords;

	if (!text_call_valid(call, 2))
		return text_invalid(call, "a string pattern takes two arguments");
	column = &call->args[0];
	if (column->column == NULL || call->args[1].column != NULL)
		return text_invalid(call, "a string pattern takes a column and a constant");
	op = text_op(call);
	pattern_copy = string_bytes(call->args[1].scalar, &pattern, &plen);
	pattern_datum = pattern_copy != NULL ? PointerGetDatum(pattern_copy) : call->args[1].scalar;
	if (op != TEXT_STARTS_WITH)
		like_pieces(pattern, plen, &like);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		keep = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			const char *string;
			int			len;
			struct varlena *copy;
			bool		result;

			if (column->column->isnull[row])
				continue;
			copy = string_bytes(column->column->values[row], &string, &len);
			if (op == TEXT_STARTS_WITH)
				result = len >= plen && memcmp(string, pattern, plen) == 0;
			else if (like.simple)
				result = like_matches(&like, string, len) == (op == TEXT_LIKE);
			else
			{
				/* Detoasted here, both are read in place and nothing is left to free. */
				Datum		value = copy != NULL ? PointerGetDatum(copy) : column->column->values[row];

				result = DatumGetBool(DirectFunctionCall2Coll(op == TEXT_LIKE ? textlike : textnlike,
															  call->inputcollid, value,
															  pattern_datum));
			}
			string_free(copy);
			if (result)
				keep |= UINT64CONST(1) << bit;
		}
		call->rows->bits[word] = keep;
	}
	if (like.pieces != NULL)
	{
		pfree(like.pieces);
		pfree(like.lens);
	}
	string_free(pattern_copy);
	return TESS_OK;
}

/* The lengths: characters, a bpchar's without trailing spaces, or bytes. */
static TessStatusCode
text_length_evaluate(TessFunctionCall *call)
{
	const TessDatumColumn *column;
	int32	   *values;
	TextOp		op;
	CharCount	count = char_count();
	int			nwords;

	if (!text_call_valid(call, 1) || call->values == NULL || call->non_nulls == NULL)
		return text_invalid(call, "a string length takes one argument");
	column = call->args[0].column;
	if (column == NULL)
		return text_invalid(call, "a string length takes a column");
	op = text_op(call);
	values = (int32 *) call->values;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			const char *string;
			int			len;
			struct varlena *copy;

			if (column->isnull[row])
				continue;
			present |= UINT64CONST(1) << bit;
			/* The bytes of a compressed or external value without reading it, as octet_length. */
			if (op == TEXT_OCTETS)
			{
				values[row] = toast_raw_datum_size(column->values[row]) - VARHDRSZ;
				continue;
			}
			copy = string_bytes(column->values[row], &string, &len);
			if (op == BPCHAR_CHARS)
				len = trimmed(string, len);
			values[row] = string_chars(string, len, count);
			string_free(copy);
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/*
 * Memory for a batch's text results, carved from blocks of the call's
 * context: a palloc per row costs as much as the piece.
 */
typedef struct TextArena
{
	MemoryContext context;
	char	   *next;
	Size		left;
} TextArena;

static text *
arena_text(TextArena *arena, int len)
{
	Size		size = MAXALIGN(VARHDRSZ + len);
	text	   *result;

	if (size > arena->left)
	{
		Size		block = Max(size, 8192);

		arena->next = MemoryContextAlloc(arena->context, block);
		arena->left = block;
	}
	result = (text *) arena->next;
	arena->next += size;
	arena->left -= size;
	SET_VARSIZE(result, VARHDRSZ + len);
	return result;
}

/*
 * The first character and the count of characters a piece takes, as the
 * core's text_substring, text_left and text_right count them: substring
 * from start for length (none: to the end) begins at Max(start, 1) and
 * ends before start + length, an overflow running to the end; left of n
 * < 0 drops the last -n characters and right of n < 0 the first -n, a
 * string's length in characters counted only then. False for a negative
 * substring length, 22011.
 */
static bool
piece_bounds(TextOp op, const TessFunctionCall *call, const char *data, int len,
			 CharCount count, int64 *skip, int64 *take)
{
	int32		n = DatumGetInt32(call->args[1].scalar);

	*skip = 0;
	*take = PG_INT64_MAX;
	if (op == TEXT_SUBSTR)
	{
		int32		end;

		*skip = Max(n, 1) - 1;
		if (call->nargs < 3)
			return true;
		if (DatumGetInt32(call->args[2].scalar) < 0)
			return false;
		if (!pg_add_s32_overflow(n, DatumGetInt32(call->args[2].scalar), &end))
			*take = end < 1 ? 0 : end - Max(n, 1);
	}
	else if (op == TEXT_LEFT)
	{
		if (n >= 0)
			*take = n;
		else
			*take = string_chars(data, len, count) + (int64) n;
	}
	else
	{
		/* -n in 64 bits: right(-2147483648) skips it all, as the core clamps. */
		if (n < 0)
			*skip = -(int64) n;
		else
			*skip = string_chars(data, len, count) - (int64) n;
	}
	return true;
}

/*
 * substring, substr, left and right of a string column by constants, and
 * rtrim, ltrim and btrim of spaces (text(bpchar) is rtrim): a text value
 * per row in the call's context.
 */
static TessStatusCode
text_piece_evaluate(TessFunctionCall *call)
{
	const TessDatumColumn *column;
	Datum	   *values;
	TextOp		op;
	CharCount	count = char_count();
	TextArena	arena = {0};
	int			nwords;

	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs < 1 || call->nargs > 3 || call->args == NULL || call->rows == NULL ||
		call->values == NULL || call->non_nulls == NULL || call->context == NULL)
		return text_invalid(call, "a string piece takes a column and constants");
	for (int arg = 0; arg < call->nargs; arg++)
		if (call->args[arg].struct_size < TESS_FUNCTION_ARG_MIN_SIZE ||
			(call->args[arg].column != NULL) != (arg == 0))
			return text_invalid(call, "a string piece takes a column and constants");
	column = call->args[0].column;
	op = text_op(call);
	if (call->nargs != (op == TEXT_SUBSTR ? Max(call->nargs, 2) :
						op == TEXT_LEFT || op == TEXT_RIGHT ? 2 : 1))
		return text_invalid(call, "a string piece takes a column and constants");
	values = (Datum *) call->values;
	arena.context = call->context;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; word < nwords; word++)
	{
		uint64		look = call->rows->bits[word];
		uint64		present = 0;

		for (; look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			const char *string;
			int			len;
			struct varlena *copy;
			int			from = 0;
			int			to;
			text	   *result;

			if (column->isnull[row])
				continue;
			copy = string_bytes(column->values[row], &string, &len);
			to = len;
			if (op == TEXT_RTRIM || op == TEXT_BTRIM)
				to = trimmed(string, len);
			if (op == TEXT_LTRIM || op == TEXT_BTRIM)
				while (from < to && string[from] == ' ')
					from++;
			if (op == TEXT_SUBSTR || op == TEXT_LEFT || op == TEXT_RIGHT)
			{
				int64		skip;
				int64		take;

				if (!piece_bounds(op, call, string, len, count, &skip, &take))
				{
					string_free(copy);
					call->non_nulls->bits[word] = present;
					if (call->status != NULL &&
						call->status->struct_size >= TESS_STATUS_MIN_SIZE)
					{
						call->status->code = TESS_ERROR_DATA_EXCEPTION;
						strlcpy(call->status->sqlstate, "22011",
								sizeof(call->status->sqlstate));
						strlcpy(call->status->message,
								"negative substring length not allowed",
								sizeof(call->status->message));
					}
					return TESS_ERROR_DATA_EXCEPTION;
				}
				from = char_offset(string, len, skip, count);
				to = take <= 0 ? from :
					from + char_offset(string + from, len - from, take, count);
			}
			result = arena_text(&arena, to - from);
			memcpy(VARDATA(result), string + from, to - from);
			string_free(copy);
			values[row] = PointerGetDatum(result);
			present |= UINT64CONST(1) << bit;
		}
		call->non_nulls->bits[word] = present;
	}
	return TESS_OK;
}

/* Register the text functions. */
void
tess_register_text_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(text_functions); i++)
		functions->add(&text_functions[i].function);
}
