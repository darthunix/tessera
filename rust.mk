# The Rust static library of the kernels, built by Cargo. Cargo decides
# whether a rebuild is needed, so the rule always runs, and a module that
# links the library is linked again only when Cargo changed it; RUST_PROFILE
# selects the debug or the release library, and both must pass the tests.
CARGO ?= cargo
RUST_PROFILE ?= release
RUST_LIB = $(srcdir)/../target/$(RUST_PROFILE)/libtessera_capi.a
ifeq ($(RUST_PROFILE),release)
RUST_BUILD_FLAGS = --release
endif

$(RUST_LIB): FORCE
	cd $(srcdir)/.. && $(CARGO) build -p tessera-capi --locked $(RUST_BUILD_FLAGS)

FORCE:
