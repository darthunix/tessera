\echo Use "CREATE EXTENSION tessera_parquet" to load this file. \quit

CREATE FUNCTION tessera_parquet_handler() RETURNS fdw_handler
    AS 'MODULE_PATHNAME' LANGUAGE C STRICT;

CREATE FUNCTION tessera_parquet_validator(text[], oid) RETURNS void
    AS 'MODULE_PATHNAME' LANGUAGE C STRICT;

CREATE FOREIGN DATA WRAPPER tessera_parquet
    HANDLER tessera_parquet_handler
    VALIDATOR tessera_parquet_validator;
