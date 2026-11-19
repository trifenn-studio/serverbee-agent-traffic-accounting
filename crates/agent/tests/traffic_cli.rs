//! Exercise the shipped offline command entrypoint, not an alternate parser.
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_serverbee-agent"))
        .env_clear()
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn offline_version_needs_no_server_or_identity() {
    let output = run(&["traffic-version"]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    let legacy = run(&[serverbee_common::constants::UPGRADE_PROBE_ARG]);
    assert!(legacy.status.success());
    assert_eq!(
        value["agent_version"],
        String::from_utf8_lossy(&legacy.stdout).trim()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn offline_check_seed_status_preserve_state_and_existing_agent_settings() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("agent.toml");
    let state_dir = temp.path().join("traffic");
    let text = format!(
        r#"
# Diagnostics work with no Server URL or credentials.
[capabilities]
allow = ["docker", "file", "terminal"]
[file]
enabled = true
root_paths = ["/fixture"]
[collector.traffic]
enabled = true
include = ["lo"]
state_dir = {:?}
report = "billing"
[collector.traffic.weights.lo]
rx_to_in_ppm = 500000
tx_to_out_ppm = 2000000
"#,
        state_dir.to_str().unwrap()
    );
    std::fs::write(&config, &text).unwrap();
    let path = config.to_str().unwrap();
    let check = run(&["traffic-check", "--config", path]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    let nic = value["interfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "lo")
        .unwrap();
    assert_eq!(nic["selected"], true);
    assert_eq!(nic["weights"]["rx_to_in_ppm"], 500000);
    let overridden = Command::new(env!("CARGO_BIN_EXE_serverbee-agent"))
        .env_clear()
        .env("SERVERBEE_COLLECTOR__TRAFFIC__INCLUDE", "[\"lo\"]")
        .env(
            "SERVERBEE_COLLECTOR__TRAFFIC__WEIGHTS__LO__TX_TO_OUT_PPM",
            "3000000",
        )
        .args(["traffic-check", "--config", path])
        .output()
        .unwrap();
    assert!(
        overridden.status.success(),
        "{}",
        String::from_utf8_lossy(&overridden.stderr)
    );
    let overridden: serde_json::Value = serde_json::from_slice(&overridden.stdout).unwrap();
    assert_eq!(
        overridden["interfaces"][0]["weights"]["tx_to_out_ppm"],
        3000000
    );
    assert!(!state_dir.exists(), "check creates no state/lock directory");
    assert!(!run(&["traffic-status", "--config", path]).status.success());
    let seed_args = [
        "traffic-seed",
        "--config",
        path,
        "--raw-in",
        "111",
        "--raw-out",
        "222",
        "--billing-in",
        "333",
        "--billing-out",
        "444",
    ];
    let seed = run(&seed_args);
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let ledger = state_dir.join("traffic-state.json");
    let before = std::fs::read(&ledger).unwrap();
    let status = run(&["traffic-status", "--config", path]);
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["state"]["totals"]["billing_out"], 444);
    assert_eq!(status["state"]["totals"]["raw_in"], 111);
    assert_eq!(std::fs::read(&ledger).unwrap(), before);
    assert!(!run(&seed_args).status.success(), "seed cannot overwrite");
    assert_eq!(std::fs::read(&ledger).unwrap(), before);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert_eq!(
        std::fs::read_dir(temp.path()).unwrap().count(),
        2,
        "no enrollment identity or upgrade state"
    );
    let missing = text.replace(
        "include = [\"lo\"]",
        "include = [\"serverbee-missing-fixture\"]",
    );
    std::fs::write(&config, missing).unwrap();
    let check = run(&["traffic-check", "--config", path]);
    assert!(!check.status.success());
    let check: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(check["missing"][0], "serverbee-missing-fixture");
    assert_eq!(std::fs::read(&ledger).unwrap(), before);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn daemon_checkpoints_offline_and_sends_native_report_after_connecting() {
    use std::process::Stdio;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serverbee_common::constants::PROTOCOL_VERSION;
    use serverbee_common::protocol::{AgentMessage, ServerMessage};
    use tokio::net::{TcpListener, UdpSocket};
    use tokio_tungstenite::tungstenite::Message;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("traffic");
    let config = temp.path().join("agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
server_url = "http://{}"
token = "local-test-token"
[ip_change]
enabled = false
[security]
enabled = false
[capabilities]
state_dir = {:?}
deny = ["ip_quality", "security_events"]
[collector.traffic]
enabled = true
include = ["lo"]
state_dir = {:?}
report = "billing"
flush_interval_secs = 5
[collector.traffic.weights.lo]
tx_to_out_ppm = 2000000
"#,
            listener.local_addr().unwrap(),
            temp.path().join("capabilities"),
            state_dir
        ),
    )
    .unwrap();
    let seed = run(&[
        "traffic-seed",
        "--config",
        config.to_str().unwrap(),
        "--raw-in",
        "0",
        "--raw-out",
        "0",
        "--billing-in",
        "0",
        "--billing-out",
        "0",
    ]);
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let seed: serde_json::Value = serde_json::from_slice(&seed.stdout).unwrap();
    let seeded_tx = seed["state"]["previous"]["lo"]["tx"].as_i64().unwrap();

    let mut agent = tokio::process::Command::new(env!("CARGO_BIN_EXE_serverbee-agent"))
        .env_clear()
        .current_dir(temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(25), async {
        // Accept TCP after startup, then hold the WebSocket handshake until
        // the process advances beyond the startup checkpoint.
        let (stream, _) = listener.accept().await.unwrap();
        let checkpoint = || {
            let data = std::fs::read(state_dir.join("traffic-state.json")).unwrap();
            serde_json::from_slice::<serde_json::Value>(&data).unwrap()
        };
        let startup_raw_out = checkpoint()["state"]["totals"]["raw_out"].as_i64().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let offline = loop {
            tick.tick().await;
            assert!(
                agent.try_wait().unwrap().is_none(),
                "Agent exited before reporting"
            );
            socket.send_to(&[0u8; 256], addr).await.unwrap();
            let state = &checkpoint()["state"];
            let totals = &state["totals"];
            let raw_out = totals["raw_out"].as_i64().unwrap();
            if raw_out > startup_raw_out {
                assert_eq!(
                    raw_out,
                    state["previous"]["lo"]["tx"].as_i64().unwrap() - seeded_tx
                );
                let billing_out = totals["billing_out"].as_i64().unwrap();
                assert_eq!(billing_out, 2 * raw_out);
                break billing_out;
            }
        };

        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let welcome = ServerMessage::Welcome {
            capability_event_ack: false,
            server_id: "traffic-fixture".into(),
            protocol_version: PROTOCOL_VERSION,
            report_interval: 1,
            capabilities: None,
        };
        ws.send(Message::Text(
            serde_json::to_string(&welcome).unwrap().into(),
        ))
        .await
        .unwrap();
        loop {
            tokio::select! {
                _ = tick.tick() => { socket.send_to(&[0u8; 256], addr).await.unwrap(); }
                frame = ws.next() => {
                    let Message::Text(text) = frame.unwrap().unwrap() else { continue };
                    if let AgentMessage::Report(report) = serde_json::from_str(&text).unwrap()
                        && report.net_out_speed > 0
                    {
                        assert!(report.net_out_transfer >= offline);
                        assert_eq!(report.net_out_transfer % 2, 0);
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("the Agent must checkpoint before Welcome and then report live speeds");
    agent.kill().await.unwrap();
}

#[test]
fn disabled_defaults_and_environment_override_never_touch_state() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("agent.toml");
    let state_dir = temp.path().join("never-created");
    std::fs::write(&config, "[collector]\nenable_temperature = false\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_serverbee-agent"))
        .env_clear()
        .env("SERVERBEE_COLLECTOR__TRAFFIC__STATE_DIR", &state_dir)
        .args(["traffic-seed", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("collector.traffic.enabled=true"));
    assert!(!state_dir.exists());
}
