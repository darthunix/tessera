# Benchmark run of 2026-10-01

Every family of [`bench/pg`](../../../bench/pg/README.md) that compares
Tessera with PostgreSQL's own executor, in full, on one machine, one after
another on a freshly created cluster. The [README](../../../README.md)'s
table cites some of these cases.

- **Tessera** at 0e27e8e (its code is that of 9dc681f; the commit adds
  plan text), release build.
- **PostgreSQL** master at 311df1dc (20devel), built without assertions;
  `shared_buffers` 2 GB, `jit` off.
- **Machine**: Apple M5 Pro (6 performance and 12 efficiency cores),
  64 GB, macOS 26.6.2, on AC power, otherwise idle.
- **Data**: `bench/pg/setup.sql` at its base size (2 M rows in the large
  tables), and ten times that for the spill family.
- **Method**: one session per family; each statement prepared once with
  `tessera.enable` on and once off, so each mode keeps its own plan; 5
  warm-up runs and 31 timed runs per mode (the spill family: 2 and 11, its
  cases run for seconds); no parallel workers, except the family "win, two
  workers", where both modes may use two. The tables show medians.
- **Speedup** is PostgreSQL's median divided by Tessera's: above 1, Tessera
  is faster. `plan_time` (join) and `plan_exprs` (exec) time the planning
  of a query alone.

Each family's directory holds `summary.txt` (minimum, median, 10th and 90th
percentile per mode), `source.txt` (commit, scale, settings, SHA-256 of the
modules and of `postgres`) and `power.txt`. [`cited-plans.md`](cited-plans.md)
holds the plans of both modes of the cases the README cites; a family's run
writes every case's plans and timings, which repeating it gives:

```sh
export PG_CONFIG=/path/to/postgresql/bin/pg_config
make && make install
bench/pg/run.sh setup            # bench/pg/run.sh setup 10 for spill
bench/pg/run.sh measure win      # bench/pg/run.sh measure win 2: two workers
bench/pg/run.sh stop
```

What each family and case measures is in
[`bench/pg/README.md`](../../../bench/pg/README.md) and in the comments of
its SQL file.

### tax

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| limit_1 | 31 | 0.003 | 0.002 | 0.67× |
| limit_1000 | 31 | 0.009 | 0.019 | 2.11× |
| limit_1m | 31 | 2.943 | 17.6 | 5.98× |
| limit_expr | 31 | 10.4 | 33.1 | 3.17× |
| limit_text | 31 | 2.955 | 7.603 | 2.57× |
| mixed_limit | 31 | 2.835 | 9.209 | 3.25× |
| offset_1m | 31 | 2.436 | 11.0 | 4.50× |
| sort_limit | 31 | 34.8 | 50.3 | 1.45× |
| wide_limit | 31 | 1.110 | 3.902 | 3.52× |

### win

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| case_filter | 31 | 15.0 | 42.7 | 2.84× |
| case_sum | 31 | 15.4 | 43.6 | 2.84× |
| count_all | 31 | 5.251 | 25.9 | 4.93× |
| dense | 31 | 10.5 | 41.0 | 3.90× |
| either | 31 | 11.3 | 38.7 | 3.41× |
| expr | 31 | 12.1 | 37.8 | 3.12× |
| four_dense | 31 | 15.5 | 62.4 | 4.03× |
| group_few | 31 | 29.9 | 100.0 | 3.35× |
| group_filter | 31 | 21.4 | 71.4 | 3.34× |
| group_many | 31 | 36.1 | 126.1 | 3.50× |
| half | 31 | 9.039 | 36.3 | 4.01× |
| in_list | 31 | 11.3 | 55.3 | 4.89× |
| int4_bigint | 31 | 9.583 | 34.2 | 3.57× |
| mixed | 31 | 3.587 | 10.5 | 2.94× |
| mixed_text | 31 | 3.372 | 10.9 | 3.23× |
| nothing | 31 | 7.471 | 27.2 | 3.65× |
| residual | 31 | 9.786 | 34.2 | 3.50× |
| sparse | 31 | 9.664 | 29.8 | 3.08× |
| sum_sparse | 31 | 10.5 | 30.4 | 2.90× |
| tree | 31 | 21.1 | 47.9 | 2.27× |
| tree_sum | 31 | 21.0 | 52.0 | 2.47× |
| two_columns | 31 | 10.9 | 39.2 | 3.61× |
| wide | 31 | 3.670 | 7.957 | 2.17× |

