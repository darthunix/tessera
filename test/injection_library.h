/*
 * The name by which an injection point loads a callback of a test module,
 * shared by the test modules that attach one.
 */
#ifndef TESSERA_TEST_INJECTION_LIBRARY_H
#define TESSERA_TEST_INJECTION_LIBRARY_H

#include "lib/stringinfo.h"
#include "miscadmin.h"

/*
 * The path of module, a file's path without its suffix, from pkglib_path:
 * an injection point loads its library from there, by a name of 127 bytes
 * at most, and a test module is not installed.
 */
/* The longest name of a library an injection point takes: INJ_LIB_MAXLEN, less its end. */
#define INJECTION_LIBRARY_MAX 127

static char *
module_from_pkglib(const char *module)
{
	StringInfoData path;
	int			common = 0;
	int			skip = 0;
	int			at;

	/* The directories the two paths share. */
	for (at = 0; pkglib_path[at] != '\0' && pkglib_path[at] == module[at]; at++)
		if (pkglib_path[at] == '/')
			common = at + 1;
	if (pkglib_path[at] == '\0' && module[at] == '/')
	{
		common = at;
		skip = 1;
	}
	initStringInfo(&path);
	if (pkglib_path[common] != '\0')
	{
		appendStringInfoString(&path, "../");
		for (at = common; pkglib_path[at] != '\0'; at++)
			if (pkglib_path[at] == '/')
				appendStringInfoString(&path, "../");
	}
	appendStringInfoString(&path, module + common + skip);
	if (path.len > INJECTION_LIBRARY_MAX)
		ereport(ERROR,
				errmsg("the path of %s from %s is longer than an injection point takes, %d bytes",
					   module, pkglib_path, INJECTION_LIBRARY_MAX),
				errhint("Build PostgreSQL nearer the checkout of Tessera."));
	return path.data;
}

#endif							/* TESSERA_TEST_INJECTION_LIBRARY_H */
