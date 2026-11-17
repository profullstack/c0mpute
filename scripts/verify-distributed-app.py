#!/usr/bin/env python3
"""Read-only, dependency-free live acceptance gate for distributed c0mpute apps.

This does NOT deploy containers, kill nodes, bypass authentication, or prove
operator independence or Docker host isolation. It checks that independent
configured gateway URLs agree about the same signed application record and
the currently advertised holder/primary state, and that the HTTP app works.

Usage:
  python3 scripts/verify-distributed-app.py \
    --app-id 1dc2367caf3f51b0c249 \
    --gateway https://gateway-a.example.org \
    --gateway https://gateway-b.example.org \
    --gateway https://gateway-c.example.org \
    --probe

Failover check, after an operator has deliberately stopped the old primary:
  python3 scripts/verify-distributed-app.py ... \
    --previous-primary 12D3... --previous-epoch 4

Do not paste or print user secrets. The gateway API exposes public records.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import unittest
from urllib.error import HTTPError, URLError
from urllib.parse import urlparse
from urllib.request import Request, urlopen

APP_ID = re.compile(r"^[0-9a-f]{20}$")
MAX_REPLY = 512 * 1024


def require(ok: bool, reason: str) -> None:
    if not ok:
        raise ValueError(reason)


def base_url(value: str) -> str:
    parsed = urlparse(value)
    require(parsed.scheme in ("http", "https"), "gateway needs an HTTP(S) URL")
    require(bool(parsed.hostname), "gateway has no hostname")
    require(parsed.username is None and parsed.password is None, "gateway URL must not contain credentials")
    require(not parsed.query and not parsed.fragment and parsed.path in ("", "/"), "gateway URL must be the origin")
    require(
        parsed.scheme == "https" or parsed.hostname in ("127.0.0.1", "localhost", "::1"),
        "plain HTTP is allowed only for local loopback test gateways",
    )
    return value.rstrip("/")


def fetch_json(url: str, timeout: float) -> dict:
    req = Request(url, headers={"Accept": "application/json"}, method="GET")
    with urlopen(req, timeout=timeout) as resp:  # uses normal system CA verification
        require(200 <= resp.status < 300, f"gateway HTTP {resp.status}")
        data = resp.read(MAX_REPLY + 1)
        require(len(data) <= MAX_REPLY, "oversized gateway response")
    obj = json.loads(data)
    require(isinstance(obj, dict), "gateway response is not an object")
    return obj


def verify_view(doc: dict, app_id: str, min_holders: int, previous: tuple[str, int] | None = None) -> dict:
    require(isinstance(doc, dict), "invalid gateway response")
    app, record = doc.get("app"), doc.get("record")
    require(isinstance(app, dict) and isinstance(record, dict), "missing app or signed record")
    require(app.get("app_id") == app_id and record.get("app_id") == app_id, "gateway returned wrong app id")
    require(app.get("owner") == record.get("owner"), "owner mismatch")
    require(app.get("seq") == record.get("seq"), "record generation mismatch")
    require(isinstance(record.get("sig"), str) and bool(record["sig"]), "missing owner signature")
    require(record.get("deleted") is not True, "app is deleted")
    replicas = app.get("replicas")
    require(type(replicas) is int and min_holders <= replicas <= 5, "incorrect replica target")
    require(replicas == record.get("replicas"), "record replica target mismatch")
    holders = app.get("holders")
    require(isinstance(holders, list), "missing holder list")
    require(
        len(holders) >= min_holders and all(isinstance(p, str) and bool(p) for p in holders),
        "not enough valid app holders",
    )
    require(len(set(holders)) == len(holders), "duplicate holder peer identity")
    primary = app.get("primary")
    require(isinstance(primary, str) and primary in holders, "primary not among holders")
    require(app.get("healthy") is True, "primary not healthy")
    require(not app.get("error"), "node reported app error")
    epoch = app.get("epoch")
    require(type(epoch) is int and epoch >= 0, "invalid primary epoch")
    if previous is not None:
        old_primary, old_epoch = previous
        require(primary != old_primary, "failover not observed: old primary still serving")
        require(epoch > old_epoch, "failover epoch did not advance")
    return {
        "app_id": app_id,
        "owner": record["owner"],
        "seq": record["seq"],
        "signed_record": json.dumps(record, sort_keys=True, separators=(",", ":")),
        "holders": sorted(holders),
        "primary": primary,
        "epoch": epoch,
        "healthy": True,
        "health_path": record.get("health_path", "/"),
    }


def verify_consensus(views: list[dict]) -> dict:
    require(len(views) >= 2, "at least two independent gateway URLs are required")
    first = views[0]
    for other in views[1:]:
        require(other["signed_record"] == first["signed_record"], "gateways disagree about signed record")
        require(other["holders"] == first["holders"], "gateways disagree about holders")
        require(other["primary"] == first["primary"], "gateways disagree about primary")
        require(other["epoch"] == first["epoch"], "gateways disagree about promotion epoch")
    return {k: v for k, v in first.items() if k != "signed_record"}


def verify_http(url: str, app_id: str, health_path: str, timeout: float) -> None:
    require(isinstance(health_path, str) and health_path.startswith("/"), "invalid health path")
    require("?" not in health_path and "#" not in health_path and "\\" not in health_path, "unsafe health path")
    require(".." not in health_path.split("/"), "health path traversal")
    probe = f"{url}/a/{app_id}/{health_path.lstrip('/')}"
    req = Request(probe, headers={"Accept": "*/*"}, method="GET")
    with urlopen(req, timeout=timeout) as resp:
        require(200 <= resp.status < 300, f"app health HTTP {resp.status}")
        resp.read(1)  # do not download or log application data


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--gateway", action="append", required=True, help="Gateway root URL; repeat on separate hosts")
    parser.add_argument("--app-id", required=True, help="Owner-signed 20-character lowercase hex ID")
    parser.add_argument("--min-holders", type=int, default=3)
    parser.add_argument("--previous-primary", help="Optional old primary peer ID for operator-driven failover test")
    parser.add_argument("--previous-epoch", type=int, help="Epoch before manual primary failure")
    parser.add_argument("--probe", action="store_true", help="GET the proxied app health endpoint on each gateway")
    parser.add_argument("--timeout", type=float, default=8.0)
    args = parser.parse_args()
    try:
        require(APP_ID.fullmatch(args.app_id) is not None, "invalid app ID")
        require(2 <= args.min_holders <= 5, "min-holders must be between 2 and 5")
        require(0 < args.timeout <= 30, "timeout must be 0..30 seconds")
        require((args.previous_primary is None) == (args.previous_epoch is None),
                "previous-primary and previous-epoch must be supplied together")
        gateways = [base_url(x) for x in args.gateway]
        require(len(gateways) >= 2 and len(set(gateways)) == len(gateways),
                "provide at least two distinct gateway URLs")
        previous = (args.previous_primary, args.previous_epoch) if args.previous_primary else None
        views = []
        for gateway in gateways:
            doc = fetch_json(f"{gateway}/apps/v1/apps/{args.app_id}", args.timeout)
            view = verify_view(doc, args.app_id, args.min_holders, previous)
            if args.probe:
                verify_http(gateway, args.app_id, view["health_path"], args.timeout)
            views.append(view)
        verified = verify_consensus(views)
        print(json.dumps({"result": "passed", "gateways": gateways, **verified}, sort_keys=True))
        return 0
    except (ValueError, HTTPError, URLError, TimeoutError, OSError, json.JSONDecodeError) as exc:
        print(json.dumps({"result": "failed", "error": str(exc)}), file=sys.stderr)
        return 1


class ValidationTests(unittest.TestCase):
    APP = "a" * 20

    @classmethod
    def fixture(cls):
        record = {"app_id": cls.APP, "owner": "owner", "seq": 42,
                  "sig": "owner-signed-record", "replicas": 3, "health_path": "/healthz"}
        app = {"app_id": cls.APP, "owner": "owner", "seq": 42,
               "replicas": 3, "holders": ["peer1", "peer2", "peer3"],
               "primary": "peer1", "epoch": 7, "healthy": True}
        return {"app": app, "record": record}

    def test_good_three_holders_and_multiple_gateways(self):
        a = verify_view(self.fixture(), self.APP, 3)
        b = verify_view(self.fixture(), self.APP, 3)
        self.assertEqual(verify_consensus([a, b])["primary"], "peer1")

    def test_failover_requires_new_peer_and_epoch(self):
        data = self.fixture()
        with self.assertRaises(ValueError):
            verify_view(data, self.APP, 3, ("peer1", 6))
        data["app"]["primary"] = "peer2"
        data["app"]["epoch"] = 8
        self.assertEqual(verify_view(data, self.APP, 3, ("peer1", 7))["epoch"], 8)

    def test_rejects_false_health_and_fake_replica_claims(self):
        for field, value in [("healthy", False), ("holders", ["peer1"]),
                             ("primary", "peer9"), ("epoch", -1)]:
            data = self.fixture()
            data["app"][field] = value
            with self.assertRaises(ValueError, msg=field):
                verify_view(data, self.APP, 3)

    def test_rejects_duplicate_holders_and_conflicting_gateways(self):
        data = self.fixture()
        data["app"]["holders"] = ["peer1", "peer1", "peer3"]
        with self.assertRaises(ValueError):
            verify_view(data, self.APP, 3)
        a = verify_view(self.fixture(), self.APP, 3)
        other = self.fixture()
        other["app"]["epoch"] = 8
        with self.assertRaises(ValueError):
            verify_consensus([a, verify_view(other, self.APP, 3)])

    def test_rejects_different_records_and_missing_signature(self):
        data = self.fixture()
        data["record"]["sig"] = ""
        with self.assertRaises(ValueError):
            verify_view(data, self.APP, 3)
        a = verify_view(self.fixture(), self.APP, 3)
        other = self.fixture()
        other["record"]["sig"] = "new-sig"
        with self.assertRaises(ValueError):
            verify_consensus([a, verify_view(other, self.APP, 3)])

    def test_url_validation_rejects_credential_and_insecure_remote(self):
        self.assertEqual(base_url("https://gateway.example.org/"), "https://gateway.example.org")
        self.assertEqual(base_url("http://127.0.0.1:7777"), "http://127.0.0.1:7777")
        for url in ["http://public.example.org", "https://admin:pw@ex.org",
                    "https://ex.org/path", "ftp://ex.org", "http://ex.org/?q=x"]:
            with self.assertRaises(ValueError):
                base_url(url)


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.argv = [sys.argv[0]]
        unittest.main()
    else:
        raise SystemExit(main())
