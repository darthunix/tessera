MODULES = tessera_bridge_test tessera_row_mask_test
REGRESS = bridge row_mask
REGRESS_OPTS = --dlpath=$(CURDIR)
PG_CPPFLAGS = -I$(srcdir)/../include
NO_INSTALL = 1

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)
