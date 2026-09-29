MODULE_big = tessera_kernels
OBJS = int.o table.o text.o date.o numeric.o
PG_CPPFLAGS = -I$(srcdir)/../include

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The module links the Rust kernels statically.
include $(srcdir)/../rust.mk
SHLIB_LINK += $(RUST_LIB)
$(shlib): $(RUST_LIB)

$(OBJS): $(srcdir)/internal.h $(wildcard $(srcdir)/../include/tessera/*.h)
