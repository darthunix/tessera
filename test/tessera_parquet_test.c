/*
 * The test module of the suite parquet: it writes the suite's Parquet
 * files through the writer of crates/tessera-parquet-testing, so that no
 * binary file is kept in the tree, and rewrites their footers and page
 * headers as older writers write them.
 */
#include "postgres.h"

#include <fcntl.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

#include "access/parallel.h"
#include "executor/executor.h"
#include "executor/spi.h"
#include "fmgr.h"
#include "funcapi.h"
#include "miscadmin.h"
#include "nodes/extensible.h"
#include "port.h"
#include "storage/fd.h"
#include "utils/builtins.h"
#include "utils/injection_point.h"
#include "utils/lsyscache.h"
#include "utils/plancache.h"
#include "utils/rel.h"
#include "utils/snapmgr.h"
#include "utils/tuplestore.h"

#include "tessera/runtime.h"

#include "injection_library.h"

PG_MODULE_MAGIC;

typedef struct TessParquetTestStatus
{
	char		message[256];
} TessParquetTestStatus;

extern bool tess_parquet_test_write(const char *path, size_t path_len, const char *spec,
									size_t spec_len, TessParquetTestStatus *status);
extern bool tess_parquet_test_rewrite(const char *path, size_t path_len, bool old_rule,
									  bool drop_encoding_stats, bool drop_null_counts,
									  int32 codec_leaf, int32 column_orders,
									  TessParquetTestStatus *status);
extern bool tess_parquet_test_head_dictionaries(const char *path, size_t path_len,
												TessParquetTestStatus *status);

PG_FUNCTION_INFO_V1(tessera_parquet_test_write);
PG_FUNCTION_INFO_V1(tessera_parquet_test_rewrite);
PG_FUNCTION_INFO_V1(tessera_parquet_test_head_dictionaries);
PG_FUNCTION_INFO_V1(tessera_parquet_test_replace);
PG_FUNCTION_INFO_V1(tessera_parquet_test_remove);
PG_FUNCTION_INFO_V1(tessera_parquet_test_cut);
PG_FUNCTION_INFO_V1(tessera_parquet_test_damage);
PG_FUNCTION_INFO_V1(tessera_parquet_test_injection);
PG_FUNCTION_INFO_V1(tessera_parquet_test_cancel_after);
PG_FUNCTION_INFO_V1(tessera_parquet_test_windows);
PG_FUNCTION_INFO_V1(tessera_parquet_test_asks);
PG_FUNCTION_INFO_V1(tessera_parquet_test_bytes);
PG_FUNCTION_INFO_V1(tessera_parquet_test_holds);
PG_FUNCTION_INFO_V1(tessera_parquet_test_point);
PG_FUNCTION_INFO_V1(tessera_parquet_test_replace_in_worker);

extern PGDLLEXPORT void tessera_parquet_test_replace_open(const char *name,
														  const void *private_data,
														  void *arg);
extern PGDLLEXPORT void tessera_parquet_test_again(const char *name, const void *private_data,
												   void *arg);
extern PGDLLEXPORT void tessera_parquet_test_fail_bytes(const char *name,
														const void *private_data, void *arg);

/* The time of modification of a stat, to the nanosecond. */
#ifdef __APPLE__
#define MTIME_OF(st) ((st).st_mtimespec)
#else
#define MTIME_OF(st) ((st).st_mtim)
#endif

extern PGDLLEXPORT void tessera_parquet_test_cancel(const char *name, const void *private_data,
													void *arg);

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
 * drop_null_counts bool, codec_leaf int, column_orders int): rewrites the
 * file's footer, its column orders kept for 1, dropped for 0, and as the
 * old rule says for -1.
 */
