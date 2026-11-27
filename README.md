# egress

The egress agent of [slipmesh](https://github.com/slipmesh): a DaemonSet that resolves each
`Egress` list on the exit nodes it names and routes the result out through them.

## The `Egress` resource

`slipmesh.net/v1alpha1`, cluster-scoped, one per list. [`deploy/egress-crd.yaml`](deploy/egress-crd.yaml)
is its CRD, as `egress crd` prints it; CI fails when the two differ, so after a change to the
types, write it again:

```bash
cargo run -- crd > deploy/egress-crd.yaml
```

```yaml
apiVersion: slipmesh.net/v1alpha1
kind: Egress
metadata:
  name: example
spec:
  nodes: [node-a]               # the exit nodes, by Node name
  interval: 86400               # seconds between resolutions, 86400 when left out
  install: false                # off: count the list, leave the kernel alone
  via:                          # optional, per family: a gateway of its family, a link, or both
    ipv4: {gateway: 192.0.2.1}
  include:                      # one kind per source; `label` keeps its networks apart
    - {label: example, asns: [AS64496]}
    - {prefixes: [192.0.2.0/24, 2001:db8::/32]}
    - {country: XX}
    - {hostnames: [example.com]}
  exclude:
    - {prefixes: [192.0.2.128/25]}
```

The API server refuses a source of no kind or of two, a malformed AS number, network or country
code, a gateway of the other family and a link name the kernel would refuse.

## Resolving a list

Each source is asked as one query per AS number, country or host name, or one for its literal
prefixes; both families are kept. AS numbers and countries go to
[RIPEstat](https://stat.ripe.net/docs/data-api/) (announced prefixes, country resources), host
names to the system's resolver. A RIPEstat or literal answer holds for `spec.interval`; a host's
for its records' shortest TTL, at least 60 s and at most the interval. The answers merge within
their label, and every network of `exclude` and every mesh endpoint is cut out of them. One query
that fails, or a list with nothing left, fails the whole resolve, and the last good list stays.

The router daemon of the Talos extensions keeps its own IPv4-only copy of this resolver for format
version 1 until that daemon stops resolving lists; neither copy follows the other.

## Moving the lists of a version 1 file

`egress-export <file>` prints one `Egress` per `bypass` list of a `slipmesh.yaml` of format
version 1, named for its node, and writes nothing. It reads the file through sliplib v0.19.0, the
last release that reads that version. A literal source's prefixes become one source per label, a
geoip source without a label gets the label the daemon gives it, and the refresh interval is
written out, the daemon's default where the file has none. Routes stay off.

```bash
cargo run --bin egress-export -- slipmesh.yaml > egress.yaml
```

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
