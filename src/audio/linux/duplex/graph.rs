//! PipeWire registry graph: nodes, ports, and the links RigWerk wants.

use std::collections::HashMap;

use pipewire as pw;
use pw::spa;

use crate::audio::output_map::MAX_FOLLOWERS;

/// One audio port of another node, as the registry describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PortRef {
    pub(crate) id: u32,
    /// `audio.channel`, e.g. `FL`, `FR`, `MONO`, `AUX0`; empty when unset.
    pub(crate) channel: String,
}

/// Chooses which of a node's ports feed (or take) RigWerk's left and right
/// port.  `FL`/`FR` win; a `MONO` port, or a node with a single port, feeds
/// both sides when `mono_to_both` is set (a mono source) and only the left
/// side otherwise (a mono sink, so the two sides are not summed into it at
/// double level).  Without channel names the first two ports in id order are
/// used.
pub(crate) fn map_ports(ports: &[PortRef], mono_to_both: bool) -> [Option<u32>; 2] {
    let find = |name: &str| {
        ports
            .iter()
            .filter(|port| port.channel.eq_ignore_ascii_case(name))
            .map(|port| port.id)
            .min()
    };
    let left = find("FL");
    let right = find("FR");
    if left.is_some() || right.is_some() {
        return match (left, right) {
            (Some(left), Some(right)) => [Some(left), Some(right)],
            // Only one side is labelled: treat it like a mono port.
            (Some(only), None) | (None, Some(only)) => mono(only, mono_to_both),
            (None, None) => [None, None],
        };
    }
    if let Some(port) = find("MONO") {
        return mono(port, mono_to_both);
    }
    let mut ids: Vec<u32> = ports.iter().map(|port| port.id).collect();
    ids.sort_unstable();
    match ids.as_slice() {
        [] => [None, None],
        [only] => mono(*only, mono_to_both),
        [first, second, ..] => [Some(*first), Some(*second)],
    }
}

fn mono(port: u32, mono_to_both: bool) -> [Option<u32>; 2] {
    if mono_to_both {
        [Some(port), Some(port)]
    } else {
        [Some(port), None]
    }
}

/// What the registry has told us about the graph, kept on the main loop.
#[derive(Default)]
pub(crate) struct Graph {
    /// Node id → (`node.name`, `media.class`).
    nodes: HashMap<u32, (String, String)>,
    /// Port id → (owning node id, direction is output, monitor, channel).
    ports: HashMap<u32, PortInfo>,
    /// Existing links as (output port, input port), by link id.
    links: HashMap<u32, (u32, u32)>,
    /// Set whenever a node, port or link came or went.
    pub(super) dirty: bool,
}

#[derive(Clone, Debug)]
struct PortInfo {
    node: u32,
    output: bool,
    monitor: bool,
    channel: String,
    name: String,
}

