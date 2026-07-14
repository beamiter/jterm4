## Summary

<!-- Explain the user-visible or architectural change. -->

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --all-features --locked`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked`
- [ ] Relevant Wayland/X11 and VTE/Block smoke checks completed

## Safety and compatibility

- [ ] No credentials, private hosts, personal paths, or captured terminal output were committed
- [ ] PTY/process cleanup and persisted-state behavior were considered
- [ ] Configuration changes preserve existing defaults or document migration
- [ ] User-facing behavior and `CHANGELOG.md` were updated where appropriate
