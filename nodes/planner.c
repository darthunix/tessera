#include "postgres.h"

#include "access/sysattr.h"
#include "access/table.h"
#include "catalog/pg_class.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/restrictinfo.h"
#include "optimizer/tlist.h"
#include "parser/parsetree.h"
#include "utils/rel.h"

#include "tessera/expr.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner side of TessFilter: a path over a base relation whose
 * clauses the node takes away from the sequential scan below the pack
 * node, so that the scan produces every row and the node filters them:
 * the leading clauses the expression compiler supports by batches, the
 * rest row by row, in the planner's order. See docs/nodes.md.
 */

/*
 * The paths cost a fraction of the core's scan, tessera.scan_cost_factor
 * (0.9): there is no cost model yet.
 */

static set_rel_pathlist_hook_type previous_set_rel_pathlist_hook = NULL;

static Plan *filter_plan(PlannerInfo *root, RelOptInfo *rel,
						 CustomPath *best_path, List *tlist, List *clauses,
						 List *custom_plans);
static double scan_time(PlannerInfo *root, RelOptInfo *rel, Path *path);

static const CustomPathMethods filter_path_methods = {
	.CustomName = "TessFilter",
	.PlanCustomPath = filter_plan,
};

const CustomScanMethods tess_filter_scan_methods = {
	.CustomName = "TessFilter",
	.CreateCustomScanState = tess_filter_create_state,
};

/*
 * A plain scan of one heap table in a SELECT, with nothing parameterized:
 * a base relation, or a partition or inheritance child, whose clauses the
 * core translated from its parent's.
 */
static bool
relation_supported(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	return IS_SIMPLE_REL(rel) &&
		rte->rtekind == RTE_RELATION && rte->relkind == RELKIND_RELATION &&
		!rte->inh && rte->tablesample == NULL &&
		root->parse->commandType == CMD_SELECT &&
		root->parse->rowMarks == NIL && rel->lateral_relids == NULL;
}

/*
 * The clauses in the order the planner evaluates them, as its static
 * order_qual_clauses sorts a plan's quals: by cost within security
 * levels, a cheap leakproof clause counting as level zero, equals in
 * their order; an insertion sort, which keeps them so.
 */
List *
tess_order_clauses(PlannerInfo *root, List *rinfos)
{
	int			count = list_length(rinfos);
	RestrictInfo **items;
	Cost	   *costs;
	Index	   *levels;
	List	   *ordered = NIL;
	int			index = 0;

	if (count <= 1)
		return list_copy(rinfos);
	items = palloc_array(RestrictInfo *, count);
	costs = palloc_array(Cost, count);
	levels = palloc_array(Index, count);
	foreach_node(RestrictInfo, rinfo, rinfos)
	{
		QualCost	cost;
		Cost		item_cost;
		Index		item_level;
		int			at = index;

		cost_qual_eval_node(&cost, (Node *) rinfo, root);
		item_cost = cost.per_tuple;
		item_level = rinfo->leakproof && item_cost < 10 * cpu_operator_cost ?
			0 : rinfo->security_level;
		while (at > 0 && (levels[at - 1] > item_level ||
						  (levels[at - 1] == item_level && costs[at - 1] > item_cost)))
		{
			items[at] = items[at - 1];
			costs[at] = costs[at - 1];
			levels[at] = levels[at - 1];
			at--;
		}
		items[at] = rinfo;
		costs[at] = item_cost;
		levels[at] = item_level;
		index++;
	}
	for (index = 0; index < count; index++)
		ordered = lappend(ordered, items[index]);
	return ordered;
}

/*
 * The clause the planner evaluates first. NULL with a pseudoconstant
 * clause: it makes the planner wrap each scan of the relation in a gating
 * Result, which the plan would then find in place of its children.
 */
static RestrictInfo *
first_clause(PlannerInfo *root, RelOptInfo *rel)
{
	foreach_ptr(RestrictInfo, rinfo, rel->baserestrictinfo)
	{
		if (rinfo->pseudoconstant)
			return NULL;
	}
	if (rel->baserestrictinfo == NIL)
		return NULL;
	return linitial(tess_order_clauses(root, rel->baserestrictinfo));
}

/* Whether the node has batch work: the first clause is a batch filter. */
static bool
clauses_supported(PlannerInfo *root, RelOptInfo *rel)
{
	RestrictInfo *first = first_clause(root, rel);

	return first != NULL &&
		tess_expr_supports_filter((Node *) first->clause, rel->relid);
}

/* The relation's targets and the clauses' columns, which the filter reads. */
static PathTarget *
filter_input_target(PlannerInfo *root, RelOptInfo *rel)
{
	PathTarget *target = copy_pathtarget(rel->reltarget);
	List	   *clauses = extract_actual_clauses(rel->baserestrictinfo, false);

	add_new_columns_to_pathtarget(target,
								  pull_var_clause((Node *) clauses,
												  PVC_RECURSE_PLACEHOLDERS));
	return set_pathtarget_cost_width(root, target);
}

/*
 * The batch child over a copy of the scan: add_path frees the core path
 * the node's path dominates. The scan reads the clauses' columns as well
 * as the relation's targets, since the node evaluates the clauses. A node
 * that reads the relation in batches natively evaluates no clause and
 * comes first; otherwise the pack node stands over the core scan.
 */
