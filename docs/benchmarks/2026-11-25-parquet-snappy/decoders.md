# The decoders on the pages of hits.parquet

Per byte decompressed, medians of seven passes over about 128 MB of
compressed pages a column. Sections 1 to 4: cargo clickbench decoders
(pmu, through sudo -n), the reader's decoder against snap's, at each
commit that changed it. Sections 5 and 6: a scratch program that
linked the object of DuckDB's snappy 1.2.1 from the build pg_duckdb
loads, counted by proc_pid_rusage (5, with the branches by pmu) and
with three sides (6).

## 2.2, the reader's decoder still snap's: the mode's noise

```text
same bytes on every page of /Users/darthunix/git/tessera-duckdb/target/clickbench-one/hits.parquet
        column  pages   in MB  out MB |        instr/B       cycles/B     branches/B      misses/KB | over snap:  instr cycles misses
           URL    490   169.1   505.4 |  2.865  2.865  1.571  1.571  0.546  0.546 45.961 45.953 |             1.000  1.000  1.000
         Title    419   128.7   426.6 |  3.589  3.589  1.584  1.584  0.618  0.618 34.856 34.860 |             1.000  1.000  1.000
       Referer    369   102.8   361.4 |  2.530  2.530  1.314  1.314  0.486  0.486 37.737 37.711 |             1.000  1.000  1.001
  SearchPhrase    392   128.7   284.6 |  5.834  5.834  2.415  2.414  0.933  0.933 43.520 43.516 |             1.000  1.000  1.000
     EventTime    273   130.1   201.8 |  5.407  5.407  1.739  1.739  1.020  1.020  2.820  2.821 |             1.000  1.000  1.000
     CounterID    452     0.1     0.1 |  2.473  1.865  0.503  0.403  0.535  0.388  6.142  4.792 |             1.326  1.249  1.282
        UserID    294   128.3   149.5 |  0.926  0.926  0.414  0.415  0.201  0.201  9.203  9.185 |             1.000  0.997  1.002
       WatchID    166   128.4   128.4 |  0.189  0.189  0.061  0.061  0.032  0.032  0.017  0.017 |             1.001  1.000  1.004
each pair: the reader's decoder, then snap's; medians of the passes
```

## 3.1, the careful loop alone

```text
same bytes on every page of /Users/darthunix/git/tessera-duckdb/target/clickbench-one/hits.parquet
        column  pages   in MB  out MB |        instr/B       cycles/B     branches/B      misses/KB | over snap:  instr cycles misses
           URL    490   169.1   505.4 |  6.742  2.865  4.082  1.574  1.788  0.546 147.451 45.972 |             2.353  2.594  3.207
         Title    419   128.7   426.6 |  8.695  3.589  4.162  1.580  2.279  0.618 140.013 34.896 |             2.423  2.634  4.012
       Referer    369   102.8   361.4 |  5.826  2.530  3.336  1.316  1.536  0.486 118.726 37.708 |             2.303  2.536  3.149
  SearchPhrase    392   128.7   284.6 | 15.067  5.834  6.856  2.406  3.980  0.933 225.045 43.630 |             2.582  2.850  5.158
     EventTime    273   130.1   201.8 | 14.181  5.407  2.990  1.742  3.886  1.020 35.572  2.875 |             2.623  1.717 12.371
     CounterID    452     0.1     0.1 |  1.940  1.865  0.469  0.439  0.518  0.388  5.553  5.781 |             1.040  1.068  0.961
        UserID    294   128.3   149.5 |  2.223  0.926  0.783  0.416  0.616  0.201 20.119  9.288 |             2.402  1.881  2.166
       WatchID    166   128.4   128.4 |  0.189  0.189  0.062  0.064  0.032  0.032  0.017  0.018 |             1.001  0.966  0.981
each pair: the reader's decoder, then snap's; medians of the passes
```

## 3.2 as first built, the position an index

