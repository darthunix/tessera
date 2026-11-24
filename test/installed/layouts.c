/*
 * The layouts the spec of string-bytes draws, of the structures by which
 * one module gives the bytes form to another, asserted against the
 * installed headers on the machine that builds them; make installcheck
 * compiles this file and runs nothing.
 */
#include "postgres.h"

#include "tessera/batch.h"
#include "tessera/function.h"
#include "tessera/text.h"

#if SIZEOF_VOID_P == 8
StaticAssertDecl(sizeof(TessBytesColumn) == 56, "TessBytesColumn is 56 bytes");
StaticAssertDecl(offsetof(TessBytesColumn, struct_size) == 0, "struct_size at 0");
StaticAssertDecl(offsetof(TessBytesColumn, nrows) == 8, "nrows at 8");
StaticAssertDecl(offsetof(TessBytesColumn, nvalues) == 12, "nvalues at 12");
StaticAssertDecl(offsetof(TessBytesColumn, isnull) == 16, "isnull at 16");
StaticAssertDecl(offsetof(TessBytesColumn, data) == 24, "data at 24");
StaticAssertDecl(offsetof(TessBytesColumn, lengths) == 32, "lengths at 32");
StaticAssertDecl(offsetof(TessBytesColumn, indexes) == 40, "indexes at 40");
StaticAssertDecl(offsetof(TessBytesColumn, dictionary) == 48, "dictionary at 48");
StaticAssertDecl(TESS_BYTES_COLUMN_MIN_SIZE == 56, "the whole structure is its minimum");
StaticAssertDecl(sizeof(TessTextArg) == 24, "TessTextArg is 24 bytes");
StaticAssertDecl(offsetof(TessTextArg, column) == 0, "column at 0");
StaticAssertDecl(offsetof(TessTextArg, scalar) == 8, "scalar at 8");
StaticAssertDecl(offsetof(TessTextArg, bytes) == 16, "bytes at 16");
StaticAssertDecl(sizeof(TessFunctionCall) == 104, "TessFunctionCall is 104 bytes");
StaticAssertDecl(offsetof(TessFunctionCall, struct_size) == 0, "struct_size at 0");
StaticAssertDecl(offsetof(TessFunctionCall, function) == 8, "function at 8");
StaticAssertDecl(offsetof(TessFunctionCall, nargs) == 16, "nargs at 16");
StaticAssertDecl(offsetof(TessFunctionCall, args) == 24, "args at 24");
StaticAssertDecl(offsetof(TessFunctionCall, inputcollid) == 32, "inputcollid at 32");
StaticAssertDecl(offsetof(TessFunctionCall, rows) == 40, "rows at 40");
StaticAssertDecl(offsetof(TessFunctionCall, values) == 48, "values at 48");
StaticAssertDecl(offsetof(TessFunctionCall, non_nulls) == 56, "non_nulls at 56");
StaticAssertDecl(offsetof(TessFunctionCall, context) == 64, "context at 64");
StaticAssertDecl(offsetof(TessFunctionCall, status) == 72, "status at 72");
StaticAssertDecl(offsetof(TessFunctionCall, decimal_rows) == 80, "decimal_rows at 80");
StaticAssertDecl(offsetof(TessFunctionCall, result_scale) == 88, "result_scale at 88");
StaticAssertDecl(offsetof(TessFunctionCall, bytes) == 96, "bytes at 96");
StaticAssertDecl(TESS_FUNCTION_CALL_BYTES_SIZE == 104, "the bytes end the structure");
#endif