### win, two workers

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| case_filter | 31 | 10.2 | 21.3 | 2.08× |
| case_sum | 31 | 10.2 | 20.5 | 2.02× |
| count_all | 31 | 5.159 | 13.0 | 2.52× |
| dense | 31 | 8.275 | 19.8 | 2.40× |
| either | 31 | 8.633 | 18.5 | 2.15× |
| expr | 31 | 9.044 | 18.5 | 2.05× |
| four_dense | 31 | 10.3 | 26.8 | 2.60× |
| group_few | 31 | 15.3 | 42.9 | 2.81× |
| group_filter | 31 | 12.2 | 31.4 | 2.58× |
| group_many | 31 | 25.6 | 133.0 | 5.20× |
| half | 31 | 7.757 | 18.5 | 2.38× |
| in_list | 31 | 8.660 | 25.4 | 2.93× |
| int4_bigint | 31 | 8.202 | 17.4 | 2.12× |
| mixed | 31 | 3.618 | 6.642 | 1.84× |
| mixed_text | 31 | 3.819 | 6.715 | 1.76× |
| nothing | 31 | 7.067 | 14.8 | 2.09× |
| residual | 31 | 7.861 | 17.1 | 2.17× |
| sparse | 31 | 8.035 | 15.4 | 1.92× |
| sum_sparse | 31 | 8.299 | 15.8 | 1.90× |
| tree | 31 | 12.2 | 22.1 | 1.81× |
| tree_sum | 31 | 12.1 | 23.8 | 1.97× |
| two_columns | 31 | 8.266 | 18.8 | 2.27× |
| wide | 31 | 4.584 | 6.448 | 1.41× |

### join

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| anti | 31 | 22.4 | 95.4 | 4.26× |
| chain | 31 | 33.3 | 113.9 | 3.42× |
| dup | 31 | 34.5 | 108.9 | 3.16× |
| dup_text | 31 | 5.490 | 18.4 | 3.35× |
| fk_count | 31 | 22.6 | 114.4 | 5.07× |
| fk_inner_col | 31 | 27.0 | 115.5 | 4.28× |
| fk_outer_col | 31 | 24.8 | 123.3 | 4.98× |
| full_join | 31 | 36.0 | 153.7 | 4.27× |
| int8 | 31 | 22.8 | 114.6 | 5.03× |
| left_dup | 31 | 2.638 | 11.1 | 4.21× |
| left_nulls | 31 | 29.0 | 134.1 | 4.62× |
| merge_forced | 31 | 205.2 | 228.0 | 1.11× |
| merge_sorted | 31 | 42.7 | 70.3 | 1.65× |
| miss | 31 | 14.0 | 66.4 | 4.75× |
| mixed | 31 | 22.6 | 114.7 | 5.09× |
| part_prune | 31 | 5.284 | 83.4 | 15.79× |
| part_prune_few | 31 | 3.837 | 68.2 | 17.77× |
| plan_time | 31 | 0.067 | 0.031 | 0.46× |
| residual | 31 | 30.8 | 136.8 | 4.43× |
| right_join | 31 | 26.9 | 129.3 | 4.80× |
| rows_parent | 31 | 55.9 | 169.4 | 3.03× |
| selective | 31 | 15.7 | 55.1 | 3.52× |
| semi | 31 | 22.4 | 108.3 | 4.84× |
| two_keys | 31 | 25.6 | 136.5 | 5.33× |

