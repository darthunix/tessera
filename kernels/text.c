/*
 * The text functions as batch functions: equality and inequality of text
 * (varchar through it) and of bpchar, whose trailing spaces do not count,
 * starts_with, LIKE and NOT LIKE of a constant pattern, and the lengths,
 * registered with TESS_FUNCTION_DETERMINISTIC_COLLATION: under a
 * deterministic collation equal strings are equal bytes and LIKE matches
 * bytes, which is all these compare; a consumer leaves any other collation
 * to the executor. And the pieces of a string, substring, left, right and
 * the trims of spaces, text values allocated in the call's context, which
 * no collation changes.
 *
 * The bytes are the Rust kernels' (tessera/text.h), a batch a call: they
 * read a string in place, compare it, match a pattern of literals and %
 * by its pieces, count its characters (a byte each, or UTF-8 by its lead
 * bytes) and give the bounds of a piece, which is copied here into blocks
 * of the call's context. A compressed or external value they leave: it is
 * detoasted here for its row, handed back as a column of that one row and
 * freed after it. A pattern with _ or an escape, and the characters of a
 * multibyte encoding other than UTF-8, go to the core's function a row.
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
#include "tessera/text.h"

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

/*
 * How the kernels count the database encoding's characters, or -1 for a
 * multibyte encoding other than UTF-8, whose characters the core counts.
 */
static int
text_chars(void)
{
	if (pg_database_encoding_max_length() == 1)
		return TESS_CHARS_BYTES;
	return GetDatabaseEncoding() == PG_UTF8 ? TESS_CHARS_UTF8 : -1;
}

/*
 * Scratch of a call: the masks and bounds the kernels write, on the stack
 * for a batch of up to this many rows, palloc'd beyond.
 */
#define SCRATCH_ROWS 1024

typedef struct TextScratch
{
	uint64		words[2][SCRATCH_ROWS / 64];
	int32		starts[SCRATCH_ROWS];
	int32		lengths[SCRATCH_ROWS];
} TextScratch;

static void *
scratch_alloc(Size size, void *local, Size local_size)
{
	return size <= local_size ? local : palloc(size);
}

static void
scratch_release(void *pointer, void *local)
{
	if (pointer != local)
		pfree(pointer);
}

/* An empty mask of the batch's rows over scratch words. */
static TessRowMask
scratch_mask(TextScratch *space, int index, int nrows)
{
	int			nwords = tess_row_mask_word_count(nrows);
	uint64	   *words = scratch_alloc(sizeof(uint64) * nwords, space->words[index],
									  sizeof(space->words[index]));

	for (int word = 0; word < nwords; word++)
		words[word] = 0;
	return (TessRowMask) {nrows, words};
}

/*
 * A row's string detoasted, as a column of that one row for the kernels,
 * with the masks of a call over it; the copy is freed by one_row_free.
 */
typedef struct OneRow
{
	Datum		value;
	bool		isnull;
	uint64		selected;
	uint64		out;
	uint64		other;
	TessDatumColumn column;
	TessRowMask rows;
	TessRowMask result;
	TessRowMask rest;
	struct varlena *copy;
} OneRow;

static void
one_row(OneRow *one, Datum value)
{
	const char *data;
	int			len;

	one->copy = string_bytes(value, &data, &len);
	one->value = one->copy != NULL ? PointerGetDatum(one->copy) : value;
	one->isnull = false;
	one->selected = 1;
	one->out = 0;
	one->other = 0;
	one->column = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	one->column.values = &one->value;
	one->column.isnull = &one->isnull;
	one->column.nrows = 1;
	one->rows = (TessRowMask) {1, &one->selected};
	one->result = (TessRowMask) {1, &one->out};
	one->rest = (TessRowMask) {1, &one->other};
}

static void
one_row_free(OneRow *one)
{
	string_free(one->copy);
}

/* A kernels' argument: its column, or its scalar's bytes read in place. */
static TessTextArg
text_arg(const TessFunctionArg *arg, struct varlena **copy)
{
	const char *data;
	int			len;

	*copy = NULL;
	if (arg->column != NULL)
		return (TessTextArg) {arg->column, 0};
	*copy = string_bytes(arg->scalar, &data, &len);
	return (TessTextArg) {NULL, *copy != NULL ? PointerGetDatum(*copy) : arg->scalar};
}

