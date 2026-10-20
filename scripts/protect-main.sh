#!/usr/bin/env bash
# Apply the rulesets in .github/rulesets/ to the GitHub repository, creating or updating each one
# by name, so running it again is safe:
# - main.json: pull requests only, squash merges, linear and signed history, CI must pass, no
#   force push or deletion of main;
# - tags.json: only repository admins create, move or delete v* release tags (release-plz tags
#   with RELEASE_PLZ_TOKEN, so that token must belong to an admin).
# Rulesets need a public repository or a paid plan. Needs the GitHub CLI and jq.
set -euo pipefail
cd "$(dirname "$0")/.."
repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"

for file in .github/rulesets/main.json .github/rulesets/tags.json; do
  existing="$(gh api --paginate "repos/$repo/rulesets" \
    --jq ".[] | select(.name == $(jq -c .name "$file")) | .id")"
  if [ -n "$existing" ]; then
    gh api -X PUT "repos/$repo/rulesets/$existing" --input "$file" --jq '"updated ruleset \(.name) (\(.id))"'
  else
    gh api -X POST "repos/$repo/rulesets" --input "$file" --jq '"created ruleset \(.name) (\(.id))"'
  fi
done
