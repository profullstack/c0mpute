# Scallion native and OpenVPN client interfaces v0.3.78

Publish a signed release of the merged Scallion native/OpenVPN client-mode diagnostics and guarded profile builder from PR #390. Neither mode offers a working VPN tunnel yet. The signed binaries must contain no anonymous-VPN claims. Requires successful CI, locked Rust workspace tests, and preceding public release v0.3.77.

CI regeneration note: validate the signed client-mode interfaces in the Rust workspace and security scanners before merging. The native and OpenVPN entries remain disabled as network transports until reviewed end-to-end.
