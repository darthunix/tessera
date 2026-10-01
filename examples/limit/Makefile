MODULE_big = tessera_limit
OBJS = module.o planner.o executor.o

PG_CONFIG ?= pg_config

# In the tree the example builds from the sources; with TESSERA_INSTALLED
# set, as a module outside the tree would, from the installed headers and
# runtime library alone (test/installed).
ifdef TESSERA_INSTALLED
PG_CPPFLAGS = -I$(includedir_server)/extension
RUNTIME_LIB = $(pkglibdir)/libtessera_runtime.a
else
PG_CPPFLAGS = -I$(srcdir)/../../include
RUNTIME_LIB = $(srcdir)/../../runtime/libtessera_runtime.a
endif

PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The example links the runtime library statically, like any node module.
ifndef TESSERA_INSTALLED
$(RUNTIME_LIB):
	$(MAKE) -C $(srcdir)/../../runtime PG_CONFIG="$(PG_CONFIG)"
endif
SHLIB_LINK += $(RUNTIME_LIB)
$(shlib): $(RUNTIME_LIB)

$(OBJS): $(srcdir)/internal.h $(wildcard $(srcdir)/../../include/tessera/*.h)