static Path *
make_child_path(PlannerInfo *root, RelOptInfo *rel, const Path *seqscan)
{
	Path	   *copy = makeNode(Path);
	Path	   *child;

	*copy = *seqscan;
	copy->pathtarget = filter_input_target(root, rel);
	child = tess_batch_scan_path(root, copy);
	return child != NULL ? child : tess_batch_input_path(root, copy);
}

/* The node's path over the child, with the scan's properties and rows. */
static CustomPath *
make_filter_path(RelOptInfo *rel, const Path *seqscan, Path *child)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path		template = *seqscan;

	template.total_cost *= tess_scan_cost_factor;
	config.template_path = &template;
	config.methods = &filter_path_methods;
	config.node = &tess_filter_node;
	config.children = list_make1(child);
	/* Expressions in the targets are computed over the batches. */
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	return tess_path_create(&config);
}

/* The unparameterized sequential scan of the list, or NULL. */
static Path *
find_seqscan(const List *pathlist)
{
	foreach_ptr(Path, path, pathlist)
	{
		if (path->pathtype == T_SeqScan && path->param_info == NULL)
			return path;
	}
	return NULL;
}

/*
 * TessFilter over a base relation whose clauses all run row by row, in
 * place of its sequential scan (a partial one gives a partial path), or
 * NULL. The relation gets it as a path (add_row_filter_paths), and an
 * inner or semi hash join takes it for its outer side: the join's Bloom
 * filter then removes rows before the row-wise clauses run.
 */
Path *
tess_filter_row_path(PlannerInfo *root, RelOptInfo *rel, Path *seqscan)
{
	Path	   *child;

	if (!*tess_runtime_api()->settings->enable || seqscan == NULL ||
		seqscan->pathtype != T_SeqScan || seqscan->param_info != NULL ||
		!relation_supported(root, rel, planner_rt_fetch(rel->relid, root)) ||
		first_clause(root, rel) == NULL || clauses_supported(root, rel) ||
		(seqscan->parallel_workers > 0 &&
		 (!seqscan->parallel_aware || !rel->consider_parallel)))
		return NULL;
	child = make_child_path(root, rel, seqscan);
	return child != NULL ? (Path *) make_filter_path(rel, seqscan, child) : NULL;
}

/*
 * The node's path in place of the sequential scan, and a partial one in
 * place of the parallel sequential scan, so that a Gather above runs the
 * node in every participant over that participant's share of the pages;
 * the partial path keeps the core scan's number of workers and rows per
 * participant, and is parallel-aware for the counters the node shares.
 */
static void
add_filter_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	Path	   *seqscan;
	Path	   *partial;
	Path	   *child;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || !clauses_supported(root, rel))
		return;
	/*
	 * Each where the core kept its sequential scan: its add_path drops the
	 * serial one for an index scan it costs less, while the partial one may
	 * stay.
	 */
	seqscan = find_seqscan(rel->pathlist);
	child = seqscan != NULL ? make_child_path(root, rel, seqscan) : NULL;
	if (child != NULL)
		add_path(rel, (Path *) make_filter_path(rel, seqscan, child));
	partial = find_seqscan(rel->partial_pathlist);
	if (partial == NULL || !partial->parallel_aware || !rel->consider_parallel)
		return;
	child = make_child_path(root, rel, partial);
	/*
	 * Parallel-aware as the template is: the child divides the work, and
	 * the node shares its counters, which takes the callbacks.
	 */
	if (child != NULL)
		add_partial_path(rel, (Path *) make_filter_path(rel, partial, child));
}

/*
 * The native scan in place of the sequential scan of a relation without
 * clauses, and in place of the parallel one: it is faster than the core's
 * under any parent, a row-wise one included, since it pins a page once
 * and deforms only the columns read (bench/pg/rowwise). The path costs
 * the filter's fraction of the scan's: there is no cost model yet.
 */
static void
add_scan_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	Path	   *seqscan;
	Path	   *copy;
	Path	   *scan;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || rel->baserestrictinfo != NIL)
		return;
	/* Each where the core kept its sequential scan, as add_filter_paths. */
	seqscan = find_seqscan(rel->pathlist);
	if (seqscan != NULL)
	{
		/* add_path frees a core path the node's dominates: the node keeps a copy. */
		copy = makeNode(Path);
		*copy = *seqscan;
		scan = tess_batch_scan_path(root, copy);
		if (scan != NULL)
		{
			scan->total_cost *= tess_scan_cost_factor;
			add_path(rel, scan);
		}
	}
	seqscan = find_seqscan(rel->partial_pathlist);
	if (seqscan == NULL || !seqscan->parallel_aware || !rel->consider_parallel)
		return;
	copy = makeNode(Path);
	*copy = *seqscan;
	scan = tess_batch_scan_path(root, copy);
	if (scan == NULL)
		return;
	scan->total_cost *= tess_scan_cost_factor;
	add_partial_path(rel, scan);
}

/*
 * TessFilter over a relation whose clauses all run row by row, in place
 * of its sequential scan and its parallel one: the node's rows cost less
 * than the core's scan under any parent (bench/pg/rowwise), and a batch
 * parent above reads its batches instead of a pack's copies.
 */
static void
add_row_filter_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	Path	   *path;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || rel->baserestrictinfo == NIL)
		return;
	path = tess_filter_row_path(root, rel, find_seqscan(rel->pathlist));
	if (path == NULL)
		return;
	add_path(rel, path);
	path = tess_filter_row_path(root, rel, find_seqscan(rel->partial_pathlist));
	if (path != NULL)
		add_partial_path(rel, path);
}

