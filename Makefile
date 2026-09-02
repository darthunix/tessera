SUBDIRS = bridge test
PG_CONFIG ?= pg_config

.PHONY: all clean install installcheck $(SUBDIRS)

all: $(SUBDIRS)

$(SUBDIRS):
	$(MAKE) -C $@ PG_CONFIG="$(PG_CONFIG)"

clean:
	@for dir in $(SUBDIRS); do \
		$(MAKE) -C $$dir PG_CONFIG="$(PG_CONFIG)" clean || exit; \
	done

install:
	$(MAKE) -C bridge PG_CONFIG="$(PG_CONFIG)" install

installcheck: all
	$(MAKE) -C test PG_CONFIG="$(PG_CONFIG)" installcheck
