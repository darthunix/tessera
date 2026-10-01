# Plans of the cases the README cites

From the run of 2026-10-01: `EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)`
of each statement as it was measured, with `tessera.enable` on (Tessera) and off (PostgreSQL).

## filter and count over 2 M rows
Family `win`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=1.00 loops=1)
   Output: (count(*))
   Input Batches: 22058
   Input Rows: 1000000
   Kernel Calls: 22058
   ->  Custom Scan (TessFilter) on public.bench_narrow (actual rows=1000000.00 loops=1)
         Batch Filter: ((bench_narrow.c1 <= 1000000) AND (bench_narrow.c2 > 0))
         Rows Removed by Batch Filter: 1000000
         Input Batches: 44117
         Input Rows: 2000000
         Output Rows: 1000000
         ->  Custom Scan (TessHeapScan) on public.bench_narrow (actual rows=2000000.00 loops=1)
               Output: c1, c2
               Batch Size: 64
               Batches: 44117
               Pages: 14706
               Deformed Datums: 3000000
               Restarted Datums: 0
```

PostgreSQL:

```
 Aggregate (actual rows=1.00 loops=1)
   Output: count(*)
   ->  Seq Scan on public.bench_narrow (actual rows=1000000.00 loops=1)
         Output: c1, c2, c3, c4, c5, c6, c7, c8
         Filter: ((bench_narrow.c1 <= 1000000) AND (bench_narrow.c2 > 0))
         Rows Removed by Filter: 1000000
```

## GROUP BY into 100 k groups over 2 M rows
Family `win`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=100001.00 loops=1)
   Output: fk, (count(*)), (max(f1))
   Group Key: bench_fact.fk
   Memory Usage: 6209 kB
   Input Batches: 42857
   Input Rows: 2000000
   Kernel Calls: 85714
   Computed Datums: 4000000
   Groups: 100001
   Table Grows: 0
   ->  Custom Scan (TessHeapScan) on public.bench_fact (actual rows=2000000.00 loops=1)
         Output: fk, f1
         Batch Size: 64
         Batches: 42857
         Pages: 14286
         Deformed Datums: 4000000
         Restarted Datums: 0
```

PostgreSQL:

```
 HashAggregate (actual rows=100001.00 loops=1)
   Output: fk, count(*), max(f1)
   Group Key: bench_fact.fk
   Batches: 1  Memory Usage: 7705kB
   ->  Seq Scan on public.bench_fact (actual rows=2000000.00 loops=1)
         Output: fk, fk8, fk_big, fk_miss, f1
```

## hash join of 2 M rows with 100 k, count
Family `join`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=1.00 loops=1)
   Output: (count(*))
   Input Batches: 42857
   Input Rows: 1846154
   Kernel Calls: 42857
   ->  Custom Scan (TessHashJoin) (actual rows=1846154.00 loops=1)
         Hash Cond: (f.fk = d.id)
         Buckets: 262144
         Memory Usage: 4169 kB
         Builds: 1
         Build Rows: 100000
         Chunks: 4
         Probe Rows: 2000000
         Matches: 1846154
         ->  Custom Scan (TessHeapScan) on public.bench_fact f (actual rows=2000000.00 loops=1)
               Output: f.fk
               Batch Size: 64
               Batches: 42857
               Pages: 14286
               Deformed Datums: 2000000
               Restarted Datums: 0
         ->  Custom Scan (TessHeapScan) on public.bench_dim d (actual rows=100000.00 loops=1)
               Output: d.id
               Batch Size: 64
               Batches: 2206
               Pages: 736
               Deformed Datums: 100000
               Restarted Datums: 0
```

PostgreSQL:

```
 Aggregate (actual rows=1.00 loops=1)
   Output: count(*)
   ->  Hash Join (actual rows=1846154.00 loops=1)
         Hash Cond: (f.fk = d.id)
         ->  Seq Scan on public.bench_fact f (actual rows=2000000.00 loops=1)
               Output: f.fk, f.fk8, f.fk_big, f.fk_miss, f.f1
         ->  Hash (actual rows=100000.00 loops=1)
               Output: d.id
               Buckets: 131072  Batches: 1  Memory Usage: 4540kB
               ->  Seq Scan on public.bench_dim d (actual rows=100000.00 loops=1)
                     Output: d.id