```text
same bytes on every page of /Users/darthunix/git/tessera-duckdb/target/clickbench-one/hits.parquet
        column  pages   in MB  out MB |        instr/B       cycles/B     branches/B      misses/KB | over snap:  instr cycles misses
           URL    490   169.1   505.4 |  2.603  2.865  0.792  1.571  0.320  0.546  7.403 45.900 |             0.909  0.504  0.161
         Title    419   128.7   426.6 |  3.298  3.589  0.995  1.588  0.405  0.618  5.989 34.825 |             0.919  0.626  0.172
       Referer    369   102.8   361.4 |  2.244  2.530  0.697  1.315  0.277  0.486  8.159 37.662 |             0.887  0.530  0.217
  SearchPhrase    392   128.7   284.6 |  5.476  5.834  1.652  2.417  0.681  0.933  5.994 43.505 |             0.939  0.684  0.138
     EventTime    273   130.1   201.8 |  6.053  5.407  1.803  1.740  0.749  1.020  0.960  2.832 |             1.119  1.036  0.339
     CounterID    452     0.1     0.1 |  1.823  1.865  0.419  0.424  0.434  0.388  5.458  4.963 |             0.978  0.988  1.100
        UserID    294   128.3   149.5 |  0.958  0.926  0.253  0.416  0.143  0.201  1.740  9.262 |             1.035  0.608  0.188
       WatchID    166   128.4   128.4 |  0.189  0.189  0.062  0.061  0.032  0.032  0.018  0.017 |             1.004  1.008  1.022
each pair: the reader's decoder, then snap's; medians of the passes
```

## 3.2 as committed, the position a pointer

```text
same bytes on every page of /Users/darthunix/git/tessera-duckdb/target/clickbench-one/hits.parquet
        column  pages   in MB  out MB |        instr/B       cycles/B     branches/B      misses/KB | over snap:  instr cycles misses
           URL    490   169.1   505.4 |  2.406  2.865  0.702  1.575  0.320  0.546  7.345 46.072 |             0.840  0.446  0.159
         Title    419   128.7   426.6 |  3.047  3.589  0.869  1.588  0.405  0.618  5.870 34.920 |             0.849  0.547  0.168
       Referer    369   102.8   361.4 |  2.077  2.530  0.622  1.317  0.277  0.486  8.055 37.816 |             0.821  0.472  0.213
  SearchPhrase    392   128.7   284.6 |  5.063  5.834  1.437  2.418  0.681  0.933  5.976 43.542 |             0.868  0.594  0.137
     EventTime    273   130.1   201.8 |  5.593  5.407  1.548  1.739  0.749  1.020  0.957  2.835 |             1.034  0.890  0.337
     CounterID    452     0.1     0.1 |  1.858  1.865  0.416  0.428  0.437  0.388  4.393  4.697 |             0.996  0.971  0.935
        UserID    294   128.3   149.5 |  0.915  0.926  0.233  0.418  0.144  0.201  1.714  9.281 |             0.988  0.557  0.185
       WatchID    166   128.4   128.4 |  0.189  0.189  0.062  0.062  0.032  0.032  0.018  0.017 |             1.004  1.004  1.019
each pair: the reader's decoder, then snap's; medians of the passes
```

## DuckDB's snappy against snap's, before the change (pmu)

