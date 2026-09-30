-- Loading and settings (plan 4.24, review items 9 and 18). Nothing of
-- Tessera is preloaded here, so the modules load in this session's order.
SELECT current_setting('shared_preload_libraries') !~ 'tessera'
   AND current_setting('session_preload_libraries') !~ 'tessera' AS nothing_preloaded;
-- A module without the bridge: the bridge must be loaded first, and the
-- failed load leaves nothing behind, so it can be loaded again after it.
LOAD 'tessera_nodes';
LOAD 'tessera_limit';
LOAD 'tessera_kernels';
-- Settings given before the modules that define them keep their values:
-- the prefix is reserved only once tessera_nodes has defined every
-- tessera.* setting, and then a name no module defines goes with a WARNING.
SET tessera.gather_tuple_share = 0.5;
SET tessera.misspelt = 1;
LOAD 'tessera';
SHOW tessera.enable;
SET tessera.scan_cost_factor = 0.7;
LOAD 'tessera_nodes';
SHOW tessera.gather_tuple_share;
SHOW tessera.scan_cost_factor;
-- The prefix is reserved now.
SET tessera.misspelt_again = 1;
LOAD 'tessera_limit';
LOAD 'tessera_kernels';