Datum
tessera_parquet_test_rewrite(PG_FUNCTION_ARGS)
{
	char	   *path = path_of(PG_GETARG_TEXT_PP(0));
	TessParquetTestStatus status;

	check(tess_parquet_test_rewrite(path, strlen(path), PG_GETARG_BOOL(1), PG_GETARG_BOOL(2),
									PG_GETARG_BOOL(3), PG_GETARG_INT32(4), PG_GETARG_INT32(5),
									&status),
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

static void
file_error(const char *what, const char *path)
{
	ereport(ERROR,
			(errcode_for_file_access(),
			 errmsg("could not %s \"%s\": %m", what, path)));
}

static struct stat
stat_of(const char *path)
{
	struct stat st;

	if (stat(path, &st) != 0)
		file_error("stat", path);
	return st;
}

/* Writes the bytes of `from` into the open file `fd`, from its start. */
static void
copy_into(const char *from, int fd, const char *to)
{
	int			in = open(from, O_RDONLY | PG_BINARY, 0);
	char		buf[8192];
	ssize_t		n;

	if (in < 0)
		file_error("open", from);
	while ((n = read(in, buf, sizeof(buf))) > 0)
		if (write(fd, buf, n) != n)
			file_error("write", to);
	if (n < 0)
		file_error("read", from);
	close(in);
}

/* Sets the time of modification of `path` to `when`, seconds. */
static void
set_time(const char *path, time_t when)
{
	struct timeval times[2] = {{when, 0}, {when, 0}};

	if (utimes(path, times) != 0)
		file_error("set the time of", path);
}

/* A new file of the bytes of `from`, of the time `was` says, renamed over
 * `to`; its temporary name is the process's, so that workers race on
 * the rename alone, which is atomic. */
static void
rename_over(const char *from, const char *to, const struct stat *was)
{
	char	   *tmp = psprintf("%s.%d.new", to, MyProcPid);
	struct timespec times[2] = {MTIME_OF(*was), MTIME_OF(*was)};
	int			fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC | PG_BINARY, 0600);

	if (fd < 0)
		file_error("open", tmp);
	copy_into(from, fd, tmp);
	if (futimens(fd, times) != 0)
		file_error("set the time of", tmp);
	close(fd);
	if (rename(tmp, to) != 0)
		file_error("rename", tmp);
}

/*
 * parquet_test_replace(from text, to text, how text): replaces the file
 * `to` by the bytes of `from`: 'in place', the same file rewritten, its
 * time a minute later than it was; 'rename', a new file renamed over it,
 * with its time as it was, to the nanosecond. Returns what changed of
 * the file's size, time and inode.
 */
Datum
tessera_parquet_test_replace(PG_FUNCTION_ARGS)
{
	char	   *from = text_to_cstring(PG_GETARG_TEXT_PP(0));
	char	   *to = text_to_cstring(PG_GETARG_TEXT_PP(1));
	char	   *how = text_to_cstring(PG_GETARG_TEXT_PP(2));
	struct stat was = stat_of(to);
	int			fd;

	if (strcmp(how, "in place") == 0)
	{
		fd = open(to, O_WRONLY | O_TRUNC | PG_BINARY, 0);
		if (fd < 0)
			file_error("open", to);
		copy_into(from, fd, to);
		close(fd);
		set_time(to, was.st_mtime + 60);
	}
	else if (strcmp(how, "rename") == 0)
		rename_over(from, to, &was);
	else
		elog(ERROR, "no replacement named %s", how);
	{
		struct stat now = stat_of(to);
		bool		same_time = MTIME_OF(now).tv_sec == MTIME_OF(was).tv_sec &&
			MTIME_OF(now).tv_nsec == MTIME_OF(was).tv_nsec;

		PG_RETURN_TEXT_P(cstring_to_text(psprintf("size %s, time %s, inode %s",
												  now.st_size == was.st_size ? "same" : "changed",
												  same_time ? "same" : "changed",
												  now.st_ino == was.st_ino &&
												  now.st_dev == was.st_dev ? "same" : "changed")));
	}
}

/* parquet_test_remove(path text): removes the file. */
Datum
tessera_parquet_test_remove(PG_FUNCTION_ARGS)
{
	char	   *path = text_to_cstring(PG_GETARG_TEXT_PP(0));

	if (unlink(path) != 0)
		file_error("remove", path);
	PG_RETURN_VOID();
}

/* parquet_test_cut(path text, bytes int): cuts the file's last bytes. */
Datum
tessera_parquet_test_cut(PG_FUNCTION_ARGS)
{
	char	   *path = text_to_cstring(PG_GETARG_TEXT_PP(0));
	struct stat st = stat_of(path);

	if (truncate(path, st.st_size - PG_GETARG_INT32(1)) != 0)
		file_error("cut", path);
	PG_RETURN_VOID();
}

/* parquet_test_damage(path text, offset int, bytes int): overwrites the
 * file's bytes from `offset` with 0xFF. */
Datum
tessera_parquet_test_damage(PG_FUNCTION_ARGS)
{
	char	   *path = text_to_cstring(PG_GETARG_TEXT_PP(0));
	int			bytes = PG_GETARG_INT32(2);
	char	   *ff = palloc(bytes);
	int			fd = open(path, O_WRONLY | PG_BINARY, 0);

	memset(ff, 0xFF, bytes);
	if (fd < 0)
		file_error("open", path);
	if (pwrite(fd, ff, bytes, PG_GETARG_INT32(1)) != bytes)
		file_error("write", path);
	close(fd);
	PG_RETURN_VOID();
}

/* Whether this build of PostgreSQL has injection points. */
Datum
tessera_parquet_test_injection(PG_FUNCTION_ARGS)
{
#ifdef USE_INJECTION_POINTS
	PG_RETURN_BOOL(true);
#else
	PG_RETURN_BOOL(false);
#endif
}

/* The windows read since the cancel was set, and the one to cancel at. */
static int	windows_seen;

typedef struct CancelAt
{
	int			pid;
	int			windows;
} CancelAt;

/* The callback of the injection point "tessera-parquet-window". */
void
tessera_parquet_test_cancel(const char *name, const void *private_data, void *arg)
{
	const CancelAt *at = private_data;

	if (at->pid != MyProcPid)
		return;
	if (++windows_seen == at->windows)
	{
		QueryCancelPending = true;
		InterruptPending = true;
	}
}

/*
 * parquet_test_cancel_after(module text, windows int): cancels this
 * backend's query when it has read `windows` windows; with `windows`
 * NULL, no longer. The callback is loaded from `module`, this module's
 * file without its suffix.
 */
Datum
tessera_parquet_test_cancel_after(PG_FUNCTION_ARGS)
{
	char	   *library = module_from_pkglib(text_to_cstring(PG_GETARG_TEXT_PP(0)));
	CancelAt	at = {MyProcPid, 0};

	InjectionPointDetach("tessera-parquet-window");
	if (PG_ARGISNULL(1))
		PG_RETURN_VOID();
	at.windows = PG_GETARG_INT32(1);
	windows_seen = 0;
	InjectionPointAttach("tessera-parquet-window", library, "tessera_parquet_test_cancel",
						 &at, sizeof(at));
	PG_RETURN_VOID();
}

/* parquet_test_windows(): the windows read since the cancel was set. */
Datum
tessera_parquet_test_windows(PG_FUNCTION_ARGS)
{
	PG_RETURN_INT32(windows_seen);
}

/* A row of parquet_test_asks: a column's value of a row, the batch's row
 * `row`, the scan's `base + row`, as one ask gave it. */
static void
put_ask(ReturnSetInfo *rsinfo, Form_pg_attribute att, int base, int row, const char *ask,
		const TessDatumColumn *column)
{
	Datum		values[4];
	bool		nulls[4] = {false, false, false, false};
	Oid			out;
	bool		varlena;

	values[0] = CStringGetTextDatum(NameStr(att->attname));
	values[1] = Int32GetDatum(base + row);
	values[2] = CStringGetTextDatum(ask);
	nulls[3] = column->isnull[row];
	if (!nulls[3])
	{
		getTypeOutputInfo(att->atttypid, &out, &varlena);
		values[3] = CStringGetTextDatum(OidOutputFunctionCall(out, column->values[row]));
	}
	tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
}

/*
 * Asks each target's column of a batch for every row: in the bytes form
 * where `bytes` and the column is offered so, as Datum values otherwise.
 */
static void
ask_every_column(TessBatch *batch, const TessLayout *layout, bool bytes)
{
	int			nrows = batch->rows.nrows;
	uint64		all = nrows == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << nrows) - 1;
	TessRowMask rows = {nrows, &all};

	for (int target = 0; target < layout->ntargets; target++)
	{
		int			column = tess_layout_column(layout, target);
		TessBytesColumn form;
		TessDatumColumn datums = TESS_STRUCT_INITIALIZER(TessDatumColumn);

		if (!bytes || !tess_batch_bytes_column(batch, column, &rows, &form))
			batch->ops->get_datum_column(batch, column, &rows, TESS_COLUMN_FOR_PROJECTION,
										 &datums);
	}
}

