# A module outside the tree builds against the installed Tessera alone: every
# installed header compiles by itself, with the include path the
# documentation gives, and TessLimit's sources (nodes/limit*.c), which take
# the public headers and the runtime library alone, build into a module of
# their own from copies against the installed headers and runtime library.
# Run after make install.
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

TESSERA_INCLUDE = $(includedir_server)/extension
INSTALLED_HEADERS = $(sort $(wildcard $(TESSERA_INCLUDE)/tessera/*.h))
NODES = $(srcdir)/../../nodes
WORK = installed-work

.PHONY: installcheck installcheck-headers installcheck-limit clean-installed

installcheck: installcheck-headers installcheck-limit

installcheck-headers:
	@test -n "$(INSTALLED_HEADERS)" || { echo "no Tessera headers in $(TESSERA_INCLUDE)/tessera"; exit 1; }
	@for header in $(INSTALLED_HEADERS); do \
		name=tessera/$$(basename $$header); \
		printf '#include "postgres.h"\n#include "%s"\n' "$$name" | \
		$(CC) $(CPPFLAGS) $(CFLAGS) -I$(includedir_server) -I$(TESSERA_INCLUDE) \
			-fsyntax-only -x c - || { echo "$$name does not compile alone"; exit 1; }; \
	done
	@echo "$(words $(INSTALLED_HEADERS)) installed headers compile alone"

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

clean-installed:
	rm -rf $(WORK)
