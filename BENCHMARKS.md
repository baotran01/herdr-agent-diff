# Release profile check

Measured on the target arm64 Mac on 2026-07-30 with Rust 1.97.1, fat LTO, one
codegen unit, stripped symbols, and abort-on-panic:

| Profile | Binary | 50 cold `status` launches |
| --- | ---: | ---: |
| `opt-level = "s"` | 2.3 MiB | 0.49 s |
| `opt-level = "3"` | 2.7 MiB | 0.42 s |

The approximately 1.4 ms difference per cold process launch is immaterial to
viewer interaction, while `"s"` reduces the binary by about 15%. The release
profile therefore keeps `"s"`.

Interactive work is kept off the input/render loop by the bounded worker in
`src/app.rs`. Snapshot generations discard obsolete scan results, filesystem
bursts are debounced for 150 ms, and rendered content is retained in a
32 MiB byte-bounded cache.
