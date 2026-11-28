use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::Duration;

use pipewire as pw;

use crate::audio::shared::Shared;

use super::graph::Graph;
use super::properties;

/// How long a requested link may take to appear before it is asked again.
const LINK_GRACE: Duration = Duration::from_secs(1);

/// Dropping the proxy before the link appears abandons it.  Once it
/// exists it lives as long as this client.
pub(super) struct MadeLink {
    link: pw::link::Link,
    asked: std::time::Instant,
    seen: bool,
}

pub(super) struct Observed {
    pub(super) rate: Arc<AtomicU32>,
    pub(super) quantum: Arc<AtomicU64>,
    pub(super) error: Arc<AtomicBool>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sync_wanted_links(
    core: &pw::core::CoreRc,
    graph: &mut Graph,
    made: &mut HashMap<(u32, u32), MadeLink>,
    own: u32,
    source: &str,
    sink: &str,
    extra_outputs: &[(usize, &str)],
    shared: &Shared,
) {
    if !graph.dirty && made.values().all(|link| link.seen) {
        // The extra-output list can change without `dirty`.
        let wanted = graph.wanted_links(own, source, sink, extra_outputs);
        let wanted_ports: HashSet<(u32, u32)> =
            wanted.iter().map(|link| (link.1, link.3)).collect();
        if made.len() == wanted_ports.len() && made.keys().all(|ports| wanted_ports.contains(ports))
        {
            return;
        }
    }
    graph.dirty = false;
    let wanted = graph.wanted_links(own, source, sink, extra_outputs);
    let wanted_ports: HashSet<(u32, u32)> = wanted.iter().map(|link| (link.1, link.3)).collect();
    // A device that came back with other ports: drop the link on the
    // server too, if it still exists.
    let stale: Vec<(u32, u32)> = made
        .keys()
        .filter(|ports| !wanted_ports.contains(ports))
        .copied()
        .collect();
    for ports in stale {
        if let Some(old) = made.remove(&ports) {
            if graph.has_link(ports.0, ports.1) {
                let _ = core.destroy_object(old.link);
            }
        }
    }
    for (out_node, out_port, in_node, in_port) in wanted {
        let ports = (out_port, in_port);
        let present = graph.has_link(out_port, in_port);
        match made.get_mut(&ports) {
            Some(link) if present => {
                link.seen = true;
                continue;
            }
            Some(link) if !link.seen && link.asked.elapsed() < LINK_GRACE => continue,
            // Removed, or never appeared: ask again.
            Some(_) => {
                made.remove(&ports);
            }
            // Someone else already linked these ports.
            None if present => continue,
            None => {}
        }
        let props = properties(&[
            ("link.output.node", &out_node.to_string()),
            ("link.output.port", &out_port.to_string()),
            ("link.input.node", &in_node.to_string()),
            ("link.input.port", &in_port.to_string()),
            ("object.linger", "false"),
        ]);
        match core.create_object::<pw::link::Link>("link-factory", &props) {
            Ok(link) => {
                made.insert(
                    ports,
                    MadeLink {
                        link,
                        asked: std::time::Instant::now(),
                        seen: false,
                    },
                );
            }
            Err(error) => shared.set_status(format!("Linking RigWerk: {error}")),
        }
    }
}
