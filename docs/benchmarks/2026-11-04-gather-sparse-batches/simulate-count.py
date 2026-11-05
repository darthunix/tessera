# Policy B: a batch at least half full, a batch without stored values and the
# scan's first go as they are; a sparse batch right after one of those goes as
# it is too; the other sparse batches are copied. Compare the planner's formula.
import random, math
CAP = 64
def sim(n, p, N, rnd):
    out = copied = buf = 0; after_pass = True
    for i in range(N):
        k = sum(rnd.random() < p for _ in range(n))
        if k == 0: continue
        if i == 0 or 2 * k >= CAP:
            if buf: out += 1; buf = 0
            out += 1; after_pass = True; continue
        if after_pass and buf == 0:
            out += 1; after_pass = False; continue
        after_pass = False
        copied += k; buf += k
        while buf >= CAP: out += 1; buf -= CAP
    if buf: out += 1
    return out / N, copied / N
def est(n, p):
    f = [math.comb(n, k) * p**k * (1-p)**(n-k) for k in range(n+1)]
    q = sum(f[k] for k in range(n+1) if 2*k >= CAP); e = f[0]; r = 1 - q - e
    dense_rows = sum(k*f[k] for k in range(n+1) if 2*k >= CAP)
    sparse_rows = n*p - dense_rows
    mean_sparse = sparse_rows / r if r > 0 else 0
    follow = r / (q + r) if q + r > 0 else 0   # the next batch with a row is sparse
    first = q * follow                        # runs a passing batch starts: their first passes
    longer = first * follow                   # of them, runs of two sparse batches or more
    copied = max(sparse_rows - first * mean_sparse, 0)
    g = q + first + max(longer, copied / CAP + longer / 2)
    return min(1.0, g), copied
rnd = random.Random(5)
worst = 0
for n in (34, 40, 50, 60, 64):
    for i in range(1, 100):
        p = i / 100
        x, c = sim(n, p, 20000, rnd)
        e, ec = est(n, p)
        if x > 0.05:
            err = e / x - 1
            if abs(err) > abs(worst): worst, at = err, (n, p, round(x, 3), round(e, 3), round(c, 1), round(ec, 1))
print("worst relative error of the count:", round(worst, 3), "at (n, p, sim, est, copied sim, copied est) =", at)
