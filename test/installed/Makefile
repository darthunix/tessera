# A module outside the tree builds against the installed Tessera alone: every
# installed header compiles by itself, with the include path the
# documentation gives, the layouts the specs draw hold (layouts.c), and
# TessLimit's sources (nodes/limit*.c), the
# test source of the suite source_scan (test/tessera_scan_source_test.c)
# and the Parquet wrapper (parquet/), which take the public headers and the
# runtime library alone, each build into a module of their own from copies
# against the installed headers and runtime library, the wrapper with the
# static library of its reader too. Run after make install.
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

TESSERA_INCLUDE = $(includedir_server)/extension
INSTALLED_HEADERS = $(sort $(wildcard $(TESSERA_INCLUDE)/tessera/*.h))
NODES = $(srcdir)/../../nodes
PARQUET = $(srcdir)/../../parquet
WORK = installed-work
SOURCE_WORK = installed-work-source
PARQUET_WORK = installed-work-parquet
# The reader of Parquet files, a static library of the workspace, and what
# it needs of the system, as rust.mk names them.
RUST_PROFILE ?= release
PARQUET_READER = $(abspath $(srcdir)/../../target/$(RUST_PROFILE)/libtessera_parquet.a)
ifeq ($(PORTNAME),darwin)
PARQUET_SYSTEM_LIBS = -framework CoreFoundation
endif

.PHONY: installcheck installcheck-headers installcheck-layouts installcheck-limit \
	installcheck-source installcheck-parquet clean-installed

installcheck: installcheck-headers installcheck-layouts installcheck-limit installcheck-source \
	installcheck-parquet

installcheck-headers:
	@test -n "$(INSTALLED_HEADERS)" || { echo "no Tessera headers in $(TESSERA_INCLUDE)/tessera"; exit 1; }
	@for header in $(INSTALLED_HEADERS); do \
		name=tessera/$$(basename $$header); \
		printf '#include "postgres.h"\n#include "%s"\n' "$$name" | \
		$(CC) $(CPPFLAGS) $(CFLAGS) -I$(includedir_server) -I$(TESSERA_INCLUDE) \
			-fsyntax-only -x c - || { echo "$$name does not compile alone"; exit 1; }; \
	done
	@echo "$(words $(INSTALLED_HEADERS)) installed headers compile alone"

# The offsets the specs draw of the structures one module gives another.
installcheck-layouts:
	$(CC) $(CPPFLAGS) $(CFLAGS) -I$(includedir_server) -I$(TESSERA_INCLUDE) \
		-fsyntax-only $(srcdir)/layouts.c
	@echo "the installed layouts are those of the specs"

installcheck-limit:
	rm -rf $(WORK) && mkdir $(WORK)
	cp $(NODES)/limit.c $(NODES)/limit_planner.c $(NODES)/limit.h $(srcdir)/limit_module.c $(WORK)/
	printf '%s\n' 'MODULE_big = tessera_limit_check' \
		'OBJS = limit_module.o limit.o limit_planner.o' \
		'PG_CPPFLAGS = -I$$(includedir_server)/extension' \
		'SHLIB_LINK += $$(pkglibdir)/libtessera_runtime.a' \
		'PGXS := $$(shell $$(PG_CONFIG) --pgxs)' \
		'include $$(PGXS)' > $(WORK)/Makefile
	$(MAKE) -C $(WORK) PG_CONFIG="$(PG_CONFIG)"
	rm -rf $(WORK)

installcheck-source:
	rm -rf $(SOURCE_WORK) && mkdir $(SOURCE_WORK)
	cp $(srcdir)/../tessera_scan_source_test.c $(SOURCE_WORK)/
	printf '%s\n' 'MODULE_big = tessera_scan_source_check' \
		'OBJS = tessera_scan_source_test.o' \
		'PG_CPPFLAGS = -I$$(includedir_server)/extension' \
		'SHLIB_LINK += $$(pkglibdir)/libtessera_runtime.a' \
		'PGXS := $$(shell $$(PG_CONFIG) --pgxs)' \
		'include $$(PGXS)' > $(SOURCE_WORK)/Makefile
	$(MAKE) -C $(SOURCE_WORK) PG_CONFIG="$(PG_CONFIG)"
	rm -rf $(SOURCE_WORK)

installcheck-parquet:
	rm -rf $(PARQUET_WORK) && mkdir $(PARQUET_WORK)
	cp $(PARQUET)/*.c $(PARQUET)/*.h $(PARQUET_WORK)/
	printf '%s\n' 'MODULE_big = tessera_parquet_check' \
		'OBJS = $(patsubst %.c,%.o,$(notdir $(wildcard $(PARQUET)/*.c)))' \
		'PG_CPPFLAGS = -I$$(includedir_server)/extension' \
		'SHLIB_LINK += $$(pkglibdir)/libtessera_runtime.a $(PARQUET_READER) $(PARQUET_SYSTEM_LIBS)' \
		'PGXS := $$(shell $$(PG_CONFIG) --pgxs)' \
		'include $$(PGXS)' > $(PARQUET_WORK)/Makefile
	$(MAKE) -C $(PARQUET_WORK) PG_CONFIG="$(PG_CONFIG)"
	rm -rf $(PARQUET_WORK)

clean-installed:
	rm -rf $(WORK) $(SOURCE_WORK) $(PARQUET_WORK)
