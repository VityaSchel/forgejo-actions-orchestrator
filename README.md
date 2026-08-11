# Forgejo Actions Orchestrator

A daemon watching an allowlist of Forgejo repositories and automatically renting an ephemeral single-use machine from a cloud provider for secure and hermetic CI execution. No RCE on your orchestrator host, no risks to your infrastructure, no DIND limitations or kernel vulnerabilities exploits.

| Provider       | Image field               | Snapshots |
| -------------- | ------------------------- | --------- |
| Hetzner Cloud  | image name or snapshot id | Yes       |
| Vultr          | numeric `os_id`           | No        |
| Cherry Servers | OS slug                   | No        |


## Install

1. **Download the pre-built binary from [Releases](https://git.hloth.dev/hloth/forgejo-actions-orchestrator/releases)** (recommended):
   ```sh
   wget https://git.hloth.dev/hloth/forgejo-actions-orchestrator/releases/download/v1.0.0/forgejo-actions-orchestrator-linux-x86_64
   install -Dm755 ./forgejo-actions-orchestrator-linux-x86_64 /usr/local/bin/forgejo-actions-orchestrator
   ```
-  or build it from source:
   ```sh
   git clone https://git.hloth.dev/hloth/forgejo-actions-orchestrator
   cd forgejo-actions-orchestrator
   # Needs the toolchain named in rust-toolchain.toml; rustup installs it on first use.
   cargo build --release --locked
   install -Dm755 target/release/forgejo-actions-orchestrator /usr/local/bin/forgejo-actions-orchestrator
   ```

1. Configure for systemd:
   ```sh
   install -Dm644 deploy/forgejo-actions-orchestrator@.service /etc/systemd/system/forgejo-actions-orchestrator@.service
   install -Dm644 config.example.toml /etc/forgejo-actions-orchestrator/my-site.toml
   install -d -m700 /etc/forgejo-actions-orchestrator/credentials/my-site
   # Edit /etc/forgejo-actions-orchestrator/my-site.toml
   ```

   - `forgejo.url` — your instance
   - `[[repo]]` — list only repositories with Actions enabled; every poll of a repo without it 404s
   - each label's `image` — confirm the snapshot id still exists; nothing pre-flights it

   Every job names a label **set**, matched exactly against one `[[label]]` block. That block decides
   the provider, the plans to try, the locations, and the image. Either a pre-baked snapshot, which
   boots fastest but versions the toolchain outside the repository and only works on Hetzner, or a
   stock OS image plus a setup step, which is slower but versions the environment with the code.

2. Set credentials

   One file per secret, `0400 root:root`, passed by systemd `LoadCredential=`.
   
   ```sh
   umask 077
   cd /etc/forgejo-actions-orchestrator/credentials/my-site
   printf %s 'TOKEN' > forgejo-runner-token
   printf %s 'TOKEN' > forgejo-status-token
   
   # To enable Hetzner:
   printf %s 'TOKEN' > hetzner-token
   
   # To enable Vultr:
   printf %s 'TOKEN' > vultr-token
   
   # To enable CherryServers:
   printf %s 'TOKEN' > cherry-token
   printf %s 'ID'    > cherry-project-id

   chmod 400 ./*
   ```

   | Credential             | Scope                                                           |
   | ---------------------- | --------------------------------------------------------------- |
   | `forgejo-runner-token` | Owner-level. Registers and deletes runners, reads the job queue |
   | `forgejo-status-token` | `write:repository`, publishes the provisioning status           |
   | `hetzner-token`        | Read & Write, scoped to a project used only by CI               |
   | `vultr-token`          | Vultr API, make sure host's IP is in the allowlist for token    |
   | `cherry-token`         | Cherry Servers API key                                          |
   | `cherry-project-id`    | Cherry Servers project ID                                       |

   Issue Forgejo tokens in Settings → Applications → New token, `repository` set to **Read and write**. The runner token needs the org's **Owners** team, the status token only needs **Write** on the repo and is kept on a separate bot account because the runner token is owner-equivalent.

## Usage

```sh
systemctl daemon-reload
systemctl enable --now forgejo-actions-orchestrator@my-site
journalctl -u forgejo-actions-orchestrator@my-site -f
```

Healthy service only logs once on launch and doesn't repeat:

```
INFO watching repos=["owner/repo"] labels=["check", "release", …] interval=15s
```

| Symptom                                                 | Cause                                                                     |
| ------------------------------------------------------- | ------------------------------------------------------------------------- |
| `243/CREDENTIALS`                                       | a `LoadCredential=` source file is missing — create it or delete the line |
| `Permission denied` reading config, restarting every 5s | config is not `0644`                                                      |
| `poll_failed` with `HTTP 404`                           | repo not in the allowlist, or Actions disabled on it                      |
| `poll_failed` with `HTTP 403`                           | runner token is not org-Owner                                             |

Do **not** disable a provider by deleting its `[[label]]` while machines are live — that removes it from the survey and they are never destroyed. Stop the daemon and delete them by hand instead.

## How a job gets a machine

1. Daemon checks every provider for machines it owns, matched by the `ci-orc-` name prefix
2. Poll each allowlisted repository for queued jobs
3. Destroy machines past `lifetime_minutes`, and machines whose job has left the queue
4. Delete runner registrations whose machine has been absent from two consecutive surveys
5. Provision a machine for each newly queued job, up to the label's `max_vms`

Cloud-init writes the runner config and `boot.sh` to the new machine. `boot.sh` downloads the Forgejo runner, verifies it against the SHA-256 you pinned, and starts it under `systemd-run` as `one-job --wait`.

The daemon is what reclaims the machine: once the job leaves the queue, two consecutive polls confirm it and step 3 destroys it. Cloud-init also arms a `poweroff` at `lifetime_minutes`, but that is only a backstop for a machine the daemon never reached — a powered-off machine still bills, so the delete is what stops the meter.

Teardown is running based on step 1 survey, a lingering VM is destroyed even after the daemon restarts. If a provider's API cannot be read, that provider is marked blind and its machines are left alone rather than being destroyed on incomplete information.

## Known gaps

- A stock image must provide `bash`, `sha256sum`, `systemd-run` and apt; `boot.sh` covers the rest
- One repository failing to poll suppresses teardown for every repository until it recovers
- A job that never gets picked up keeps its machine until `lifetime_minutes`, and blocks its label

## License

[MIT](./LICENSE)

## Donate

[hloth.dev/donate](https://hloth.dev/donate)
