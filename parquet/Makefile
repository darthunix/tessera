MODULE_big = tessera_parquet
OBJS = fdw.o files.o options.o
PG_CPPFLAGS = -I$(srcdir)/../include

EXTENSION = tessera_parquet
DATA = tessera_parquet--0.1.sql
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The module links the reader of Parquet files statically.
include $(srcdir)/../rust.mk
SHLIB_LINK += $(RUST_PARQUET_LIB) $(RUST_PARQUET_SYSTEM_LIBS)
$(shlib): $(RUST_PARQUET_LIB) $(RUST_PROFILE_STAMP)

$(OBJS): $(srcdir)/tessera_parquet.h $(srcdir)/tessera_parquet_reader.h
