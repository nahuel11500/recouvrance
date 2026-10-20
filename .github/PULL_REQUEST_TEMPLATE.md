<!-- Title: a conventional commit, e.g. "feat: show GPU usage on nodes"; it becomes the changelog line. -->

## What and why

<!-- What this changes and the problem it solves. Link the issue: Fixes #123 -->

## How it was checked

<!-- Tests added, clusters tried (kind, EKS…), snapshots of UI changes. -->

## Checklist

- [ ] `cargo fmt`, `cargo clippy --all-targets --features dev -- -D warnings` and `cargo test --features dev` pass
- [ ] The title is a [conventional commit](https://www.conventionalcommits.org) (`feat: …`, `fix: …`)
- [ ] Docs updated if behavior changed
- [ ] Writes (if any) are confirmed, show the `kubectl` equivalent and use a dry run when possible