```

## hash join of 20 M rows with 1 M spilling to disk (work_mem 4 MB)
Family `spill`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=1.00 loops=1)
   Output: (sum(d.d1))
   Input Batches: 288610
   Input Rows: 18461539
   Kernel Calls: 288606
   Computed Datums: 18461539
   ->  Custom Scan (TessHashJoin) (actual rows=18461539.00 loops=1)
         Output: d.d1
         Hash Cond: (f.fk = d.id)
         Buckets: 1024
         Memory Usage: 8187 kB
         Batches: 16
         Disk Usage: 84504 kB
         Rows Removed by Bloom Filter: 0
         Builds: 1
         Build Rows: 1000000
         Chunks: 342
         Resident Partitions: 0
         Spilled Chunks: 10271
         Tail Chunks Kept: 0
         Probe Rows: 20000000
         Matches: 18461539
         Bloom Filters: 1
         ->  Custom Scan (TessHeapScan) on public.bench_fact f (actual rows=20000000.00 loops=1)
               Output: f.fk
               Batch Size: 64
               Batches: 428572
               Pages: 142858
               Deformed Datums: 20000000
               Restarted Datums: 0
         ->  Custom Scan (TessHeapScan) on public.bench_dim d (actual rows=1000000.00 loops=1)
               Output: d.d1, d.id
               Batch Size: 64
               Batches: 22058
               Pages: 7353
               Deformed Datums: 2000000
               Restarted Datums: 0
```

PostgreSQL:

```
 Aggregate (actual rows=1.00 loops=1)
   Output: sum(d.d1)
   ->  Hash Join (actual rows=18461539.00 loops=1)
         Output: d.d1
         Hash Cond: (f.fk = d.id)
         ->  Seq Scan on public.bench_fact f (actual rows=20000000.00 loops=1)
               Output: f.fk
         ->  Hash (actual rows=1000000.00 loops=1)
               Output: d.d1, d.id
               Buckets: 262144  Batches: 8  Memory Usage: 6935kB
               ->  Seq Scan on public.bench_dim d (actual rows=1000000.00 loops=1)
                     Output: d.d1, d.id
```

## GROUP BY of 20 M rows spilling to disk (work_mem 4 MB)
Family `spill`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=1.00 loops=1)
   Output: (count(*)), (sum(g.s))
   Input Batches: 15643
   Input Rows: 1000001
   Kernel Calls: 15643
   Computed Datums: 1000001
   ->  Custom Scan (TessPack) (actual rows=1000001.00 loops=1)
         Output: g.s
         Rows Kept As: forwarded batches
         Batches: 15643
         ->  Subquery Scan on g (never executed)
               Output: g.s
               ->  Custom Scan (TessAgg) (actual rows=1000001.00 loops=1)
                     Output: bench_fact.fk, (sum(bench_fact.f1))
                     Group Key: bench_fact.fk
                     Memory Usage: 7300 kB
                     Batches: 32
                     Disk Usage: 218236 kB
                     Input Batches: 428572
                     Input Rows: 20000000
                     Kernel Calls: 428572
                     Computed Datums: 40000000
                     Groups: 1000001
                     Table Grows: 232
                     Evictions: 4332
                     Spilled Chunks: 23792
                     ->  Custom Scan (TessHeapScan) on public.bench_fact (actual rows=20000000.00 loops=1)
                           Output: bench_fact.fk, bench_fact.f1
                           Batch Size: 64
                           Batches: 428572
                           Pages: 142858
                           Deformed Datums: 40000000
                           Restarted Datums: 0
```

PostgreSQL:

```
 Aggregate (actual rows=1.00 loops=1)
   Output: count(*), sum((sum(bench_fact.f1)))
   ->  HashAggregate (actual rows=1000001.00 loops=1)
         Output: bench_fact.fk, sum(bench_fact.f1)
         Group Key: bench_fact.fk
         Batches: 17  Memory Usage: 8345kB  Disk Usage: 457168kB
         ->  Seq Scan on public.bench_fact (actual rows=20000000.00 loops=1)
               Output: bench_fact.fk, bench_fact.fk8, bench_fact.fk_big, bench_fact.fk_miss, bench_fact.f1
```

## ORDER BY of 2 M integers
Family `sort`.

Tessera:

```
 Custom Scan (TessLimit) (actual rows=0.00 loops=1)
   Output: k4, v
   Input Batches: 31250
   Input Rows: 2000000
   Output Rows: 0
   ->  Custom Scan (TessSort) (actual rows=2000000.00 loops=1)
         Output: k4, v
         Sort Key: bench_sort.k4
         Sort Method: in memory
         Memory Usage: 117714 kB
         Input Batches: 37035
         Input Rows: 2000000
         ->  Custom Scan (TessHeapScan) on public.bench_sort (actual rows=2000000.00 loops=1)
               Output: k4, v
               Batch Size: 64
               Batches: 37035
               Pages: 18518
               Deformed Datums: 4000000
               Restarted Datums: 0
