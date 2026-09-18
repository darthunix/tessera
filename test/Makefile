MODULES = tessera_batch_test tessera_binding_test tessera_bridge_test \
	tessera_consumer_test tessera_kernels_test tessera_node_test \
	tessera_producer_test tessera_request_test tessera_row_mask_test \
	tessera_source_test tessera_transfer_test
REGRESS = batch binding bridge kernels modules node request row_mask source \
	transfer
REGRESS_OPTS = --dlpath=$(CURDIR)
PG_CPPFLAGS = -I$(srcdir)/../include
NO_INSTALL = 1

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# The kernels test links the Rust static library. Cargo decides whether the
# library needs rebuilding, so its rule always runs; the profile selects the
# debug or the release library, and both must pass.
CARGO ?= cargo
RUST_PROFILE ?= release
RUST_LIB = $(srcdir)/../target/$(RUST_PROFILE)/libtessera_capi.a
ifeq ($(RUST_PROFILE),release)
RUST_BUILD_FLAGS = --release
endif

.PHONY: $(RUST_LIB)
$(RUST_LIB):
	cd $(srcdir)/.. && $(CARGO) build -p tessera-capi --locked $(RUST_BUILD_FLAGS)

# The module rule of src/makefiles/Makefile.darwin with the library added.
tessera_kernels_test$(DLSUFFIX): tessera_kernels_test.o $(RUST_LIB)
	$(CC) $(CFLAGS) $< $(RUST_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