/*
 * parquet_test_asks(query text, max_rows int, skip int[, ask_skipped
 * bool]): runs the query, which plans as TessParquetScan alone, in
 * batches of max_rows rows at most, skips `skip` batches, of max_rows rows
 * each, asking each target's column of them for every row where
 * `ask_skipped`, takes the next batch's last row out of its mask, and asks
 * each target's column of that batch for its last row ('first'), then for
 * the others ('second'), then for the last again ('again'), keeping the
 * arrays each ask gave, and reads the last row of the first ask's arrays
 * after the second ('kept'). Returns the rows (column, row of the scan,
 * ask, value).
 */
Datum
tessera_parquet_test_asks(PG_FUNCTION_ARGS)
{
	ReturnSetInfo *rsinfo = (ReturnSetInfo *) fcinfo->resultinfo;
	char	   *sql = text_to_cstring(PG_GETARG_TEXT_PP(0));
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	SPIPlanPtr	plan;
	CachedPlan *cplan;
	QueryDesc  *query;
	PlanState  *state;
	TessInput  *input;
	TessBatch  *batch;
	const TessLayout *layout;
	TupleDesc	desc;
	int			max_rows = PG_GETARG_INT32(1);
	int			skip = PG_GETARG_INT32(2);
	bool		ask_skipped = PG_NARGS() > 3 && PG_GETARG_BOOL(3);
	int			base = max_rows * skip;
	int			last;

	InitMaterializedSRF(fcinfo, 0);
	SPI_connect();
	plan = SPI_prepare(sql, 0, NULL);
	if (plan == NULL)
		elog(ERROR, "could not plan %s", sql);
	cplan = SPI_plan_get_cached_plan(plan);
	query = CreateQueryDesc(linitial_node(PlannedStmt, cplan->stmt_list), sql,
							GetActiveSnapshot(), InvalidSnapshot, None_Receiver, NULL, NULL, 0);
	ExecutorStart(query, 0);
	state = query->planstate;
	if (!IsA(state, CustomScanState) ||
		strcmp(((CustomScanState *) state)->methods->CustomName, "TessParquetScan") != 0)
		elog(ERROR, "the query does not plan as TessParquetScan alone");
	desc = RelationGetDescr(((ScanState *) state)->ss_currentRelation);
	input = tess_input_create(CurrentMemoryContext, state);
	request.output_mode = TESS_OUTPUT_BATCH;
	request.max_batch_rows = max_rows;
	tess_input_set_request(input, &request);
	for (int i = 0; i < skip; i++)
	{
		if ((batch = tess_input_next(input)) == NULL)
			elog(ERROR, "the scan holds fewer batches than it skips");
		if (ask_skipped)
			ask_every_column(batch, tess_input_layout(input), false);
		tess_input_finish(input);
	}
	batch = tess_input_next(input);
	if (batch == NULL || batch->rows.nrows != max_rows)
		elog(ERROR, "the batch does not hold %d rows", max_rows);
	last = max_rows - 1;
	/* A row a consumer took out of the mask is still made when asked. */
	tess_row_mask_clear(&batch->rows, last);
	layout = tess_input_layout(input);
	for (int target = 0; target < layout->ntargets; target++)
	{
		int			column = tess_layout_column(layout, target);
		Form_pg_attribute att = TupleDescAttr(desc, column);
		uint64		only_last = UINT64CONST(1) << last;
		uint64		rest = only_last - 1;
		TessRowMask mask = {max_rows, &only_last};
		TessDatumColumn first = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		TessDatumColumn second = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		TessDatumColumn again = TESS_STRUCT_INITIALIZER(TessDatumColumn);

		batch->ops->get_datum_column(batch, column, &mask, TESS_COLUMN_FOR_PROJECTION, &first);
		put_ask(rsinfo, att, base, last, "first", &first);
		mask.bits = &rest;
		batch->ops->get_datum_column(batch, column, &mask, TESS_COLUMN_FOR_PROJECTION, &second);
		for (int row = 0; row < last; row++)
			put_ask(rsinfo, att, base, row, "second", &second);
		put_ask(rsinfo, att, base, last, "kept", &first);
		mask.bits = &only_last;
		batch->ops->get_datum_column(batch, column, &mask, TESS_COLUMN_FOR_PROJECTION, &again);
		put_ask(rsinfo, att, base, last, "again", &again);
	}
	tess_input_finish(input);
	ExecutorFinish(query);
	ExecutorEnd(query);
	FreeQueryDesc(query);
	ReleaseCachedPlan(cplan, NULL);
	SPI_finish();
	return (Datum) 0;
}

