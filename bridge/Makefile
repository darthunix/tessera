MODULE_big = tessera
OBJS = binding.o bridge.o source.o
PG_CPPFLAGS = -I$(srcdir)/../include

EXTENSION = tessera
DATA = tessera--0.1.sql
HEADERS = \
	../include/tessera/abi.h \
	../include/tessera/batch.h \
	../include/tessera/binding.h \
	../include/tessera/bridge.h \
	../include/tessera/layout.h \
	../include/tessera/request.h \
	../include/tessera/row_mask.h \
	../include/tessera/source.h
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

$(OBJS): $(srcdir)/internal.h \
	$(wildcard $(srcdir)/../include/tessera/*.h)
