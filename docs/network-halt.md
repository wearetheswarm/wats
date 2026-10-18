# Network halt

A last-resort switch stops the public network if it runs away: a quorum of
guardian keys, compiled into the binary, signs a halt, and every node that
accepts it relays the halt and stops. The design and its limits are in
[ADR 0016](adr/0016-network-halt.md). What it means for a node, `wats
resume` and the restart policies are in
[agent.md](agent.md#guardian-halts). What a stolen guardian key could do
(stop the public network, nothing more) is in the
[threat model](threat-model.md).

The power is minimal. A halt carries no code, deletes nothing, changes no
setting and upgrades nothing, and it only concerns the public network.
Each node can override it with `wats resume`. Opting out entirely is a
config file key, `halt.accept = false`, which the node warns about at every
start and in `status`.

The guardian set and the threshold change only through a release,
announced in its notes: 1-of-1 (the founder) at first, then 2-of-3 and
3-of-5 with independent guardians. An empty set disables halts.

## Current guardians

Threshold: **1 of 1**. Approval window: 24 hours. In force from the first
release after 0.2.0.

| Guardian | Public key (Ed25519, hex) | Since |
|---|---|---|
| Vincent Carluer (founder) | `3f1675fefba2606f3b5187597e9e9087c708cf4c98c5709b3523186a114370bb` | 2026-10-18 |

The source of truth is `GUARDIANS` and `HALT_THRESHOLD` in
`crates/wats/src/halt.rs`. A test checks that the keys are valid and the
threshold reachable.

## Becoming a guardian

1. The new guardian generates their own key on their own machine, and keeps
   it cold (below, "Guardian key"). Nobody else ever sees the secret.
2. They send only the public key (64 hex), through a channel the
   maintainers can verify, e.g. signed or read aloud.
3. A release adds the key to `GUARDIANS` and sets the new threshold, and
   the approval window if it changes. Its release notes announce the set:
   every key, the threshold and the window. The set takes effect for each
   node when it upgrades to that release.

## Removing or rotating a guardian

A guardian whose key is lost or stolen, or who steps down, is removed by a
release without that key. Rotating is the same release with a new key in
place of the old one.

- **Nodes on the new release** ignore the removed key: its halts, approvals
  and unhalts no longer count for them, including events it signed earlier.
  The state is recomputed from the current set, so a halt that held only
  thanks to that key is lifted on those nodes at their next start.
- **Nodes on older releases** still count the old key until they upgrade.
  With a stolen key, the thief can stop them until they upgrade, and
  `wats resume` brings them back meanwhile. A stolen key can do nothing
  else (ADR 0016 §6).
- **Below the threshold** (keys lost faster than releases replace them), no
  halt or unhalt is possible. Nodes keep running, and a halt already in
  effect is cleared by `wats resume` or by a release with a new set.

## Guardian key

This section is for guardians. A guardian key is a dedicated Ed25519 key:
never a node or author key, never in GitHub, and never on a machine that is
online day to day.

1. **Generate it** on a trusted machine, ideally offline:

   ```sh
   wats guardian keygen --out guardian.key
   # seed written to guardian.key (mode 0600)
   # public key: <64 hex>
   ```

   The file holds the 32-byte seed in hex. `keygen` never overwrites a file.
2. **Encrypt it at once with a passphrase**, then delete the clear copy:

   ```sh
   age -p -o guardian.key.age guardian.key
   rm guardian.key        # or shred / rm -P where available
   ```

3. **Keep `guardian.key.age` cold.** Store one copy on a USB key and one in
   a private repository, and never keep it on the online machine. The
   passphrase lives in your password manager, not next to the file.
4. **Send the public key** (64 hex). It goes into `GUARDIANS` in a pull
   request, and ships with the next release.

## Signing a halt, an approval or an unhalt

The secret never touches the disk in clear: every guardian command reads it
from standard input with `--key -`.

- **On an offline machine**, sign with `--offline`, which prints the signed
  event, then carry the file to any online machine and publish it there:

  ```sh
  # offline, with the USB key
  age -d guardian.key.age | wats guardian halt --key - --reason "…" --offline > halt.json
  # online
  wats guardian publish halt.json
  ```

- **On an online machine**, when speed matters more, decrypt from the USB
  key straight into the command. It publishes through the running node, or
  through a short-lived one:

  ```sh
  age -d /Volumes/KEY/guardian.key.age | wats guardian halt --key - --reason "…"
  ```

- **Approvals** (with more than one guardian) name the halt or the unhalt
  they approve, and must arrive within 24 hours of it:

  ```sh
  age -d guardian.key.age | wats guardian approve --key - <id>
  ```

- **Lifting a halt** takes an unhalt under the same quorum:

  ```sh
  age -d guardian.key.age | wats guardian unhalt --key - <halt id> --reason "…"
  ```

`--key <file>` also reads a clear file, for tests only.
