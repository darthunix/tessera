MODULE_big = tessera
OBJS = bridge.o
PG_CPPFLAGS = -I$(srcdir)/../include

EXTENSION = tessera
DATA = tessera--0.1.sql
HEADERS = \
	../include/tessera/abi.h \
	../include/tessera/bridge.h \
	../include/tessera/row_mask.h
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

$(OBJS): $(wildcard $(srcdir)/../include/tessera/*.h)
