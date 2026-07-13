# Private AEC Host Handoff

Do not put completed values for this template in the public repository. Send
them through HotCRP or another AEC-approved anonymous communication channel.

## Connection

- Hostname or IP: `<private>`
- SSH port: `<private>`
- Evaluator username: `<private>`
- Account expiration: `<private>`
- Artifact root after login: `~/ae`
- Release tag: `sosp26-ae-v1`
- Authentication: evaluator-provided, comment-free SSH public key
- Privilege: passwordless `sudo -n` is enabled only as required by the artifact

Do not request an evaluator's name, email address, or other personal details.
SSH keys should be relayed through the AEC channel. Inform the chairs that the
host necessarily retains normal SSH, sudo, and system accounting logs; the
artifact itself contains no analytics or tracking.

## First Login

```sh
cat ~/README-FIRST.txt
cd ~/ae
git describe --tags --exact-match
./reproduce.sh check
./reproduce.sh primary dry-run
./reproduce.sh fig10 smoke
```

The administrator should preinstall `/run/lock/cswitch-ae.lock` as a
root-owned, world-readable, non-writable file. Every evaluator receives a
separate checkout. Full campaigns must never overlap.

## Operational Restrictions

- Do not reboot the server or change BIOS, NPS, NUMA, DIMM, or CXL settings.
- Do not run unrelated performance workloads during an AE campaign.
- Do not retain personal data, private keys, access tokens, or unrelated
  credentials on the host.
- Use the machine-wide lock and coordinate any maintenance through the AEC
  channel.
- Report account expiration and planned log-retention policy privately.

This access model follows the
[Systems Research Artifacts packaging guide](https://sysartifacts.github.io/packaging-guide),
which permits SSH access to specialized hardware while requiring evaluator
anonymity.