/*
 * Equality and inequality: equal lengths and equal bytes, a bpchar's
 * without trailing spaces; a NULL on a column's row drops it, as a strict
 * predicate does.
 */
static TessStatusCode
text_compare_evaluate(TessFunctionCall *call)
{
	TextOp		op;
	bool		bpchar;
	bool		equal;
	struct varlena *left_copy;
	struct varlena *right_copy;
	TessTextArg left;
	TessTextArg right;
	TextScratch space;
	TessRowMask rest;
	TessStatusCode code;
	int			nwords;

	if (!text_call_valid(call, 2))
		return text_invalid(call, "a string comparison takes two arguments");
	if (call->args[0].column == NULL && call->args[1].column == NULL)
		return text_invalid(call, "a string comparison needs a column argument");
	op = text_op(call);
	bpchar = op == BPCHAR_EQ || op == BPCHAR_NE;
	equal = op == TEXT_EQ || op == BPCHAR_EQ;
	left = text_arg(&call->args[0], &left_copy);
	right = text_arg(&call->args[1], &right_copy);
	rest = scratch_mask(&space, 0, call->rows->nrows);
	code = tess_text_compare(equal, bpchar, &left, &right, call->rows, &rest, call->status);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	for (int word = 0; code == TESS_OK && word < nwords; word++)
	{
		for (uint64 look = rest.bits[word]; code == TESS_OK && look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			OneRow		left_row;
			OneRow		right_row;
			TessTextArg one_left = left;
			TessTextArg one_right = right;

			one_row(&left_row, left.column != NULL ? left.column->values[row] : left.scalar);
			one_row(&right_row, right.column != NULL ? right.column->values[row] : right.scalar);
			if (left.column != NULL)
				one_left.column = &left_row.column;
			if (right.column != NULL)
				one_right.column = &right_row.column;
			left_row.out = 1;
			code = tess_text_compare(equal, bpchar, &one_left, &one_right, &left_row.result,
									 &left_row.rest, call->status);
			if (left_row.out != 0)
				call->rows->bits[word] |= UINT64CONST(1) << bit;
			one_row_free(&left_row);
			one_row_free(&right_row);
		}
	}
	scratch_release(rest.bits, space.words[0]);
	string_free(left_copy);
	string_free(right_copy);
	return code;
}

/*
 * starts_with, LIKE and NOT LIKE of a string column against a constant:
 * the prefix's bytes, the pattern's pieces, or the core's function a row
 * for another pattern or in a multibyte encoding other than UTF-8, where
 * a piece's bytes may start inside a character.
 */
static TessStatusCode
text_pattern_evaluate(TessFunctionCall *call)
{
	const TessDatumColumn *column;
	TextOp		op;
	const char *pattern;
	int			plen;
	struct varlena *pattern_copy;
	Datum		pattern_datum;
	TextScratch space;
	TessRowMask rest;
	TessStatusCode code = TESS_OK;
	bool		simple = false;
	int			nwords;

	if (!text_call_valid(call, 2))
		return text_invalid(call, "a string pattern takes two arguments");
	column = call->args[0].column;
	if (column == NULL || call->args[1].column != NULL)
		return text_invalid(call, "a string pattern takes a column and a constant");
	op = text_op(call);
	pattern_copy = string_bytes(call->args[1].scalar, &pattern, &plen);
	pattern_datum = pattern_copy != NULL ? PointerGetDatum(pattern_copy) : call->args[1].scalar;
	rest = scratch_mask(&space, 0, call->rows->nrows);
	nwords = tess_row_mask_word_count(call->rows->nrows);
	if (op == TEXT_STARTS_WITH)
	{
		simple = true;
		code = tess_text_starts_with(column, pattern, plen, call->rows, &rest, call->status);
	}
	else if (text_chars() >= 0)
		code = tess_text_like(column, pattern, plen, op == TEXT_NOT_LIKE, call->rows, &rest,
							  &simple, call->status);
	for (int word = 0; code == TESS_OK && word < nwords; word++)
	{
		/* The rows the kernels left, or every row of a pattern they do not take. */
		uint64		look = simple ? rest.bits[word] : call->rows->bits[word];

		if (!simple)
			call->rows->bits[word] = 0;
		for (; code == TESS_OK && look != 0; look &= look - 1)
		{
			int			bit = pg_rightmost_one_pos64(look);
			int			row = word * 64 + bit;
			OneRow		one;
			bool		result;

			if (column->isnull[row])
				continue;
			one_row(&one, column->values[row]);
			if (simple)
			{
				one.out = 1;
				code = op == TEXT_STARTS_WITH ?
					tess_text_starts_with(&one.column, pattern, plen, &one.result, &one.rest,
										  call->status) :
					tess_text_like(&one.column, pattern, plen, op == TEXT_NOT_LIKE, &one.result,
								   &one.rest, &simple, call->status);
				result = one.out != 0;
			}
			else
				result = DatumGetBool(DirectFunctionCall2Coll(op == TEXT_LIKE ? textlike : textnlike,
															  call->inputcollid, one.value,
															  pattern_datum));
			one_row_free(&one);
			if (result)
				call->rows->bits[word] |= UINT64CONST(1) << bit;
		}
	}
	scratch_release(rest.bits, space.words[0]);
	string_free(pattern_copy);
	return code;
}

