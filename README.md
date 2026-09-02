# Tessera

A tessera is one of the small tiles used to make an ancient mosaic. This
project follows the same idea: small, independently tested modules combine to
form a complete batch execution engine for PostgreSQL.

Tessera will provide a bridge between extensions, a batch runtime,
computational kernels, reusable spill support, executor nodes, and example
data sources. Together, these modules are intended to make batch executors
composable in the same spirit as DataFusion.

Tessera currently targets PostgreSQL master and is not ready for production
use.