```text
same bytes on every page: yes
        column  pages    in MB   out MB |  snap i/B  snap c/B   GB/s |  duck i/B  duck c/B   GB/s |  i d/s  c d/s
           URL    490      169      505 |     2.866     1.573   2.86 |     2.617     0.710   6.33 |  0.913  0.452
               cycles/B snap 1.572..1.576 duck 0.710..0.711
               pmu, user space of the thread: instr/B snap 2.865 duck 2.617; cycles/B snap 1.569 duck 0.709; branches/B snap 0.546 duck 0.327; misses/KB snap 45.94 duck 7.31
         Title    419      128      426 |     3.590     1.588   2.84 |     3.315     0.875   5.15 |  0.923  0.551
               cycles/B snap 1.588..1.589 duck 0.874..0.875
               pmu, user space of the thread: instr/B snap 3.589 duck 3.314; cycles/B snap 1.585 duck 0.873; branches/B snap 0.618 duck 0.413; misses/KB snap 34.96 duck 5.74
       Referer    369      102      361 |     2.531     1.317   3.42 |     2.274     0.633   7.13 |  0.899  0.480
               cycles/B snap 1.315..1.317 duck 0.632..0.633
               pmu, user space of the thread: instr/B snap 2.530 duck 2.274; cycles/B snap 1.313 duck 0.631; branches/B snap 0.486 duck 0.286; misses/KB snap 37.70 duck 8.03
  SearchPhrase    392      128      284 |     5.836     2.421   1.86 |     5.599     1.474   3.06 |  0.959  0.609
               cycles/B snap 2.419..2.423 duck 1.473..1.476
               pmu, user space of the thread: instr/B snap 5.834 duck 5.598; cycles/B snap 2.416 duck 1.471; branches/B snap 0.933 duck 0.684; misses/KB snap 43.62 duck 5.93
     EventTime    273      130      201 |     5.408     1.745   2.58 |     6.087     1.557   2.89 |  1.126  0.892
               cycles/B snap 1.745..1.750 duck 1.557..1.559
               pmu, user space of the thread: instr/B snap 5.407 duck 6.087; cycles/B snap 1.742 duck 1.555; branches/B snap 1.020 duck 0.749; misses/KB snap 2.83 duck 0.94
     CounterID    452        0        0 |     2.530     0.548  12.67 |     4.946     1.035   5.13 |  1.955  1.889
               cycles/B snap 0.473..1.039 duck 0.984..1.373
               pmu, user space of the thread: instr/B snap 1.923 duck 4.339; cycles/B snap 0.354 duck 0.860; branches/B snap 0.397 duck 0.759; misses/KB snap 0.72 duck 5.17
        UserID    294      128      149 |     0.926     0.417  10.80 |     1.304     0.328  13.76 |  1.408  0.786
               cycles/B snap 0.413..0.420 duck 0.327..0.329
               pmu, user space of the thread: instr/B snap 0.926 duck 1.304; cycles/B snap 0.415 duck 0.327; branches/B snap 0.201 duck 0.147; misses/KB snap 9.29 duck 1.73
       WatchID    166      128      128 |     0.189     0.062  72.25 |     0.190     0.062  73.06 |  1.009  1.001
               cycles/B snap 0.061..0.067 duck 0.061..0.062
               pmu, user space of the thread: instr/B snap 0.189 duck 0.190; cycles/B snap 0.061 duck 0.062; branches/B snap 0.032 duck 0.032; misses/KB snap 0.02 duck 0.02
```

## snap's, DuckDB's and the reader's, at 3.2 as committed

```text
same bytes on every page: yes
        column  pages    in MB   out MB |  snap i/B  snap c/B   GB/s |  duck i/B  duck c/B   GB/s |  i d/s  c d/s
           URL    490     505 | snap  2.866  1.570 | duck  2.617  0.710 | ours  2.406  0.703 | ours/snap c 0.447 | ours/duck c 0.990 i 0.920
         Title    419     426 | snap  3.589  1.586 | duck  3.315  0.874 | ours  3.047  0.869 | ours/snap c 0.548 | ours/duck c 0.994 i 0.919
       Referer    369     361 | snap  2.531  1.315 | duck  2.274  0.632 | ours  2.078  0.624 | ours/snap c 0.474 | ours/duck c 0.987 i 0.914
  SearchPhrase    392     284 | snap  5.835  2.418 | duck  5.598  1.472 | ours  5.064  1.439 | ours/snap c 0.595 | ours/duck c 0.978 i 0.904
     EventTime    273     201 | snap  5.408  1.745 | duck  6.087  1.557 | ours  5.594  1.553 | ours/snap c 0.890 | ours/duck c 0.997 i 0.919
        UserID    294     149 | snap  0.926  0.417 | duck  1.304  0.328 | ours  0.915  0.235 | ours/snap c 0.562 | ours/duck c 0.715 i 0.702
```
