/*
 * Greengage 7 has no injection points: a point does nothing, as it does
 * in a PostgreSQL build without USE_INJECTION_POINTS.
 */
#define INJECTION_POINT(name, arg) ((void) 0)
