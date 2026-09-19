# The Rust static library of the kernels, built by Cargo. Cargo decides
# whether a rebuild is needed, so the rule always runs; RUST_PROFILE selects
# the debug or the release library, and both must pass the tests.
CARGO ?= cargo
RUST_PROFILE ?= release
RUST_LIB = $(srcdir)/../target/$(RUST_PROFILE)/libtessera_capi.a
ifeq ($(RUST_PROFILE),release)
RUST_BUILD_FLAGS = --release
endif

.PHONY: $(RUST_LIB)
$(RUST_LIB):
	cd $(srcdir)/.. && $(CARGO) build -p tessera-capi --locked $(RUST_BUILD_FLAGS)
