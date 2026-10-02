SUBDIRS = bridge kernels runtime nodes test
PG_CONFIG ?= pg_config
CARGO ?= cargo

.PHONY: all clean install installcheck $(SUBDIRS) \
	rust rust-release rust-check rust-mutants rust-loom rust-clean \
	tpch tpch-check tpch-stop

all: $(SUBDIRS)

$(SUBDIRS):
	$(MAKE) -C $@ PG_CONFIG="$(PG_CONFIG)"

# Modules link the runtime archive and the tests link it and the Rust
# library, so a parallel make must build those first.
nodes: runtime
test: runtime kernels nodes

clean:
	@for dir in $(SUBDIRS); do \
		$(MAKE) -C $$dir PG_CONFIG="$(PG_CONFIG)" clean || exit; \
	done

install:
	$(MAKE) -C bridge PG_CONFIG="$(PG_CONFIG)" install
	$(MAKE) -C kernels PG_CONFIG="$(PG_CONFIG)" install
	$(MAKE) -C runtime PG_CONFIG="$(PG_CONFIG)" install
	$(MAKE) -C nodes PG_CONFIG="$(PG_CONFIG)" install

installcheck: all
	$(MAKE) -C test PG_CONFIG="$(PG_CONFIG)" installcheck
	$(MAKE) -C test/installed PG_CONFIG="$(PG_CONFIG)" installcheck

rust:
	$(CARGO) build --workspace --locked

rust-release:
	$(CARGO) build --workspace --locked --release

rust-check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings
	$(CARGO) test --workspace --locked
	$(CARGO) test --workspace --locked --release

# Every mutant of the Rust crates (.cargo/mutants.toml): hours; CI runs
# the mutants of a pull request's changed lines, and all of them on request.
rust-mutants:
	$(CARGO) mutants --workspace --jobs 4

# The loom model of the hash table's concurrent protocol; a target
# directory of its own, since --cfg loom rebuilds every crate.
rust-loom:
	RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 CARGO_TARGET_DIR=target/loom \
		$(CARGO) test -p tessera-kernels --lib --release --locked table::loom

rust-clean:
	$(CARGO) clean

# Queries derived from TPC-H on a cluster of their own, with Tessera on and
# off (bench/tpch/README.md). TPCH_SF is the scale factor; TPCH_FLAGS adds
# flags, such as --queries core or --workers 2.
TPCH_SF ?= 1
TPCH = PG_CONFIG="$(PG_CONFIG)" $(CARGO) run --release --locked -p tessera-tpch --

tpch:
	$(TPCH) run --sf $(TPCH_SF) $(TPCH_FLAGS)

tpch-check:
	$(TPCH) check --sf $(TPCH_SF) $(TPCH_FLAGS)

tpch-stop:
	$(TPCH) stop --sf $(TPCH_SF)
