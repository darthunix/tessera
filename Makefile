SUBDIRS = bridge kernels runtime nodes test
PG_CONFIG ?= pg_config
CARGO ?= cargo

.PHONY: all clean install installcheck $(SUBDIRS) \
	rust rust-release rust-check rust-clean

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

rust:
	$(CARGO) build --workspace --locked

rust-release:
	$(CARGO) build --workspace --locked --release

rust-check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings
	$(CARGO) test --workspace --locked
	$(CARGO) test --workspace --locked --release

rust-clean:
	$(CARGO) clean
