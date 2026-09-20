# Runtime helpers for batch nodes, as a static library that node modules
# and tests link. Built with the PostgreSQL flags of PG_CONFIG.
PG_CPPFLAGS = -I$(srcdir)/../include

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

RUNTIME_LIB = libtessera_runtime.a
OBJS = api.o builder.o input.o output.o plan.o planner.o unary.o
override CFLAGS += $(CFLAGS_SL) $(CFLAGS_SL_MODULE)
EXTRA_CLEAN = $(RUNTIME_LIB) $(OBJS)

all: $(RUNTIME_LIB)

$(RUNTIME_LIB): $(OBJS)
	rm -f $@
	$(AR) $(AROPT) $@ $^

$(OBJS): $(wildcard $(srcdir)/../include/tessera/*.h)

install: install-runtime

.PHONY: install-runtime
install-runtime: $(RUNTIME_LIB)
	$(MKDIR_P) '$(DESTDIR)$(pkglibdir)'
	$(INSTALL_STLIB) $(RUNTIME_LIB) '$(DESTDIR)$(pkglibdir)/'

uninstall: uninstall-runtime

.PHONY: uninstall-runtime
uninstall-runtime:
	rm -f '$(DESTDIR)$(pkglibdir)/$(RUNTIME_LIB)'