### spill (data ×10)

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| dim_16mb | 11 | 786.8 | 4025.2 | 5.12× |
| dim_4mb | 11 | 528.7 | 2189.3 | 4.14× |
| dim_64mb | 11 | 551.8 | 4333.9 | 7.85× |
| dim_anti_16mb | 11 | 675.8 | 3451.4 | 5.11× |
| dim_anti_4mb | 11 | 487.6 | 2035.7 | 4.17× |
| dim_anti_64mb | 11 | 429.6 | 3594.1 | 8.37× |
| dim_left_16mb | 11 | 803.1 | 4225.7 | 5.26× |
| dim_left_4mb | 11 | 545.9 | 2325.5 | 4.26× |
| dim_left_64mb | 11 | 569.9 | 4638.1 | 8.14× |
| group_fk_16mb | 11 | 1049.9 | 3387.3 | 3.23× |
| group_fk_4mb | 11 | 952.3 | 2599.3 | 2.73× |
| group_fk_64mb | 11 | 447.0 | 2643.9 | 5.92× |
| group_mixed_16mb | 11 | 343.0 | 1558.4 | 4.54× |
| group_mixed_4mb | 11 | 388.9 | 1078.8 | 2.77× |
| group_mixed_64mb | 11 | 412.8 | 1642.2 | 3.98× |
| mixed_text_16mb | 11 | 917.4 | 3367.4 | 3.67× |
| mixed_text_4mb | 11 | 1048.2 | 2415.7 | 2.30× |
| mixed_text_64mb | 11 | 1055.8 | 5324.6 | 5.04× |

### sort

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| distinct_agg | 31 | 19.7 | 80.2 | 4.07× |
| distinct_few | 31 | 19.5 | 85.9 | 4.41× |
| distinct_group | 31 | 38.8 | 157.8 | 4.07× |
| distinct_many | 31 | 50.2 | 336.1 | 6.69× |
| order_big | 31 | 48.4 | 116.9 | 2.41× |
| order_desc | 31 | 48.9 | 137.1 | 2.80× |
| order_few | 31 | 42.1 | 122.3 | 2.91× |
| order_few_text | 31 | 581.9 | 774.3 | 1.33× |
| order_filter | 31 | 14.3 | 41.7 | 2.91× |
| order_int | 31 | 43.0 | 113.9 | 2.65× |
| order_multi | 31 | 55.1 | 313.6 | 5.69× |
| order_numeric | 31 | 91.0 | 287.7 | 3.16× |
| order_out | 31 | 51.7 | 137.7 | 2.66× |
| order_sorted | 31 | 23.0 | 80.7 | 3.50× |
| order_text_c | 31 | 311.3 | 368.3 | 1.18× |
| order_text_out | 31 | 64.3 | 147.7 | 2.30× |
| order_wide | 31 | 57.9 | 130.0 | 2.24× |
| topn_few | 31 | 15.6 | 65.2 | 4.19× |
| topn_many | 31 | 44.8 | 114.5 | 2.56× |
| topn_numeric | 31 | 58.6 | 156.4 | 2.67× |
| topn_offset | 31 | 17.9 | 67.5 | 3.77× |
| topn_reverse | 31 | 40.1 | 90.3 | 2.25× |
| topn_text_c | 31 | 48.6 | 92.4 | 1.90× |

### setop

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| setop_count | 31 | 23.5 | 92.4 | 3.93× |
| setop_except | 31 | 28.9 | 84.0 | 2.90× |
| setop_except_all | 31 | 38.0 | 99.1 | 2.61× |
| setop_few | 31 | 24.7 | 101.3 | 4.10× |
| setop_intersect_all | 31 | 66.8 | 296.7 | 4.44× |
| setop_intersect_text | 31 | 68.0 | 110.3 | 1.62× |
| setop_join | 31 | 25.2 | 101.2 | 4.02× |
| setop_known | 31 | 37.7 | 139.2 | 3.69× |
| setop_like | 31 | 17.0 | 46.2 | 2.72× |
| setop_many | 31 | 68.1 | 467.9 | 6.87× |
| setop_nested | 31 | 35.1 | 140.9 | 4.01× |
| setop_part | 31 | 9.924 | 33.4 | 3.37× |
| setop_prune_exec | 31 | 5.636 | 30.2 | 5.35× |
| setop_prune_init | 31 | 5.656 | 30.8 | 5.44× |
| setop_prune_union | 31 | 5.567 | 30.1 | 5.41× |
| setop_rows | 31 | 20.4 | 74.1 | 3.63× |

