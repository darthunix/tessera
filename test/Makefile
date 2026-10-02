MODULES = tessera_batch_test tessera_binding_test tessera_bridge_test \
	tessera_builder_test tessera_consumer_test tessera_expr_test \
	tessera_function_test tessera_heap_batch_test tessera_heap_deform_test \
	tessera_input_test tessera_interrupt_test tessera_kernel_ops_test tessera_kernels_test \
	tessera_kernels_module_test \
	tessera_node_test tessera_output_test tessera_pack_test \
	tessera_plan_test tessera_planner_test tessera_producer_test \
	tessera_project_test \
	tessera_request_test tessera_row_mask_test tessera_rows_test tessera_shared_stats_test \
	tessera_source_test tessera_spill_test tessera_table_test tessera_transfer_test \
	tessera_unary_test
REGRESS = agg batch binding bridge builder coverage expr filter forward function heap_batch \
	heap_deform index input interrupts join kernel_ops kernels kernels_module limit modules node output \
	pack parallel plan planner project project_provider request row_mask rows settings shared_stats sort \
	source spill table transfer types union unary
# The suites use ICU collations, which need a UTF-8 database, and expect
# the plans of a database whose collation is not C (where a LIKE prefix
# makes no index range): a temporary instance takes the environment's
# locale, C.UTF-8 or another UTF-8 one.
REGRESS_OPTS = --dlpath=$(CURDIR) --encoding=UTF8
PG_CPPFLAGS = -I$(srcdir)/../include
NO_INSTALL = 1

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# Every module is rebuilt when a public header changes.
$(MODULES:%=%.o): $(wildcard $(srcdir)/../include/tessera/*.h)

# The tests below link the Rust static library of the kernels or the
# runtime library: the module rule of PGXS for the platform takes
# LDFLAGS_SL after the object, so the library joins it for those targets.
include $(srcdir)/../rust.mk
RUNTIME_LIB = $(srcdir)/../runtime/libtessera_runtime.a
$(RUNTIME_LIB):
	$(MAKE) -C $(srcdir)/../runtime PG_CONFIG="$(PG_CONFIG)"

RUST_TESTS = tessera_kernels_test tessera_table_test \
	tessera_kernel_ops_test tessera_spill_test tessera_rows_test
RUNTIME_TESTS = tessera_builder_test tessera_output_test tessera_input_test \
	tessera_plan_test tessera_planner_test tessera_pack_test tessera_unary_test \
	tessera_expr_test tessera_heap_batch_test tessera_project_test \
	tessera_shared_stats_test \
	tessera_kernel_ops_test tessera_spill_test tessera_rows_test
$(RUST_TESTS:%=%$(DLSUFFIX)): $(RUST_LIB) $(RUST_PROFILE_STAMP)
$(RUST_TESTS:%=%$(DLSUFFIX)): LDFLAGS_SL += $(RUST_LIB)
$(RUNTIME_TESTS:%=%$(DLSUFFIX)): $(RUNTIME_LIB)
$(RUNTIME_TESTS:%=%$(DLSUFFIX)): LDFLAGS_SL += $(RUNTIME_LIB)
