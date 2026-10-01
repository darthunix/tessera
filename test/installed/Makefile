# A module outside the tree builds against the installed Tessera alone: every
# installed header compiles by itself, with the include path the
# documentation gives, and the TessLimit example builds from a copy of its
# sources against the installed headers and runtime library. Run after
# make install.
PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

TESSERA_INCLUDE = $(includedir_server)/extension
INSTALLED_HEADERS = $(sort $(wildcard $(TESSERA_INCLUDE)/tessera/*.h))
EXAMPLE = $(srcdir)/../../examples/limit
WORK = installed-work

.PHONY: installcheck installcheck-headers installcheck-example clean-installed

installcheck: installcheck-headers installcheck-example

installcheck-headers:
	@test -n "$(INSTALLED_HEADERS)" || { echo "no Tessera headers in $(TESSERA_INCLUDE)/tessera"; exit 1; }
	@for header in $(INSTALLED_HEADERS); do \
		name=tessera/$$(basename $$header); \
		printf '#include "postgres.h"\n#include "%s"\n' "$$name" | \
		$(CC) $(CPPFLAGS) $(CFLAGS) -I$(includedir_server) -I$(TESSERA_INCLUDE) \
			-fsyntax-only -x c - || { echo "$$name does not compile alone"; exit 1; }; \
	done
	@echo "$(words $(INSTALLED_HEADERS)) installed headers compile alone"

installcheck-example:
	rm -rf $(WORK) && mkdir $(WORK)
	cp $(EXAMPLE)/Makefile $(EXAMPLE)/*.c $(EXAMPLE)/*.h $(WORK)/
	$(MAKE) -C $(WORK) PG_CONFIG="$(PG_CONFIG)" TESSERA_INSTALLED=1
	rm -rf $(WORK)

clean-installed:
	rm -rf $(WORK)