/*
 * The references to windows' arrays alive in this backend, counted by the
 * reader of tessera_parquet's library: this module carries a copy of the
 * reader of its own, whose count is another.
 */
static uint64
holds_alive(void)
{
	static uint64 (*holds) (void) = NULL;

	if (holds == NULL)
		holds = (uint64 (*) (void)) load_external_function("$libdir/tessera_parquet",
															"tess_parquet_test_holds",
															true, NULL);
	return holds();
}

/* parquet_test_holds(): the references alive in this backend. */
Datum
tessera_parquet_test_holds(PG_FUNCTION_ARGS)
{
	PG_RETURN_INT64((int64) holds_alive());
}

/* A row of parquet_test_bytes: a column's bytes of a row, as one ask gave it. */
static void
put_bytes(ReturnSetInfo *rsinfo, Form_pg_attribute att, int base, int row, const char *ask,
		  const TessBytesColumn *column)
{
	Datum		values[6];
	bool		nulls[6] = {false, false, false, false, false, false};

	values[0] = CStringGetTextDatum(NameStr(att->attname));
	values[1] = Int32GetDatum(base + row);
	values[2] = CStringGetTextDatum(ask);
	nulls[3] = column->isnull[row];
	/* The node gives an asked NULL row no place, length or index. */
	if (nulls[3] && (column->data[row] != NULL || column->lengths[row] != 0 ||
					 (column->indexes != NULL && column->indexes[row] != 0)))
		elog(ERROR, "NULL row %d of the bytes form has a place, a length or an index", row);
	if (!nulls[3])
	{
		bytea	   *bytes = palloc(VARHDRSZ + column->lengths[row]);

		if (column->data[row] == NULL)
			elog(ERROR, "a row of the bytes form not NULL whose place is NULL");
		SET_VARSIZE(bytes, VARHDRSZ + column->lengths[row]);
		memcpy(VARDATA(bytes), column->data[row], column->lengths[row]);
		values[3] = PointerGetDatum(bytes);
	}
	nulls[4] = column->indexes == NULL || column->isnull[row];
	if (!nulls[4] && (column->dictionary == 0 || column->indexes[row] < 0 ||
					  (uint32) column->indexes[row] >= column->nvalues))
		elog(ERROR, "row %d's index %d names no value of the dictionary's %u", row,
			 column->indexes[row], column->nvalues);
	values[4] = nulls[4] ? (Datum) 0 : Int32GetDatum(column->indexes[row]);
	values[5] = BoolGetDatum(column->dictionary != 0);
	tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
}

