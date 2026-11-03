# A/B: d4eac1d768d18cef37fc9418419d7b7810c4c171 against b6ddc7ea7cce8e3da60a9478bf9ee10fcacdf462

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases all; repetitions the family's; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## indexjoin

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| cross_type | 2.766 | 1.621 | 0.607 | 0.586 | 0.953 | faster |
| date_key | 2.784 | 1.727 | 0.622 | 0.620 | 0.966 | faster |
| few | 0.980 | 0.839 | 0.981 | 0.856 | 0.847 |  |
| four | 1.498 | 1.135 | 0.804 | 0.758 | 0.935 | faster |
| hot_key | 237.854 | 226.049 | 0.988 | 0.950 | 0.897 |  |
| left_miss | 2.505 | 2.432 | 0.991 | 0.971 | 0.813 |  |
| pk | 3.135 | 1.656 | 0.622 | 0.528 | 0.885 | faster |
| pk_20k | 18.405 | 4.550 | 0.246 | 0.247 | 0.936 | faster |
| pk_shuffled | 7.272 | 5.079 | 0.927 | 0.699 | 0.841 | faster |
| repeated | 2.901 | 2.632 | 1.009 | 0.907 | 0.864 |  |
| scattered | 4.631 | 2.309 | 0.686 | 0.499 | 0.872 | faster |
| sparse | 5.129 | 3.530 | 0.801 | 0.688 | 0.830 | faster |
| text_key | 6.042 | 5.673 | 0.948 | 0.939 | 0.849 |  |

Runs: base pg-indexjoin-clKC60, pg-indexjoin-t56MmG; candidate pg-indexjoin-7EsYXT, pg-indexjoin-K4TnhP.

