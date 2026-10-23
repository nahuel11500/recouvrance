# Notes for coding agents

## Building

- **Use at most half the machine's cores.** Set the job count on every cargo command
  that compiles (build, run, test, clippy, check, doc):

  ```bash
  CARGO_BUILD_JOBS=$(( $(nproc) / 2 )) cargo build --release --features dev
  ```

  A release build links with thin LTO in one codegen unit and takes several minutes;
  with every core busy the machine becomes unusable for the person working on it.
- **One build at a time.** Several agents or worktrees may share the machine. Before
  starting a build, check that no other is running (`pgrep -a cargo`) and wait for it to
  finish: two builds at half the cores each take all of them.
- Before a PR: `cargo fmt`, `cargo clippy --all-targets --features dev -- -D warnings`
  and `cargo test --features dev`, all with the job count above.
