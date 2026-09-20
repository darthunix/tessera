MODULE_big = tessera_nodes
OBJS = filter.o module.o pack.o planner.o
PG_CPPFLAGS = -I$(srcdir)/../include

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The module links the runtime library statically.
RUNTIME_LIB = $(srcdir)/../runtime/libtessera_runtime.a
$(RUNTIME_LIB):
	$(MAKE) -C $(srcdir)/../runtime PG_CONFIG="$(PG_CONFIG)"
SHLIB_LINK += $(RUNTIME_LIB)
$(shlib): $(RUNTIME_LIB)

$(OBJS): $(srcdir)/internal.h $(wildcard $(srcdir)/../include/tessera/*.h)
