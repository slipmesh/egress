//! The routes of every list this node installs. All of them share one table and one protocol, read
//! from the node's annotations, and the routes in the kernel are the last good list.

use anyhow::{Context, Result};
use futures::{FutureExt, Stream, StreamExt};
use sliplib_config::annotations::{route_proto_id, route_table};
use sliplib_net::route::{MAIN_TABLE, Route, RtClient};
use sliplib_net::{Family, IpNet};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;

/// Below BIRD's 32, so both stay in the table and this one wins; not 0, which IPv4 lists back as
/// none.
pub const ROUTE_METRIC: u32 = 16;
const CLASS: &str = "egress";
/// A burst of kernel changes, this agent's own among them, gives one reconcile.
const SETTLE: Duration = Duration::from_secs(1);
const AFTER_FAILURE: Duration = Duration::from_secs(30);

/// The table and protocol of this node's routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    pub table: u32,
    pub protocol: u8,
}

/// The tag a node's `annotations` give: none without a route table, as before the switch.
pub fn tag(annotations: &BTreeMap<String, String>) -> Result<Option<Tag>> {
    let table_key = route_table(CLASS);
    let Some(table) = annotations.get(&table_key) else {
        return Ok(None);
    };
    let table = match table.as_str() {
        "main" => MAIN_TABLE,
        number => number
            .parse()
            .with_context(|| format!("{table_key} is {number:?}, not a table"))?,
    };
    let protocol_key = route_proto_id(CLASS);
    let protocol = annotations
        .get(&protocol_key)
        .with_context(|| format!("{protocol_key} is missing beside {table_key}"))?;
    let protocol = protocol
        .parse()
        .with_context(|| format!("{protocol_key} is {protocol:?}, not a protocol"))?;
    Ok(Some(Tag { table, protocol }))
}

/// The routes of `networks` under `tag`, each family's through its default route, `v4` or `v6`;
/// none for a family without one.
pub fn routes(
    networks: impl IntoIterator<Item = IpNet>,
    tag: Tag,
    v4: Option<&Route>,
    v6: Option<&Route>,
) -> Vec<Route> {
    networks
        .into_iter()
        .filter_map(|destination| {
            let default = if destination.addr().is_ipv4() { v4 } else { v6 }?;
            Some(Route {
                destination,
                table: tag.table,
                protocol: tag.protocol,
                gateway: default.gateway,
                oif: default.oif,
                metric: Some(ROUTE_METRIC),
            })
        })
        .collect()
}

/// What turns the kernel's `listed` routes into `wanted`: the routes to delete, then the routes to
/// replace. Nothing is deleted until `may_delete`.
pub fn plan(wanted: &[Route], listed: &[Route], may_delete: bool) -> (Vec<Route>, Vec<Route>) {
    // A replace overwrites the route of the same destination and metric, and no other.
    let key = |route: &Route| (route.destination, route.metric);
    let wanted_keys: HashSet<_> = wanted.iter().map(key).collect();
    let delete = listed
        .iter()
        .filter(|route| may_delete && !wanted_keys.contains(&key(route)))
        .cloned()
        .collect();
    let listed: HashMap<_, _> = listed.iter().map(|route| (key(route), route)).collect();
    let replace = wanted
        .iter()
        .filter(|route| listed.get(&key(route)) != Some(route))
        .cloned()
        .collect();
    (delete, replace)
}

/// What this node's routes should be.
#[derive(Debug, Default)]
struct Wanted {
    tag: Option<Tag>,
    /// Each installing list's last good networks, none before its first.
    lists: HashMap<String, Option<Vec<IpNet>>>,
    /// Whether every `Egress` has been listed once, so that no installing list is missing.
    listed: bool,
}

impl Wanted {
    /// The tag, every installing list's networks together, and whether every installing list is
    /// known, so that a route not among them may go.
    fn get(&self) -> (Option<Tag>, BTreeSet<IpNet>, bool) {
        let networks = self.lists.values().flatten().flatten().copied().collect();
        let known = self.listed && self.lists.values().all(Option::is_some);
        (self.tag, networks, known)
    }
}

/// What this node's routes should be, and the wake-up of the loop that installs them.
#[derive(Debug, Default)]
pub struct Installer {
    wanted: Mutex<Wanted>,
    wake: Notify,
}

