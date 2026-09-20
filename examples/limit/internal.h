/* Definitions shared within the TessLimit example module. */
#ifndef TESSERA_LIMIT_INTERNAL_H
#define TESSERA_LIMIT_INTERNAL_H

#include "tessera/node.h"
#include "tessera/planner.h"

#define TESS_LIMIT_NODE_NAME "tessera.limit"

extern const TessNode tess_limit_node;
extern const CustomScanMethods tess_limit_scan_methods;

extern void tess_limit_planner_init(void);
extern Node *tess_limit_create_state(CustomScan *cscan);

#endif							/* TESSERA_LIMIT_INTERNAL_H */
