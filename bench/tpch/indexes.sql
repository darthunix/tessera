-- The second variant of the schema (--schema indexed): indexes on foreign
-- keys and dates, which the specification allows (one table's foreign key
-- or single date column). They change the plans of Q17 and Q20, whose
-- correlated subqueries otherwise scan lineitem once per outer row, and
-- bring in the index scans of Tessera. The foreign keys that a primary
-- key already leads (l_orderkey, ps_partkey) need none of their own.
CREATE INDEX IF NOT EXISTS lineitem_part_supp ON lineitem (l_partkey, l_suppkey);
CREATE INDEX IF NOT EXISTS lineitem_supp ON lineitem (l_suppkey);
CREATE INDEX IF NOT EXISTS lineitem_shipdate ON lineitem (l_shipdate);
CREATE INDEX IF NOT EXISTS orders_cust ON orders (o_custkey);
CREATE INDEX IF NOT EXISTS orders_date ON orders (o_orderdate);
CREATE INDEX IF NOT EXISTS partsupp_supp ON partsupp (ps_suppkey);
CREATE INDEX IF NOT EXISTS customer_nation ON customer (c_nationkey);
CREATE INDEX IF NOT EXISTS supplier_nation ON supplier (s_nationkey);
CREATE INDEX IF NOT EXISTS nation_region ON nation (n_regionkey);
