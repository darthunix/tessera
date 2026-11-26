# The core the modules build against. Greengage 7 answers
# `pg_config --gp_version`; PostgreSQL does not. On Greengage the
# headers the core lacks come from include/compat/greengage.
TESS_GREENGAGE := $(if $(shell $(PG_CONFIG) --gp_version 2>/dev/null),1)
ifdef TESS_GREENGAGE
TESS_HOST_CPPFLAGS = -I$(srcdir)/../include/compat/greengage \
	-include $(srcdir)/../include/tessera/compat.h
endif
