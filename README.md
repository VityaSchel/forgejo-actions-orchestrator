# Forgejo Actions Orchestrator

A daemon that watches an allowlist of Forgejo repositories and rents a single-use cloud machine for each queued job. Jobs run on that machine instead of the orchestrator host, so no RCE on your host, no Docker-in-Docker limitations, no kernel vulerabilities or exploits.

A job names a set of labels, and the `[[machine]]` entry answering to that exact set is built:

```yaml
runs-on: [build, hetzner]
```

```toml
[[machine]]
labels = [["build", "hetzner"], ["build", "cherry"], ["build", "vultr"]]
image = "debian"
plans = "8c16g"
lifetime_minutes = 90
job_timeout_minutes = 70
allowed_events = ["workflow_dispatch"]
```

One label names the provider. See [config.example.toml](./config.example.toml).

| Provider       | `provider` | `image`                         | `locations`                   |
| -------------- | ---------- | ------------------------------- | ----------------------------- |
| Hetzner Cloud  | `hetzner`  | image name or snapshot id       | `fsn1`, `hel1`                |
| Vultr          | `vultr`    | numeric `os_id`                 | `ams`, `ewr`                  |
| Cherry Servers | `cherry`   | OS slug                         | `LT-Siauliai`, `US-Chicago`   |
| Scaleway       | `scaleway` | marketplace label or image UUID | `fr-par-1`, `nl-ams-1`        |
| Gcore          | `gcore`    | image UUID                      | numeric region id: `30`, `76` |

Each provider's list inside a `[plans]` table takes that provider's server type names, tried in order. The end of [config.example.toml](./config.example.toml) lists every type with its price.

- Hetzner resolves an image name for each plan's architecture, so one plan ladder can mix x86 and Arm. A snapshot id boots only on the architecture it was taken on.
- Scaleway resolves a marketplace label to the variant each plan and zone supports.
- Scaleway and Gcore image UUIDs exist in one zone or region only.

## Install

The steps use `example` as the instance name. Choose your own instance name and use it in the config file, the credentials directory and the systemd drop-in.

