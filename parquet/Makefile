MODULE_big = tessera_parquet
OBJS = analyze.o fdw.o files.o node.o options.o scan.o
PG_CPPFLAGS = -I$(srcdir)/../include

EXTENSION = tessera_parquet
DATA = tessera_parquet--0.1.sql
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The module links the runtime library and the reader of Parquet files
# statically.
RUNTIME_LIB = $(srcdir)/../runtime/libtessera_runtime.a
$(RUNTIME_LIB):
	$(MAKE) -C $(srcdir)/../runtime PG_CONFIG="$(PG_CONFIG)"
include $(srcdir)/../rust.mk
SHLIB_LINK += $(RUNTIME_LIB) $(RUST_PARQUET_LIB) $(RUST_PARQUET_SYSTEM_LIBS)
$(shlib): $(RUNTIME_LIB) $(RUST_PARQUET_LIB) $(RUST_PROFILE_STAMP)

$(OBJS): $(srcdir)/tessera_parquet.h $(srcdir)/tessera_parquet_reader.h \
	$(wildcard $(srcdir)/../include/tessera/*.h)
