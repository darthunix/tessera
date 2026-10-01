MODULE_big = tessera
OBJS = binding.o bridge.o function.o kernel.o node.o registry.o source.o
PG_CPPFLAGS = -I$(srcdir)/../include

EXTENSION = tessera
DATA = tessera--0.1.sql
# Every header of the contract and the runtime, which include one another:
# a module outside the tree builds against the installed set alone
# (test/installed checks it).
HEADERS = $(sort $(wildcard ../include/tessera/*.h))
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

$(OBJS): $(srcdir)/internal.h \
	$(wildcard $(srcdir)/../include/tessera/*.h)
