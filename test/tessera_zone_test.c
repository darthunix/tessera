/*
 * The answer of one second of the time zone's rounds (kernels/date.c):
 * when the time zone library fails to find the span of an instant, a
 * failure its sources call unreachable, the offset of that one second is
 * the answer. The callback of the injection point "tessera-zone-span"
 * makes every span the library finds a failure, so that this path runs
 * through C and the kernels.
 */
#include "postgres.h"

#include "fmgr.h"
#include "utils/builtins.h"
#include "utils/injection_point.h"

#include "injection_library.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_zone_fail);
PG_FUNCTION_INFO_V1(tessera_test_zone_failures);

/* The spans made failures since the callback was attached. */
static int32 failures = 0;

PGDLLEXPORT void tessera_test_zone_span(const char *name, const void *private_data,
										void *arg);

/* What kernels/date.c gives the injection point. */
typedef struct ZoneSpanResult
{
	int		   *found;
	long int   *before;
} ZoneSpanResult;

/*
 * The callback of "tessera-zone-span": the library's result becomes -1,
 * and the offset it found a wrong one, so that only pg_localtime's, which
 * the failure calls for, gives the right answers.
 */
void
tessera_test_zone_span(const char *name, const void *private_data, void *arg)
{
	ZoneSpanResult *result = arg;

	*result->found = -1;
	*result->before = 12345;
	failures++;
}

/*
 * Make every span the library finds a failure from now on, the callback
 * loaded from module, this module's file without its suffix; with NULL,
 * no longer.
 */
Datum
tessera_test_zone_fail(PG_FUNCTION_ARGS)
{
	InjectionPointDetach("tessera-zone-span");
	failures = 0;
	if (PG_ARGISNULL(0))
		PG_RETURN_VOID();
	InjectionPointAttach("tessera-zone-span",
						 module_from_pkglib(text_to_cstring(PG_GETARG_TEXT_PP(0))),
						 "tessera_test_zone_span", NULL, 0);
	PG_RETURN_VOID();
}

/* The spans made failures since the callback was attached. */
Datum
tessera_test_zone_failures(PG_FUNCTION_ARGS)
{
	PG_RETURN_INT32(failures);
}
