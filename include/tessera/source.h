/* Registry of batch sources provided by independent Tessera extensions. */
#ifndef TESSERA_SOURCE_H
#define TESSERA_SOURCE_H

#include "postgres.h"

#include "nodes/pathnodes.h"

#include "tessera/abi.h"

#define TESS_SOURCE_ABI_VERSION 0
#define TESS_SOURCE_REGISTRY_OPS_ABI_VERSION 0

/*
 * A source's answer for one clause of the relation it reads. A source
 * that does not use a clause leaves TESS_CLAUSE_UNSUPPORTED, and
 * TessFilter checks it. TESS_CLAUSE_PRUNE_ONLY: the source may skip rows
 * the clause rejects, and TessFilter still checks it on the rows that
 * reach it. TESS_CLAUSE_EXACT: every row the source returns passes the
 * clause, and TessFilter does not check it.
 */
#define TESS_CLAUSE_UNSUPPORTED 0
#define TESS_CLAUSE_PRUNE_ONLY 1
#define TESS_CLAUSE_EXACT 2

/*
 * The clauses of the relation offered to a source, in the order the core
 * evaluates them. Everything is Tessera's and valid only during the call
 * of scan_rows: clauses and usable are read only, and the source writes
 * answers, which are TESS_CLAUSE_UNSUPPORTED on entry. A source may use,
 * that is answer other than TESS_CLAUSE_UNSUPPORTED for, only a clause
 * whose usable entry is true: one that calls no volatile function, holds
 * no SubPlan, and is leakproof or of the relation's lowest security level.
 * Another answer for a clause that is not usable, a value that is none of
 * the three, or an answer for a clause whose expression is not among the
 * returned path's expressions is an error, XX000. A source reads a field
 * after answers only when struct_size includes it.
 */
typedef struct TessScanClauses
{
	Size		struct_size;
	int			nclauses;
	RestrictInfo *const *clauses;
	const bool *usable;
	int32	   *answers;
} TessScanClauses;

#define TESS_SCAN_CLAUSES_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessScanClauses, answers)

/*
 * Stable identity of one batch source: a module that reads relations of
 * a kind the heap scan node does not, foreign tables in particular, in
 * batches through a node of its own.
 *
 * The provider owns the TessSource structure and the string referenced by
 * name. The registry stores the provider's pointer without copying the
 * structure or name string. The structure and name string must remain valid
 * and unchanged from add until remove returns. The provider must ensure
 * that all consumers have finished using their borrowed pointers before
 * remove, then may free its allocations. scan_rows is an optional trailing
 * field: a consumer reads it only when struct_size includes it.
 */
typedef struct TessSource
{
	uint32		abi_version;
	Size		struct_size;
	const char *name;
	/*
	 * Optional; NULL when the source reads no relation. Tessera's scan
	 * planner calls it with a copy of the core's scan path of a relation (a
	 * foreign table's ForeignPath, its fdw_private included) and the
	 * relation's clauses, for the serial and the partial path apart. The
	 * copy is Tessera's, lives as long as the planning of the statement and
	 * shares the wrapper's private data: the source must not change it nor
	 * make it a child of its path. Its target holds the relation's targets
	 * and every column the clauses read.
	 *
	 * Return NULL when the source does not read the relation, and then
	 * whatever it wrote into the answers is ignored; or a new path built by
	 * tess_path_create for a node of the node registry, which becomes
	 * Tessera's, so the source keeps no pointer to it: of the same
	 * relation, without parameters or path keys, with
	 * CUSTOMPATH_SUPPORT_PROJECTION, parallel-aware with the given path's
	 * workers when the given path is partial (a partial offer is always
	 * parallel-aware, with workers) and serial otherwise,
	 * parallel-safe only where the given path is, its rows and costs the
	 * source's estimate. Every row it returns passes each clause answered
	 * TESS_CLAUSE_EXACT, and it leaves out a row only by such a clause or
	 * when it proves that a clause it uses rejects the row. The node holds
	 * the clauses it uses among its path's and its plan's expressions, its
	 * plan has no qualifiers and takes the relation as its scan tuple, and
	 * it computes a clause's side without columns through PostgreSQL's
	 * evaluation of expressions, when it is first asked for rows and after
	 * each rescan. A check of a clause by the source's own code raises no
	 * error and writes no message that depends on the rows' values. Any
	 * other path is an error, XX000. See docs/source.md.
	 */
	CustomPath *(*scan_rows) (PlannerInfo *root, Path *path,
							  TessScanClauses *clauses);
} TessSource;

#define TESS_SOURCE_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessSource, name)

/* Backend-local registry shared by independently built extensions. */
typedef struct TessSourceRegistryOps
{
	uint32		abi_version;
	Size		struct_size;
	/*
	 * Register without copying. Repeating the same registration is safe.
	 * ERROR: XX000 for NULL or a source without a name, 0A000 for another
	 * version or a size below TESS_SOURCE_MIN_SIZE, 42710 for another
	 * source of a name registered.
	 */
	void		(*add) (const TessSource *source);
	/*
	 * Unregister this exact source without freeing it or waiting for users.
	 * NULL and repeated calls for an object that is still alive are safe.
	 */
	void		(*remove) (const TessSource *source);
	/*
	 * Find by case-sensitive name, or return NULL. The borrowed pointer does
	 * not extend the source's lifetime and must not be used after remove.
	 * The consumer must not modify or free the source or its name.
	 */
	const TessSource *(*find) (const char *name);
	/*
	 * The sources in the order they were registered: next(NULL) the first,
	 * next(source) the one registered after it, NULL after the last or when
	 * none is registered. The pointer is borrowed as find's. A pointer that
	 * is not registered is an error, XX000. Check the table with
	 * tess_source_registry_check before calling it.
	 */
	const TessSource *(*next) (const TessSource *previous);
} TessSourceRegistryOps;

#define TESS_SOURCE_REGISTRY_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessSourceRegistryOps, next)

#endif /* TESSERA_SOURCE_H */
