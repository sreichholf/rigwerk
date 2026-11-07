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
    let port =
        |graph: &mut Graph, id: u32, node: u32, dir: &str, ch: &str, name: &str, monitor: bool| {
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
