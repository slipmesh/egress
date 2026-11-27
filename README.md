# egress

The egress agent of [slipmesh](https://github.com/slipmesh): a DaemonSet that resolves each
`Egress` list on the exit nodes it names and routes the result out through them.

## The `Egress` resource

`slipmesh.net/v1alpha1`, cluster-scoped, one per list. [`deploy/egress-crd.yaml`](deploy/egress-crd.yaml)
is its CRD, as `egress crd` prints it; a test fails when the two differ, so after a change to the
types, write it again:

```bash
cargo run -- crd > deploy/egress-crd.yaml
```

[`docs/example/egress.yaml`](docs/example/egress.yaml) is a list with every field:

- `nodes`: the exit nodes, by `Node` name;
- `ttl`: seconds an answer holds, 86400 when left out;
- `install`: off, the list is counted and the kernel left alone;
- `via`: optional, per family, a gateway of that family, a link, or both;
- `include` and `exclude`: one kind per source, `asns`, `prefixes`, `country` or `hostnames`;
  `label` keeps a source's networks from merging with another label's, and `ttl` overrides how
  long its answers hold.

The API server refuses a source of no kind or of two, a malformed AS number, network or country
code, a gateway of the other family and a link name the kernel would refuse. Every list and string
is bounded, which the API server needs to size the rules' cost; CI applies the CRD and the example
to a throwaway cluster, since no local evaluator sizes them.

## Resolving a list

Each source is asked as one query per AS number, country or host name, or one for its literal
prefixes; both families are kept. AS numbers and countries go to
[RIPEstat](https://stat.ripe.net/docs/data-api/) (announced prefixes, country resources with IPv4
written as prefixes), host names to the system's resolver, A and AAAA each asked on its own. A
source's own `ttl` is how long each of its answers holds; without one, a RIPEstat or literal answer
holds for `spec.ttl`, and a host's for its records' shortest TTL, at most `spec.ttl` and at least
60 s, even where `spec.ttl` is shorter. The answers merge within their label, an empty label being
none, and every network of `exclude` and every mesh endpoint is cut out of them.

A list's queries run at once. One that fails fails the whole resolve: a RIPEstat error, an answer
of the wrong shape or over 64 MiB, or either family of a host failing to look up, though a family
without records is only empty. So do a host with no address, a list with nothing left, and a
resolve that outlasts its budget of 90 s per round of five RIPEstat requests plus 15 s for the
hosts.

The router daemon of the Talos extensions keeps its own IPv4-only copy of this resolver for format
version 1 until that daemon stops resolving lists; a defect found in one copy is checked against
the other.

## Moving the lists of a version 1 file

`egress-export <file>` prints one `Egress` per node with a `bypass` list in a `slipmesh.yaml` of
format version 1, named for it, its lists merged in file order as version 1 merges them, and
writes nothing. It writes a node's mesh name as its `Node` name, which holds while the two are
the same. It reads the file through sliplib v0.19.0, the last release that reads that version. A
literal source's prefixes become one source per label, each written as it was read; a geoip source
without a label gets the label the daemon gives it; the refresh interval is written out as `ttl`,
the daemon's default where the file has none. Routes stay off.

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
