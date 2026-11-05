# The planner's count of the batches a gathering input gives
# (nodes/join_planner.c, shape_binomial and gathered_batches) against the
# input's rules played batch by batch: pages of per_page rows cut into
# batches of 64 and a short last one, each row kept with chance p. A
# batch half full or more goes as it is; a sparse one right after one
# that went as it is goes as it is too; the others are copied into
# batches of 64. Prints the worst relative error over the grid.
import math
import random

CAP = 64


def sizes(per_page):
    npos = math.ceil(per_page / CAP)
    return [CAP] * (npos - 1) + [max(round(per_page - CAP * (npos - 1)), 1)]


def simulate(per_page, p, pages, rnd):
    out = copied = held = 0
    after_pass = True
    first = True
    for _ in range(pages):
        for n in sizes(per_page):
            k = sum(rnd.random() < p for _ in range(n))
            if k == 0:
                continue
            if first or 2 * k >= CAP:
                if held:
                    out += 1
                    held = 0
                out += 1
                after_pass = True
                first = False
                continue
            if after_pass and held == 0:
                out += 1
                after_pass = False
                continue
            after_pass = False
            copied += k
            held += k
            while held >= CAP:
                out += 1
                held -= CAP
    if held:
        out += 1
    return out, copied


def law(n, p):
    dense = sparse = sparse_rows = 0.0
    for k in range(n + 1):
        f = math.comb(n, k) * p ** k * (1 - p) ** (n - k)
        if 2 * k >= CAP:
            dense += f
        elif k > 0:
            sparse += f
            sparse_rows += k * f
    return dense, sparse, sparse_rows


def estimate(per_page, p, pages):
    laws = [law(n, p) for n in sizes(per_page)]
    npos = len(laws)
    passing = sparse_rows = firsts = first_rows = longer = 0.0
    first_at = []
    for pos, (dense, sparse, rows) in enumerate(laws):
        bd, bs, _ = laws[pos - 1]
        after_dense = bd / (bd + bs) if bd + bs > 0 else 0
        f = sparse * after_dense
        first_at.append(f)
        passing += pages * dense
        sparse_rows += pages * rows
        firsts += pages * f
        first_rows += pages * f * rows / sparse if sparse > 0 else 0
    for pos, (dense, sparse, _) in enumerate(laws):
        nxt = sparse / (dense + sparse) if dense + sparse > 0 else 0
        longer += pages * first_at[pos - 1] * nxt
    copied = max(sparse_rows - first_rows, 0)
    runs = longer + 1
    batches = sum(1 - (1 - p) ** n for n in sizes(per_page)) * pages
    gathered = 1 + passing + firsts + max(runs, copied / CAP + runs / 2)
    return max(min(gathered, batches), 1), copied


rnd = random.Random(7)
worst = (0,)
for per_page in (30, 45, 60, 64, 70, 80, 100, 128, 150, 200, 290):
    for p in (0.01, 0.05, 0.1, 0.2, 0.3, 0.4, 0.45, 0.5, 0.55, 0.6, 0.7, 0.8, 0.9, 0.99):
        sim, _ = simulate(per_page, p, 3000, rnd)
        est, _ = estimate(per_page, p, 3000)
        err = est / sim - 1
        # A count of a few batches is off by one at worst.
        if sim >= 100 and abs(err) > abs(worst[0]):
            worst = (err, per_page, p, sim, round(est))
print("worst relative error over counts of 100 batches or more:",
      round(worst[0], 3), "at (rows a page, p, simulated, estimated) =", worst[1:])