```

PostgreSQL:

```
 Limit (actual rows=0.00 loops=1)
   Output: k4, v
   ->  Sort (actual rows=2000000.00 loops=1)
         Output: k4, v
         Sort Key: bench_sort.k4
         Sort Method: quicksort  Memory: 96028kB
         ->  Seq Scan on public.bench_sort (actual rows=2000000.00 loops=1)
               Output: k4, v
```

## top 10 of 2 M rows
Family `sort`.

Tessera:

```
 Custom Scan (TessLimit) (actual rows=10.00 loops=1)
   Output: k4, v
   Input Batches: 1
   Input Rows: 10
   Output Rows: 10
   ->  Custom Scan (TessSort) (actual rows=10.00 loops=1)
         Output: k4, v
         Sort Key: bench_sort.k4
         Sort Method: top-N in memory
         Memory Usage: 69 kB
         Input Batches: 37035
         Input Rows: 2000000
         ->  Custom Scan (TessHeapScan) on public.bench_sort (actual rows=2000000.00 loops=1)
               Output: k4, v
               Batch Size: 64
               Batches: 37035
               Pages: 18518
               Deformed Datums: 2000141
               Restarted Datums: 0
```

PostgreSQL:

```
 Limit (actual rows=10.00 loops=1)
   Output: k4, v
   ->  Sort (actual rows=10.00 loops=1)
         Output: k4, v
         Sort Key: bench_sort.k4
         Sort Method: top-N heapsort  Memory: 25kB
         ->  Seq Scan on public.bench_sort (actual rows=2000000.00 loops=1)
               Output: k4, v
```

## UNION ALL of two filtered scans, aggregated
Family `setop`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=1.00 loops=1)
   Output: (count(*)), (sum(s.c))
   Input Batches: 43489
   Input Rows: 2000000
   Kernel Calls: 80544
   Computed Datums: 2000000
   ->  Custom Scan (TessAppend) (actual rows=2000000.00 loops=1)
         Output: s.c
         Batches: 43489
         ->  Custom Scan (TessPack) (actual rows=1000000.00 loops=1)
               Output: unnamed_subquery.c
               Rows Kept As: forwarded batches
               Batches: 22059
               ->  Custom Scan (TessFilter) on public.bench_narrow (actual rows=1000000.00 loops=1)
                     Output: bench_narrow.c2
                     Batch Filter: (bench_narrow.c1 > 1000000)
                     Rows Removed by Batch Filter: 1000000
                     Input Batches: 44117
                     Input Rows: 2000000
                     Output Rows: 1000000
                     ->  Custom Scan (TessHeapScan) on public.bench_narrow (actual rows=2000000.00 loops=1)
                           Output: bench_narrow.c2, bench_narrow.c1
                           Batch Size: 64
                           Batches: 44117
                           Pages: 14706
                           Deformed Datums: 3000000
                           Restarted Datums: 0
         ->  Custom Scan (TessPack) (actual rows=1000000.00 loops=1)
               Output: unnamed_subquery_1.f1
               Rows Kept As: forwarded batches
               Batches: 21430
               ->  Custom Scan (TessFilter) on public.bench_fact (actual rows=1000000.00 loops=1)
                     Output: bench_fact.f1
                     Batch Filter: (bench_fact.f1 > 1000000)
                     Rows Removed by Batch Filter: 1000000
                     Input Batches: 42857
                     Input Rows: 2000000
                     Output Rows: 1000000
                     ->  Custom Scan (TessHeapScan) on public.bench_fact (actual rows=2000000.00 loops=1)
                           Output: bench_fact.f1
                           Batch Size: 64
                           Batches: 42857
                           Pages: 14286
                           Deformed Datums: 2000000
                           Restarted Datums: 0
```

PostgreSQL:

```
 Aggregate (actual rows=1.00 loops=1)
   Output: count(*), sum(bench_narrow.c2)
   ->  Append (actual rows=2000000.00 loops=1)
         ->  Seq Scan on public.bench_narrow (actual rows=1000000.00 loops=1)
               Output: bench_narrow.c2
               Filter: (bench_narrow.c1 > 1000000)
               Rows Removed by Filter: 1000000
         ->  Seq Scan on public.bench_fact (actual rows=1000000.00 loops=1)
               Output: bench_fact.f1
               Filter: (bench_fact.f1 > 1000000)
               Rows Removed by Filter: 1000000
```

