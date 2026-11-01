#!/usr/bin/env python3
"""Featherweight bench: build a track, run it the way the board does, check the hash,
report wall time measured inside the container and memory.peak of the container cgroup.

    python tools/bench.py d --gate G1          # ex-tiny + 4 tiny seeds
    python tools/bench.py d --gate G3 --runs 3 # full seed 1, 3 runs
    python tools/bench.py d --sets s2-sample/d --runs 5 --no-build

Datasets and expected hashes come from DATA_ROOT (public data, read-only).
The image tag is featherweight/track-<t>, so it never collides with other checkouts.
"""
import argparse, hashlib, os, shutil, subprocess, sys, statistics

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA_ROOT = os.environ.get("FW_DATA", r"C:\Users\Gaurav\Music\speed-demon-research\Third-of-the-Code\data")
# The same datasets copied into a docker volume: bind mounts from Windows are slow and distort the build phase.
VOLUME = os.environ.get("FW_VOLUME", "fw-data")
OUTPUT = {"a": "summary.txt", "b": "results.txt", "c": "aggregates.txt", "d": "distances.txt"}
GATES = {
    "G1": ["ex-tiny", "s2-tiny", "s3-tiny", "s4-tiny", "s5-tiny"],
    "G2": ["s2-sample", "s3-sample", "s4-sample", "s5-sample"],
    "G3": [""],  # full seed 1 lives at DATA_ROOT/<track>
}
# Inside the container: time only the solution, then read the cgroup high-water mark.
# /out is a tmpfs, as on the board: the output is charged to memory.peak and can be mmapped.
PROBE = ('s=$(date +%s%N); /app/solution; c=$?; e=$(date +%s%N); '
         'echo "BENCH $c $((e-s)) $(cat /sys/fs/cgroup/memory.peak) $(ls /out | paste -sd /)"; '
         'sha256sum /out/* 2>/dev/null | head -1')


def expected_hashes():
    table = {}
    with open(os.path.join(DATA_ROOT, "expected.sha256"), encoding="utf-8") as f:
        for line in f:
            if line.strip():
                digest, key = line.split()
                table[key] = digest
    return table


def build(track, tag):
    r = subprocess.run(["docker", "build", "-q", "--network=none", "-t", tag,
                        os.path.join(ROOT, f"track-{track}")], capture_output=True, text=True)
    if r.returncode:
        sys.exit(f"build failed:\n{r.stderr[-3000:]}")


def run_once(track, tag, data, rel, threads):
    out = os.path.join(ROOT, ".out", f"bench-{track}")
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    os.chmod(out, 0o777)
    mount = (["--mount", f"type=volume,src={VOLUME},dst=/data,readonly,volume-subpath={rel}"] if VOLUME
             else ["-v", f"{data}:/data:ro"])
    env = ["-e", f"TRACK={track}"] + (["-e", f"FW_THREADS={threads}"] if threads else [])
    # Experiment switches (FW_RENUM=0 and so on) pass straight through to the solution.
    env += [x for k, v in os.environ.items() if k.startswith("FW_") and k not in ("FW_DATA", "FW_VOLUME")
            for x in ("-e", f"{k}={v}")]
    cmd = ["docker", "run", "--rm", "--network=none", "--read-only", "--tmpfs", "/tmp:rw,size=1g,exec",
           "--memory=8g", "--memory-swap=8g", "--pids-limit=512", "--cap-drop=ALL",
           "--security-opt=no-new-privileges", "--user", "65534:65534", "--ulimit", "nofile=8192:8192",
           *env, *mount, "--tmpfs", "/out:rw,size=1g,mode=1777",
           "--entrypoint", "/bin/sh", tag, "-c", PROBE]
    r = subprocess.run(cmd, capture_output=True, text=True)
    line = [l for l in r.stdout.splitlines() if l.startswith("BENCH ")]
    if not line:
        sys.exit(f"no BENCH line; stdout={r.stdout[-500:]!r} stderr={r.stderr[-1500:]!r}")
    parts = line[-1].split()
    code, ns, peak, files = parts[1], parts[2], parts[3], (parts[4] if len(parts) > 4 else "")
    if code != "0":
        sys.exit(f"solution exited {code}; stderr={r.stderr[-1500:]!r}")
    if files.strip("/") != OUTPUT[track]:
        sys.exit(f"/out must hold exactly {OUTPUT[track]}, found {files!r}")
    h = [l.split()[0] for l in r.stdout.splitlines() if l.endswith(OUTPUT[track])][0]
    return int(ns) / 1e6, int(peak) / 1e6, h, r.stderr.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("track", choices=sorted(OUTPUT))
    ap.add_argument("--gate", choices=sorted(GATES))
    ap.add_argument("--sets", nargs="*", default=[], help="dataset prefixes, e.g. s2-sample")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--threads", type=int, default=0, help="sets FW_THREADS for the solution")
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--stderr", action="store_true", help="print the solution's stderr (phase timers)")
    a = ap.parse_args()

    tag = os.environ.get("FW_TAG", f"featherweight/track-{a.track}")  # one tag per worktree
    if not a.no_build:
        build(a.track, tag)
    want = expected_hashes()
    sets = (GATES[a.gate] if a.gate else []) + a.sets
    if not sets:
        sys.exit("give --gate or --sets")
    failed = 0
    for s in sets:
        rel = f"{s}/{a.track}" if s else a.track
        data = os.path.join(DATA_ROOT, *rel.split("/"))
        exp = want.get(f"{rel}/{OUTPUT[a.track]}")
        ts, ms, ok = [], [], True
        if a.runs > 1:
            run_once(a.track, tag, data, rel, a.threads)  # warm-up: page cache, as on the board
        for _ in range(a.runs):
            t, m, h, err = run_once(a.track, tag, data, rel, a.threads)
            ts.append(t); ms.append(m)
            ok = ok and (exp is None or h == exp)
            if a.stderr and err:
                print("   ", err.replace("\n", "\n    "))
        failed += not ok
        print(f"{'ok  ' if ok else 'FAIL'} {rel:<14} T med {statistics.median(ts):8.1f} ms  min {min(ts):8.1f}"
              f"  M max {max(ms):6.2f} MB  runs {a.runs}{'' if exp else '  (no expected hash)'}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
