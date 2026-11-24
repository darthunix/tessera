/*
 * The layouts the spec of string-bytes draws, of the structures by which
 * one module gives the bytes form to another, asserted against the
 * installed headers on the machine that builds them; make installcheck
 * compiles this file and runs nothing.
 */
#include "postgres.h"

#include "tessera/batch.h"
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
#endif