/*
 * The lengths: characters, a bpchar's without trailing spaces, or bytes;
 * the characters of a multibyte encoding other than UTF-8 by the core's
 * functions a row.
 */
static TessStatusCode
text_length_evaluate(TessFunctionCall *call)
{
	const TessDatumColumn *column;
	int32	   *values;
	TextOp		op;
	int			chars = text_chars();
	TessTextLength length;
	TextScratch space;
	TessRowMask rest;
	TessStatusCode code;
	int			nwords;

	if (!text_call_valid(call, 1) || call->values == NULL || call->non_nulls == NULL)
		return text_invalid(call, "a string length takes one argument");
	column = call->args[0].column;
	if (column == NULL)
		return text_invalid(call, "a string length takes a column");
	op = text_op(call);
	values = (int32 *) call->values;
	length = op == TEXT_OCTETS ? TESS_LENGTH_OCTETS :
		op == BPCHAR_CHARS ? TESS_LENGTH_BPCHAR_CHARS : TESS_LENGTH_CHARS;
	nwords = tess_row_mask_word_count(call->rows->nrows);
	if (chars < 0 && op != TEXT_OCTETS)
	{
		for (int word = 0; word < nwords; word++)
		{
			uint64		present = 0;

			for (uint64 look = call->rows->bits[word]; look != 0; look &= look - 1)
			{
				int			bit = pg_rightmost_one_pos64(look);
				int			row = word * 64 + bit;

				if (column->isnull[row])
					continue;
				values[row] = DatumGetInt32(DirectFunctionCall1(op == BPCHAR_CHARS ? bpcharlen : textlen,
																column->values[row]));
				present |= UINT64CONST(1) << bit;
			}
			call->non_nulls->bits[word] = present;
		}
		return TESS_OK;
	}
	rest = scratch_mask(&space, 0, call->rows->nrows);
	code = tess_text_lengths(length, chars < 0 ? TESS_CHARS_BYTES : chars, column, call->rows,
							 values, call->non_nulls, &rest, call->status);
	for (int word = 0; code == TESS_OK && word < nwords; word++)
	{
		for (uint64 look = rest.bits[word]; code == TESS_OK && look != 0; look &= look - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(look);
			OneRow		one;
			int32		one_value;
			uint64		present = 0;
			TessRowMask one_present = {1, &present};

			/* The bytes of a compressed or external value without reading it, as octet_length. */
			if (op == TEXT_OCTETS)
			{
				values[row] = toast_raw_datum_size(column->values[row]) - VARHDRSZ;
				continue;
			}
			one_row(&one, column->values[row]);
			code = tess_text_lengths(length, chars, &one.column, &one.rows, &one_value,
									 &one_present, &one.rest, call->status);
			values[row] = one_value;
			one_row_free(&one);
		}
	}
	scratch_release(rest.bits, space.words[0]);
	return code;
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
arena_text(TextArena *arena, const char *data, int len)
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
	memcpy(VARDATA(result), data, len);
	return result;
}

/* A piece of the core's function, for the characters the kernels do not count. */
static Datum
piece_core(TextOp op, const TessFunctionCall *call, Datum value)
{
	switch (op)
	{
		case TEXT_SUBSTR:
			return call->nargs == 3 ?
				DirectFunctionCall3(text_substr, value, call->args[1].scalar, call->args[2].scalar) :
				DirectFunctionCall2(text_substr_no_len, value, call->args[1].scalar);
		case TEXT_LEFT:
			return DirectFunctionCall2(text_left, value, call->args[1].scalar);
		default:
			return DirectFunctionCall2(text_right, value, call->args[1].scalar);
	}
}

