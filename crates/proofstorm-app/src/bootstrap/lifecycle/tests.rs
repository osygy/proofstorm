use super::*;

fn journal(installation: &Installation, phase: Phase) -> Journal {
    Journal {
        format_version: 1,
        installation_id: installation.id.clone(),
        phase,
        controller: None,
        workloads: None,
        workloads_stopped: false,
        tools_running: true,
        addresses: BTreeMap::new(),
    }
}

/// Runs `pin` against a fake Docker whose container has `endpoint` on network `net`.
fn pin_transcript(endpoint: &Value, refuse_fixed: bool) -> (bool, Vec<String>) {
    let calls = std::cell::RefCell::new(Vec::new());
    let run = |args: &[&str]| -> Result<String> {
        calls.borrow_mut().push(args.join(" "));
        match args {
            ["inspect", ..] => Ok(json!({ "net": endpoint }).to_string()),
            ["network", "connect", "--ip", ..] if refuse_fixed => anyhow::bail!(
                "user specified IP address is supported only when connecting to networks with user configured subnets"
            ),
            ["network", ..] => Ok(String::new()),
            _ => unreachable!("unexpected docker call {args:?}"),
        }
    };
    let pinned = pin(&run, "id", "net", Ipv4Addr::new(172, 18, 0, 2)).unwrap();
    (pinned, calls.into_inner()[1..].to_vec())
}

#[test]
fn stopped_containers_are_pinned_to_their_recorded_address() {
    let (pinned, calls) = pin_transcript(&json!({"IPAMConfig": null, "IPAddress": ""}), false);
    assert!(pinned);
    assert_eq!(
        calls,
        [
            "network disconnect net id",
            "network connect --ip 172.18.0.2 net id"
        ]
    );
    // A container pinned elsewhere moves to the recorded address.
    let (pinned, calls) =
        pin_transcript(&json!({"IPAMConfig": {"IPv4Address": "172.18.0.9"}}), false);
    assert!(pinned && calls.len() == 2);
    let (pinned, calls) =
        pin_transcript(&json!({"IPAMConfig": {"IPv4Address": "172.18.0.2"}}), false);
    assert!(pinned && calls.is_empty());
    // A stop that ended between disconnect and connect is finished, not repeated.
    let (pinned, calls) = pin_transcript(&Value::Null, false);
    assert!(pinned);
    assert_eq!(calls, ["network connect --ip 172.18.0.2 net id"]);
    // Docker refusing a fixed address must never leave the container off the network.
    let (pinned, calls) = pin_transcript(&json!({"IPAMConfig": null}), true);
    assert!(!pinned);
    assert_eq!(calls.last().unwrap(), "network connect net id");
}

#[test]
fn addresses_are_read_from_the_runtime_network_only() {
    let docker = |networks: Value| move |_: &[&str]| -> Result<String> { Ok(networks.to_string()) };
    let running =
        json!({"net": {"IPAddress": "172.18.0.3"}, "bridge": {"IPAddress": "172.17.0.2"}});
    assert_eq!(
        address(&docker(running), "id", "net").unwrap(),
        Some(Ipv4Addr::new(172, 18, 0, 3))
    );
    assert_eq!(
        address(&docker(json!({"net": {"IPAddress": ""}})), "id", "net").unwrap(),
        None
    );
    assert_eq!(
        address(
            &docker(json!({"bridge": {"IPAddress": "172.17.0.2"}})),
            "id",
            "net"
        )
        .unwrap(),
        None
    );
    assert!(
        address(
            &docker(json!({"net": {"IPAddress": "invalid IP"}})),
            "id",
            "net"
        )
        .is_err()
    );
}

#[test]
fn journals_written_before_address_pinning_still_load() {
    let journal: Journal = serde_json::from_value(json!({"format_version": 1, "installation_id": "a",
        "phase": "stopped", "controller": null, "workloads": null, "workloads_stopped": true, "tools_running": false}))
    .unwrap();
    assert!(journal.addresses.is_empty());
}

#[tokio::test]
async fn shutdown_fences_new_calls_and_waits_for_existing_leases() {
    let root = tempfile::tempdir().unwrap();
    let installation = Installation::initialize(root.path(), None, None).unwrap();
    let first = access(Some(&installation)).unwrap();
    let second = access(Some(&installation)).unwrap();
    save(&installation.home, &journal(&installation, Phase::Stopping)).unwrap();
    assert!(
        access(Some(&installation))
            .unwrap_err()
            .to_string()
            .contains("stopping")
    );
    assert!(exclusive(&installation.home, Instant::now()).await.is_err());
    drop(first);
    assert!(exclusive(&installation.home, Instant::now()).await.is_err());
    drop(second);
    let shutdown = exclusive(&installation.home, Instant::now()).await.unwrap();
    fs::remove_file(installation.home.join(JOURNAL)).unwrap();
    assert!(access(Some(&installation)).is_err());
    drop(shutdown);
    assert!(access(Some(&installation)).is_ok());
}

#[test]
fn partial_transitions_remain_fenced_and_never_affect_another_home() {
    let root = tempfile::tempdir().unwrap();
    let first = Installation::initialize(&root.path().join("first"), None, None).unwrap();
    let other = Installation::initialize(&root.path().join("other"), None, None).unwrap();
    for phase in [Phase::Stopping, Phase::Stopped, Phase::Starting] {
        save(&first.home, &journal(&first, phase)).unwrap();
        assert!(access(Some(&first)).is_err());
        assert!(access(Some(&other)).is_ok());
        assert_eq!(read(&first.home).unwrap().unwrap().phase, phase);
    }
    fs::copy(first.home.join(JOURNAL), other.home.join(JOURNAL)).unwrap();
    assert!(read(&other.home).is_err());
    fs::write(first.home.join(JOURNAL), b"interrupted json").unwrap();
    assert!(access(Some(&first)).is_err());
}

#[test]
fn linked_transition_and_lock_files_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let installation = Installation::initialize(root.path(), None, None).unwrap();
    let unrelated = root.path().join("unrelated");
    fs::write(&unrelated, b"untouched").unwrap();
    std::os::unix::fs::symlink(&unrelated, root.path().join(JOURNAL)).unwrap();
    assert!(read(root.path()).is_err());
    fs::remove_file(root.path().join(JOURNAL)).unwrap();
    std::os::unix::fs::symlink(&unrelated, root.path().join("runtime-access.lock")).unwrap();
    assert!(access(Some(&installation)).is_err());
    assert_eq!(fs::read(unrelated).unwrap(), b"untouched");
}
