#!/usr/bin/env python3
"""Prove that rustie-node leaves read `.split` data through the shared Cachey pool.

Uses Cachey's own counters (`/metrics`: page_request_total access / cache_hit / download, where
`download` is a real S3 page fetch) plus query results, over these phases:

  1 cold pool      fresh Cachey, fresh nodes    -> pages accessed AND downloaded from S3
  2 warm nodes     same queries again           -> (informational: node-local caches absorb them)
  3 warm pool      restart nodes, Cachey kept   -> pages accessed, ZERO new S3 downloads
  4 failover       kill node-2                  -> its splits move to nodes that never read them;
                                                   they are served from the pool, ZERO downloads
  5 negative       nodes up, then Cachey stopped, fallback off
                                                -> queries FAIL (no silent bypass to S3)

Nodes run with --no-cachey-fallback so a bypass cannot hide. Total hits must be identical in
phases 1, 3 and 4. Run:  python3 scripts/cachey_proof.py
"""
import json, os, re, signal, socket, subprocess, sys, time, urllib.error, urllib.parse, urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)
RUN = ROOT / ".run" / "proof"
CFG_SRC = Path(os.environ.get("DEPLOY_CONFIG", "configs/rustie-deploy.local.yaml"))
CFG = RUN / "proof-deploy.yaml"
BIN = ROOT / "target" / os.environ.get("PROFILE", "debug")
CACHEY = "http://127.0.0.1:9020"
SERVE = "http://127.0.0.1:8080"
NODES = [("rustie-node-1", 7281), ("rustie-node-2", 7381), ("rustie-node-3", 7481)]
COMPOSE = ["docker", "compose", "-f", "docker-compose.local.yml"]


def load_env():
    env = dict(os.environ)
    envfile = ROOT / ".env"
    if envfile.exists():
        for line in envfile.read_text().splitlines():
            m = re.match(r"\s*([A-Za-z_][A-Za-z0-9_]*)=(.*)", line)
            if m:
                env[m.group(1)] = m.group(2).split(" #")[0].strip().strip("'\"")
    env.setdefault("PG_PASSWORD", "rustie")
    env["AWS_EC2_METADATA_DISABLED"] = "true"
    missing = [k for k in ("S3_ENDPOINT", "S3_REGION", "S3_ACCESS_KEY", "S3_SECRET_KEY") if not env.get(k)]
    if missing:
        sys.exit(f"missing in .env: {', '.join(missing)} (see .env.example)")
    return env


ENV = load_env()


def sh(cmd, **kw):
    return subprocess.run(cmd, env=ENV, check=True, text=True, **kw)


def port_open(port, host="127.0.0.1"):
    with socket.socket() as s:
        s.settimeout(0.5)
        return s.connect_ex((host, port)) == 0


def wait(pred, what, timeout=90):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return
        time.sleep(0.5)
    sys.exit(f"timed out waiting for {what}; see {RUN}/*.log")


def http(url, timeout=60):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
    except Exception as e:  # connection refused, timeout
        return 0, str(e)


def metrics():
    status, text = http(f"{CACHEY}/metrics", timeout=5)
    if status != 200:
        return {"access": 0, "hit": 0, "download": 0}
    tot = {"access": 0, "cache_hit": 0, "download": 0}
    for line in text.splitlines():
        m = re.match(r'cachey_page_request_total\{[^}]*type="(\w+)"[^}]*\}\s+([\d.e+]+)', line)
        if m and m.group(1) in tot:
            tot[m.group(1)] += int(float(m.group(2)))
    return {"access": tot["access"], "hit": tot["cache_hit"], "download": tot["download"]}


def delta(a, b):
    return {k: b[k] - a[k] for k in a}


# ---------------------------------------------------------------- cluster control
procs = {}


def start_cluster(fallback=False):
    RUN.mkdir(parents=True, exist_ok=True)
    for name, _ in NODES:
        cmd = [str(BIN / "rustie-node"), "--deploy-config", str(CFG), "--node-id", name]
        if not fallback:
            cmd.append("--no-cachey-fallback")
        procs[name] = subprocess.Popen(cmd, env=ENV, stdout=open(RUN / f"{name}.log", "w"), stderr=subprocess.STDOUT)
    for name, port in NODES:
        wait(lambda p=port: port_open(p), f"{name} gRPC :{port}")
    procs["serve"] = subprocess.Popen([str(BIN / "rustie-serve"), "--deploy-config", str(CFG)], env=ENV,
                                      stdout=open(RUN / "serve.log", "w"), stderr=subprocess.STDOUT)
    wait(lambda: port_open(8080), "rustie-serve :8080")


def stop(name):
    p = procs.pop(name, None)
    if p and p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(timeout=20)
        except subprocess.TimeoutExpired:
            p.kill()


def stop_cluster():
    for name in ["serve"] + [n for n, _ in NODES]:
        stop(name)


# ---------------------------------------------------------------- queries
def load_queries():
    qs = []
    for f in sorted((ROOT / "benchmarks/wiki5k/queries").glob("*.txt")):
        qs += [l.strip() for l in f.read_text().splitlines() if l.strip() and not l.startswith("#")]
    qs += ["[lemma=president]", "[word=born] >nsubjpass []", "[entity=I-ORG] >nmod_of []"]
    return qs


def search(q):
    url = SERVE + "/v1/search?" + urllib.parse.urlencode({"q": q, "limit": 5, "count": "true"})
    status, body = http(url)
    if status == 200:
        j = json.loads(body)
        return status, j["total_hits"], j["took_ms"]
    return status, None, 0