/*
 * TessFilter over the node's scan of a bitmap's pages in place of each of
 * the core's bitmap heap scans of the relation, unparameterized and
 * serial: the filter evaluates every clause, the index's among them,
 * which a lossy page needs rechecked and an exact one does not (a filter
 * in batches is cheap), and the path costs the filter's fraction of the
 * core's. With partial, in place of the core's parallel ones, whose bitmap
 * the participants share, the node's scan dividing its pages among them:
 * only where the model's time of the partial scan is below serial_time,
 * the relation's fastest serial scan, or where serial_time is negative, no
 * model ranking the relation.
 */
static void
add_bitmap_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte,
				 bool partial, double serial_time)
{
	List	   *bitmaps = NIL;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || first_clause(root, rel) == NULL ||
		(partial && !rel->consider_parallel))
		return;
	/* add_path frees a core path the node's dominates: copies are taken first. */
	foreach_ptr(Path, path, partial ? rel->partial_pathlist : rel->pathlist)
	{
		BitmapHeapPath *copy;

		if (!IsA(path, BitmapHeapPath) || path->param_info != NULL ||
			path->parallel_aware != partial)
			continue;
		if (partial && serial_time >= 0 && scan_time(root, rel, path) >= serial_time)
			continue;
		copy = palloc_object(BitmapHeapPath);
		memcpy(copy, path, sizeof(BitmapHeapPath));
		bitmaps = lappend(bitmaps, copy);
	}
	foreach_ptr(BitmapHeapPath, bitmap, bitmaps)
	{
		Path	   *scan = tess_heap_bitmap_path(root, bitmap,
												 filter_input_target(root, rel));

		if (scan == NULL)
			continue;
		scan = (Path *) make_filter_path(rel, &bitmap->path, scan);
		if (partial)
			add_partial_path(rel, scan);
		else
			add_path(rel, scan);
	}
}

/*
 * The node over each of the core's unparameterized, serial index scans and
 * index-only scans of the relation, in its order, TessFilter above it when
 * the relation has clauses: the filter evaluates every clause, the index's
 * too, and the path costs the filter's fraction of the core's and keeps
 * its order. With partial, over the core's parallel ones, whose scan
 * divides the work among the participants, which makes the node's path
 * over it partial: only where the model's time of the partial scan
 * (partial_time, the workers' start in it) is below serial_time, the
 * relation's fastest serial scan, or where serial_time is negative, no
 * model ranking the relation. The core's cost does not tell: at 10 % of
 * bench_idx the node's parallel index-only scan took 3.3 ms against 3.0
 * serially, at 15 % 4.1 against 4.5, and its parallel index scan of 10 %
 * 4.8 against 4.0 for its serial bitmap.
 */
static void
add_index_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte,
				bool partial, double serial_time)
{
	List	   *indexes = NIL;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) ||
		(rel->baserestrictinfo != NIL && first_clause(root, rel) == NULL) ||
		(partial && !rel->consider_parallel))
		return;
	/* add_path frees a core path the node's dominates: copies are taken first. */
	foreach_ptr(Path, path, partial ? rel->partial_pathlist : rel->pathlist)
	{
		IndexPath  *copy;

		if (!IsA(path, IndexPath) ||
			(path->pathtype != T_IndexScan && path->pathtype != T_IndexOnlyScan) ||
			path->param_info != NULL || path->parallel_aware != partial)
			continue;
		if (partial && serial_time >= 0 && scan_time(root, rel, path) >= serial_time)
			continue;
		copy = makeNode(IndexPath);
		memcpy(copy, path, sizeof(IndexPath));
		indexes = lappend(indexes, copy);
	}
	foreach_ptr(IndexPath, index, indexes)
	{
		Path	   *scan;

		if (rel->baserestrictinfo == NIL)
		{
			scan = tess_heap_index_path(root, index, rel->reltarget);
			if (scan == NULL)
				continue;
			scan->total_cost = index->path.total_cost * tess_scan_cost_factor;
		}
		else
		{
			scan = tess_heap_index_path(root, index, filter_input_target(root, rel));
			if (scan == NULL)
				continue;
			scan = (Path *) make_filter_path(rel, &index->path, scan);
		}
		if (partial)
			add_partial_path(rel, scan);
		else
			add_path(rel, scan);
	}
}

/*
 * The model of the node's scans (bench/pg/scancost fits it, docs/nodes.md
 * explains it): a scan's time in units of a page of the node's full scan,
 * by which the planner ranks the node's full scan against the index
 * scans. The core costs them all for its own speeds, and the node's full
 * scan exceeds the core's by far more than its index scans do (3.5 to 4.5
 * times against 1.2 to 1.4): at a share of the core's cost each, a full
 * scan of 2 M rows lost to an index scan of up to half of them, which took
 * 3.2 times as long.
 */
static double
full_scan_time(RelOptInfo *rel)
{
	return rel->pages * tess_scan_page_cost + rel->tuples * tess_scan_tuple_cost;
}

/*
 * The node's full scan: the heap scan without a child, or the filter over
 * it.
 */
static bool
is_full_scan(Path *path)
{
	if (tess_path_node(path) == &tess_filter_node)
		path = linitial(((CustomPath *) path)->custom_paths);
	return tess_path_node(path) == &tess_heap_scan_node &&
		((CustomPath *) path)->custom_paths == NIL;
}