/*
 * parquet_test_bytes(query text, max_rows int, skip int, mask bigint,
 * rest bool, again bool[, ask_skipped bool]): runs the query, which plans
 * as TessParquetScan alone, in batches of max_rows rows at most, skips
 * `skip` batches, asking each target's column of them for every row in
 * the bytes form, or as Datum values where not offered so, where
 * `ask_skipped`, after a rescan too, and
 * asks each target's column of the next batch in the bytes form for the
 * rows of `mask` ('first'); with `rest`, then for every row as Datum
 * values and every other row as bytes ('rest'), and reads the first ask's
 * places again ('kept'). `again` is for windows read again at each ask:
 * the column is asked as Datum values before its first ask, so that the
 * first ask's window is a read behind, whose arrays only the batch's
 * reference keeps once the later asks replace them, and at the end the
 * scan is rescanned, the batches skipped again and the rows of `mask`
 * asked once more ('rescanned'). A
 * column not offered gives one row, its row -1, 'not offered', and an
 * empty mask one row, its row -1, 'empty', with its dictionary. Fails
 * where no reference to a window holds the bytes while the batch lives,
 * or one is left after a rescan or the scan's end, and on an asked NULL
 * row with a place, a length or an index. Returns the rows
 * (column, row of the scan, ask, bytes, index, through a dictionary).
 */
