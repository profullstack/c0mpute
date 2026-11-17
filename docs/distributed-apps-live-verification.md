# Distributed apps: live gateway, replica and failover verification

The **shipping** `c0mpute apps` protocol supports owner-signed OCI
app records, three-holder P2P placement, one promoted primary, signed
updates, sealed state snapshots, and gateway proxying. Before claiming
production hosting availability, verify the behavior on actual
independent machines.

The lightweight, read-only `scripts/verify-distributed-app.py` uses
only Python's standard library. It is safe to run from an authorized
operator workstation; it never deploys, deletes, mutates, or kills
containers.

## First deploy a controlled test app

Create an HTTP server with a `/healthz` endpoint, then deploy it
using the existing CLI:

```sh
c0mpute apps deploy ./api --name api --port 8080 --state /data
c0mpute apps status api
```

Obtain the 20-character app ID and choose **at least two distinct**
gateway origins on **physically/network independent hosts**.
The script cannot prove their independence from URLs alone.

```sh
python3 scripts/verify-distributed-app.py \
  --app-id 1dc2367caf3f51b0c249 \
  --gateway https://gateway-a.example.org \
  --gateway https://gateway-b.example.org \
  --gateway https://gateway-c.example.org \
  --min-holders 3 --probe
```

The script checks: each gateway returns the expected signed record,
all gateways agree on record/generation/holders/primary/epoch,
three distinct holder IDs exist, primary is a holder, the primary
reports healthy, and (with `--probe`) each gateway can route to the
app's health endpoint with HTTP 2xx. The script requires HTTPS except
for localhost/127.0.0.1/::1 test gateways. It prints a compact JSON
pass or failure reason and exits nonzero on failure.

The script does **not** itself cryptographically verify the signature;
the c0mpute nodes must do that. It verifies that each gateway reports
an identical nonempty signed record, not that the nodes actually
reside on separate physical machines.

## Failure-injection validation

Record the existing primary and epoch from the successful report.
Only on disposable staging hosts, have the authorized operator kill
that primary or disconnect it from the network. Then rerun:

```sh
python3 scripts/verify-distributed-app.py \
  --app-id 1dc2367caf3f51b0c249 \
  --gateway https://gateway-a.example.org \
  --gateway https://gateway-b.example.org \
  --previous-primary OLD_PEER_ID --previous-epoch OLD_EPOCH --probe
```

The new primary must differ from the old one, its epoch must be
greater, at least three current holders must be announced (unless
the operator explicitly adjusts `--min-holders` to test degraded
service), and both gateways must agree and proxy successfully.
Replicas are eventually consistent: wait for the configured warmup
and promotion delay; do not treat a single transient failure as
proof the system never converges.

Run isolated validator tests without Docker or network access:

```sh
python3 scripts/verify-distributed-app.py --self-test
```

## Additional acceptance criteria outside this script

- Verify snapshot contents and bounded data loss, not only `epoch`.
- Verify containers cannot reach the host, LAN, cloud metadata, or
  other tenants via **IPv4 and IPv6**. See the fail-closed policy
  being hardened in PR #393.
- Prove separate physical operators, resource quotas, secrets
  admission policy and teardown.
- Check custom-domain TXT ownership and TLS certificate behavior.
- Inspect logs/latencies/availability under partitions and two
  competing primaries; test failure at several points in the
  snapshot cycle.
- Verify verified signed release distribution and actual dev2
  deployment (see #391).
- Meter, settle and dispute container compute/storage/egress fees.

Track the overall production launch in #394. A green script is
**necessary evidence, not sufficient evidence** for production
multi-tenant container security.
