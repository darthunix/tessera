/*
 * The test module of the suite parquet: it writes the suite's Parquet
 * files through the writer of crates/tessera-parquet-testing, so that no
 * binary file is kept in the tree, and rewrites their footers and page
 * headers as older writers write them.
 */
#include "postgres.h"

#include "fmgr.h"
#include "port.h"
#include "utils/builtins.h"

PG_MODULE_MAGIC;

typedef struct TessParquetTestStatus
{
	char		message[256];
} TessParquetTestStatus;

extern bool tess_parquet_test_write(const char *path, size_t path_len, const char *spec,
									size_t spec_len, TessParquetTestStatus *status);
extern bool tess_parquet_test_rewrite(const char *path, size_t path_len, bool old_rule,
									  bool drop_encoding_stats, bool drop_null_counts,
									  int32 codec_leaf, TessParquetTestStatus *status);
extern bool tess_parquet_test_head_dictionaries(const char *path, size_t path_len,
												TessParquetTestStatus *status);

PG_FUNCTION_INFO_V1(tessera_parquet_test_write);
PG_FUNCTION_INFO_V1(tessera_parquet_test_rewrite);
PG_FUNCTION_INFO_V1(tessera_parquet_test_head_dictionaries);

static void
check(bool ok, const TessParquetTestStatus *status)
{
	if (!ok)
		ereport(ERROR,
				(errcode(ERRCODE_EXTERNAL_ROUTINE_EXCEPTION),
				 errmsg("could not write a test file: %s", status->message)));
}

/* The path of the argument, its directory made where it is missing. */
static char *
path_of(text *arg)
{
	char	   *path = text_to_cstring(arg);
	char	   *dir = pstrdup(path);

	get_parent_directory(dir);
	if (dir[0] != '\0' && pg_mkdir_p(dir, 0700) != 0)
		ereport(ERROR,
				(errcode_for_file_access(),
				 errmsg("could not make directory \"%s\": %m", dir)));
	return path;
}

/* parquet_test_write(path text, spec text): writes the file of the spec. */
Datum
tessera_parquet_test_write(PG_FUNCTION_ARGS)
{
	char	   *path = path_of(PG_GETARG_TEXT_PP(0));
	char	   *spec = text_to_cstring(PG_GETARG_TEXT_PP(1));
	TessParquetTestStatus status;

	check(tess_parquet_test_write(path, strlen(path), spec, strlen(spec), &status), &status);
	PG_RETURN_VOID();
}

/*
 * parquet_test_rewrite(path text, old_rule bool, drop_encoding_stats bool,
 * drop_null_counts bool, codec_leaf int): rewrites the file's footer.
 */
Datum
tessera_parquet_test_rewrite(PG_FUNCTION_ARGS)
{
	char	   *path = path_of(PG_GETARG_TEXT_PP(0));
	TessParquetTestStatus status;

	check(tess_parquet_test_rewrite(path, strlen(path), PG_GETARG_BOOL(1), PG_GETARG_BOOL(2),
									PG_GETARG_BOOL(3), PG_GETARG_INT32(4), &status),
		  &status);
	PG_RETURN_VOID();
}

/* parquet_test_head_dictionaries(path text): dictionary pages headed
 * PLAIN_DICTIONARY, as ClickBench's writer heads them. */
Datum
tessera_parquet_test_head_dictionaries(PG_FUNCTION_ARGS)
{
	char	   *path = path_of(PG_GETARG_TEXT_PP(0));
	TessParquetTestStatus status;

	check(tess_parquet_test_head_dictionaries(path, strlen(path), &status), &status);
	PG_RETURN_VOID();
}
