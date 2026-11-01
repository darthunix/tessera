MODULE_big = tessera_nodes
OBJS = agg.o agg_generic.o agg_group.o agg_keydict.o agg_planner.o agg_spill.o append.o filter.o gather.o gather_planner.o hashjoin.o hashjoin_begin.o hashjoin_pairs.o hashjoin_shared.o hashjoin_spill.o heapscan.o join_planner.o limit.o limit_planner.o module.o pack.o scan_planner.o sort.o sort_external.o sort_planner.o sort_topn.o table_chunks.o
PG_CPPFLAGS = -I$(srcdir)/../include $(TESS_HOST_CPPFLAGS)

PG_CONFIG ?= pg_config
include ../host.mk
ifdef TESS_MPP
# The Greenplum family runs no PostgreSQL parallel query, so no gather
# nodes; mpp_stubs.c stands in for their entry points and for the parts
# not ported yet.
OBJS := $(filter-out gather.o gather_planner.o join_planner.o,$(OBJS)) mpp_stubs.o
endif
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The module links the runtime library statically.
RUNTIME_LIB = $(srcdir)/../runtime/libtessera_runtime.a
$(RUNTIME_LIB):
	$(MAKE) -C $(srcdir)/../runtime PG_CONFIG="$(PG_CONFIG)"
SHLIB_LINK += $(RUNTIME_LIB)
$(shlib): $(RUNTIME_LIB)

$(OBJS): $(wildcard $(srcdir)/*.h) $(wildcard $(srcdir)/../include/tessera/*.h)