impl Installer {
    fn change(&self, change: impl FnOnce(&mut Wanted)) {
        change(
            &mut self
                .wanted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.wake.notify_one();
    }

    pub fn set_tag(&self, tag: Option<Tag>) {
        self.change(|wanted| wanted.tag = tag);
    }

    /// Every `Egress` has been listed once.
    pub fn listed(&self) {
        self.change(|wanted| wanted.listed = true);
    }

    /// `name` installs, its networks not yet resolved.
    pub fn pending(&self, name: &str) {
        self.change(|wanted| {
            wanted.lists.insert(name.to_owned(), None);
        });
    }

    /// `name`'s last good networks, if it installs.
    pub fn resolved(&self, name: &str, networks: Vec<IpNet>) {
        self.change(|wanted| {
            if let Some(last) = wanted.lists.get_mut(name) {
                *last = Some(networks);
            }
        });
    }

    pub fn forget(&self, name: &str) {
        self.change(|wanted| {
            wanted.lists.remove(name);
        });
    }
}

/// Keeps the kernel's routes as `installer` wants them, after every change it or the kernel
/// reports. The routes stay when it ends.
pub async fn keep_installed(
    rt: RtClient,
    mut changes: impl Stream<Item = ()> + Unpin,
    installer: &Installer,
) {
    let mut installed_under = None;
    loop {
        tokio::select! {
            () = installer.wake.notified() => {}
            Some(()) = changes.next() => {}
        }
        tokio::time::sleep(SETTLE).await;
        while let Some(Some(())) = changes.next().now_or_never() {}
        let wanted = installer
            .wanted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get();
        if let Err(e) = reconcile(&rt, wanted, &mut installed_under).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "failed to install the routes, retrying"
            );
            tokio::time::sleep(AFTER_FAILURE).await;
            installer.wake.notify_one();
        }
    }
}

/// The tag routes were installed under, `installed_under`, when `tag` is another.
fn retired(installed_under: Option<Tag>, tag: Option<Tag>) -> Option<Tag> {
    installed_under.filter(|old| Some(*old) != tag)
}