/* The node's path, which the ranking may add again at a lower cost. */
static bool
is_node_scan(Path *path)
{
	return tess_path_node(path) == &tess_filter_node ||
		tess_path_node(path) == &tess_heap_scan_node;
}

/*
 * The pages a bitmap reads: the core's estimate for rows at random
 * places (compute_bitmap_pages), which the column's correlation c moves
 * toward the pages its rows fill in the table's order, by c² as the core
 * weighs an index scan's reads: the ordered id's bitmap of 10 % of
 * bench_idx read 1450 pages, which the core estimated at all 14 500.
 */
static double
bitmap_pages(PlannerInfo *root, RelOptInfo *rel, Path *bitmapqual, double *tuples)
{
	double		correlation = IsA(bitmapqual, IndexPath) ?
		tess_index_correlation(root, ((IndexPath *) bitmapqual)->indexinfo) : 0;
	Cost		unused;
	double		random = compute_bitmap_pages(root, rel, bitmapqual, 1.0, &unused, tuples);
	double		ordered = rel->tuples > 0 ? ceil(*tuples * rel->pages / rel->tuples) : random;
	double		weight = correlation * correlation;

	return weight * Min(ordered, random) + (1.0 - weight) * random;
}

/*
 * The first attribute of the relation whose values vary in length, past
 * which a column's offset in a row is not known before the row is read;
 * 0 without one.
 */
static AttrNumber
first_varlena(PlannerInfo *root, RelOptInfo *rel)
{
	Relation	relation = table_open(planner_rt_fetch(rel->relid, root)->relid, NoLock);
	TupleDesc	desc = RelationGetDescr(relation);
	AttrNumber	first = 0;

	for (int i = 0; i < desc->natts && first == 0; i++)
	{
		if (TupleDescCompactAttr(desc, i)->attlen < 0)
			first = i + 1;
	}
	table_close(relation, NoLock);
	return first;
}

/*
 * The filter's time past what a scan's time a row counts, for the rows
 * entering it (bench/pg/scancost fits it): the scan's price of a row
 * holds one batch clause on a column no varlena precedes. Each later
 * clause, in the planner's order, sees the rows the earlier ones left:
 * one in batches tessera.filter_clause_cost of them a row, one by rows
 * tessera.filter_row_clause_cost and tessera.filter_row_operator_cost an
 * operator of the core's cost; a clause on a column past a varlena, the
 * first too, deforms it for the rows it sees, tessera.deform_varlena_cost
 * a row, unless an earlier clause did (deform, false for an index-only
 * scan, whose rows come from the index). A clause among conditions, the
 * clauses an index answers, leaves every row the index gave. The model's
 * full scan without it took 2.2 ms for bench_mixed's rows with a clause
 * by rows on a text after the first clause, which took 5.1.
 */
static double
filter_time(PlannerInfo *root, RelOptInfo *rel, double rows, bool deform, List *conditions)
{
	AttrNumber	varlena = deform && rel->baserestrictinfo != NIL ? first_varlena(root, rel) : 0;
	Bitmapset  *deformed = NULL;
	double		time = 0;
	bool		first = true;

	foreach_node(RestrictInfo, rinfo, tess_order_clauses(root, rel->baserestrictinfo))
	{
		Bitmapset  *columns = NULL;
		bool		past = false;
		int			member = -1;

		pull_varattnos((Node *) rinfo->clause, rel->relid, &columns);
		while ((member = bms_next_member(columns, member)) >= 0)
		{
			if (varlena > 0 && member + FirstLowInvalidHeapAttributeNumber > varlena &&
				!bms_is_member(member, deformed))
				past = true;
			deformed = bms_add_member(deformed, member);
		}
		if (past)
			time += rows * tess_deform_varlena_cost;
		if (!first && tess_expr_supports_filter((Node *) rinfo->clause, rel->relid))
			time += rows * tess_filter_clause_cost;
		else if (!first)
		{
			QualCost	cost;

			cost_qual_eval_node(&cost, (Node *) rinfo->clause, root);
			time += rows * (tess_filter_row_clause_cost +
							tess_filter_row_operator_cost * cost.per_tuple / cpu_operator_cost);
		}
		if (!list_member_ptr(conditions, rinfo))
			rows *= clause_selectivity(root, (Node *) rinfo, 0, JOIN_INNER, NULL);
		first = false;
	}
	return time;
}

/*
 * The relation's clauses an index scan or a bitmap answers: an index's
 * conditions, those of every index of a BitmapAnd; a BitmapOr's none of
 * the relation's own.
 */
static List *
index_conditions(Path *path)
{
	List	   *conditions = NIL;

	if (IsA(path, IndexPath))
	{
		foreach_node(IndexClause, clause, ((IndexPath *) path)->indexclauses)
			conditions = lappend(conditions, clause->rinfo);
	}
	else if (IsA(path, BitmapAndPath))
	{
		foreach_ptr(Path, child, ((BitmapAndPath *) path)->bitmapquals)
			conditions = list_concat(conditions, index_conditions(child));
	}
	return conditions;
}

/*
 * A partial scan's time, from the serial time and the pages the scan
 * touches: the workers' start and finish (tessera.scan_parallel_setup_cost),
 * then the serial time shared among the participants, the leader reading
 * a page in its serial time c, a worker in c and the toll of a page it
 * touches first (tessera.scan_worker_page_cost): a worker is a process
 * begun for the query, which maps every page of the shared buffers it
 * reads (fork gives it none of the leader's), so it counts c / (c + toll)
 * of the leader. The toll is the core's too, but a page of the node's full
 * scan takes a quarter of the core's time: two workers sped the node's
 * scan of bench_idx 1.2 times and the core's 1.9 times
 * (target/bench-runs/pg-workers-nIhBf4, bench/pg/scancost).
 */
