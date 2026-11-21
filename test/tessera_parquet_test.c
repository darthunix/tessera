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

#include "fmgr.h"
#include "miscadmin.h"
#include "port.h"
#include "storage/fd.h"
#include "utils/builtins.h"
#include "utils/injection_point.h"

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
									  int32 codec_leaf, TessParquetTestStatus *status);
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
	{
		char	   *tmp = psprintf("%s.new", to);
		struct timespec times[2] = {MTIME_OF(was), MTIME_OF(was)};

		fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC | PG_BINARY, 0600);
		if (fd < 0)
			file_error("open", tmp);
		copy_into(from, fd, tmp);
		if (futimens(fd, times) != 0)
			file_error("set the time of", tmp);
		close(fd);
		if (rename(tmp, to) != 0)
			file_error("rename", tmp);
	}
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
