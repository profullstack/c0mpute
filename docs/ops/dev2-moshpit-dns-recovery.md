# dev2 network outage: Moshpit DNS safe recovery

**Status:** Run only from the hosting provider's *console*, not from GitHub
Actions. As of November 17, 2026, all four recent `Deploy to dev2` jobs
failed before invoking the remote deploy script because TCP/22 timed out.
The signed c0mpute v0.3.78 release exists, but the GitHub runner did not
deploy that commit to dev2.

## Why this is a possible DNS conflict

`moshcode dns enable` may install
`/etc/systemd/resolved.conf.d/moshpit.conf` with:

```ini
[Resolve]
DNS=127.0.0.1:5354
Domains=~.
```

`Domains=~.` sends **all** DNS queries through the local Moshpit bridge.
If that process, port, or its clearnet upstreams are unavailable, ordinary
hostname lookups can fail. The moshpit-proxy installer may also have written
`00-moshpit.conf`, so removing only `moshpit.conf` may not undo routing.

A timeout connecting to SSH port 22 does **not** by itself prove DNS
failed: check IP routing, upstream ACLs, SSH listener and firewalls too.

## Recover from the provider's web/VNC/rescue console

First run read-only checks:

```sh
ip -br addr
ip route
ping -c 2 -W 2 1.1.1.1
getent ahostsv4 github.com
resolvectl status
ls -l /etc/resolv.conf
sudo grep -REn 'DNS=|Domains=' /etc/systemd/resolved.conf /etc/systemd/resolved.conf.d 2>/dev/null
sudo systemctl status systemd-resolved moshpit-dns --no-pager
```

If an interactive operator confirms Moshpit resolver routing is the
problem and has a known-good console session, use the guarded script:

```sh
python3 scripts/dev2-dns-recovery.py             # dry run only
sudo python3 scripts/dev2-dns-recovery.py --apply
```

The script removes only recognized Moshpit-owned resolver drop-ins,
backs them up with mode 0600 under `/var/backups/` and restarts
`systemd-resolved`. It verifies that `github.com` and `c0mpute.com`
resolve using the machine's normal resolver, and restores the exact
original files if restarting or verification fails. It never edits
`/etc/resolv.conf`, assigns an arbitrary public resolver, disables
firewall rules, restarts the entire machine, or changes SSH settings.

If the failure is caused by an unrelated drop-in, missing upstream,
broken `systemd-resolved`, NetworkManager, netplan or provider ACL,
the script refuses/does not solve it. Read the logs rather than
bypassing the guards. Backups are deliberately retained for inspection.

After successful DNS recovery verify external reachability:

```sh
resolvectl query github.com
sudo ss -lntp | grep ':22'
sudo systemctl status ssh --no-pager
sudo ufw status verbose
sudo journalctl -u ssh -n 40 --no-pager
```

Provider console access or an authorized overlay is required if SSH
cannot reach the host. Do not run `ufw disable`, unconditionally
flush iptables/nftables, or bypass SSH host key verification.

## Re-enable Moshpit without breaking clearnet

Moshpit's CLI supports *per-suffix* routing via
`Domains=~moshpit ~eggs` instead of `Domains=~.`. Keep regular DNS
resolvers selected by DHCP/provider/netplan; only explicitly opted-in
Moshpit namespaces should use the local bridge on infrastructure
servers. Test under a controlled change window and verify public DNS,
private DNS, TCP/UDP, SSH and outbound HTTPS again after enabling.

## Deployment proof

Once the server's SSH path has been restored, the GitHub
[Deploy to dev2 workflow](../.github/workflows/deploy-dev2.yml) can
rerun against the **exact** expected master SHA. Confirm it executes
`deploy-app.sh`, reports success and the live application identifies
that version. A passing GitHub *release* job is not proof of a dev2
deployment. Track the connectivity incident in issue #391.

This recovery does not implement a working Scallion VPN or establish
distributed-container readiness.
