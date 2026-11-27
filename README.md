# egress

The egress agent of [slipmesh](https://github.com/slipmesh): a DaemonSet that resolves each
`Egress` list on the exit nodes it names and routes the result out through them.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny --locked check licenses bans sources
```

CI runs the same on every pull request and every push to `main` and `release-*`: fmt, deny and
the notices once, clippy and the tests for each of the x86-64 and arm64 musl targets, and
`cargo audit`, which also runs weekly. The licenses a dependency may come with are the ones
[`deny.toml`](deny.toml) allows; [`about.toml`](about.toml) repeats them for the notices the image
carries, rendered through [`about.hbs`](about.hbs). Code style:
[docs/CODESTYLE.md](docs/CODESTYLE.md).

## Release

1. Run the Release PR workflow under Actions, with a version or none to take it from the commits
   since the last tag. It opens `chore: release vX.Y.Z`, which bumps `Cargo.toml` and
   `Cargo.lock` and regenerates `CHANGELOG.md`.
2. Merging that pull request builds `ghcr.io/slipmesh/egress:sha-<commit>`, tags it `vX.Y.Z`, tags
   the commit and creates a GitHub Release with no assets.

Every other push and pull request only checks; a pull request also builds the image and pushes
nothing.

## License

MIT or Apache-2.0, at your option - see [`LICENSE-MIT`](./LICENSE-MIT) and
[`LICENSE-APACHE`](./LICENSE-APACHE).

The image carries the licenses and notices of what is linked into the binary in
`/usr/share/licenses/egress/`: `THIRD-PARTY-NOTICES` for the crates, rendered with
[cargo-about](https://embarkstudios.github.io/cargo-about/) from [`about.toml`](about.toml) and
[`about.hbs`](about.hbs), and `COPYRIGHT-rust-std.html` and `COPYRIGHT-musl` for the Rust standard
library and the musl libc. Every pull request renders the first, so a dependency under a license
`about.toml` does not accept fails there.