def run_round(qs, retry_s=0):
    ok, failed, hits, took = 0, [], {}, 0
    t0 = time.time()
    for q in qs:
        deadline = time.time() + retry_s
        while True:
            status, total, ms = search(q)
            if status == 200 or time.time() >= deadline or 400 <= status < 500:
                break
            time.sleep(1)
        if status == 200:
            ok += 1; hits[q] = total; took += ms
        else:
            failed.append((q, status))
    return {"ok": ok, "failed": failed, "hits": hits, "took_ms": took, "wall_s": round(time.time() - t0, 2)}


def show(title, r, d):
    print(f"\n[{title}]  queries ok={r['ok']} failed={len(r['failed'])}  wall={r['wall_s']}s  sum(took_ms)={r['took_ms']}")
    print(f"    cachey pages: accessed={d['access']}  cache hits={d['hit']}  downloaded from S3={d['download']}"
          f" (~{d['download'] * 16} MiB)")
    for q, s in r["failed"]:
        print(f"    FAILED (HTTP {s}): {q}")


def main():
    if any(port_open(p) for _, p in NODES) or port_open(8080):
        sys.exit("a cluster is already running on :7281/:8080; stop it first: scripts/local-cluster.sh down")
    RUN.mkdir(parents=True, exist_ok=True)
    src = CFG_SRC.read_text()
    src = re.sub(r"(cachey:\n  enabled: )\w+", r"\1true", src)
    src = re.sub(r"(split_cache:\n  enabled: )\w+", r"\1false", src)
    CFG.write_text(src)
    print("building binaries ...")
    sh(["cargo", "build"] + (["--release"] if BIN.name == "release" else []) + ["-p", "rustie-node", "-p", "rustie-search", "--bins"],
       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sh(["docker", "start", "rustie-postgres"], stdout=subprocess.DEVNULL)
    wait(lambda: subprocess.run(["docker", "exec", "rustie-postgres", "pg_isready", "-U", "rustie"], capture_output=True).returncode == 0, "postgres", 30)

    print("resetting Cachey (fresh, empty pool) ...")
    sh(COMPOSE + ["down", "-v"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sh(COMPOSE + ["up", "-d", "cachey"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait(lambda: http(f"{CACHEY}/stats", 3)[0] == 200, "cachey /stats", 60)

    verdicts = []
    qs = load_queries()
    try:
        start_cluster()
        m0 = metrics()
        r1 = run_round(qs, retry_s=60)
        m1 = metrics(); d1 = delta(m0, m1)
        bad = {q for q, s in r1["failed"] if 400 <= s < 500}
        if bad:
            print("dropping invalid patterns:", *sorted(bad), sep="\n    ")
            qs = [q for q in qs if q not in bad]
            r1["failed"] = [(q, s) for q, s in r1["failed"] if q not in bad]
        show("1 cold pool: fresh Cachey + fresh nodes", r1, d1)
        verdicts.append(("nodes read splits through Cachey (pages accessed > 0)", d1["access"] > 0))
        verdicts.append(("cold pool filled from S3 (downloads > 0)", d1["download"] > 0))
        verdicts.append(("all queries succeeded with fallback disabled", not r1["failed"]))

        r2 = run_round(qs); m2 = metrics()
        show("2 same queries, warm nodes (node-local caches absorb them)", r2, delta(m1, m2))

        stop_cluster(); start_cluster()
        m3 = metrics()
        r3 = run_round(qs, retry_s=30); m4 = metrics(); d3 = delta(m3, m4)
        show("3 warm pool: nodes restarted (empty memory), Cachey kept", r3, d3)
        verdicts.append(("restarted nodes still read via Cachey (accessed > 0)", d3["access"] > 0))
        verdicts.append(("warm pool: ZERO S3 downloads", d3["download"] == 0))
        verdicts.append(("warm pool: every accessed page was a cache hit", d3["hit"] == d3["access"] and d3["access"] > 0))
        verdicts.append(("total_hits identical to phase 1", r3["hits"] == r1["hits"] and not r3["failed"]))

        m5 = metrics()
        stop("rustie-node-2")
        print("\n... node-2 killed; waiting for the cluster to re-place its splits ...")
        r4 = run_round(qs, retry_s=90)
        # re-run once more so every query has definitely run on the surviving nodes
        r4 = run_round(qs, retry_s=30) if r4["failed"] else r4
        m6 = metrics(); d4 = delta(m5, m6)
        show("4 failover: node-2's splits now served by nodes that never read them", r4, d4)
        verdicts.append(("failover: queries succeed on 2 nodes", not r4["failed"]))
        verdicts.append(("failover: ZERO S3 downloads (pool shared across nodes)", d4["download"] == 0))
        verdicts.append(("total_hits identical after failover", r4["hits"] == r1["hits"]))

        stop_cluster()
        print("\n... fresh nodes (empty memory), then Cachey stopped; fallback is off ...")
        start_cluster(fallback=False)
        sh(["docker", "stop", "rustie-cachey-s3"], stdout=subprocess.DEVNULL)
        r5 = run_round(qs[:3])
        show("5 negative control: Cachey down, fallback off", r5, {"access": 0, "hit": 0, "download": 0})
        verdicts.append(("Cachey down => queries FAIL (no silent bypass to S3)", len(r5["failed"]) == len(qs[:3])))
    finally:
        stop_cluster()
        sh(COMPOSE + ["up", "-d", "cachey"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    print("\n================ verdict ================")
    for name, ok in verdicts:
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    sys.exit(0 if all(ok for _, ok in verdicts) else 1)


if __name__ == "__main__":
    main()
