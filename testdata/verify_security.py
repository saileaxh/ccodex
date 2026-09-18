"""E2E security verification against a fresh relay on 127.0.0.1:8321."""
import json
import sys
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:8321"
ok = True


def call(method, path, body=None, bearer=None):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
    )
    req.add_header("Content-Type", "application/json")
    if bearer:
        req.add_header("Authorization", f"Bearer {bearer}")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, resp.read().decode(), resp.headers
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), e.headers


def check(name, cond, detail=""):
    global ok
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail else ""))
    if not cond:
        ok = False


# 1. /health is minimal
s, b, _ = call("GET", "/health")
check("health minimal", s == 200 and json.loads(b) == {"status": "ok"}, b[:80])

# 2. security headers on HTML and API
s, _, h = call("GET", "/")
check(
    "html security headers",
    h.get("X-Content-Type-Options") == "nosniff"
    and h.get("X-Frame-Options") == "DENY"
    and "default-src 'self'" in (h.get("Content-Security-Policy") or ""),
)
s, _, h = call("GET", "/health")
check("api nosniff header", h.get("X-Content-Type-Options") == "nosniff")

# 3. admin API refuses before setup
s, b, _ = call("GET", "/admin/api/overview")
check("admin 401 pre-setup", s == 401)

# 4. first-run setup is open (documented first-run claim; no setup token)
s, b, _ = call("POST", "/admin/api/admin-key", {"new": "panel-secret-1"})
check("open first-run setup", s == 200 and json.loads(b).get("ok") is True, b[:80])

# 5. admin key works; argon2id stored on disk
s, b, _ = call("GET", "/admin/api/overview", bearer="panel-secret-1")
check("admin login works", s == 200)
admin_json = open(r"E:\repos\ccodex\target\sec_test\admin.json", encoding="utf-8").read()
check("argon2id on disk", "$argon2id$" in admin_json and "panel-secret-1" not in admin_json)

# 7. create key -> full value once; list -> masked
s, b, _ = call("POST", "/admin/api/keys", {"name": "t1"}, bearer="panel-secret-1")
full = json.loads(b).get("key", "")
check("create returns full key", full.startswith("sk-") and len(full) > 90)
s, b, _ = call("GET", "/admin/api/keys", bearer="panel-secret-1")
listed = json.loads(b)["keys"][0]["key"]
check("list masked", "…" in listed and full not in b, listed)

# 8. /v1 now requires auth (open mode ended once a key exists)
s, b, _ = call("POST", "/v1/responses", {"model": "gpt-5.5", "input": []})
check("v1 unauth 401", s == 401, b[:60])
s, b, _ = call("POST", "/v1/responses", {"model": "gpt-5.5", "input": []}, bearer=full)
check("v1 with key passes auth", s != 401, f"status={s} (no account upstream is fine)")

# 9. admin throttle: 5 failures then lockout (even the correct key gets 429)
for i in range(5):
    s, _, _ = call("GET", "/admin/api/overview", bearer="wrong-key")
    assert s == 401, f"attempt {i}: {s}"
s, b, _ = call("GET", "/admin/api/overview", bearer="panel-secret-1")
check("throttle locks out", s == 429, b[:70])

# 10. oversized body rejected 413 (streaming 129MB would OOM without the cap)
big = b'{"model":"gpt-5.5","input":"' + b"x" * (129 * 1024 * 1024) + b'"}'
req = urllib.request.Request(BASE + "/v1/responses", data=big, method="POST")
req.add_header("Content-Type", "application/json")
req.add_header("Authorization", f"Bearer {full}")
try:
    with urllib.request.urlopen(req, timeout=30) as resp:
        check("129MB body rejected", False, f"got {resp.status}")
except urllib.error.HTTPError as e:
    check("129MB body rejected", e.code == 413, f"status={e.code}")

print("SECURITY E2E:", "ALL OK" if ok else "FAILURES")
sys.exit(0 if ok else 1)