## a month of dates out of 2 M rows through a BRIN index, aggregated
Family `index`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=1.00 loops=1)
   Output: (count(*)), (sum(w))
   Input Batches: 938
   Input Rows: 60000
   Kernel Calls: 1876
   Computed Datums: 60000
   ->  Custom Scan (TessFilter) on public.bench_idx (actual rows=60000.00 loops=1)
         Output: w
         Batch Filter: ((bench_idx.d >= '2000-02-01'::date) AND (bench_idx.d <= '2000-03-01'::date))
         Rows Removed by Batch Filter: 10563
         Input Batches: 1103
         Input Rows: 70563
         Output Rows: 60000
         ->  Custom Scan (TessHeapScan) on public.bench_idx (actual rows=70563.00 loops=1)
               Output: w, d
               Exact Heap Blocks: 0
               Lossy Heap Blocks: 512
               Batch Size: 64
               Batches: 1103
               Pages: 512
               Deformed Datums: 130563
               Restarted Datums: 0
               ->  Bitmap Index Scan on bench_idx_d (actual rows=5120.00 loops=1)
                     Index Cond: ((bench_idx.d >= '2000-02-01'::date) AND (bench_idx.d <= '2000-03-01'::date))
                     Index Searches: 1
```

PostgreSQL:

```
 Aggregate (actual rows=1.00 loops=1)
   Output: count(*), sum(w)
   ->  Bitmap Heap Scan on public.bench_idx (actual rows=60000.00 loops=1)
         Output: id, k, w, d, t
         Recheck Cond: ((bench_idx.d >= '2000-02-01'::date) AND (bench_idx.d <= '2000-03-01'::date))
         Rows Removed by Index Recheck: 10563
         Heap Blocks: lossy=512
         ->  Bitmap Index Scan on bench_idx_d (actual rows=5120.00 loops=1)
               Index Cond: ((bench_idx.d >= '2000-02-01'::date) AND (bench_idx.d <= '2000-03-01'::date))
               Index Searches: 1
```

## the same filter and count with two parallel workers
Family `win-2-workers`.

Tessera:

```
 Custom Scan (TessAgg) (actual rows=1.00 loops=1)
   Output: (count(*))
   Partial Mode: Finalize
   Input Batches: 3
   Input Rows: 3
   Kernel Calls: 0
   Computed Datums: 3
   ->  Custom Scan (TessGather) (actual rows=3.00 loops=1)
         Output: (PARTIAL count(*))
         Workers Planned: 2
         Workers Launched: 2
         Messages: 2
         Rows from Workers: 2
         Rows of the Leader: 1
         ->  Parallel Custom Scan (TessSend) (actual rows=0.00 loops=2)
               Output: (PARTIAL count(*))
               Worker 0:  actual rows=0.00 loops=1
               Worker 1:  actual rows=0.00 loops=1
               ->  Parallel Custom Scan (TessAgg) (actual rows=1.00 loops=3)
                     Output: (PARTIAL count(*))
                     Partial Mode: Partial
                     Input Batches: 22058
                     Input Rows: 1000000
                     Kernel Calls: 22058
                     Worker 0:  actual rows=1.00 loops=1
                     Worker 1:  actual rows=1.00 loops=1
                     ->  Parallel Custom Scan (TessFilter) on public.bench_narrow (actual rows=333333.33 loops=3)
                           Batch Filter: ((bench_narrow.c1 <= 1000000) AND (bench_narrow.c2 > 0))
                           Rows Removed by Batch Filter: 333333
                           Input Batches: 44117
                           Input Rows: 2000000
                           Output Rows: 1000000
                           Worker 0:  actual rows=181696.00 loops=1
                           Worker 1:  actual rows=181696.00 loops=1
                           ->  Parallel Custom Scan (TessHeapScan) on public.bench_narrow (actual rows=666666.67 loops=3)
                                 Output: c1, c2
                                 Batch Size: 64
                                 Batches: 44117
                                 Pages: 14706
                                 Deformed Datums: 3000000
                                 Restarted Datums: 0
                                 Worker 0:  actual rows=434792.00 loops=1
                                 Worker 1:  actual rows=435880.00 loops=1
```

PostgreSQL:

```
 Finalize Aggregate (actual rows=1.00 loops=1)
   Output: count(*)
   ->  Gather (actual rows=3.00 loops=1)
         Output: (PARTIAL count(*))
         Workers Planned: 2
         Workers Launched: 2
         ->  Partial Aggregate (actual rows=1.00 loops=3)
               Output: PARTIAL count(*)
               Worker 0:  actual rows=1.00 loops=1
               Worker 1:  actual rows=1.00 loops=1
               ->  Parallel Seq Scan on public.bench_narrow (actual rows=333333.33 loops=3)
                     Output: c1, c2, c3, c4, c5, c6, c7, c8
                     Filter: ((bench_narrow.c1 <= 1000000) AND (bench_narrow.c2 > 0))
                     Rows Removed by Filter: 333333
                     Worker 0:  actual rows=293760.00 loops=1
                     Worker 1:  actual rows=292672.00 loops=1
```