static double
partial_time(double time, double pages, int workers)
{
	double		page = pages > 0 ? time / pages : 0;
	double		share = page > 0 ? page / (page + tess_scan_worker_page_cost) : 1;
	double		divisor = (parallel_leader_participation ? 1.0 : 0.0) + workers * share;

	return tess_scan_parallel_setup_cost + (divisor > 0 ? time / divisor : time);
}

/*
 * A partial index or index-only scan's time, from the serial time: the
 * workers' start and finish, and the serial time past what the leader
 * reads alone while the workers start (half the start and finish), shared
 * among the participants, a worker reading at tessera.index_worker_share
 * of the leader's pace: the core's parallel btree scan hands its leaf
 * pages on one at a time. Two workers read 30 % of bench_idx's ordered id
 * 1.7 times as fast as the leader alone and 20 % of the scattered k's
 * index 1.4 times, where the toll a page counted 2.1 and 2.2
 * (bench/pg/scancost).
 */
static double
partial_index_time(double time, int workers)
{
	double		head = parallel_leader_participation ? tess_scan_parallel_setup_cost / 2 : 0;
	double		divisor = (parallel_leader_participation ? 1.0 : 0.0) +
		workers * tess_index_worker_share;

	return tess_scan_parallel_setup_cost + (divisor > 0 ? Max(time - head, 0) / divisor : time);
}

/*
 * The time a scan of the relation takes the node, which is also a floor
 * of the core's own scan of the same kind: a full scan by the table's
 * pages and rows; an index-only or index scan by the rows the index's
 * conditions select (an index-only scan's rows on pages not all visible
 * read the table as an index scan's do); a bitmap by its pages
 * (bitmap_pages) and rows, a row of an index out of the table's order
 * taking more, its bitmap built from rows in no order of their pages;
 * each with the filter's time over the rows entering it (filter_time); a
 * partial full scan's or bitmap's by partial_time, from the pages the scan
 * touches, the table's or the bitmap's, a partial bitmap's building by one
 * participant; a partial index or index-only scan's by partial_index_time.
 * -1 for a parameterized path or one of another kind. The core's own cost does not
 * serve: its time a unit of cost varied four times over its bitmaps
 * (bench/pg/scancost).
 */
static double
scan_time(PlannerInfo *root, RelOptInfo *rel, Path *path)
{
	Path	   *scan = path;
	double		time;
	double		pages;

	if (path->param_info != NULL)
		return -1;
	if (tess_path_node(scan) == &tess_filter_node)
		scan = linitial(((CustomPath *) scan)->custom_paths);
	if (tess_path_node(scan) == &tess_heap_scan_node)
	{
		if (((CustomPath *) scan)->custom_paths == NIL)
			scan = NULL;
		else
			scan = linitial(((CustomPath *) scan)->custom_paths);
	}
	else if (tess_path_node(scan) != NULL)
		return -1;
	if (scan == NULL || scan->pathtype == T_SeqScan)
	{
		time = full_scan_time(rel) + filter_time(root, rel, rel->tuples, true, NIL);
		pages = rel->pages;
	}
	else if (IsA(scan, IndexPath) &&
			 (scan->pathtype == T_IndexOnlyScan || scan->pathtype == T_IndexScan))
	{
		IndexPath  *index = (IndexPath *) scan;
		double		visible = scan->pathtype == T_IndexOnlyScan ? rel->allvisfrac : 0;

		time = index->indexselectivity * rel->tuples *
			(visible * tess_index_only_tuple_cost + (1.0 - visible) * tess_index_tuple_cost) +
			filter_time(root, rel, index->indexselectivity * rel->tuples,
						scan->pathtype == T_IndexScan, index_conditions(scan));
		if (path->parallel_workers > 0)
			return partial_index_time(time, path->parallel_workers);
		pages = 0;
	}
	else if (IsA(scan, BitmapHeapPath))
	{
		Path	   *bitmapqual = ((BitmapHeapPath *) scan)->bitmapqual;
		double		correlation = IsA(bitmapqual, IndexPath) ?
			tess_index_correlation(root, ((IndexPath *) bitmapqual)->indexinfo) : 0;
		double		scatter = 1.0 - correlation * correlation;
		double		tuples;

		pages = bitmap_pages(root, rel, bitmapqual, &tuples);
		time = pages * tess_bitmap_page_cost +
			tuples * (tess_bitmap_tuple_cost + tess_bitmap_scatter_cost * scatter) +
			filter_time(root, rel, tuples, true, index_conditions(bitmapqual));
		/*
		 * A partial bitmap is built by one participant, the others waiting:
		 * only the reading of its pages is shared, and the building overlaps
		 * the workers' start, half the start and finish (bench/pg/scancost
		 * fitted 0.9 ms of 1.85). A BRIN bitmap is built by ranges of
		 * pages, at next to no cost.
		 */
		if (path->parallel_workers > 0)
		{
			double		build = tess_bitmap_only_brin(bitmapqual) ? 0 :
				Min(time, tuples * (tess_bitmap_build_cost +
									tess_bitmap_build_scatter_cost * scatter));

			return partial_time(time - build, pages, path->parallel_workers) +
				Max(build - tess_scan_parallel_setup_cost / 2, 0);
		}
	}
	else
		return -1;
	if (path->parallel_workers > 0)
		time = partial_time(time, pages, path->parallel_workers);
	return time;
}

