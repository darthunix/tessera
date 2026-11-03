# A/B: ad081e4a907d3e1d48f64daf5a2fd23fcc8a0949 against b6ddc7ea7cce8e3da60a9478bf9ee10fcacdf462

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases all; repetitions the family's; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## indexjoin

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| cross_type | 2.603 | 1.608 | 0.613 | 0.618 | 0.969 | faster |
| date_key | 2.612 | 1.661 | 0.624 | 0.636 | 0.993 | faster |
| few | 0.841 | 0.835 | 0.994 | 0.993 | 0.968 |  |
| four | 1.430 | 1.117 | 0.791 | 0.780 | 0.961 | faster |
| hot_key | 218.009 | 222.269 | 1.010 | 1.020 | 1.027 |  |
| left_miss | 2.383 | 1.712 | 0.727 | 0.718 | 0.984 | faster |
| pk | 2.593 | 1.658 | 0.625 | 0.639 | 0.996 | faster |
| pk_20k | 14.970 | 4.190 | 0.271 | 0.280 | 1.013 | faster |
| pk_shuffled | 4.345 | 4.869 | 1.071 | 1.121 | 0.889 | **slower** |
| repeated | 2.532 | 2.494 | 0.989 | 0.985 | 0.985 |  |
| scattered | 3.124 | 2.245 | 0.718 | 0.719 | 1.036 | faster |
| sparse | 3.310 | 4.209 | 1.290 | 1.272 | 0.967 | **slower** |
| text_key | 5.034 | 5.436 | 1.098 | 1.080 | 1.012 | **slower** |

Runs: base pg-indexjoin-7uuqsz, pg-indexjoin-zXbVCN; candidate pg-indexjoin-G6QBhn, pg-indexjoin-Nn41S9.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
