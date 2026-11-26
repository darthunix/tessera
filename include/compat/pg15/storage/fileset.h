/*
 * Before PostgreSQL 15 a core has no FileSet: its shared files are
 * SharedFileSet, used only by PostgreSQL parallel queries, which the
 * forks of that age Tessera builds for do not run.
 */