/*
 * The node's full scan, serial or partial, from the core's sequential scan
 * given, at cost; NULL where the node does not read the relation so.
 */
static Path *
full_scan_path(PlannerInfo *root, RelOptInfo *rel, Path *seqscan, Cost cost)
{
	Path	   *scan;

	if (rel->baserestrictinfo == NIL)
		scan = tess_batch_scan_path(root, seqscan);
	else
	{
		Path	   *child = make_child_path(root, rel, seqscan);

		scan = child != NULL ? (Path *) make_filter_path(rel, seqscan, child) : NULL;
	}
	if (scan == NULL)
		return NULL;
	scan->startup_cost = Min(scan->startup_cost, cost);
	scan->total_cost = cost;
	return scan;
}

/*
 * The node's scans of a relation ranked by the model's times rather than
 * the core's costs, which the node's full scan outruns by far more than
 * its index scans: from the slowest, each of the node's paths costs just
 * below (0.99) the cheapest of those the model finds slower, never lower,
 * so that the relation's cheapest cost, which the joins above read,
 * hardly moves; a path costing that already stays, one above is added
 * again at it (add_path drops the dearer copy), and an ordered path stays
 * beside an unordered one for a sort's comparison. The node's full scan
 * joins the ranking where add_path had dropped it: made anew from
 * seqscan, the core's path copied before, or from a sequential scan made
 * here where the core's add_path had dropped its own for an index scan
 * it costs less. The serial ranking leaves in serial the time and cost
 * of the fastest scan the list holds, -1 as the time where the relation
 * is not ranked. The partial list is ranked by a partial scan's time
 * (partial_time), and its costs follow the serial list's: each of the
 * node's partial paths costs its time at serial's cost a unit of time,
 * less parallel_setup_cost, which the gather over it adds, so that the
 * core's choice between the gather and a serial path follows the model,
 * rising where the model finds the workers not worth their start (the
 * path leaves the list and is added again at its cost), still below the
 * slower scans and below a core's scan the model times alike. The core's
 * own costs made that choice before: a partial full scan at 0.99 of the
 * core's parallel index scan beat a serial bitmap at 15 % of the ordered
 * id of bench_idx, 6.85 ms against 6.1. Only for a table the cache holds
 * (the core's effective_cache_size), which the model measured, and a
 * relation whose first clause runs in batches (filter_time counts the
 * later ones, in batches or by rows).
 */
/* The core's path under the node's, or the path itself. */
static Path *
core_scan(Path *path)
{
	if (tess_path_node(path) == &tess_filter_node)
		path = linitial(((CustomPath *) path)->custom_paths);
	if (tess_path_node(path) == &tess_heap_scan_node && ((CustomPath *) path)->custom_paths != NIL)
		path = linitial(((CustomPath *) path)->custom_paths);
	return path;
}

/*
 * The node's bitmap of an index whose serial scan the serial list holds,
 * where the list holds no bitmap of it: the core's add_path dropped it for
 * the ordered index scan it costs less (a bitmap of the ordered id of
 * bench_idx took 4.0 ms at 10 % against 5.7 for the index scan), and its
 * add_partial_path the partial bitmap for the parallel index scan, as
 * create_partial_bitmap_paths makes it (its workers by the bitmap's pages).
 * Not added yet; NULL where there is a bitmap of the index, or the node
 * takes none.
 */
static Path *
missing_bitmap(PlannerInfo *root, RelOptInfo *rel, IndexPath *index, bool partial)
{
	BitmapHeapPath *bitmap;
	Path	   *scan;
	int			workers = 0;

	if (index->path.pathtype != T_IndexScan || index->path.param_info != NULL ||
		index->path.parallel_workers > 0 || index->indexorderbys != NIL ||
		!index->indexinfo->amhasgetbitmap)
		return NULL;
	foreach_ptr(Path, path, partial ? rel->partial_pathlist : rel->pathlist)
	{
		Path	   *child = core_scan(path);

		if (IsA(child, BitmapHeapPath) && IsA(((BitmapHeapPath *) child)->bitmapqual, IndexPath) &&
			((IndexPath *) ((BitmapHeapPath *) child)->bitmapqual)->indexinfo == index->indexinfo)
			return NULL;
	}
	if (partial)
	{
		workers = compute_parallel_worker(rel, compute_bitmap_pages(root, rel, (Path *) index,
																	1.0, NULL, NULL),
										  -1, max_parallel_workers_per_gather);
		if (workers <= 0)
			return NULL;
	}
	bitmap = create_bitmap_heap_path(root, rel, (Path *) index, NULL, 1.0, workers);
	scan = tess_heap_bitmap_path(root, bitmap, filter_input_target(root, rel));
	return scan != NULL ? (Path *) make_filter_path(rel, &bitmap->path, scan) : NULL;
}

/* A scan in the ranking: its time and cost, and the node's path copied, if any. */
typedef struct RankedScan
{
	double		time;
	Cost		cost;
	/* The path in the list, NULL for one to be made. */
	Path	   *path;
	/* The node's path, copied before add_path may free the original. */
	Path	   *copy;
	bool		full;
	/* The copy costs anew and is added again. */
	bool		again;
} RankedScan;

