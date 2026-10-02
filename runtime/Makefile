# Runtime helpers for batch nodes, as a static library that node modules
# and tests link. Built with the PostgreSQL flags of PG_CONFIG.
PG_CPPFLAGS = -I$(srcdir)/../include

# Set before PGXS is read: its clean rule takes EXTRA_CLEAN as it is read,
# and a later definition never reaches it. The list is not OBJS, which PGXS
# would take for a module's objects and, with LLVM, build bitcode for.
RUNTIME_LIB = libtessera_runtime.a
RUNTIME_OBJS = api.o builder.o expr.o heap_batch.o input.o node.o output.o plan.o planner.o \
	project.o qual.o rows.o shared_stats.o spill.o unary.o
EXTRA_CLEAN = $(RUNTIME_LIB) $(RUNTIME_OBJS)

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

override CFLAGS += $(CFLAGS_SL) $(CFLAGS_SL_MODULE)

all: $(RUNTIME_LIB)

$(RUNTIME_LIB): $(RUNTIME_OBJS)
	rm -f $@
	$(AR) $(AROPT) $@ $^

$(RUNTIME_OBJS): $(wildcard $(srcdir)/../include/tessera/*.h)

install: install-runtime

.PHONY: install-runtime
install-runtime: $(RUNTIME_LIB)
	$(MKDIR_P) '$(DESTDIR)$(pkglibdir)'
	$(INSTALL_STLIB) $(RUNTIME_LIB) '$(DESTDIR)$(pkglibdir)/'

uninstall: uninstall-runtime

.PHONY: uninstall-runtime
uninstall-runtime:
	rm -f '$(DESTDIR)$(pkglibdir)/$(RUNTIME_LIB)'
