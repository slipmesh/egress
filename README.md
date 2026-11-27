# egress

The egress agent of [slipmesh](https://github.com/slipmesh): a DaemonSet that resolves each
`Egress` list on the exit nodes it names and routes the result out through them.

## The `Egress` resource

`slipmesh.net/v1alpha1`, cluster-scoped, one per list. [`deploy/egress-crd.yaml`](deploy/egress-crd.yaml)
is its CRD, as `egress crd egress` prints it; a test fails when the two differ, so after a change to the
types, write it again:

```bash
cargo run -- crd egress > deploy/egress-crd.yaml
```

[`docs/example/egress.yaml`](docs/example/egress.yaml) is a list with every field:

- `nodes`: the exit nodes, by `Node` name;
- `ttl`: seconds an answer holds, 86400 when left out;
- `install`: off, the list is counted and the kernel left alone;
- `include` and `exclude`: one kind per source, `asns`, `prefixes`, `country` or `hostnames`;
  `label` keeps a source's networks from merging with another label's, and `ttl` overrides how
  long its answers hold.

The API server refuses a source of no kind or of two, and a malformed AS number, network or
country code. Every list and string is bounded, which the API server needs to size the rules'
cost; CI applies the CRD and the example to a throwaway cluster, since no local evaluator sizes
them.

Routes leave through each family's default route's gateway.

## Resolving a list

Each source is asked as one query per AS number, country or host name, or one for its literal
prefixes; both families are kept. AS numbers and countries go to
[RIPEstat](https://stat.ripe.net/docs/data-api/) (announced prefixes, country resources with IPv4
written as prefixes), host names to the system's resolver, A and AAAA each asked on its own. A
source's own `ttl` is how long each of its answers holds; without one, a RIPEstat or literal answer
holds for `spec.ttl`, and a host's for its records' shortest TTL, at most `spec.ttl` and at least
60 s, even where `spec.ttl` is shorter. The answers merge within their label, an empty label being
none, and every network of `exclude` is cut out of them.

Every node's endpoint is cut out too, so no list routes the mesh's own tunnels. The endpoints come
from the `Mesh` called `mesh`, which `slipmesh manifest` writes; its CRD is
[`deploy/mesh-crd.yaml`](deploy/mesh-crd.yaml), as `egress crd mesh` prints it, checked by the same
test. A list's first resolve waits until the agent has read it. Each endpoint is asked again on
every resolve, a host name with both families, and a change to the endpoints composes every list
again at once. Without that object, or with an endpoint failing to look up, the resolve fails.

A list's queries run at once. Each RIPEstat request has 90 s and each family of a host 15 s; a
query that fails is asked again, up to six attempts, waiting 5 s and twice as long after each
failure, at most 3 min. A RIPEstat error, an answer of the wrong shape or over 64 MiB, and either
family of a host failing to look up are failures; a family without records is only empty. The
resolve fails once one query has given up, and so do a host with no address and a list with
nothing left.

The router daemon of the Talos extensions keeps its own IPv4-only copy of this resolver for format
version 1 until that daemon stops resolving lists; a defect found in one copy is checked against
the other.

## Running the agent

```bash
kubectl apply -f deploy/egress-crd.yaml -f deploy/mesh-crd.yaml -f deploy/daemonset.yaml
```

[`deploy/daemonset.yaml`](deploy/daemonset.yaml) runs `egress run` on every node, in
`kube-system`, with its ServiceAccount and the RBAC it needs: to watch every `Egress`, its own
`Node` and the `Mesh`, and to patch the lists' status; `NET_ADMIN` for the routes. A pod resolves each `Egress`
that names its node, `NODE_NAME` through the Downward API, and nothing else. Its answers live in
memory: each is asked again
once its lifetime is over, and the list is composed again. A changed `spec` starts the list anew.
After a query gives up, the list is tried again 3 min later.

After every resolve the node applies its own entry of `status.nodes`, through server-side apply
with the field manager `egress/<node>`: `observedGeneration`, `resolvedAt` of the last good
resolve, the prefix count per family and the `Resolved` condition, its message the error of a
failed one. A list that stops naming the node loses that entry.

`--metrics <address:port>` is where `/metrics` is served; the DaemonSet gives the node's
InternalIP, `status.hostIP`, and port 9478. The series are `slipmesh_egress_prefixes{egress,family}`,
`slipmesh_egress_last_resolve_timestamp_seconds{egress}` and
`slipmesh_egress_resolve_errors_total{egress}`.

### Routes

A list with `install` on has its networks routed out of the node. Every such list of a node shares
one table and one protocol, the node's annotations `slipmesh.net/route_table.egress` (a number, or
`main`) and `slipmesh.net/route_proto_id.egress`; without the table annotation the agent installs
and deletes nothing. Each route leaves through the gateway and link of its family's default route
in `main`, with metric 16 in both families, below BIRD's 32; a family without a default route gets
no routes. The routes of the node's lists together are reconciled with the kernel's routes under
that table and protocol after every resolve, change of a list or the annotations, and change to
the kernel's links, routes or addresses, a burst of them waiting 1 s and giving one reconcile.

The routes in the kernel are the last good list: a failed resolve leaves them, and none is deleted
until every `Egress` has been listed and every installing list has resolved once since the start
or its last change. Routes found under the table and protocol at the start, whoever installed
them, are kept until then. A table or protocol changed while the agent runs deletes the routes
under the old one; one changed while it is stopped leaves them. Stopping the agent leaves the
routes.

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
