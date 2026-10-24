# Contributing to Recouvrance

Thanks for helping. Bug reports, ideas, docs and code are all welcome.

## Before you start

- **Bugs and ideas**: open an issue with the templates. For a bug, say which Kubernetes version
  and platform, and what you expected.
- **Larger changes**: open an issue first so we can agree on the approach before you write code.
- **Security issues**: do not open a public issue; see [SECURITY.md](SECURITY.md).

## Development setup

See [docs/development.md](docs/development.md) for the build, the checks CI runs and the developer
tools (off-screen snapshots and smoke checks against a cluster). Test write actions on a
throwaway cluster such as [kind](https://kind.sigs.k8s.io), never on a shared one.

## Pull requests

`main` is protected: changes go through pull requests, merged by squash once the `CI result`
check passes, so history stays linear. Your branch's commits don't need to be signed: GitHub signs
the squash commit it writes on `main`.

1. Fork, then branch from `main`.
2. Keep the change focused; one topic per pull request.
3. Run the checks:
   ```sh
   cargo fmt --all
   cargo clippy --all-targets --features dev -- -D warnings
   cargo test --features dev
   ```
4. Add tests for logic (parsing, rules, filters, diagnosis…) and describe how you checked UI changes
   (a snapshot PNG in the pull request helps).
5. Update the docs when behavior changes. `CHANGELOG.md` is written from the commits at release.
6. Title the pull request as a [conventional commit](https://www.conventionalcommits.org): it becomes
   the commit on `main`, picks the next version and becomes the changelog line.
   - `feat: show GPU usage on nodes` → minor version, *Added*
   - `fix(logs): keep the scroll position` → patch version, *Fixed*
   - `perf: …`, `refactor: …`, `docs: …`; `chore:`, `ci:`, `build:`, `test:` stay out of the changelog
   - `feat!: …` or a `BREAKING CHANGE:` footer for breaking changes

## Releases

[release-plz](https://release-plz.dev) opens a *Release vX.Y.Z* pull request with the version
bump and the changelog when its workflow is run by hand (Actions → Release-plz → Run workflow; run
it again to bring the pull request up to date with `main`). Merging it tags `vX.Y.Z` (releases go to GitHub only; the crate is not published to crates.io); the tag
starts the [dist](https://opensource.axo.dev/cargo-dist) release workflow, which builds the
binaries (Linux and macOS, x86_64 and arm64), the install script, the updater, a `.deb` and an
AppImage, and publishes the GitHub release.

## Code conventions

- Modules by concern, files under ~800 lines; see [architecture](docs/architecture.md).
- No `unsafe`; no panics on user or cluster input (`anyhow` errors, shown to the user).
- Writes go through a confirmation that shows the `kubectl` equivalent, with a server dry run when
  the API supports it. Read-only stays the default.
- UI text is short and plain; say what happens, not how.

## License

By contributing, you agree that your contributions are licensed under the project's dual
MIT OR Apache-2.0 license.
