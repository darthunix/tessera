MODULES = tessera_batch_test tessera_binding_test tessera_bridge_test \
	tessera_builder_test tessera_consumer_test tessera_expr_test \
	tessera_function_test tessera_heap_batch_test tessera_heap_deform_test \
	tessera_input_test tessera_kernel_ops_test tessera_kernels_test \
	tessera_kernels_module_test \
	tessera_node_test tessera_output_test tessera_pack_test \
	tessera_plan_test tessera_planner_test tessera_producer_test \
	tessera_project_test \
	tessera_request_test tessera_row_mask_test tessera_rows_test tessera_shared_stats_test \
	tessera_source_test tessera_spill_test tessera_table_test tessera_transfer_test \
	tessera_unary_test
REGRESS = agg batch binding bridge builder coverage expr filter forward function heap_batch \
	heap_deform index input join kernel_ops kernels kernels_module limit modules node output \
	pack parallel plan planner project project_provider request row_mask rows settings shared_stats sort \
	source spill table transfer types union unary
REGRESS_OPTS = --dlpath=$(CURDIR)
PG_CPPFLAGS = -I$(srcdir)/../include
NO_INSTALL = 1

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# Every module is rebuilt when a public header changes.
$(MODULES:%=%.o): $(wildcard $(srcdir)/../include/tessera/*.h)

# The kernels test links the Rust static library directly.
include $(srcdir)/../rust.mk

# The module rule of src/makefiles/Makefile.darwin with the library added.
tessera_kernels_test$(DLSUFFIX): tessera_kernels_test.o $(RUST_LIB)
	$(CC) $(CFLAGS) $< $(RUST_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_table_test$(DLSUFFIX): tessera_table_test.o $(RUST_LIB)
	$(CC) $(CFLAGS) $< $(RUST_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@

# The builder test links the runtime library the same way.
RUNTIME_LIB = $(srcdir)/../runtime/libtessera_runtime.a
$(RUNTIME_LIB):
	$(MAKE) -C $(srcdir)/../runtime PG_CONFIG="$(PG_CONFIG)"
tessera_builder_test$(DLSUFFIX): tessera_builder_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_output_test$(DLSUFFIX): tessera_output_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_input_test$(DLSUFFIX): tessera_input_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_plan_test$(DLSUFFIX): tessera_plan_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_planner_test$(DLSUFFIX): tessera_planner_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_pack_test$(DLSUFFIX): tessera_pack_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_unary_test$(DLSUFFIX): tessera_unary_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_expr_test$(DLSUFFIX): tessera_expr_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_heap_batch_test$(DLSUFFIX): tessera_heap_batch_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_project_test$(DLSUFFIX): tessera_project_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_shared_stats_test$(DLSUFFIX): tessera_shared_stats_test.o $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@

# The kernel registry test links both: the kernels it installs and the
# runtime that reads them back.
tessera_kernel_ops_test$(DLSUFFIX): tessera_kernel_ops_test.o $(RUST_LIB) $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUST_LIB) $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_spill_test$(DLSUFFIX): tessera_spill_test.o $(RUST_LIB) $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUST_LIB) $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
tessera_rows_test$(DLSUFFIX): tessera_rows_test.o $(RUST_LIB) $(RUNTIME_LIB)
	$(CC) $(CFLAGS) $< $(RUST_LIB) $(RUNTIME_LIB) $(LDFLAGS) $(LDFLAGS_SL) -bundle $(BE_DLLLIBS) -o $@
