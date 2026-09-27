MODULE_big = tessera
OBJS = binding.o bridge.o function.o kernel.o node.o source.o
PG_CPPFLAGS = -I$(srcdir)/../include

EXTENSION = tessera
DATA = tessera--0.1.sql
HEADERS = \
	../include/tessera/abi.h \
	../include/tessera/batch.h \
	../include/tessera/binding.h \
	../include/tessera/bridge.h \
	../include/tessera/expr.h \
	../include/tessera/function.h \
	../include/tessera/heap_deform.h \
	../include/tessera/kernel_ops.h \
	../include/tessera/kernels.h \
	../include/tessera/layout.h \
	../include/tessera/node.h \
	../include/tessera/plan.h \
	../include/tessera/planner.h \
	../include/tessera/request.h \
	../include/tessera/row_mask.h \
	../include/tessera/runtime.h \
	../include/tessera/sort.h \
	../include/tessera/source.h \
	../include/tessera/spill.h \
	../include/tessera/status.h \
	../include/tessera/table.h
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

$(OBJS): $(srcdir)/internal.h \
	$(wildcard $(srcdir)/../include/tessera/*.h)