impl Graph {
    fn node_by_name(&self, name: &str) -> Option<u32> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.0 == name)
            .map(|(id, _)| *id)
            .min()
    }

    fn ports_of(&self, node: u32, output: bool, monitor: bool) -> Vec<PortRef> {
        let mut ports: Vec<PortRef> = self
            .ports
            .iter()
            .filter(|(_, port)| {
                port.node == node && port.output == output && port.monitor == monitor
            })
            .map(|(id, port)| PortRef {
                id: *id,
                channel: port.channel.clone(),
            })
            .collect();
        ports.sort_by_key(|port| port.id);
        ports
    }

    /// The output ports a source feeds RigWerk from.  Plain outputs come
    /// first.  A node with only monitor outputs (a virtual source such as
    /// `Audio/Source/Virtual` publishes its capture ports that way) uses
    /// those, unless it is a sink: a sink's monitor is its own playback, and
    /// taking it would feed RigWerk's output back into its input.
    fn source_ports(&self, node: u32) -> Vec<PortRef> {
        let plain = self.ports_of(node, true, false);
        if !plain.is_empty() {
            return plain;
        }
        let is_sink = self
            .nodes
            .get(&node)
            .is_some_and(|(_, class)| class.starts_with("Audio/Sink"));
        if is_sink {
            Vec::new()
        } else {
            self.ports_of(node, true, true)
        }
    }

    /// RigWerk's own port by name (`input_FL`, `output_FR`, …).
    fn own_port(&self, node: u32, name: &str) -> Option<u32> {
        self.ports
            .iter()
            .filter(|(_, port)| port.node == node && port.name == name)
            .map(|(id, _)| *id)
            .min()
    }

    /// The links the bridge needs, as (output node, output port, input node,
    /// input port).  Empty until RigWerk's own ports and the device ports
    /// are all known.  `followers` is `(slot, sink node.name)` for each
    /// hardware follower that should play; virtual slots are omitted.
    pub(crate) fn wanted_links(
        &self,
        own: u32,
        source: &str,
        sink: &str,
        followers: &[(usize, &str)],
    ) -> Vec<(u32, u32, u32, u32)> {
        let mut wanted = Vec::new();
        if let Some(source_node) = self.node_by_name(source) {
            let from = map_ports(&self.source_ports(source_node), true);
            for (side, name) in ["input_FL", "input_FR"].iter().enumerate() {
                if let (Some(out_port), Some(in_port)) = (from[side], self.own_port(own, name)) {
                    wanted.push((source_node, out_port, own, in_port));
                }
            }
        }
        self.link_sink(&mut wanted, own, sink, ["output_FL", "output_FR"]);
        for &(slot, name) in followers {
            if slot >= MAX_FOLLOWERS || name.is_empty() {
                continue;
            }
            let ports = [format!("follow_{slot}_FL"), format!("follow_{slot}_FR")];
            self.link_sink(
                &mut wanted,
                own,
                name,
                [ports[0].as_str(), ports[1].as_str()],
            );
        }
        wanted
    }

    fn link_sink(
        &self,
        wanted: &mut Vec<(u32, u32, u32, u32)>,
        own: u32,
        sink: &str,
        port_names: [&str; 2],
    ) {
        let Some(sink_node) = self.node_by_name(sink) else {
            return;
        };
        let to = map_ports(&self.ports_of(sink_node, false, false), false);
        for (side, name) in port_names.iter().enumerate() {
            if let (Some(out_port), Some(in_port)) = (self.own_port(own, name), to[side]) {
                wanted.push((own, out_port, sink_node, in_port));
            }
        }
    }

    pub(super) fn has_link(&self, output_port: u32, input_port: u32) -> bool {
        self.links
            .values()
            .any(|link| *link == (output_port, input_port))
    }

    pub(super) fn add_global(
        &mut self,
        id: u32,
        kind: &pw::types::ObjectType,
        props: &spa::utils::dict::DictRef,
    ) {
        match kind {
            pw::types::ObjectType::Node => {
                if let Some(name) = props.get("node.name") {
                    let class = props.get("media.class").unwrap_or("").to_owned();
                    self.nodes.insert(id, (name.to_owned(), class));
                    self.dirty = true;
                }
            }
            pw::types::ObjectType::Port => {
                let node = props.get("node.id").and_then(|text| text.parse().ok());
                let Some(node) = node else {
                    return;
                };
                self.ports.insert(
                    id,
                    PortInfo {
                        node,
                        output: props.get("port.direction") == Some("out"),
                        monitor: props.get("port.monitor") == Some("true"),
                        channel: props.get("audio.channel").unwrap_or("").to_owned(),
                        name: props.get("port.name").unwrap_or("").to_owned(),
                    },
                );
                self.dirty = true;
            }
            pw::types::ObjectType::Link => {
                let output = props
                    .get("link.output.port")
                    .and_then(|text| text.parse().ok());
                let input = props
                    .get("link.input.port")
                    .and_then(|text| text.parse().ok());
                if let (Some(output), Some(input)) = (output, input) {
                    self.links.insert(id, (output, input));
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    pub(super) fn remove_global(&mut self, id: u32) {
        let removed = self.nodes.remove(&id).is_some()
            | self.ports.remove(&id).is_some()
            | self.links.remove(&id).is_some();
        if removed {
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{map_ports, Graph, PortRef};

    fn port(id: u32, channel: &str) -> PortRef {
        PortRef {
            id,
            channel: channel.to_owned(),
        }
    }

    #[test]
    fn stereo_ports_map_by_channel_name() {
        let ports = [port(12, "FR"), port(11, "FL"), port(13, "RL")];
        assert_eq!(map_ports(&ports, true), [Some(11), Some(12)]);
        assert_eq!(map_ports(&ports, false), [Some(11), Some(12)]);
    }

    #[test]
    fn a_mono_source_feeds_both_sides_and_a_mono_sink_takes_the_left() {
        let ports = [port(40, "MONO")];
        assert_eq!(map_ports(&ports, true), [Some(40), Some(40)]);
        assert_eq!(map_ports(&ports, false), [Some(40), None]);
        let unnamed = [port(7, "")];
        assert_eq!(map_ports(&unnamed, true), [Some(7), Some(7)]);
    }

    #[test]
    fn unnamed_ports_use_the_first_two_by_id() {
        let ports = [port(9, "AUX1"), port(5, "AUX0"), port(30, "AUX2")];
        assert_eq!(map_ports(&ports, true), [Some(5), Some(9)]);
        assert_eq!(map_ports(&[], true), [None, None]);
    }

    #[test]
    fn a_single_labelled_side_counts_as_mono() {
        assert_eq!(map_ports(&[port(3, "FL")], true), [Some(3), Some(3)]);
        assert_eq!(map_ports(&[port(3, "FR")], false), [Some(3), None]);
    }

    fn dict(pairs: &[(&str, &str)]) -> pipewire::properties::PropertiesBox {
        super::super::properties(pairs)
    }

    fn add_node(graph: &mut Graph, id: u32, name: &str, class: &str) {
        graph.add_global(
            id,
            &pipewire::types::ObjectType::Node,
            dict(&[("node.name", name), ("media.class", class)]).dict(),
        );
    }

    fn add_port(graph: &mut Graph, id: u32, node: u32, output: bool, name: &str, monitor: bool) {
        let node = node.to_string();
        let channel = &name[name.len() - 2..];
        graph.add_global(
            id,
            &pipewire::types::ObjectType::Port,
            dict(&[
                ("node.id", &node),
                ("port.direction", if output { "out" } else { "in" }),
                ("audio.channel", channel),
                ("port.name", name),
                ("port.monitor", if monitor { "true" } else { "false" }),
            ])
            .dict(),
        );
    }

    /// RigWerk (node 3, ports 30-33) and a stereo sink (node 2, ports 20/21
    /// with monitors 22/23).
    fn rigwerk_and_sink() -> Graph {
        let mut graph = Graph::default();
        add_node(&mut graph, 2, "speakers", "Audio/Sink");
        add_port(&mut graph, 20, 2, false, "playback_FL", false);
        add_port(&mut graph, 21, 2, false, "playback_FR", false);
        add_port(&mut graph, 22, 2, true, "monitor_FL", true);
        add_port(&mut graph, 23, 2, true, "monitor_FR", true);
        add_node(&mut graph, 3, "RigWerk", "");
        add_port(&mut graph, 30, 3, false, "input_FL", false);
        add_port(&mut graph, 31, 3, false, "input_FR", false);
        add_port(&mut graph, 32, 3, true, "output_FL", false);
        add_port(&mut graph, 33, 3, true, "output_FR", false);
        graph
    }

    #[test]
    fn a_virtual_source_with_only_monitor_ports_is_linked() {
        let mut graph = rigwerk_and_sink();
        add_node(&mut graph, 1, "vsrc", "Audio/Source/Virtual");
        add_port(&mut graph, 10, 1, true, "capture_FL", true);
        add_port(&mut graph, 11, 1, true, "capture_FR", true);
        add_port(&mut graph, 12, 1, false, "input_FL", false);
        add_port(&mut graph, 13, 1, false, "input_FR", false);
        assert_eq!(
            graph.wanted_links(3, "vsrc", "speakers", &[]),
            vec![
                (1, 10, 3, 30),
                (1, 11, 3, 31),
                (3, 32, 2, 20),
                (3, 33, 2, 21)
            ]
        );
    }

    #[test]
    fn a_sink_is_never_used_as_a_source_through_its_monitor() {
        let mut graph = rigwerk_and_sink();
        add_node(&mut graph, 4, "other_sink", "Audio/Sink");
        add_port(&mut graph, 40, 4, false, "playback_FL", false);
        add_port(&mut graph, 41, 4, true, "monitor_FL", true);
        add_port(&mut graph, 42, 4, true, "monitor_FR", true);
        assert_eq!(
            graph.wanted_links(3, "other_sink", "speakers", &[]),
            vec![(3, 32, 2, 20), (3, 33, 2, 21)]
        );
        // Plain outputs win over monitor outputs on a source.
        add_node(&mut graph, 5, "mixed", "Audio/Source");
        add_port(&mut graph, 50, 5, true, "monitor_FL", true);
        add_port(&mut graph, 51, 5, true, "capture_FL", false);
        add_port(&mut graph, 52, 5, true, "capture_FR", false);
        assert_eq!(
            graph.wanted_links(3, "mixed", "speakers", &[])[..2],
            [(5, 51, 3, 30), (5, 52, 3, 31)]
        );
    }

    #[test]
    fn wanted_links_wire_source_to_rigwerk_to_sink_and_skip_monitors() {
        use pipewire::types::ObjectType;
        let mut graph = Graph::default();
        let node = |graph: &mut Graph, id, name| {
            graph.add_global(id, &ObjectType::Node, dict(&[("node.name", name)]).dict());
        };
        let port = |graph: &mut Graph,
                    id: u32,
                    node: u32,
                    dir: &str,
                    ch: &str,
                    name: &str,
                    monitor: bool| {
            let node = node.to_string();
            graph.add_global(
                id,
                &ObjectType::Port,
                dict(&[
                    ("node.id", &node),
                    ("port.direction", dir),
                    ("audio.channel", ch),
                    ("port.name", name),
                    ("port.monitor", if monitor { "true" } else { "false" }),
                ])
                .dict(),
            );
        };
        node(&mut graph, 1, "gp5");
        node(&mut graph, 2, "speakers");
        node(&mut graph, 3, "RigWerk");
        port(&mut graph, 10, 1, "out", "FL", "capture_FL", false);
        port(&mut graph, 11, 1, "out", "FR", "capture_FR", false);
        port(&mut graph, 20, 2, "in", "FL", "playback_FL", false);
        port(&mut graph, 21, 2, "in", "FR", "playback_FR", false);
        port(&mut graph, 22, 2, "out", "FL", "monitor_FL", true);
        port(&mut graph, 23, 2, "out", "FR", "monitor_FR", true);
        port(&mut graph, 30, 3, "in", "FL", "input_FL", false);
        port(&mut graph, 31, 3, "in", "FR", "input_FR", false);
        port(&mut graph, 32, 3, "out", "FL", "output_FL", false);
        port(&mut graph, 33, 3, "out", "FR", "output_FR", false);
        assert!(graph.dirty);
        assert_eq!(
            graph.wanted_links(3, "gp5", "speakers", &[]),
            vec![
                (1, 10, 3, 30),
                (1, 11, 3, 31),
                (3, 32, 2, 20),
                (3, 33, 2, 21)
            ]
        );

        // An existing link is seen; removing the source drops its links.
        graph.add_global(
            50,
            &ObjectType::Link,
            dict(&[("link.output.port", "10"), ("link.input.port", "30")]).dict(),
        );
        assert!(graph.has_link(10, 30));
        graph.dirty = false;
        graph.remove_global(1);
        graph.remove_global(10);
        graph.remove_global(11);
        assert!(graph.dirty);
        assert_eq!(
            graph.wanted_links(3, "gp5", "speakers", &[]),
            vec![(3, 32, 2, 20), (3, 33, 2, 21)]
        );
    }

    #[test]
    fn follower_sinks_link_from_preallocated_port_pairs() {
        let mut graph = rigwerk_and_sink();
        add_port(&mut graph, 34, 3, true, "follow_0_FL", false);
        add_port(&mut graph, 35, 3, true, "follow_0_FR", false);
        add_node(&mut graph, 6, "phones", "Audio/Sink");
        add_port(&mut graph, 60, 6, false, "playback_FL", false);
        add_port(&mut graph, 61, 6, false, "playback_FR", false);
        let links = graph.wanted_links(3, "missing", "speakers", &[(0, "phones")]);
        assert!(links.contains(&(3, 32, 2, 20)));
        assert!(links.contains(&(3, 33, 2, 21)));
        assert!(links.contains(&(3, 34, 6, 60)));
        assert!(links.contains(&(3, 35, 6, 61)));
        // A missing follower stays in the session: no link, bridge keeps going.
        let only_clock = graph.wanted_links(3, "missing", "speakers", &[(0, "gone")]);
        assert_eq!(only_clock, vec![(3, 32, 2, 20), (3, 33, 2, 21)]);
    }
}
