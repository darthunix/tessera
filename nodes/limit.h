/*
 * TessLimit, the node of LIMIT and OFFSET over a batch child. Its files,
 * limit.c and limit_planner.c, take the public headers and the runtime
 * library alone, as a node of another extension would (the node-writing
 * guide's example; test/installed builds them outside the tree).
 */
#ifndef TESSERA_NODES_LIMIT_H
#define TESSERA_NODES_LIMIT_H

#include "tessera/node.h"
#include "tessera/planner.h"

#define TESS_LIMIT_NODE_NAME "tessera.limit"

extern const TessNode tess_limit_node;
extern const CustomScanMethods tess_limit_scan_methods;

extern void tess_limit_planner_init(void);
extern Node *tess_limit_create_state(CustomScan *cscan);

#endif							/* TESSERA_NODES_LIMIT_H */
