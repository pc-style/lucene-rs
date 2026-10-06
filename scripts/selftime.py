"""Self time by leaf frame from an async-profiler collapsed-stack file (only samples under Bench.main)."""
import sys, collections, re
self_t = collections.Counter(); total = 0
for line in open(sys.argv[1]):
    stack, n = line.rstrip().rsplit(" ", 1); n = int(n)
    frames = stack.split(";")
    if not any("Bench.main" in f for f in frames):
        continue
    total += n
    leaf = re.sub(r"_\[[a-z0-9]\]$", "", frames[-1])
    self_t[leaf] += n
print(f"samples under Bench.main: {total}")
for f, n in self_t.most_common(int(sys.argv[2]) if len(sys.argv) > 2 else 22):
    print(f"{100*n/total:5.1f}%  {f}")
