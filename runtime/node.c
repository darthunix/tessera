#include "postgres.h"

#include "commands/explain_format.h"
#include "executor/executor.h"

#include "tessera/runtime_node.h"

void
tess_node_require_forward(int eflags, const char *name)
{
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "%s supports neither backward scan nor mark/restore", name);
}

void
tess_explain_kb(const char *label, uint64 bytes, ExplainState *es)
{
	ExplainPropertyInteger(label, "kB", (int64) ((bytes + 1023) / 1024), es);
}

void
tess_rescan_child(PlanState *parent, PlanState *child, TessInput *input)
{
	if (parent->chgParam != NULL)
		UpdateChangedParamSet(child, parent->chgParam);
	ExecReScan(child);
	if (input != NULL)
		tess_input_rescan(input);
}

TessProjection *
tess_node_projection(CustomScanState *css, TupleTableSlot *scan_slot,
					 const TessLayout *scan_tuple, int base_columns, List *computed)
{
	TessProjectionConfig config = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

	config.parent_context = css->ss.ps.state->es_query_cxt;
	config.parent = &css->ss.ps;
	config.econtext = css->ss.ps.ps_ExprContext;
	config.scan_slot = scan_slot;
	config.scan_tuple = scan_tuple;
	config.base_columns = base_columns;
	config.computed = computed;
	return tess_projection_create(&config);
}
