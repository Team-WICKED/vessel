//! `vesseld()` builds and finds the node binary.

#[test]
fn the_node_binary_is_built_and_runs() {
    let path = vessel_testkit::vesseld().unwrap();
    let output = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("vesseld"));
    // Cached: the second call returns at once with the same path.
    assert_eq!(vessel_testkit::vesseld().unwrap(), path);
}
