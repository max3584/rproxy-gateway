#!/usr/bin/env python3
"""The compatibility matrix (e2e.yml, job compat), worked out from what is released today, so the
newest Kubernetes and Gateway API are always in it without editing the workflow:

- every Gateway API minor from v1.0 (its newest patch), and the next minor's release candidate, on
  the newest Kubernetes;
- every Kubernetes minor the chart allows (kubeVersion) up to the newest (its newest patch), each
  with the newest Gateway API whose CRDs its API server accepts (CRD_NEEDS);
- the next Kubernetes minor's beta / release candidate, with the newest Gateway API (recorded);
- Kubernetes below the chart's range, each with the Gateway API of its time (FLOOR; recorded, the
  chart's kubeVersion is relaxed for these runs only).

strict: the latest two Gateway API minors (the conformance badge counts them) on a Kubernetes the
chart allows must pass conformance core; everything else is recorded. A Kubernetes release with
no kindest/node image yet (the newest patch often comes out before kind publishes it, and the next
minor's beta / rc never gets one) has its node image built once from the release and kept in GHCR
(NODE_REPO; the job node-images); older minors use kind's newest image of the minor.

Prints {"include": [...], "builds": [...], "kind": "<newest kind release>"} as JSON (GITHUB_TOKEN raises the GitHub
API rate limit). Run it locally to see the matrix: scripts/compat-matrix.py | jq
"""
import json
import os
import re
import sys
import urllib.request

# the oldest Kubernetes minor each Gateway API minor's CRDs install on: they use CEL functions older
# API servers do not have (v1.5: isIP, v1.6: dns1123Label). A newer minor is assumed to need what the
# newest known one needs (CRD_NEEDS_DEFAULT) until it is added here
CRD_NEEDS = {(1, 0): 23, (1, 1): 23, (1, 2): 25, (1, 3): 25, (1, 4): 25, (1, 5): 31, (1, 6): 32}
CRD_NEEDS_DEFAULT = 32
# below the chart's range: (Kubernetes minor, Gateway API minor), what it was paired with when it was
# current. Found so far: 1.26 passes the e2e, 1.23-1.25 lack a PodDisruptionBudget field, 1.22 does
# not take the CRDs (README "Supported versions")
FLOOR = [(22, (1, 0)), (23, (1, 0)), (24, (1, 0)), (25, (1, 2)), (25, (1, 4)), (26, (1, 2)), (26, (1, 4)),
         (27, (1, 2)), (27, (1, 4)), (28, (1, 2)), (28, (1, 4))]
NODE_REPO = "ghcr.io/max3584/rproxy-gateway/kind-node"
SEMVER = re.compile(r"^v(\d+)\.(\d+)\.(\d+)(?:-(alpha|beta|rc)\.(\d+))?$")


def get(url, token=None):
    req = urllib.request.Request(url, headers={"User-Agent": "rproxy-gateway-compat"})
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read().decode()


def parse(tag):
    m = SEMVER.match(tag.strip())
    if not m:
        return None
    major, minor, patch, pre, n = m.groups()
    # a release sorts after its pre-releases
    order = {"alpha": 0, "beta": 1, "rc": 2, None: 3}[pre]
    return (int(major), int(minor), int(patch), order, int(n or 0)), pre


def chart_floor():
    with open(os.path.join(os.path.dirname(__file__), "..", "charts", "rproxy-gateway", "Chart.yaml")) as f:
        m = re.search(r'^kubeVersion:\s*"?>=\s*1\.(\d+)', f.read(), re.M)
    return int(m.group(1))


def kindest_tags():
    tags, url = set(), "https://hub.docker.com/v2/repositories/kindest/node/tags?page_size=100"
    while url:
        page = json.loads(get(url))
        tags |= {t["name"] for t in page["results"]}
        url = page.get("next")
    return tags


