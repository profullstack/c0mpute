# Scallion relay participation (advertise only)

Status: nodes **advertise** Scallion relay roles and verify each other's
descriptors. **Nothing is relayed.** No circuit is built between real nodes,
no stream is opened and no traffic leaves an exit: the link layer, the stream
layer and the exit are not implemented (issue #386). `c0mpute scallion modes`
still reports both client modes as not ready. This is not a VPN and it makes
no anonymity claim.

What this slice adds is the part every later slice needs first: a node says
which relay roles it would take, signs that statement with its identity, and
other nodes keep a verified list of who said what.

Code: `node/crates/c0mpute-net/src/scallion_participation.rs` (roles,
directory), `node/crates/c0mpute-core/src/scallion_node.rs` (config, the
worker's descriptor, the ad field, the local listing),
`node/crates/c0mpute-cli/src/scallion_cli.rs` (commands).

## Defaults

| Role | Default | How it changes |
| --- | --- | --- |
| guard (entry) + middle | **on** for every worker | `c0mpute scallion relay off`, `[scallion] relay = false`, or `C0MPUTE_SCALLION=0` |
| exit | **off** | opt in only: `c0mpute scallion exit on`, `[scallion] exit = true`, or `C0MPUTE_SCALLION_EXIT=1` |

- Exit is never implied by relay participation. A node with exit off builds a
  descriptor with `exit = false` and an empty exit port list.
- Turning relay participation off turns everything off: the node publishes no
  descriptor at all, whatever the exit setting says.
- Private mode (`worker start --private`, `[private] enabled`) publishes no
  capability ad, so it publishes no descriptor either.
- Anything unclear about the exit opt-in leaves exit off: an unrecognized
  `C0MPUTE_SCALLION_EXIT` value, an empty port list, port 0, a port that does
  not parse, or more than 64 ports. The node stays a guard and middle relay
  and logs a warning.

## Config

```toml
[scallion]
relay = true          # advertise guard + middle (default true)
exit = false          # advertise exit too (default false, opt-in)
exit_ports = [80, 443] # only read when exit is on
# operator = "example operator"   # optional self-declared label
```

The default exit ports are 80 and 443, chosen conservatively in this slice
(`ExitPolicy` itself has no default list: it requires the operator to name
ports). The port list goes through the same checks as `ExitPolicy::opt_in`.

`operator` is a printable ASCII label of at most 64 bytes that rides in the
descriptor so nodes run by one operator can say so. It is a claim, not proof;
see the threat model on self-declared operator labels.

## Environment

For a fleet that does not want to edit a file on every box:

| Variable | Effect |
| --- | --- |
| `C0MPUTE_SCALLION` | `0`/`false`/`no`/`off`: no participation. Any other value: on. Unset: the config decides. |
| `C0MPUTE_SCALLION_EXIT` | `1`/`true`/`yes`/`on`: opt in as an exit. `0`/`false`/`no`/`off`: exit off. Any other value: exit off, with a warning. Unset: the config decides. |
| `C0MPUTE_SCALLION_EXIT_PORTS` | Comma-separated ports, overriding `exit_ports`. Does not opt in by itself. |
| `C0MPUTE_SCALLION_OPERATOR` | Overrides `operator`. |

The environment is read by the worker process, so for a systemd unit it goes
in the unit (`Environment=C0MPUTE_SCALLION_EXIT=1`), not in a login shell.

## Commands

```sh
c0mpute scallion status            # what this node advertises and why; what a running worker reports
c0mpute scallion relay on|off      # guard + middle participation
c0mpute scallion exit on           # opt in as an exit to the configured ports (80,443 by default)
c0mpute scallion exit on --ports 443,8443
c0mpute scallion exit off
c0mpute scallion relays            # relays a running worker has heard, with verified roles
```

`relay` and `exit` write `[scallion]` in the config file. A running worker
keeps advertising what it started with; restart it to apply a change.
`c0mpute status` and `c0mpute status --json` (key `scallion`) show the same
roles.

`status` and `relays` ask the local worker's gateway
(`GET /scallion/v1/relays` on `http://127.0.0.1:7777`, `--gateway` to change
it). The route is read-only and returns only what is already public in
capability ads. It exists on workers with the gateway role (a default role).

## What a worker does

On `c0mpute worker start`, when participation is on:

1. Loads the relay static key from `<config dir>/scallion-relay.key`, creating
   it on first use: 32 random bytes, mode 600, written the same way as
   `identity.key`. It is separate from the identity key, so deleting the file
   rotates the relay key without changing the peer id.
2. Builds a relay descriptor (see [scallion-handshake.md](scallion-handshake.md)):
   peer id, Noise static public key, roles, exit ports, operator label, and a
   validity of 6 hours.
3. Signs it with the node identity.
4. Publishes it inside the capability ad on `c0mpute/cap/v1`, which the worker
   already sends every 60 seconds:

   ```json
   { "hardware": { "scallion": { "v": 1, "descriptor": "<base64 signed descriptor>" } } }
   ```

   The same signed descriptor is reused until less than 3 hours of validity
   remain, then a fresh one is signed, so a listed descriptor never gets close
   to expiry while the node is up.

A worker that does not participate sends its ad without the `scallion` field.

## What every node does with descriptors it hears

The capability registry reads `hardware.scallion.descriptor` from each ad and
keeps it only if:

- `SignedRelayDescriptor::verify` accepts it (signature, signer is the named
  peer, inside its validity period, usable Noise key, well-formed roles and
  ports), and
- the peer it names is the peer that signed the gossip message, and
- it is not older than the descriptor already held for that peer (an old
  descriptor, for example from before the operator turned exit off, cannot be
  played back).

The directory is in memory only, holds at most 4096 relays, and forgets a
relay when its descriptor expires, when it has not been heard for 5 minutes,
or when the peer sends an ad without a descriptor.

## What this does not do

- It does not relay, forward or exit anything. `RelayTable` is not connected
  to any socket.
- It does not use the directory to plan or build circuits.
- It does not prove that two relays have different operators. The operator
  label is self-declared.
- It does not hide who asks for the directory, and it does not design private
  descriptor fetching. Every node hears every ad.
- It does not revoke a descriptor before it expires (6 hours at most here).
- An exit opt-in today only changes what the node advertises. The exit
  itself, with `ExitPolicy` enforcement, abuse handling and consent UX, is
  still to be built and reviewed.

## Tests

```sh
cargo test -p c0mpute-net scallion_participation --locked
cargo test -p c0mpute-core scallion_node --locked
cargo test -p c0mpute-cli scallion --locked
```

They check: the defaults (relay on, exit off); that exit opt-in produces a
descriptor with the exit role and its ports; that exit off never advertises
exit; that participation off produces no descriptor; that a descriptor
round-trips through sign and verify and fails after expiry or tampering; that
the directory refuses a descriptor relayed by another peer, garbage and a
replayed older descriptor; that doubtful exit settings leave exit off; and
that the relay key file persists with mode 600.
