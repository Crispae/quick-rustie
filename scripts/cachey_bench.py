#!/usr/bin/env python3
"""Like-for-like: direct S3 vs on-disk split cache vs Cachey, on the same Wiki5k queries.

Topologies
  cluster   3 rustie-nodes + rustie-serve gateway (the multi-node deployment)
  embedded  one rustie-serve with the search stack in-process

Per configuration and phase (same query list, sequential, 1 client):
  cold      empty disk caches (node data dirs / split-cache dir wiped, Cachey volume reset), fresh processes
  restart   processes restarted (empty memory), disk caches KEPT   <- what a redeploy / new searcher sees
  hot       same processes, N repeats (memory caches warm; should be equal across modes)

Runs are interleaved (direct, X, direct, X ...) to expose network drift to Contabo.
Cachey runs with fallback disabled so a silent S3 bypass cannot flatter it.

  python3 scripts/cachey_bench.py [--rounds 2] [--hot 5] [--topology cluster|embedded|both]
"""
import argparse, json, os, re, shutil, statistics as st, subprocess, sys, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import cachey_proof as cp  # noqa: E402  (loads .env, defines cluster helpers)

ROOT = cp.ROOT
BENCH = ROOT / ".run" / "bench"
RESULTS = ROOT / "benchmarks" / "wiki5k" / "results"
HOME_DIRS = Path.home() / ".rustie" / "local"
SERVE_SPLIT_DIR = HOME_DIRS / "serve-split-cache"
MODES = {
    "direct": dict(cachey=False, split=False),
    "split_cache": dict(cachey=False, split=True),
    "cachey": dict(cachey=True, split=False),
}


def make_cfg(mode, topology):
    src = cp.CFG_SRC.read_text()
    m = MODES[mode]
    src = re.sub(r"(cachey:\n  enabled: )\w+", rf"\g<1>{str(m['cachey']).lower()}", src)
    src = re.sub(r"(cachey:\n(?:  .*\n)*?  fallback: )\w+", r"\g<1>false", src)
    src = re.sub(r"(split_cache:\n  enabled: )\w+", rf"\g<1>{str(m['split']).lower()}", src)
    if topology == "embedded":
        src = re.sub(r"\n  gateway_node: .*", "", src)
        src = src.replace("split_cache:\n  enabled:", f"split_cache:\n  dir: {SERVE_SPLIT_DIR}\n  enabled:", 1)
    BENCH.mkdir(parents=True, exist_ok=True)
    out = BENCH / f"{mode}-{topology}.yaml"
    out.write_text(src)
    return out


def dir_bytes(p):
    return sum(f.stat().st_size for f in Path(p).rglob("*") if f.is_file()) if Path(p).exists() else 0


def split_cache_bytes(topology):
    if topology == "embedded":
        return dir_bytes(SERVE_SPLIT_DIR)
    return sum(dir_bytes(d / "searcher-split-cache") for d in HOME_DIRS.glob("rustie-node-*"))