/* The serial list's fastest scan after its ranking: its time and cost. */
typedef struct SerialScan
{
	double		time;
	Cost		cost;
} SerialScan;

static int
compare_ranked(const void *a, const void *b)
{
	double		ta = ((const RankedScan *) a)->time;
	double		tb = ((const RankedScan *) b)->time;

	return ta > tb ? -1 : ta < tb ? 1 : 0;
}

static void
rank_scans(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte, Path *seqscan, bool partial,
		   SerialScan *serial)
{
	List	   *pathlist = partial ? rel->partial_pathlist : rel->pathlist;
	RankedScan *scans;
	int			count = 0;
	bool		have_full = false;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || rel->pages > (BlockNumber) effective_cache_size ||
		(rel->baserestrictinfo != NIL && !clauses_supported(root, rel)) ||
		(partial && !rel->consider_parallel))
		return;
	if (seqscan == NULL)
	{
		int			workers = partial ?
			compute_parallel_worker(rel, rel->pages, -1, max_parallel_workers_per_gather) : 0;

		if (!partial || workers > 0)
			seqscan = create_seqscan_path(root, rel, NULL, workers);
	}
	/* The list's scans, a bitmap made for each serial path at most, the full scan. */
	scans = palloc0_array(RankedScan, list_length(pathlist) + list_length(rel->pathlist) + 1);
	foreach_ptr(Path, path, pathlist)
	{
		RankedScan *scan = &scans[count];

		scan->time = scan_time(root, rel, path);
		if (scan->time < 0)
			continue;
		scan->cost = path->total_cost;
		scan->path = path;
		scan->full = is_full_scan(path);
		if (is_node_scan(path))
		{
			scan->copy = (Path *) makeNode(CustomPath);
			memcpy(scan->copy, path, sizeof(CustomPath));
		}
		have_full |= scan->full;
		count++;
	}
	/*
	 * The bitmaps of the indexes the core's add_path or add_partial_path
	 * dropped, to be added: of an index the serial list scans, or holds a
	 * bitmap of.
	 */
	if (rel->baserestrictinfo != NIL)
	{
		List	   *indexes = NIL;

		foreach_ptr(Path, path, rel->pathlist)
		{
			Path	   *child = core_scan(path);
			Path	   *bitmap;

			if (IsA(child, BitmapHeapPath))
				child = ((BitmapHeapPath *) child)->bitmapqual;
			if (!IsA(child, IndexPath) ||
				list_member_ptr(indexes, ((IndexPath *) child)->indexinfo))
				continue;
			indexes = lappend(indexes, ((IndexPath *) child)->indexinfo);
			bitmap = missing_bitmap(root, rel, (IndexPath *) child, partial);
			if (bitmap == NULL)
				continue;
			scans[count].time = scan_time(root, rel, bitmap);
			scans[count].cost = -1;
			scans[count].copy = bitmap;
			count++;
		}
	}
	/* The full scan add_path dropped, to be made anew. */
	if (!have_full && seqscan != NULL)
	{
		scans[count].time = scan_time(root, rel, seqscan);
		scans[count].cost = -1;
		scans[count].full = true;
		count++;
	}
	qsort(scans, count, sizeof(RankedScan), compare_ranked);
	for (int i = 0; i < count; i++)
	{
		Cost		bound = -1;
		Cost		cost;

		if (scans[i].copy == NULL && !(scans[i].full && scans[i].cost < 0))
			continue;
		/*
		 * The slower scans bound it, and in the partial list a core's scan
		 * the model times alike, a floor of the core's: the node's path over
		 * it, or its full scan beside the core's, costs no more.
		 */
		for (int j = 0; j < count; j++)
		{
			if (j != i && scans[j].cost >= 0 &&
				(scans[j].time > scans[i].time ||
				 (partial && scans[j].time == scans[i].time &&
				  scans[j].path != NULL && scans[j].copy == NULL)) &&
				(bound < 0 || scans[j].cost < bound))
				bound = scans[j].cost;
		}
		if (partial && serial->time > 0)
		{
			/*
			 * A partial scan costs its time at the serial list's price of
			 * a unit of time, less what a gather adds for the start: the
			 * gather over it costs the serial list's fastest scan's cost
			 * as their times compare.
			 */
			cost = Max(serial->cost / serial->time * scans[i].time - parallel_setup_cost, 0);
			if (bound >= 0)
				cost = Min(cost, 0.99 * bound);
			if (scans[i].cost >= 0 && fabs(cost - scans[i].cost) <= 1e-9 * Max(cost, 1))
				continue;
		}
		else
		{
			if (bound < 0 || (scans[i].cost >= 0 && scans[i].cost <= 0.99 * bound))
				continue;
			cost = 0.99 * bound;
		}
		if (scans[i].copy == NULL &&
			(seqscan == NULL || (scans[i].copy = full_scan_path(root, rel, seqscan, cost)) == NULL))
			continue;
		scans[i].copy->startup_cost = Min(scans[i].copy->startup_cost, cost);
		scans[i].copy->total_cost = cost;
		scans[i].cost = cost;
		scans[i].again = true;
	}
	/*
	 * A partial path whose cost rises leaves the list first, add_partial_path
	 * keeping the cheaper original otherwise; the list holds every original
	 * yet, no path added here having freed one.
	 */
	if (partial)
	{
		for (int i = 0; i < count; i++)
		{
			if (scans[i].again && scans[i].path != NULL)
				rel->partial_pathlist = list_delete_ptr(rel->partial_pathlist, scans[i].path);
		}
	}
	for (int i = 0; i < count; i++)
	{
		if (!scans[i].again)
			continue;
		if (partial)
			add_partial_path(rel, scans[i].copy);
		else
			add_path(rel, scans[i].copy);
	}
	if (!partial)
	{
		serial->time = -1;
		for (int i = count - 1; i >= 0; i--)
		{
			if (scans[i].cost >= 0)
			{
				serial->time = scans[i].time;
				serial->cost = scans[i].cost;
				break;
			}
		}
	}
}

