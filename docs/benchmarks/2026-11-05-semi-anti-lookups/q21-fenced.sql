select s_name, count(*) as numwait
from (
	select l1.l_orderkey, l1.l_suppkey, s_name
	from supplier, lineitem l1, nation
	where s_suppkey = l1.l_suppkey
		and l1.l_receiptdate > l1.l_commitdate
		and s_nationkey = n_nationkey
		and n_name = 'SAUDI ARABIA'
		and not exists (
			select * from lineitem l3
			where l3.l_orderkey = l1.l_orderkey
				and l3.l_suppkey <> l1.l_suppkey
				and l3.l_receiptdate > l3.l_commitdate)
	offset 0
) l1, orders
where o_orderkey = l1.l_orderkey
	and o_orderstatus = 'F'
	and exists (
		select * from lineitem l2
		where l2.l_orderkey = l1.l_orderkey
			and l2.l_suppkey <> l1.l_suppkey)
group by s_name
order by numwait desc, s_name
limit 100;