def newest_image(images, minor):
    """kind's newest image of a Kubernetes minor (releases only), or None."""
    tags = sorted((parse(t)[0], t) for t in images if parse(t) and parse(t)[1] is None and parse(t)[0][1] == minor)
    return tags[-1][1] if tags else None


def k8s_versions(floor, images):
    """Each minor from `floor` up to the newest (kind's newest image of the minor; the newest minor at its
    newest patch), and the next minor's beta / rc."""
    newest = get("https://dl.k8s.io/release/stable.txt").strip()
    top = parse(newest)[0][1]
    stable = {}
    for minor in range(floor, top):
        stable[minor] = newest_image(images, minor) or get(f"https://dl.k8s.io/release/stable-1.{minor}.txt").strip()
    stable[top] = newest
    nxt = get("https://dl.k8s.io/release/latest.txt").strip()
    key, pre = parse(nxt)
    upcoming = nxt if pre in ("beta", "rc") and key[1] > top else None
    return stable, top, upcoming


def gwapi_versions(token):
    """The newest patch of each Gateway API minor from v1.0, and the next minor's release candidate."""
    releases = json.loads(get("https://api.github.com/repos/kubernetes-sigs/gateway-api/releases?per_page=100", token))
    stable, pres = {}, []
    for r in releases:
        if r.get("draft"):
            continue
        p = parse(r["tag_name"])
        if not p:
            continue
        key, pre = p
        minor = key[:2]
        if minor < (1, 0):
            continue
        if pre is None:
            if minor not in stable or key > parse(stable[minor])[0]:
                stable[minor] = r["tag_name"]
        elif pre == "rc":
            pres.append((key, r["tag_name"]))
    top = max(stable)
    upcoming = max((p for p in pres if p[0][:2] > top), default=None)
    return stable, upcoming[1] if upcoming else None


def needs(gw_minor):
    return CRD_NEEDS.get(gw_minor, CRD_NEEDS_DEFAULT)


def main():
    token = os.environ.get("GITHUB_TOKEN")
    floor = chart_floor()
    images = kindest_tags()
    k8s, k_top, k_next = k8s_versions(floor, images)
    gw, gw_next = gwapi_versions(token)
    gw_minors = sorted(gw)
    latest_two = set(gw_minors[-2:])
    newest_k8s = k8s[k_top]

    include, seen = [], set()

    def add(k, g, kind):
        if (k, g) in seen:
            return
        seen.add((k, g))
        entry = {"k8s": k, "gwapi": g, "image": f"kindest/node:{k}" if k in images else f"{NODE_REPO}:{k}"}
        if kind == "strict":
            entry["strict"] = True
        if kind == "floor":
            entry["floor"] = True
        include.append(entry)

    # every Gateway API minor (and the next rc) on the newest Kubernetes
    for minor in gw_minors:
        add(newest_k8s, gw[minor], "strict" if minor in latest_two else "recorded")
    if gw_next:
        add(newest_k8s, gw_next, "recorded")
    # every Kubernetes minor the chart allows, with the newest Gateway API its API server takes
    for minor in range(floor, k_top):
        fits = [m for m in gw_minors if needs(m) <= minor]
        if fits:
            add(k8s[minor], gw[fits[-1]], "strict" if fits[-1] in latest_two else "recorded")
    # the next Kubernetes minor (beta / rc), recorded
    if k_next:
        add(k_next, gw[gw_minors[-1]], "recorded")
    # below the chart's range
    for minor, gw_minor in FLOOR:
        if minor >= floor or gw_minor not in gw:
            continue
        image = newest_image(images, minor)
        if image:
            add(image, gw[gw_minor], "floor")

    kind = json.loads(get("https://api.github.com/repos/kubernetes-sigs/kind/releases/latest", token))["tag_name"]
    builds = sorted({e["k8s"] for e in include if e["image"].startswith(NODE_REPO)})
    json.dump({"include": include, "builds": builds, "kind": kind}, sys.stdout)
    print()


if __name__ == "__main__":
    main()