Datum
tessera_parquet_test_bytes(PG_FUNCTION_ARGS)
{
	ReturnSetInfo *rsinfo = (ReturnSetInfo *) fcinfo->resultinfo;
	char	   *sql = text_to_cstring(PG_GETARG_TEXT_PP(0));
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	SPIPlanPtr	plan;
	CachedPlan *cplan;
	QueryDesc  *query;
	PlanState  *state;
	TessInput  *input;
	TessBatch  *batch;
	const TessLayout *layout;
	TupleDesc	desc;
	int			max_rows = PG_GETARG_INT32(1);
	int			skip = PG_GETARG_INT32(2);
	uint64		mask = (uint64) PG_GETARG_INT64(3);
	bool		ask_rest = PG_GETARG_BOOL(4);
	bool		again = PG_GETARG_BOOL(5);
	bool		ask_skipped = PG_NARGS() > 6 && PG_GETARG_BOOL(6);
	int			base = max_rows * skip;
	uint64		holds_before;
	bool		offered = false;

	InitMaterializedSRF(fcinfo, 0);
	SPI_connect();
	plan = SPI_prepare(sql, 0, NULL);
	if (plan == NULL)
		elog(ERROR, "could not plan %s", sql);
	cplan = SPI_plan_get_cached_plan(plan);
	query = CreateQueryDesc(linitial_node(PlannedStmt, cplan->stmt_list), sql,
							GetActiveSnapshot(), InvalidSnapshot, None_Receiver, NULL, NULL, 0);
	ExecutorStart(query, 0);
	holds_before = holds_alive();
	state = query->planstate;
	if (!IsA(state, CustomScanState) ||
		strcmp(((CustomScanState *) state)->methods->CustomName, "TessParquetScan") != 0)
		elog(ERROR, "the query does not plan as TessParquetScan alone");
	desc = RelationGetDescr(((ScanState *) state)->ss_currentRelation);
	input = tess_input_create(CurrentMemoryContext, state);
	request.output_mode = TESS_OUTPUT_BATCH;
	request.max_batch_rows = max_rows;
	tess_input_set_request(input, &request);
	for (int i = 0; i < skip; i++)
	{
		if ((batch = tess_input_next(input)) == NULL)
			elog(ERROR, "the scan holds fewer batches than it skips");
		if (ask_skipped)
			ask_every_column(batch, tess_input_layout(input), true);
		tess_input_finish(input);
	}
	batch = tess_input_next(input);
	if (batch == NULL || batch->rows.nrows != max_rows)
		elog(ERROR, "the batch does not hold %d rows", max_rows);
	layout = tess_input_layout(input);
	for (int target = 0; target < layout->ntargets; target++)
	{
		int			column = tess_layout_column(layout, target);
		Form_pg_attribute att = TupleDescAttr(desc, column);
		uint64		all = max_rows == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << max_rows) - 1;
		uint64		first_rows = mask & all;
		uint64		rest_rows = all & ~first_rows;
		TessRowMask rows = {max_rows, &first_rows};
		TessBytesColumn first;
		TessBytesColumn rest;
		TessDatumColumn datums = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		char	  **copies = palloc0_array(char *, max_rows);

		if (again)
		{
			rows.bits = &all;
			batch->ops->get_datum_column(batch, column, &rows, TESS_COLUMN_FOR_PROJECTION,
										 &datums);
			rows.bits = &first_rows;
		}
		if (!tess_batch_bytes_column(batch, column, &rows, &first))
		{
			Datum		values[6] = {0};
			bool		nulls[6] = {false, false, false, true, true, false};

			values[0] = CStringGetTextDatum(NameStr(att->attname));
			values[1] = Int32GetDatum(-1);
			values[2] = CStringGetTextDatum("not offered");
			values[5] = BoolGetDatum(false);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
			continue;
		}
		/* A reference lives where an ask made rows. */
		offered |= first_rows != 0 || ask_rest;
		if (first_rows == 0)
		{
			/* An ask of no row: whether it names a dictionary. */
			Datum		values[6] = {0};
			bool		nulls[6] = {false, false, false, true, true, false};

			values[0] = CStringGetTextDatum(NameStr(att->attname));
			values[1] = Int32GetDatum(-1);
			values[2] = CStringGetTextDatum("empty");
			values[5] = BoolGetDatum(first.dictionary != 0);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
		}
		for (int row = 0; row < max_rows; row++)
			if (first_rows >> row & 1)
			{
				put_bytes(rsinfo, att, base, row, "first", &first);
				if (!first.isnull[row])
				{
					copies[row] = palloc(Max(first.lengths[row], 1));
					memcpy(copies[row], first.data[row], first.lengths[row]);
				}
			}
		if (!ask_rest)
			continue;
		rows.bits = &all;
		batch->ops->get_datum_column(batch, column, &rows, TESS_COLUMN_FOR_PROJECTION, &datums);
		rows.bits = &rest_rows;
		if (!tess_batch_bytes_column(batch, column, &rows, &rest))
			elog(ERROR, "a column given as bytes once and not again");
		for (int row = 0; row < max_rows; row++)
			if (rest_rows >> row & 1)
				put_bytes(rsinfo, att, base, row, "rest", &rest);
		for (int row = 0; row < max_rows; row++)
			if (first_rows >> row & 1)
			{
				if (!first.isnull[row] &&
					memcmp(copies[row], first.data[row], first.lengths[row]) != 0)
					elog(ERROR, "the bytes of row %d changed after later asks", row);
				put_bytes(rsinfo, att, base, row, "kept", &first);
			}
	}
	if (offered && holds_alive() == holds_before)
		elog(ERROR, "no reference to a window holds the batch's bytes");
	tess_input_finish(input);
	if (again)
	{
		ExecReScan(state);
		tess_input_rescan(input);
		if (holds_alive() != holds_before)
			elog(ERROR, "references to windows left after a rescan");
		for (int i = 0; i <= skip; i++)
		{
			batch = tess_input_next(input);
			if (batch == NULL)
				elog(ERROR, "the scan holds fewer batches after a rescan");
			if (i < skip && ask_skipped)
				ask_every_column(batch, layout, true);
			if (i < skip)
				tess_input_finish(input);
		}
		for (int target = 0; target < layout->ntargets; target++)
		{
			int			column = tess_layout_column(layout, target);
			uint64		all = max_rows == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << max_rows) - 1;
			uint64		rows_bits = mask & all;
			TessRowMask rows = {max_rows, &rows_bits};
			TessBytesColumn rescanned;

			if (!tess_batch_bytes_column(batch, column, &rows, &rescanned))
				continue;
			for (int row = 0; row < max_rows; row++)
				if (rows_bits >> row & 1)
					put_bytes(rsinfo, TupleDescAttr(desc, column), base, row, "rescanned",
							  &rescanned);
		}
		tess_input_finish(input);
	}
	ExecutorFinish(query);
	ExecutorEnd(query);
	if (holds_alive() != holds_before)
		elog(ERROR, "references to windows left after the scan's end");
	FreeQueryDesc(query);
	ReleaseCachedPlan(cplan, NULL);
	SPI_finish();
	return (Datum) 0;
}

