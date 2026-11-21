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

# The reader of Parquet files, which the module tessera_parquet links, and
# its writer of test files, which the suite's test module links.
RUST_PARQUET_LIB = $(srcdir)/../target/$(RUST_PROFILE)/libtessera_parquet.a
RUST_PARQUET_TESTING_LIB = $(srcdir)/../target/$(RUST_PROFILE)/libtessera_parquet_testing.a
$(RUST_PARQUET_LIB): FORCE
	cd $(srcdir)/.. && $(CARGO) build -p tessera-parquet --locked $(RUST_BUILD_FLAGS)
$(RUST_PARQUET_TESTING_LIB): FORCE
	cd $(srcdir)/.. && $(CARGO) build -p tessera-parquet-testing --locked $(RUST_BUILD_FLAGS)
# What the reader needs of the system: Apache Arrow's time zones read the
# system's through CoreFoundation on macOS.
ifeq ($(PORTNAME),darwin)
RUST_PARQUET_SYSTEM_LIBS = -framework CoreFoundation
endif

# The profile the modules link: a module that links the library depends
# on this file too, which changes only when the profile does, so switching
# the profile links the modules again even when the other profile's library
# is older than them.
RUST_PROFILE_STAMP = $(srcdir)/../target/tessera-rust-profile
$(RUST_PROFILE_STAMP): FORCE
	@mkdir -p $(@D)
	@echo $(RUST_PROFILE) | cmp -s - $@ 2>/dev/null || echo $(RUST_PROFILE) > $@

# PostgreSQL's Makefile.global declares .SECONDARY: with no prerequisites,
# under which a missing prerequisite without a recipe forces nothing; a
# phony one does, so Cargo runs on every build and decides itself.
.PHONY: FORCE
FORCE:
