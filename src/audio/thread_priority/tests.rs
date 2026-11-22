use std::path::{Path, PathBuf};

use super::{
    fast_cpus_in, narrowing, parse_cpu_list, pin_worker_to_fast_cores, prepare_fast_cores,
    priority_for, read_small, PinOnce, PromoteOnce, MAX_CPUS, SYSFS_READ_LIMIT,
};

#[test]
fn the_rt_limit_caps_the_priority() {
    assert_eq!(priority_for(95, 1, 99), 70);
    assert_eq!(priority_for(40, 1, 99), 40);
    // No limit: try the wanted priority and let the kernel decide.
    assert_eq!(priority_for(0, 1, 99), 70);
    assert_eq!(priority_for(libc::RLIM_INFINITY, 1, 99), 70);
}

#[test]
fn promotion_is_tried_once_and_never_panics() {
    let mut once = PromoteOnce::default();
    std::thread::spawn(move || {
        once.run();
        once.run();
        assert!(once.tried);
    })
    .join()
    .unwrap();
}

#[test]
fn pinning_is_tried_once_and_never_panics() {
    // Whatever this machine's sysfs says, preparing, pinning a worker and a
    // first callback must not panic, and later calls do nothing.
    prepare_fast_cores();
    std::thread::spawn(|| {
        pin_worker_to_fast_cores();
        let mut once = PinOnce::default();
        once.run();
        once.run();
        assert!(once.tried);
    })
    .join()
    .unwrap();
}

#[test]
fn a_cpu_list_parses_ranges_singles_and_spaces() {
    assert_eq!(
        parse_cpu_list("0-3,8,10-11\n"),
        Some(vec![0, 1, 2, 3, 8, 10, 11])
    );
    assert_eq!(parse_cpu_list(" 4 , 2-3 "), Some(vec![2, 3, 4]));
    assert_eq!(
        parse_cpu_list("1,1,0-1"),
        Some(vec![0, 1]),
        "sorted, no repeats"
    );
    assert_eq!(parse_cpu_list(""), Some(vec![]));
    assert_eq!(parse_cpu_list("\n"), Some(vec![]));
}

#[test]
fn a_malformed_cpu_list_is_refused() {
    for text in [
        "0-",
        "-3",
        "3-1",
        "a",
        "0,,1",
        "0-3-5",
        "1.5",
        "0x2",
        "0-1023,1024",
        "99999999999999999999",
        "\u{0}",
    ] {
        assert_eq!(parse_cpu_list(text), None, "{text:?}");
    }
    assert_eq!(
        parse_cpu_list(&format!("0-{}", MAX_CPUS - 1)).map(|cpus| cpus.len()),
        Some(MAX_CPUS)
    );
}

/// A fake `/sys/devices` under the temp folder, removed when dropped.
struct Sysfs(PathBuf);

impl Sysfs {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("rigwerk-sysfs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, path: &str, text: &str) -> &Self {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        self
    }

    fn capacities(&self, capacities: &[&str]) -> &Self {
        for (cpu, capacity) in capacities.iter().enumerate() {
            self.write(&format!("system/cpu/cpu{cpu}/cpu_capacity"), capacity);
        }
        self
    }

    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for Sysfs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_intel_hybrid_pins_to_its_p_cores() {
    // i7-12700H: 6 P-cores with SMT (0-11), 8 E-cores (12-19).
    let sys = Sysfs::new("intel");
    sys.write("system/cpu/online", "0-19\n")
        .write("cpu_core/cpus", "0-11\n")
        .write("cpu_atom/cpus", "12-19\n");
    assert_eq!(fast_cpus_in(sys.root()), Some((0..12).collect()));
}

#[test]
fn offline_p_cores_are_left_out() {
    let sys = Sysfs::new("offline");
    sys.write("system/cpu/online", "0-3,12-19")
        .write("cpu_core/cpus", "0-11")
        .write("cpu_atom/cpus", "12-19");
    assert_eq!(fast_cpus_in(sys.root()), Some(vec![0, 1, 2, 3]));
}

#[test]
fn big_little_pins_to_the_highest_capacity() {
    // 4 little cores at 446, 4 big at 1024 (RK3588-like).
    let sys = Sysfs::new("biglittle");
    sys.write("system/cpu/online", "0-7")
        .capacities(&["446", "446", "446", "446", "1024\n", "1024", "1024", "1024"]);
    assert_eq!(fast_cpus_in(sys.root()), Some(vec![4, 5, 6, 7]));
}

#[test]
fn equal_cores_are_not_pinned() {
    // A Pi 4 or 5: four identical cores.
    let sys = Sysfs::new("equal");
    sys.write("system/cpu/online", "0-3")
        .capacities(&["1024", "1024", "1024", "1024"]);
    assert_eq!(fast_cpus_in(sys.root()), None);
    // A desktop with nothing but the online list.
    let plain = Sysfs::new("plain");
    plain.write("system/cpu/online", "0-15");
    assert_eq!(fast_cpus_in(plain.root()), None);
}

#[test]
fn messy_or_missing_sysfs_pins_nothing() {
    // No sysfs at all.
    let empty = Sysfs::new("empty");
    assert_eq!(fast_cpus_in(empty.root()), None);
    assert_eq!(fast_cpus_in(Path::new("/nonexistent/rigwerk/sysfs")), None);

    // One CPU, or an online list that is not a list.
    for online in ["0", "", "garbage", "3-1", "0-4096"] {
        let sys = Sysfs::new("online");
        sys.write("system/cpu/online", online)
            .write("cpu_core/cpus", "0-1")
            .write("cpu_atom/cpus", "2-3");
        assert_eq!(fast_cpus_in(sys.root()), None, "online {online:?}");
    }

    // A capacity missing for one CPU, unreadable, negative, or all zero.
    for capacities in [
        &["1024", "512", "512"][..],
        &["1024", "", "512", "512"][..],
        &["1024", "fast", "512", "512"][..],
        &["1024", "-1", "512", "512"][..],
        &["0", "0", "0", "0"][..],
    ] {
        let sys = Sysfs::new("capacity");
        sys.write("system/cpu/online", "0-3").capacities(capacities);
        assert_eq!(fast_cpus_in(sys.root()), None, "capacities {capacities:?}");
    }

    // A capacity file that is a directory.
    let dir = Sysfs::new("dir");
    dir.write("system/cpu/online", "0-1")
        .write("system/cpu/cpu0/cpu_capacity/x", "1")
        .write("system/cpu/cpu1/cpu_capacity", "512");
    assert_eq!(fast_cpus_in(dir.root()), None);
}

#[test]
fn a_broken_intel_list_falls_back_to_capacities_or_nothing() {
    // `cpu_core` without `cpu_atom` (a PMU of that name on a non-hybrid CPU):
    // not trusted, and without capacities nothing is pinned.
    let alone = Sysfs::new("alone");
    alone
        .write("system/cpu/online", "0-7")
        .write("cpu_core/cpus", "0-7");
    assert_eq!(fast_cpus_in(alone.root()), None);

    // Garbage in the P-core list: the capacities still decide.
    let garbage = Sysfs::new("garbage");
    garbage
        .write("system/cpu/online", "0-3")
        .write("cpu_core/cpus", "zero-eleven")
        .write("cpu_atom/cpus", "2-3")
        .capacities(&["1024", "1024", "600", "600"]);
    assert_eq!(fast_cpus_in(garbage.root()), Some(vec![0, 1]));

    // A P-core list that names every online CPU, or none of them.
    for core in ["0-3", "8-11", ""] {
        let sys = Sysfs::new("core");
        sys.write("system/cpu/online", "0-3")
            .write("cpu_core/cpus", core)
            .write("cpu_atom/cpus", "2-3");
        let expected = (core == "0-3").then(|| vec![0, 1]);
        assert_eq!(fast_cpus_in(sys.root()), expected, "cpu_core {core:?}");
    }
}

#[test]
fn an_oversized_sysfs_file_is_not_read() {
    let sys = Sysfs::new("huge");
    let huge = "0,".repeat(SYSFS_READ_LIMIT as usize);
    sys.write("system/cpu/online", &huge);
    assert_eq!(read_small(&sys.root().join("system/cpu/online")), None);
    assert_eq!(fast_cpus_in(sys.root()), None);
    // Exactly at the limit is still read.
    let edge = Sysfs::new("edge");
    let text = format!("{:<width$}", "0-1", width = SYSFS_READ_LIMIT as usize);
    edge.write("system/cpu/online", &text);
    assert_eq!(
        read_small(&edge.root().join("system/cpu/online")),
        Some(text)
    );
}

#[test]
fn a_restricted_process_keeps_only_its_fast_cores() {
    let fast: Vec<usize> = (0..12).collect();
    // `taskset -c 0-3,12-15`: P-cores 0-3 of those.
    let allowed: Vec<usize> = (0..4).chain(12..16).collect();
    assert_eq!(narrowing(&fast, &allowed), Some(vec![0, 1, 2, 3]));
    // Already only P-cores: nothing to narrow.
    assert_eq!(narrowing(&fast, &[0, 1, 2]), None);
    // Only E-cores allowed: the user's choice stands.
    assert_eq!(narrowing(&fast, &[12, 13]), None);
}
