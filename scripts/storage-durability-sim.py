#!/usr/bin/env python3
"""Durability and cost model for the c0mpute storage tiers (CIP-001).

Reproduces the tables in docs/prds/001-storage-program.md. No dependencies
beyond the stdlib so it runs anywhere, including CI.

    ./scripts/storage-durability-sim.py
    ./scripts/storage-durability-sim.py --k 20 --n 32 --payout 0.002

The availability model is deliberately simple: shard hosts are treated as
independent Bernoulli samples. That is an *upper bound* on real durability —
it ignores correlated failure (shared ASN, region, power, or a bad release
rolling out everywhere at once), which is why CIP-003 requires placement
diversity. Read these numbers as "no better than this".
"""

from __future__ import annotations

import argparse
from math import comb, log10

# Schemes we compare against. (k, n, label)
SCHEMES = [
    (10, 14, "RS 10/14 (standard)"),
    (1, 3, "3-copy (hot)"),
    (20, 32, "RS 20/32 (critical)"),
    (29, 80, "Storj RS 29/80"),
]

AVAILABILITIES = [0.90, 0.95, 0.99, 0.999]


def p_unreadable(n: int, k: int, p: float) -> float:
    """P(fewer than k of n shards available), shards independent w.p. p."""
    return sum(comb(n, i) * p**i * (1 - p) ** (n - i) for i in range(k))


def nines(u: float) -> float:
    return log10(1 / u) if u > 0 else float("inf")


def expansion(k: int, n: int) -> float:
    return n / k


def repair_amplification(k: int) -> int:
    """Bytes that must be read to rebuild one byte of lost shard.

    Replication (k=1) copies a survivor: 1x. An erasure code must reconstruct
    from k shards: kx. This is the term that makes churn expensive.
    """
    return k


def durability_table() -> None:
    print("Probability an object is unreadable at a random instant\n")
    header = f"{'p(node)':>9} | " + " | ".join(f"{lbl:>22}" for _, _, lbl in SCHEMES)
    print(header)
    print("-" * len(header))
    for p in AVAILABILITIES:
        cells = []
        for k, n, _ in SCHEMES:
            u = p_unreadable(n, k, p)
            cells.append(f"{u:>10.1e} ({nines(u):>4.1f}n)")
        print(f"{p:>9.3f} | " + " | ".join(f"{c:>22}" for c in cells))


def cost_table(payout_per_raw_gb: float, egress_payout: float, churn: float) -> None:
    print(f"\n\nCost per usable GB-month at ${payout_per_raw_gb}/raw GB-month\n")
    header = (
        f"{'scheme':>22} | {'expand':>7} | {'COGS':>9} | {'repair x':>9} "
        f"| {'repair GB/mo':>13} | {'if repair paid':>15}"
    )
    print(header)
    print("-" * len(header))
    for k, n, lbl in SCHEMES:
        exp = expansion(k, n)
        cogs = exp * payout_per_raw_gb
        amp = repair_amplification(k)
        # Bytes read per usable GB per month to rebuild what churn destroyed.
        repair_gb = amp * exp * churn
        paid = repair_gb * egress_payout
        print(
            f"{lbl:>22} | {exp:>7.2f} | ${cogs:>8.5f} | {amp:>8}x "
            f"| {repair_gb:>13.2f} | ${paid:>14.5f}"
        )
    print(
        f"\nAt {churn:.0%} monthly churn. The last column is why repair egress is an\n"
        "unpaid provider obligation (CIP-001): paying it would swamp the margin."
    )


def margin_table(payout_per_raw_gb: float) -> None:
    tiers = [
        ("hot", 1, 3, 0.006),
        ("standard", 10, 14, 0.0035),
        ("critical", 20, 32, 0.005),
    ]
    print("\n\nRetail margin by tier\n")
    header = f"{'tier':>10} | {'expand':>7} | {'COGS':>9} | {'retail':>9} | {'margin':>7}"
    print(header)
    print("-" * len(header))
    for name, k, n, retail in tiers:
        cogs = expansion(k, n) * payout_per_raw_gb
        margin = (retail - cogs) / retail
        print(
            f"{name:>10} | {expansion(k, n):>7.2f} | ${cogs:>8.5f} "
            f"| ${retail:>8.4f} | {margin:>6.0%}"
        )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--k", type=int, help="data shards, for a one-off scheme")
    ap.add_argument("--n", type=int, help="total shards, for a one-off scheme")
    ap.add_argument(
        "--payout", type=float, default=0.0015, help="provider $/raw GB-month"
    )
    ap.add_argument(
        "--egress-payout", type=float, default=0.002, help="provider $/GB served"
    )
    ap.add_argument("--churn", type=float, default=0.05, help="monthly node churn")
    args = ap.parse_args()

    if args.k and args.n:
        print(f"RS {args.k}/{args.n}: expansion {expansion(args.k, args.n):.2f}x, "
              f"repair amplification {repair_amplification(args.k)}x\n")
        for p in AVAILABILITIES:
            u = p_unreadable(args.n, args.k, p)
            print(f"  p={p:.3f} -> {u:.3e} unreadable ({nines(u):.1f} nines)")
        return

    durability_table()
    cost_table(args.payout, args.egress_payout, args.churn)
    margin_table(args.payout)


if __name__ == "__main__":
    main()
