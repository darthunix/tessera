/* Definitions shared within the Tessera nodes module. */
#ifndef TESSERA_NODES_INTERNAL_H
#define TESSERA_NODES_INTERNAL_H

#include "tessera/node.h"
#include "tessera/planner.h"

#define TESS_FILTER_NODE_NAME "tessera.filter"

extern const TessNode tess_pack_node;
extern const CustomScanMethods tess_pack_scan_methods;

extern const TessNode tess_filter_node;
extern const CustomScanMethods tess_filter_scan_methods;
extern Node *tess_filter_create_state(CustomScan *cscan);
extern void tess_filter_planner_init(void);

#endif							/* TESSERA_NODES_INTERNAL_H */
