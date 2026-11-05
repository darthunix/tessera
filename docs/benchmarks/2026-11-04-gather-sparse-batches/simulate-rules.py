# Rules of gathering over a heap scan's batches, simulated: pages of
# per_page rows cut into batches of 64 and a short rest, each row kept by
# the filter with chance p. A consumer's time is counted as 1 us a batch
# and 12 ns a copied row (three columns of about 4 ns), over the time
# without gathering. Rules:
#   approved  a batch half full goes as it is, any sparse one is copied;
#   kept      as approved, but a sparse batch right after one that went
#             as it is goes as it is too (the rule the change keeps);
#   fill      a batch that fits whole into the gathered rows is copied,
#             full or not (refused: it copies whole batches).
# The scan's first batch goes as it is under every rule.
import random

CAP = 64


def batches(pages, per_page, p, rnd):
    out = []
    for _ in range(pages):
        left = per_page
        while left > 0:
            n = min(CAP, left)
            left -= n
            out.append(sum(rnd.random() < p for _ in range(n)))
    return [k for k in out if k]


def run(rule, seq):
    out = copied = held = 0
    after_pass = True
    for i, k in enumerate(seq):
        dense = 2 * k >= CAP
        if i == 0:
            out += 1
            continue
        if rule == "fill" and held and held + k <= CAP:
            copied += k
            held += k
            if held == CAP:
                out += 1
                held = 0
            continue
        if dense:
            if held:
                out += 1
                held = 0
            out += 1
            after_pass = True
            continue
        if rule == "kept" and after_pass and held == 0:
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


rnd = random.Random(3)
worst = {"approved": 0, "kept": 0, "fill": 0}
print("per_page    p | approved  kept  fill")
for per_page in (40, 60, 83, 110, 140):
    for p in (0.02, 0.1, 0.2, 0.35, 0.45, 0.5, 0.55, 0.6, 0.7, 0.8, 0.9):
        seq = batches(2000, per_page, p, rnd)
        row = []
        for rule in ("approved", "kept", "fill"):
            out, copied = run(rule, seq)
            time = (out + copied * 0.012) / len(seq)
            worst[rule] = max(worst[rule], time)
            row.append(f"{time:5.2f}")
        print(f"{per_page:8} {p:4.2f} |    " + "  ".join(row))
print("worst:", {rule: round(time, 2) for rule, time in worst.items()})
