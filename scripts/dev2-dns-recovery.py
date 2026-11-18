#!/usr/bin/env python3
"""Console-only DNS recovery for an Ubuntu dev2 host with Moshpit routing.

Dry-run by default. NEVER intended to run from the c0mpute deployment
workflow: a machine that cannot accept SSH cannot be fixed over SSH.

  sudo python3 scripts/dev2-dns-recovery.py
  sudo python3 scripts/dev2-dns-recovery.py --apply

Only recognized, Moshpit-owned systemd-resolved drop-ins are touched. This
does not replace /etc/resolv.conf, stop Moshpit, modify firewall rules, or
change unrelated DNS configuration. On failed restart or DNS verification it
restores every affected file before attempting to restart the resolver again.
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

RESOLVED_DIR = Path("/etc/systemd/resolved.conf.d")
BACKUP_DIR = Path("/var/backups")
NAMES = ("moshpit.conf", "00-moshpit.conf")
CHECK_NAMES = ("github.com", "c0mpute.com")
LOOPBACK_DNS = re.compile(r"^\s*DNS\s*=\s*(?:127\.0\.0\.1|::1|\[::1\])(?::\d+)?(?:\s|$)", re.I | re.M)
ROUTE_ALL = re.compile(r"^\s*Domains\s*=.*(?:^|\s)~\.(?=\s|$)", re.I | re.M)


def owned(name: str, data: str) -> bool:
    """Recognize only the two Moshpit installers' own routing drop-ins."""
    if name not in NAMES or "[Resolve]" not in data:
        return False
    if name == "moshpit.conf":
        return ("Written by `moshcode dns enable`" in data
                or "Moshpit TLDs" in data or "Moshpit" in data) and bool(
                    LOOPBACK_DNS.search(data) or ROUTE_ALL.search(data))
    # The moshpit-proxy installer writes 00-moshpit.conf and may not
    # contain the CLI's ownership marker. Avoid touching an unrelated
    # file sharing that name unless it routes DNS to the bridge.
    return bool(LOOPBACK_DNS.search(data) and ROUTE_ALL.search(data))


def candidates(directory: Path = RESOLVED_DIR) -> list[Path]:
    paths = []
    for name in NAMES:
        p = directory / name
        if p.is_symlink():
            raise RuntimeError(f"refusing symlinked resolver override: {p}")
        if not p.exists():
            continue
        if not p.is_file():
            raise RuntimeError(f"refusing non-file resolver override: {p}")
        data = p.read_text(encoding="utf-8")
        if owned(name, data):
            paths.append(p)
        else:
            print(f"SKIP nonmatching drop-in: {p}")
    return paths


def run(*args: str, timeout: int = 12) -> None:
    subprocess.run(list(args), check=True, timeout=timeout,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def dns_works() -> bool:
    # These public hostnames should resolve through the host's ordinary DNS.
    # The result does not prove SSH/firewall or the wider network is healthy.
    for name in CHECK_NAMES:
        try:
            result = subprocess.run(
                ["getent", "ahostsv4", name], check=False, timeout=8,
                capture_output=True, text=True,
            )
            if result.returncode != 0 or not result.stdout.strip():
                print(f"VERIFY DNS failed: {name}")
                return False
        except (OSError, subprocess.TimeoutExpired):
            print(f"VERIFY DNS timed out: {name}")
            return False
    return True


def apply(directory: Path = RESOLVED_DIR, backups: Path = BACKUP_DIR) -> int:
    paths = candidates(directory)
    if not paths:
        print("No recognizable Moshpit-owned drop-in found. No changes made.")
        return 2

    # Snapshot first and refuse to proceed if any backup can't be made.
    backups.mkdir(mode=0o700, parents=True, exist_ok=True)
    backup = Path(tempfile.mkdtemp(prefix="c0mpute-dns-recovery-", dir=backups))
    os.chmod(backup, 0o700)
    copied = []
    try:
        for source in paths:
            target = backup / source.name
            shutil.copy2(source, target, follow_symlinks=False)
            os.chmod(target, 0o600)
            copied.append((source, target))
        for source, _ in copied:
            source.unlink()
        try:
            run("systemctl", "restart", "systemd-resolved")
            if not dns_works():
                raise RuntimeError("ordinary DNS lookups did not recover")
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired,
                OSError, RuntimeError) as error:
            print(f"Recovery verification failed: {error}; restoring originals.")
            for source, target in copied:
                shutil.copy2(target, source, follow_symlinks=False)
            try:
                run("systemctl", "restart", "systemd-resolved")
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as restore_error:
                print(f"CRITICAL: resolver restart after rollback failed: {restore_error}", file=sys.stderr)
            print(f"Originals restored; retained backup: {backup}", file=sys.stderr)
            return 1
    except BaseException:
        # Unexpected interruption/error: restore if we had already changed
        # anything. We never touch non-Moshpit resolver configuration.
        for source, target in copied:
            if not source.exists() and target.exists():
                shutil.copy2(target, source, follow_symlinks=False)
        raise

    print(f"DNS restored for ordinary names; backup retained: {backup}")
    print("NEXT: test network IP routing, sshd listener and firewall separately.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true",
                        help="Back up and remove only recognized overrides, restart and verify resolver")
    args = parser.parse_args()
    try:
        paths = candidates()
        print("Recognized Moshpit resolver overrides:")
        for path in paths:
            print(f"  {path}")
        if not paths:
            print("No recognized Moshpit override. DNS may fail for another reason.")
            return 2
        if not args.apply:
            print("DRY RUN; no files or services changed. Use --apply from server console.")
            return 0
        if os.geteuid() != 0:
            print("--apply requires root on the server console", file=sys.stderr)
            return 2
        return apply()
    except (OSError, UnicodeError, RuntimeError) as error:
        print(f"Refusing to change DNS: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