/*
 * The node's paths, the full scan ranked against the index scans, then
 * TessGather over the cheapest partial path, before the core gathers it.
 */
static void
set_rel_pathlist(PlannerInfo *root, RelOptInfo *rel, Index rti,
				 RangeTblEntry *rte)
{
	Path	   *seqscan;
	Path	   *copy = NULL;
	Path	   *partial = NULL;
	SerialScan	serial = {-1, 0};

	if (previous_set_rel_pathlist_hook != NULL)
		previous_set_rel_pathlist_hook(root, rel, rti, rte);
	/* add_path frees a core path the node's dominates: the ranking keeps copies. */
	seqscan = find_seqscan(rel->pathlist);
	if (seqscan != NULL)
	{
		copy = makeNode(Path);
		*copy = *seqscan;
	}
	seqscan = find_seqscan(rel->partial_pathlist);
	if (seqscan != NULL && seqscan->parallel_aware)
	{
		partial = makeNode(Path);
		*partial = *seqscan;
	}
	add_filter_paths(root, rel, rte);
	add_row_filter_paths(root, rel, rte);
	add_scan_paths(root, rel, rte);
	add_bitmap_paths(root, rel, rte, false, -1);
	add_index_paths(root, rel, rte, false, -1);
	rank_scans(root, rel, rte, copy, false, &serial);
	add_index_paths(root, rel, rte, true, serial.time);
	add_bitmap_paths(root, rel, rte, true, serial.time);
	rank_scans(root, rel, rte, partial, true, &serial);
	tess_gather_add_paths(root, rel);
}

/*
 * The scan below was planned with the relation's clauses, as every scan
 * of the relation is; the node takes them over, so that the scan produces
 * every row. Anything else in that place is a planner change unknown here.
 */
static void
take_clauses(CustomScan *pack, List *clauses)
{
	Plan	   *scan;

	if (list_length(pack->custom_plans) != 1)
		elog(ERROR, "TessFilter expected a pack node with one child");
	scan = linitial(pack->custom_plans);
	if (!IsA(scan, SeqScan) || !equal(scan->qual, clauses))
		elog(ERROR, "TessFilter found its clauses missing from the scan below");
	scan->qual = NIL;
}

/*
 * The scan tuple is the child's target list: entry k is the child's
 * column of its target k. The node's own targets, PostgreSQL's projection
 * among them, are derived from it when the plan is read.
 */
static void
map_scan_tuple(TessLayout *layout, const TessPlanChild *child)
{
	int			ntargets = list_length(child->plan->targetlist);
	int		   *map = ntargets > 0 ? palloc_array(int, ntargets) : NULL;

	layout->ncolumns = child->layout.ncolumns;
	layout->ntargets = ntargets;
	for (int target = 0; target < ntargets; target++)
		map[target] = tess_layout_column(&child->layout, target);
	layout->target_columns = map;
}

static Plan *
filter_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
			List *tlist, List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	List	   *actual = extract_actual_clauses(clauses, false);
	List	   *batch_clauses = NIL;
	List	   *residual = NIL;
	List	   *order = NIL;
	TessPlanWriter *writer;

	if (!tess_plan_child(best_path, custom_plans, 0, &child) ||
		!IsA(child.plan, CustomScan))
		elog(ERROR, "TessFilter expected a batch child");
	/* A native scan was planned without the clauses; a core scan had them. */
	if (strcmp(child.node->name, TESS_HEAP_SCAN_NODE_NAME) == 0)
	{
		if (child.plan->qual != NIL)
			elog(ERROR, "TessFilter expected a scan without clauses");
	}
	else
		take_clauses((CustomScan *) child.plan, actual);
	/*
	 * The clauses arrive in evaluation order and keep it: each runs in
	 * batches if the compiler takes it, else by rows, and the plan data
	 * records which in turn.
	 */
	foreach_ptr(Node, clause, actual)
	{
		bool		batch = tess_expr_supports_filter(clause, rel->relid);

		if (batch)
			batch_clauses = lappend(batch_clauses, clause);
		else
			residual = lappend(residual, clause);
		order = lappend_int(order, batch ? 1 : 0);
	}
	if (order == NIL)
		elog(ERROR, "TessFilter found no clause");
	writer = tess_plan_writer_create(TESS_FILTER_DATA, TESS_FILTER_DATA_VERSION);
	tess_plan_write_int_list(writer, "order", order);
	map_scan_tuple(&layout, &child);
	config.methods = &tess_filter_scan_methods;
	config.layout_policy = TESS_LAYOUT_PROJECTED;
	config.explicit_layout = &layout;
	config.qual = residual;
	config.expressions = batch_clauses;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	config.scan_targetlist = child.plan->targetlist;
	config.scanrelid = rel->relid;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

void
tess_filter_planner_init(void)
{
	previous_set_rel_pathlist_hook = set_rel_pathlist_hook;
	set_rel_pathlist_hook = set_rel_pathlist;
}
