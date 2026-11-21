/*
 * The options of the wrapper tessera_parquet: none for a server, `files`
 * for a table, `column`, `unit` and `utf8` for a column, with the
 * privileges they take, as the spec's requirement "A Parquet foreign
 * table" gives them.
 */
#include "postgres.h"

#include "access/reloptions.h"
#include "catalog/pg_attribute.h"
#include "catalog/pg_authid.h"
#include "catalog/pg_foreign_table.h"
#include "commands/defrem.h"
#include "foreign/foreign.h"
#include "miscadmin.h"
#include "utils/acl.h"
#include "utils/builtins.h"
#include "utils/lsyscache.h"
#include "utils/varlena.h"

#include "tessera_parquet.h"

PG_FUNCTION_INFO_V1(tessera_parquet_validator);

/* Whether the role may make the server read files by its own rights. */
static void
check_file_privilege(const char *what)
{
	if (!superuser() && !has_privs_of_role(GetUserId(), ROLE_PG_READ_SERVER_FILES))
		ereport(ERROR,
				(errcode(ERRCODE_INSUFFICIENT_PRIVILEGE),
				 errmsg("permission denied to %s", what),
				 errdetail("Only roles with privileges of the \"pg_read_server_files\" role may %s.",
						   what)));
}

/*
 * The paths of the option `files`, a comma-separated list, each absolute:
 * an empty list and a path that is not absolute are refused, 22023.
 */
static List *
split_files(const char *value)
{
	List	   *paths = NIL;
	char	   *copy = pstrdup(value);
	char	   *start = copy;

	for (;;)
	{
		char	   *comma = strchr(start, ',');
		char	   *end = comma != NULL ? comma : start + strlen(start);
		char	   *path;

		while (start < end && isspace((unsigned char) *start))
			start++;
		while (end > start && isspace((unsigned char) end[-1]))
			end--;
		path = pnstrdup(start, end - start);
		if (path[0] == '\0')
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_PARAMETER_VALUE),
					 errmsg("option \"files\" holds an empty path")));
		if (path[0] != '/')
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_PARAMETER_VALUE),
					 errmsg("option \"files\" holds a path that is not absolute: \"%s\"",
							path)));
		paths = lappend(paths, path);
		if (comma == NULL)
			break;
		start = comma + 1;
	}
	return paths;
}

static int32
unit_of(const char *value)
{
	if (strcmp(value, "days") == 0)
		return TESS_PARQUET_UNIT_DAYS;
	if (strcmp(value, "seconds") == 0)
		return TESS_PARQUET_UNIT_SECONDS;
	if (strcmp(value, "milliseconds") == 0)
		return TESS_PARQUET_UNIT_MILLISECONDS;
	if (strcmp(value, "microseconds") == 0)
		return TESS_PARQUET_UNIT_MICROSECONDS;
	ereport(ERROR,
			(errcode(ERRCODE_INVALID_PARAMETER_VALUE),
			 errmsg("invalid value for option \"unit\": \"%s\"", value),
			 errhint("Valid values are \"days\", \"seconds\", \"milliseconds\" and \"microseconds\".")));
	pg_unreachable();
}

static int32
utf8_of(const char *value)
{
	if (strcmp(value, "check") == 0)
		return TESS_PARQUET_UTF8_CHECK;
	if (strcmp(value, "trust") == 0)
		return TESS_PARQUET_UTF8_TRUST;
	ereport(ERROR,
			(errcode(ERRCODE_INVALID_PARAMETER_VALUE),
			 errmsg("invalid value for option \"utf8\": \"%s\"", value),
			 errhint("Valid values are \"check\" and \"trust\".")));
	pg_unreachable();
}

static void
unknown_option(DefElem *def)
{
	ereport(ERROR,
			(errcode(ERRCODE_FDW_INVALID_OPTION_NAME),
			 errmsg("invalid option \"%s\"", def->defname)));
}

Datum
tessera_parquet_validator(PG_FUNCTION_ARGS)
{
	List	   *options = untransformRelOptions(PG_GETARG_DATUM(0));
	Oid			catalog = PG_GETARG_OID(1);
	bool		files = false;
	bool		column = false;
	bool		trust = false;

	foreach_node(DefElem, def, options)
	{
		if (catalog == ForeignTableRelationId && strcmp(def->defname, "files") == 0)
		{
			check_file_privilege("set the files of a Parquet table");
			(void) split_files(defGetString(def));
			files = true;
		}
		else if (catalog == AttributeRelationId && strcmp(def->defname, "column") == 0)
			column = true;
		else if (catalog == AttributeRelationId && strcmp(def->defname, "unit") == 0)
			(void) unit_of(defGetString(def));
		else if (catalog == AttributeRelationId && strcmp(def->defname, "utf8") == 0)
			trust = utf8_of(defGetString(def)) == TESS_PARQUET_UTF8_TRUST;
		else
			unknown_option(def);
	}
	if (catalog == ForeignTableRelationId && !files)
		ereport(ERROR,
				(errcode(ERRCODE_FDW_DYNAMIC_PARAMETER_VALUE_NEEDED),
				 errmsg("a Parquet table needs the option \"files\"")));
	if (trust)
	{
		if (!column)
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_PARAMETER_VALUE),
					 errmsg("a column with utf8 'trust' must name its file column by the option \"column\"")));
		check_file_privilege("change the options of a column of trusted text");
	}
	PG_RETURN_VOID();
}

/* The paths and patterns of the option `files` of the table `relid`. */
List *
parquet_table_files(Oid relid)
{
	ForeignTable *table = GetForeignTable(relid);

	foreach_node(DefElem, def, table->options)
	{
		if (strcmp(def->defname, "files") == 0)
			return split_files(defGetString(def));
	}
	ereport(ERROR,
			(errcode(ERRCODE_FDW_DYNAMIC_PARAMETER_VALUE_NEEDED),
			 errmsg("a Parquet table needs the option \"files\"")));
	pg_unreachable();
}

/* The column `attnum` of the table `relid`, with its options. */
ParquetColumn
parquet_column(Oid relid, int attnum)
{
	ParquetColumn column;

	column.attnum = attnum;
	column.file_column = get_attname(relid, attnum, false);
	get_atttypetypmodcoll(relid, attnum, &column.type_oid, &column.typmod,
						  &(Oid){InvalidOid});
	column.unit = TESS_PARQUET_UNIT_NONE;
	column.utf8 = TESS_PARQUET_UTF8_NONE;
	foreach_node(DefElem, def, GetForeignColumnOptions(relid, attnum))
	{
		if (strcmp(def->defname, "column") == 0)
			column.file_column = defGetString(def);
		else if (strcmp(def->defname, "unit") == 0)
			column.unit = unit_of(defGetString(def));
		else if (strcmp(def->defname, "utf8") == 0)
			column.utf8 = utf8_of(defGetString(def));
	}
	return column;
}
