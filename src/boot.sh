#!/bin/bash
set -euo pipefail

readonly etc=/etc/forgejo-actions-orchestrator
readonly runner=/usr/local/bin/forgejo-runner

missing=()
command -v curl >/dev/null 2>&1 || missing+=(curl)
command -v git >/dev/null 2>&1 || missing+=(git)
if [ ${#missing[@]} -gt 0 ]; then
	export DEBIAN_FRONTEND=noninteractive
	apt-get update -y
	apt-get install -y ca-certificates "${missing[@]}"
fi

curl -fsSL -o "$runner" "$(cat "$etc/runner-url")"
sha256sum -c "$etc/runner.sha256"
chmod +x "$runner"

labels=()
while IFS= read -r label || [ -n "$label" ]; do
	if [ -n "$label" ]; then
		labels+=(--label "$label")
	fi
done <"$etc/runner-labels"

if [ ${#labels[@]} -eq 0 ]; then
	echo "no runner labels; the job would never be claimed" >&2
	exit 1
fi

systemd-run --collect --unit=forgejo-actions-runner --setenv=HOME=/root \
	"$runner" --config "$etc/runner-config.yml" one-job \
	--url "$(cat "$etc/forgejo-url")" \
	--uuid "$(cat "$etc/runner-uuid")" \
	--token-url "file://$etc/runner-token" \
	"${labels[@]}" \
	--handle "$(cat "$etc/job-handle")" \
	--wait
