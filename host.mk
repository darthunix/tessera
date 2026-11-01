# The core the modules build against, told by what it has
# (include/tessera/compat.h): the release of PostgreSQL it is or forks,
# and whether it is a fork of the Greenplum family, which answers
# `pg_config --gp_version`.
TESS_PG_MAJOR := $(shell $(PG_CONFIG) --version | sed -E 's/^[^0-9]*([0-9]+).*/\1/')
TESS_MPP := $(if $(shell $(PG_CONFIG) --gp_version 2>/dev/null),1)
# The Greenplum family writes varlena headers in network byte order on
# every machine; the kernels are built to read them so (rust.mk), and
# check at load that the core writes them as they read (kernels/int.c).
TESS_NETWORK_VARLENA := $(TESS_MPP)
# The headers a release added, for an older core: include/compat/pgN for
# every N after the core's release. compat.h comes first in every file.
TESS_COMPAT_RELEASES := $(foreach n,13 14 15 16 17 18 19 20,$(if $(shell test $(n) -gt $(TESS_PG_MAJOR) && echo 1),$(n)))
TESS_HOST_CPPFLAGS = $(foreach n,$(TESS_COMPAT_RELEASES),$(if $(wildcard $(srcdir)/../include/compat/pg$(n)),-I$(srcdir)/../include/compat/pg$(n))) \
	-include $(srcdir)/../include/tessera/compat.h \
	-DTESS_NETWORK_VARLENA=$(if $(TESS_NETWORK_VARLENA),1,0)
