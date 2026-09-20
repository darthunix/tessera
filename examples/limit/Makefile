MODULE_big = tessera_limit
OBJS = module.o planner.o executor.o
PG_CPPFLAGS = -I$(srcdir)/../../include

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The example links the runtime library statically, like any node module.
RUNTIME_LIB = $(srcdir)/../../runtime/libtessera_runtime.a
$(RUNTIME_LIB):
	$(MAKE) -C $(srcdir)/../../runtime PG_CONFIG="$(PG_CONFIG)"
SHLIB_LINK += $(RUNTIME_LIB)
$(shlib): $(RUNTIME_LIB)

$(OBJS): $(srcdir)/internal.h $(wildcard $(srcdir)/../../include/tessera/*.h)