1. Download the binary from [Releases](https://git.hloth.dev/hloth/forgejo-actions-orchestrator/releases) (4.6 MB):

   ```sh
   wget https://git.hloth.dev/hloth/forgejo-actions-orchestrator/releases/download/v1.2.0/forgejo-actions-orchestrator-linux-x86_64
   install -Dm755 forgejo-actions-orchestrator-linux-x86_64 /usr/local/bin/forgejo-actions-orchestrator
   ```

   Builds are [reproducible](./CONTRIBUTING.md#cross-compile).

   Or build it from source. `rustup` installs the toolchain from `rust-toolchain.toml` on first use.

   ```sh
   git clone https://git.hloth.dev/hloth/forgejo-actions-orchestrator
   cd forgejo-actions-orchestrator
   cargo build --release --locked
   install -Dm755 target/release/forgejo-actions-orchestrator /usr/local/bin/forgejo-actions-orchestrator
   ```

2. Install the systemd unit, the provider drop-in and the config. In a source checkout, skip `wget` and take the first two files from `deploy/`.

   ```sh
   RAW=https://git.hloth.dev/hloth/forgejo-actions-orchestrator/raw/branch/main
   wget $RAW/deploy/forgejo-actions-orchestrator@.service $RAW/deploy/providers.example.conf $RAW/config.example.toml

   install -Dm644 forgejo-actions-orchestrator@.service /etc/systemd/system/forgejo-actions-orchestrator@.service
   install -Dm644 providers.example.conf /etc/systemd/system/forgejo-actions-orchestrator@example.service.d/providers.conf
   install -Dm644 config.example.toml /etc/forgejo-actions-orchestrator/example.toml
   ```

   In `providers.conf`, delete the lines for providers you don't use.

   In `example.toml`, set:

   - `forgejo.url`
   - a `[repo."owner/name"]` per repository, only ones with Actions enabled, whose `max_vms` is both its provider allowlist and its per-provider quota
   - a `[provider.<name>]` per cloud you use, with its locations
   - an `[image.<alias>]` per operating system, with each provider's id, checking that a snapshot id still exists
   - a `[plans.<name>]` per hardware floor, with each provider's ordered plan ladder
   - a `[[machine]]` per recipe, listing the label sets it answers to

3. Create the credentials, one file per secret:

   ```sh
   install -d -m700 /etc/forgejo-actions-orchestrator/credentials/example
   cd /etc/forgejo-actions-orchestrator/credentials/example
   umask 077

   printf %s 'TOKEN' > forgejo-runner-token
   printf %s 'TOKEN' > forgejo-status-token

   # Only the providers you use
   printf %s 'TOKEN' > hetzner-token
   printf %s 'TOKEN' > vultr-token
   printf %s 'TOKEN' > cherry-token
   printf %s 'ID'    > cherry-project-id
   printf %s 'KEY'   > scaleway-token
   printf %s 'ID'    > scaleway-project-id
   printf %s 'TOKEN' > gcore-token
   printf %s 'ID'    > gcore-project-id

   chmod 400 ./*
   ```

   | File                                    | Contents                                                                  |
   | --------------------------------------- | ------------------------------------------------------------------------- |
   | `forgejo-runner-token`                  | Token of an org Owner. Registers runners, reads the job queue             |
   | `forgejo-status-token`                  | Token with Write on the repositories. Posts commit statuses               |
   | `hetzner-token`                         | Read & Write API token                                                    |
   | `vultr-token`                           | API key, with the host's IP in its access control list                    |
   | `cherry-token`, `cherry-project-id`     | API key and project ID                                                    |
   | `scaleway-token`, `scaleway-project-id` | IAM API secret key with Instances and Block Storage write, and project ID |
   | `gcore-token`, `gcore-project-id`       | Permanent API token and Cloud project ID                                  |

   Issue Forgejo tokens in Settings → Applications → New token, `repository` set to **Read and write**.

   The daemon destroys any machine whose name starts with `machine_prefix` and matches no class. Give it a cloud project, or on Vultr an account, that runs nothing else.

## Usage

```sh
systemctl daemon-reload
systemctl enable --now forgejo-actions-orchestrator@example
journalctl -u forgejo-actions-orchestrator@example -f
```

On start it logs:

```
INFO watching repos=["owner/repo"] labels=["build", "check", "hetzner"] interval=15s
```

After that it only logs machines created and destroyed, refused jobs and errors.

| Symptom                                                 | Cause                                                                                |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `243/CREDENTIALS`                                       | a credential file is missing. Create it or remove the provider from `providers.conf` |
| `is LoadCredential=… missing from the unit?`            | a `[provider]` table names a provider that `providers.conf` is missing               |
| `Permission denied` reading config, restarting every 5s | config is not `0644` mode                                                            |
| `poll_failed` with `HTTP 404`                           | wrong `[repo."owner/name"]` key, or Actions disabled on it                            |
| `poll_failed` with `HTTP 403`                           | runner token is not an org Owner                                                     |
| `held back: this provider's machines are not visible`   | the provider API failed to list machines, see the `poll_failed` before it            |
| `runner … is not ephemeral`                             | Forgejo is too old to register ephemeral runners                                     |

If machines are running when you edit the config:

- Removing a `[provider]` table, removing a Scaleway or Gcore location, or changing `machine_prefix` makes the daemon lose track of those machines. They keep billing until you delete them by hand.
- Removing or renaming a label set destroys its idle machines once their job leaves the queue. A machine whose job is still queued or running survives the rename and is capped by the longest `lifetime_minutes` in the config.

> [!NOTE]
> **How a job gets a machine**
> 
> Every `poll_interval_secs` the daemon:
> 
> 1. Lists each provider's machines whose names start with `machine_prefix`
> 2. Polls each `[repo]` for queued and running jobs
> 3. Destroys machines older than `lifetime_minutes` plus `reconcile_grace_secs`, and machines whose job was absent from the last two polls
> 4. Deletes the runner registration of any machine absent from the last two listings and gone for at least `reconcile_grace_secs`
> 5. Registers an ephemeral runner for each new queued job and creates its machine, up to the repository's `max_vms` for that provider. It tries every plan in every location, in order, and reports the outcome as a commit status.
> 
> Cloud-init writes the runner config and `boot.sh` to the machine. `boot.sh` downloads the Forgejo runner, checks it against your pinned SHA-256 and runs it for that one job. Cloud-init also powers the machine off after `lifetime_minutes`, but a powered-off machine keeps billing until the daemon destroys it.
> 
> The daemon finds machines by listing them from the provider, so it also cleans up machines created before a restart. If a provider's API fails, the daemon leaves that provider's machines alone and holds back its jobs until the API recovers.

## Known gaps

- Images need cloud-init, `bash`, `sha256sum` and `systemd-run`, plus curl and git or apt to install them. Only x86_64 and aarch64 work.
- If any repository fails to poll, the daemon stops destroying machines of finished jobs in every repository until polling works again. `lifetime_minutes` still applies.
- A job the runner never picks up, for example after a checksum mismatch, gets a new machine every `lifetime_minutes` until you cancel it.
- Scaleway and Gcore bill the 80 GB boot volume separately, and nothing sweeps a volume that a failed delete leaves behind. Check block storage after a Scaleway `sweep_failed` alert. Gcore deletes volumes in a background task the daemon never checks, so look at its volume list now and then.

## License

[MIT](./LICENSE)

## Donate

[hloth.dev/donate](https://hloth.dev/donate)