/* The callback of "tessera-parquet-again": this backend reads windows again. */
void
tessera_parquet_test_again(const char *name, const void *private_data, void *arg)
{
	if (*(const int *) private_data == MyProcPid)
		*(bool *) arg = true;
}

/* The callback of "tessera-parquet-bytes": this backend's query fails there. */
void
tessera_parquet_test_fail_bytes(const char *name, const void *private_data, void *arg)
{
	if (*(const int *) private_data == MyProcPid)
		ereport(ERROR,
				(errmsg("an error injected after a reference to a window was taken")));
}

/*
 * parquet_test_point(module text, point text, on bool): from now on, or
 * no longer, this backend's Parquet scans read the batch's window again
 * at each ask ("tessera-parquet-again"), or fail once a batch's bytes hold
 * a reference ("tessera-parquet-bytes"). The callback is loaded from
 * `module`, this module's file without its suffix.
 */
Datum
tessera_parquet_test_point(PG_FUNCTION_ARGS)
{
	char	   *library = module_from_pkglib(text_to_cstring(PG_GETARG_TEXT_PP(0)));
	char	   *point = text_to_cstring(PG_GETARG_TEXT_PP(1));
	int			pid = MyProcPid;
	const char *callback;

	if (strcmp(point, "tessera-parquet-again") == 0)
		callback = "tessera_parquet_test_again";
	else if (strcmp(point, "tessera-parquet-bytes") == 0)
		callback = "tessera_parquet_test_fail_bytes";
	else
		elog(ERROR, "no callback of the injection point \"%s\"", point);
	InjectionPointDetach(point);
	if (PG_GETARG_BOOL(2))
		InjectionPointAttach(point, library, callback, &pid, sizeof(pid));
	PG_RETURN_VOID();
}

/*
 * The callback of the injection point "tessera-parquet-open": in a worker
 * of a parallel plan, the file a scan is about to open replaced by a
 * rename with one of its bytes and time.
 */
void
tessera_parquet_test_replace_open(const char *name, const void *private_data, void *arg)
{
	const char *path = arg;
	struct stat was;

	if (!IsParallelWorker())
		return;
	was = stat_of(path);
	rename_over(path, path, &was);
}

/*
 * parquet_test_replace_in_worker(module text, on bool): replaces each file
 * a worker opens from now on, or no longer. The callback is loaded from
 * `module`, this module's file without its suffix.
 */
Datum
tessera_parquet_test_replace_in_worker(PG_FUNCTION_ARGS)
{
	char	   *library = module_from_pkglib(text_to_cstring(PG_GETARG_TEXT_PP(0)));

	InjectionPointDetach("tessera-parquet-open");
	if (PG_GETARG_BOOL(1))
		InjectionPointAttach("tessera-parquet-open", library, "tessera_parquet_test_replace_open",
							 NULL, 0);
	PG_RETURN_VOID();
}