/// Deletes the routes under a tag no longer given, then makes the kernel's routes under `tag` the
/// `networks`.
async fn reconcile(
    rt: &RtClient,
    (tag, networks, may_delete): (Option<Tag>, BTreeSet<IpNet>, bool),
    installed_under: &mut Option<Tag>,
) -> Result<()> {
    if let Some(old) = retired(*installed_under, tag) {
        for route in rt.routes(old.table, old.protocol).await? {
            rt.route_del(&route).await?;
        }
        tracing::info!(
            table = old.table,
            protocol = old.protocol,
            "routes under the old tag deleted"
        );
    }
    *installed_under = tag;
    let Some(tag) = tag else { return Ok(()) };
    let v4 = rt.default_route(Family::Ipv4).await?;
    let v6 = rt.default_route(Family::Ipv6).await?;
    let wanted = routes(networks, tag, v4.as_ref(), v6.as_ref());
    let listed = rt.routes(tag.table, tag.protocol).await?;
    let (delete, replace) = plan(&wanted, &listed, may_delete);
    // Deletes first: one without a metric may take the route of any metric at its destination.
    for route in &delete {
        rt.route_del(route).await?;
    }
    for route in &replace {
        rt.route_replace(route).await?;
    }
    if !delete.is_empty() || !replace.is_empty() {
        tracing::info!(
            deleted = delete.len(),
            replaced = replace.len(),
            "routes installed"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nets(nets: &[&str]) -> Vec<IpNet> {
        nets.iter().map(|net| net.parse().unwrap()).collect()
    }

    fn annotations(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    const TABLE: &str = "slipmesh.net/route_table.egress";
    const PROTOCOL: &str = "slipmesh.net/route_proto_id.egress";

    #[test]
    fn a_node_gives_its_tag_by_its_annotations_and_none_without_a_table() {
        assert_eq!(tag(&annotations(&[(PROTOCOL, "152")])).unwrap(), None);
        assert_eq!(
            tag(&annotations(&[(TABLE, "main"), (PROTOCOL, "152")])).unwrap(),
            Some(Tag {
                table: MAIN_TABLE,
                protocol: 152
            })
        );
        assert_eq!(
            tag(&annotations(&[(TABLE, "151"), (PROTOCOL, "152")])).unwrap(),
            Some(Tag {
                table: 151,
                protocol: 152
            })
        );
        for refused in [
            annotations(&[(TABLE, "151")]),
            annotations(&[(TABLE, "x"), (PROTOCOL, "152")]),
            annotations(&[(TABLE, "151"), (PROTOCOL, "x")]),
        ] {
            assert!(tag(&refused).is_err(), "{refused:?}");
        }
    }

    const TAG: Tag = Tag {
        table: 152,
        protocol: 152,
    };

    fn route(destination: &str, gateway: &str, oif: u32, metric: u32) -> Route {
        Route {
            destination: destination.parse().unwrap(),
            table: TAG.table,
            protocol: TAG.protocol,
            gateway: Some(gateway.parse().unwrap()),
            oif: Some(oif),
            metric: Some(metric),
        }
    }

    #[test]
    fn a_route_leaves_through_its_familys_default_route_and_a_family_without_one_gets_none() {
        let default = |destination: &str, gateway: &str| Route {
            destination: destination.parse().unwrap(),
            table: MAIN_TABLE,
            protocol: 4,
            gateway: Some(gateway.parse().unwrap()),
            oif: Some(2),
            metric: None,
        };
        let v4 = default("0.0.0.0/0", "192.0.2.1");
        let v6 = default("::/0", "2001:db8::1");
        let networks = nets(&["198.51.100.0/24", "3fff::/20"]);
        assert_eq!(
            routes(networks.clone(), TAG, Some(&v4), Some(&v6)),
            [
                route("198.51.100.0/24", "192.0.2.1", 2, ROUTE_METRIC),
                route("3fff::/20", "2001:db8::1", 2, ROUTE_METRIC),
            ]
        );
        assert_eq!(
            routes(networks, TAG, Some(&v4), None),
            [route("198.51.100.0/24", "192.0.2.1", 2, ROUTE_METRIC)]
        );
    }

    #[test]
    fn a_plan_replaces_what_differs_and_deletes_what_no_list_wants_once_it_may() {
        let kept = route("198.51.100.0/24", "192.0.2.1", 2, ROUTE_METRIC);
        let moved = route("203.0.113.0/24", "192.0.2.1", 2, ROUTE_METRIC);
        let new = route("192.0.2.128/25", "192.0.2.1", 2, ROUTE_METRIC);
        let wanted = [kept.clone(), moved.clone(), new.clone()];
        let unwanted = route("198.18.0.0/15", "192.0.2.1", 2, ROUTE_METRIC);
        // Another metric than this agent's: a replace would leave it beside the new one.
        let other_metric = route("198.51.100.0/24", "192.0.2.1", 2, 0);
        let listed = [
            kept,
            route("203.0.113.0/24", "192.0.2.254", 3, ROUTE_METRIC),
            unwanted.clone(),
            other_metric.clone(),
        ];
        assert_eq!(
            plan(&wanted, &listed, true),
            (
                vec![unwanted, other_metric],
                vec![moved.clone(), new.clone()]
            )
        );
        assert_eq!(plan(&wanted, &listed, false), (vec![], vec![moved, new]));
    }

    #[test]
    fn deletes_wait_until_every_list_was_listed_and_has_resolved_once() {
        let installer = Installer::default();
        let get = || installer.wanted.lock().unwrap().get();
        installer.set_tag(Some(TAG));
        installer.pending("a");
        installer.pending("b");
        installer.resolved("a", nets(&["192.0.2.0/24", "3fff::/20"]));
        // Not installing: its networks are none of the node's.
        installer.resolved("c", nets(&["198.51.100.0/24"]));
        assert_eq!(
            get(),
            (
                Some(TAG),
                nets(&["192.0.2.0/24", "3fff::/20"]).into_iter().collect(),
                false
            )
        );
        installer.resolved("b", nets(&["192.0.2.0/24"]));
        assert!(!get().2, "before every Egress was listed");
        installer.listed();
        assert!(get().2);
        installer.forget("a");
        assert_eq!(get().1, nets(&["192.0.2.0/24"]).into_iter().collect());
    }

    #[test]
    fn the_routes_under_a_tag_go_once_the_node_gives_another_or_none() {
        let other = Tag {
            table: MAIN_TABLE,
            ..TAG
        };
        assert_eq!(retired(Some(TAG), Some(TAG)), None);
        assert_eq!(retired(Some(TAG), Some(other)), Some(TAG));
        assert_eq!(retired(Some(TAG), None), Some(TAG));
        assert_eq!(retired(None, Some(TAG)), None);
    }
}