def wipe_disk_state(topology):
    for d in HOME_DIRS.glob("rustie-node-*"):
        shutil.rmtree(d, ignore_errors=True)
    shutil.rmtree(SERVE_SPLIT_DIR, ignore_errors=True)
    cp.sh(cp.COMPOSE + ["down", "-v"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    cp.sh(cp.COMPOSE + ["up", "-d", "cachey"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    cp.wait(lambda: cp.http(f"{cp.CACHEY}/stats", 3)[0] == 200, "cachey /stats", 60)


def wait_cluster_ready(n=3, timeout=120):
    """All nodes must be live+ready members before the first query (else the cold pass fails)."""
    def ready():
        status, body = cp.http("http://127.0.0.1:7280/api/v1/cluster", 3)
        if status != 200:
            return False
        try:
            return len(json.loads(body).get("ready_nodes", [])) >= n
        except ValueError:
            return False
    cp.wait(ready, f"{n} ready cluster members", timeout)
    time.sleep(2)


def start(topology, cfg, cachey):
    cp.CFG = cfg
    if topology == "cluster":
        cp.start_cluster(fallback=not cachey)
        wait_cluster_ready()
        return
    cmd = [str(cp.BIN / "rustie-serve"), "--deploy-config", str(cfg), "--refresh-secs", "0"]
    cp.procs["serve"] = subprocess.Popen(cmd, env=cp.ENV, stdout=open(cp.RUN / "serve.log", "w"), stderr=subprocess.STDOUT)
    cp.wait(lambda: cp.port_open(8080), "rustie-serve :8080")


def stop_all():
    cp.stop_cluster()


def one_pass(qs):
    per = {}
    for q in qs:
        t0 = time.perf_counter()
        status, total, ms = cp.search(q)
        wall = (time.perf_counter() - t0) * 1000
        if status != 200:
            per[q] = dict(ok=False, status=status, ms=wall)
        else:
            per[q] = dict(ok=True, hits=total, ms=wall, took_ms=ms)
    return per


def summarize(per):
    ok = [v for v in per.values() if v["ok"]]
    return dict(ok=len(ok), failed=len(per) - len(ok), total_ms=round(sum(v["ms"] for v in per.values()), 1),
                hits={q: v.get("hits") for q, v in per.items()})


def wait_split_cache_full(topology, want_bytes, timeout=180):
    end = time.time() + timeout
    while time.time() < end:
        if split_cache_bytes(topology) >= want_bytes:
            return True
        time.sleep(2)
    return False


def run_config(mode, topology, qs, hot_reps, index_bytes):
    cfg = make_cfg(mode, topology)
    cachey = MODES[mode]["cachey"]
    wipe_disk_state(topology)
    res = dict(mode=mode, topology=topology)
    start(topology, cfg, cachey)
    m0 = cp.metrics() if cachey else None
    # -- cold
    t0 = time.perf_counter()
    cold = one_pass(qs)
    res["cold"] = summarize(cold); res["cold"]["per_query_ms"] = {q: round(v["ms"], 1) for q, v in cold.items()}
    if cachey:
        res["cold"]["cachey"] = cp.delta(m0, cp.metrics())
    if MODES[mode]["split"]:
        res["split_cache_filled"] = wait_split_cache_full(topology, int(index_bytes * 0.9))
        res["split_cache_bytes"] = split_cache_bytes(topology)
    # -- hot (same processes)
    hot = [summarize(one_pass(qs))["total_ms"] for _ in range(hot_reps)]
    res["hot_total_ms"] = dict(median=round(st.median(hot), 1), min=min(hot), all=hot)
    # -- restart: memory empty, disk caches kept
    stop_all()
    start(topology, cfg, cachey)
    m1 = cp.metrics() if cachey else None
    rs = one_pass(qs)
    res["restart"] = summarize(rs); res["restart"]["per_query_ms"] = {q: round(v["ms"], 1) for q, v in rs.items()}
    if cachey:
        res["restart"]["cachey"] = cp.delta(m1, cp.metrics())
    stop_all()
    return res


def fmt(res):
    c, r, h = res["cold"], res["restart"], res["hot_total_ms"]
    extra = ""
    if "cachey" in c:
        extra = (f"  cachey cold: access={c['cachey']['access']} dl={c['cachey']['download']}"
                 f" | restart: access={r['cachey']['access']} dl={r['cachey']['download']}")
    if "split_cache_bytes" in res:
        extra = f"  split cache on disk: {res['split_cache_bytes'] / 2**20:.0f} MiB (filled={res['split_cache_filled']})"
    fails = c["failed"] + r["failed"]
    return (f"{res['topology']:8} {res['mode']:12} cold {c['total_ms']:8.0f} ms | restart {r['total_ms']:8.0f} ms |"
            f" hot(median of {len(h['all'])}) {h['median']:7.0f} ms | failed={fails}{extra}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=2)
    ap.add_argument("--hot", type=int, default=5)
    ap.add_argument("--topology", choices=["cluster", "embedded", "both"], default="both")
    a = ap.parse_args()
    if any(cp.port_open(p) for _, p in cp.NODES) or cp.port_open(8080):
        sys.exit("a cluster/serve is already running; stop it first: scripts/local-cluster.sh down")
    cp.sh(["docker", "start", "rustie-postgres"], stdout=subprocess.DEVNULL)
    cp.wait(lambda: subprocess.run(["docker", "exec", "rustie-postgres", "pg_isready", "-U", "rustie"], capture_output=True).returncode == 0, "postgres", 30)
    qs = cp.load_queries()
    # index size (bytes on Contabo) from the metastore, to know when the split cache is "full"
    out = subprocess.run(["docker", "exec", "rustie-postgres", "psql", "-U", "rustie", "-d", "rustie", "-Atc",
                          "select coalesce(sum(split_size_bytes),0) from splits s join indexes i using (index_uid) where i.index_id='wiki5k-contabo' and s.split_state='Published'"],
                         capture_output=True, text=True).stdout.strip()
    index_bytes = int(out or 0) or 200 * 2**20
    print(f"queries: {len(qs)}   index ~{index_bytes / 2**20:.0f} MiB   binaries: {cp.BIN}")
    plan = []
    if a.topology in ("cluster", "both"):
        for _ in range(a.rounds):
            plan += [("direct", "cluster"), ("cachey", "cluster")]
        plan += [("split_cache", "cluster")]
    if a.topology in ("embedded", "both"):
        for _ in range(a.rounds):
            plan += [("direct", "embedded"), ("split_cache", "embedded"), ("cachey", "embedded")]
    results = []
    try:
        for mode, topo in plan:
            print(f"\n>>> {topo} / {mode} ...", flush=True)
            r = run_config(mode, topo, qs, a.hot, index_bytes)
            results.append(r)
            print("   ", fmt(r), flush=True)
    finally:
        stop_all()
        cp.sh(cp.COMPOSE + ["up", "-d", "cachey"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # hit counts must agree everywhere
    ref = results[0]["cold"]["hits"]
    same = all(r["cold"]["hits"] == ref and r["restart"]["hits"] == ref for r in results)
    RESULTS.mkdir(parents=True, exist_ok=True)
    out_path = RESULTS / f"cachey_bench_{time.strftime('%Y%m%d_%H%M%S')}.json"
    out_path.write_text(json.dumps(dict(queries=qs, results=results, hits_identical=same), indent=1))
    print("\n=========== summary (sum of query wall times, ms) ===========")
    for r in results:
        print(fmt(r))
    print(f"\ntotal_hits identical across every run/mode: {same}\nsaved: {out_path}")
    sys.exit(0 if same else 1)


if __name__ == "__main__":
    main()