/*
 * substring, substr, left and right of a string column by constants, and
 * rtrim, ltrim and btrim of spaces (text(bpchar) is rtrim): a text value
 * per row in the call's context, the kernels' bounds of each piece.
 */
static TessStatusCode
text_piece_evaluate(TessFunctionCall *call)
{
	static const TessTextPiece pieces[] = {
		[TEXT_SUBSTR] = TESS_PIECE_SUBSTRING, [TEXT_LEFT] = TESS_PIECE_LEFT,
		[TEXT_RIGHT] = TESS_PIECE_RIGHT, [TEXT_RTRIM] = TESS_PIECE_RTRIM,
		[TEXT_LTRIM] = TESS_PIECE_LTRIM, [TEXT_BTRIM] = TESS_PIECE_BTRIM,
	};
	const TessDatumColumn *column;
	Datum	   *values;
	TextOp		op;
	int			chars = text_chars();
	TextArena	arena = {0};
	TextScratch space;
	TessRowMask rest;
	int32	   *starts;
	int32	   *lengths;
	int32		first = 0;
	int32		second = 0;
	TessStatusCode code;
	int			nrows;
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
	nrows = call->rows->nrows;
	nwords = tess_row_mask_word_count(nrows);
	if (call->nargs >= 2)
		first = DatumGetInt32(call->args[1].scalar);
	if (call->nargs == 3)
		second = DatumGetInt32(call->args[2].scalar);
	/* The trims count no characters: spaces are bytes in every encoding. */
	if (chars < 0 && (op == TEXT_SUBSTR || op == TEXT_LEFT || op == TEXT_RIGHT))
	{
		MemoryContext old = MemoryContextSwitchTo(call->context);

		for (int word = 0; word < nwords; word++)
		{
			uint64		present = 0;

			for (uint64 look = call->rows->bits[word]; look != 0; look &= look - 1)
			{
				int			bit = pg_rightmost_one_pos64(look);
				int			row = word * 64 + bit;

				if (column->isnull[row])
					continue;
				values[row] = piece_core(op, call, column->values[row]);
				present |= UINT64CONST(1) << bit;
			}
			call->non_nulls->bits[word] = present;
		}
		MemoryContextSwitchTo(old);
		return TESS_OK;
	}
	rest = scratch_mask(&space, 0, nrows);
	starts = scratch_alloc(sizeof(int32) * nrows, space.starts, sizeof(space.starts));
	lengths = scratch_alloc(sizeof(int32) * nrows, space.lengths, sizeof(space.lengths));
	code = tess_text_pieces(pieces[op], first, second, call->nargs == 3,
							chars < 0 ? TESS_CHARS_BYTES : chars, column, call->rows, starts,
							lengths, call->non_nulls, &rest, call->status);
	for (int word = 0; code == TESS_OK && word < nwords; word++)
	{
		uint64		done = call->non_nulls->bits[word] & ~rest.bits[word];

		for (uint64 look = done; look != 0; look &= look - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(look);
			const char *data = VARDATA_ANY(DatumGetPointer(column->values[row]));

			values[row] = PointerGetDatum(arena_text(&arena, data + starts[row], lengths[row]));
		}
		for (uint64 look = rest.bits[word]; code == TESS_OK && look != 0; look &= look - 1)
		{
			int			row = word * 64 + pg_rightmost_one_pos64(look);
			OneRow		one;
			int32		start;
			int32		len;
			uint64		present = 0;
			TessRowMask one_present = {1, &present};

			one_row(&one, column->values[row]);
			code = tess_text_pieces(pieces[op], first, second, call->nargs == 3,
									chars < 0 ? TESS_CHARS_BYTES : chars, &one.column, &one.rows,
									&start, &len, &one_present, &one.rest, call->status);
			if (code == TESS_OK)
				values[row] = PointerGetDatum(arena_text(&arena,
														 VARDATA_ANY(DatumGetPointer(one.value)) + start,
														 len));
			one_row_free(&one);
		}
	}
	scratch_release(lengths, space.lengths);
	scratch_release(starts, space.starts);
	scratch_release(rest.bits, space.words[0]);
	return code;
}

/* Register the text functions. */
void
tess_register_text_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(text_functions); i++)
		functions->add(&text_functions[i].function);
}