### index

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| bm_both | 31 | 7.059 | 7.222 | 1.02× |
| bm_dense | 31 | 10.7 | 17.5 | 1.65× |
| bm_either | 31 | 8.322 | 8.451 | 1.02× |
| bm_mid | 31 | 6.810 | 8.062 | 1.18× |
| bm_rows | 31 | 6.477 | 7.830 | 1.21× |
| bm_sparse | 31 | 2.885 | 3.040 | 1.05× |
| bm_third | 31 | 11.1 | 31.2 | 2.80× |
| brin_month | 31 | 0.625 | 2.057 | 3.29× |
| brin_months | 31 | 2.963 | 9.700 | 3.27× |
| brin_rows | 31 | 0.415 | 1.472 | 3.55× |
| brin_week | 31 | 0.285 | 0.937 | 3.29× |
| ios_count | 31 | 3.230 | 4.526 | 1.40× |
| ios_fifth | 31 | 6.486 | 9.161 | 1.41× |
| ios_group | 31 | 3.613 | 4.745 | 1.31× |
| ios_half | 31 | 9.280 | 23.3 | 2.51× |
| ios_order | 31 | 1.721 | 2.390 | 1.39× |
| ios_rows | 31 | 1.617 | 1.719 | 1.06× |
| ios_sparse | 31 | 0.327 | 0.450 | 1.38× |
| ios_sum | 31 | 3.378 | 4.531 | 1.34× |
| ix_filter | 31 | 2.230 | 3.637 | 1.63× |
| ix_half | 31 | 10.9 | 38.5 | 3.53× |
| ix_order | 31 | 1.790 | 2.030 | 1.13× |
| ix_range | 31 | 4.403 | 7.549 | 1.71× |
| ix_rows | 31 | 2.036 | 3.429 | 1.68× |
| ix_short | 31 | 1.270 | 2.235 | 1.76× |
| ix_third | 31 | 10.4 | 23.1 | 2.22× |

### anyagg

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| g_avg_float8 | 31 | 4.397 | 11.9 | 2.70× |
| g_avg_int8 | 31 | 7.609 | 12.8 | 1.69× |
| g_bit_or | 31 | 14.2 | 35.6 | 2.50× |
| g_filter | 31 | 8.092 | 14.4 | 1.78× |
| g_group_few | 31 | 17.2 | 35.1 | 2.04× |
| g_group_many | 31 | 17.9 | 40.5 | 2.26× |
| g_max_text | 31 | 10.5 | 14.9 | 1.42× |
| g_sum_int8 | 31 | 5.568 | 11.6 | 2.08× |
| g_sum_numeric | 31 | 3.248 | 16.6 | 5.12× |
| g_three | 31 | 18.9 | 21.2 | 1.12× |

### anykey

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| any_distinct | 31 | 14.8 | 34.4 | 2.33× |
| any_join_numeric | 31 | 87.4 | 175.9 | 2.01× |
| any_join_semi | 31 | 41.6 | 69.0 | 1.66× |
| any_join_text | 31 | 70.5 | 146.6 | 2.08× |
| any_join_two | 31 | 85.9 | 172.4 | 2.01× |
| any_numeric | 31 | 17.1 | 36.1 | 2.11× |
| any_text_agg | 31 | 14.3 | 33.1 | 2.31× |
| any_text_col | 31 | 31.3 | 78.7 | 2.52× |
| any_text_few | 31 | 13.6 | 32.7 | 2.40× |
| any_union | 31 | 28.3 | 68.1 | 2.40× |

### exec

| case | runs | Tessera, ms | PostgreSQL, ms | speedup |
|---|---:|---:|---:|---:|
| even_join | 11 | 49.5 | 174.7 | 3.53× |
| noprefix_icu | 11 | 205.7 | 294.2 | 1.43× |
| out_group | 11 | 68.3 | 215.4 | 3.15× |
| out_sort | 11 | 248.5 | 208.9 | 0.84× |
| plan_exprs | 11 | 0.102 | 0.054 | 0.53× |
| prefix_c | 11 | 220.6 | 271.2 | 1.23× |
| prefix_icu | 11 | 697.5 | 755.8 | 1.08× |
| skew_join | 11 | 45.6 | 148.4 | 3.25× |

