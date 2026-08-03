# Agent instructions

- After every fix, rebuild the newest release version before verifying the result. Use the host-appropriate release target (for this macOS arm64 workspace: `cargo build --release --target aarch64-apple-darwin`). Restart or reopen any running Herdr viewer so it loads the rebuilt binary.
