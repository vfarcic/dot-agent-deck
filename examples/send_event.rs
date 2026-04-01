use std::io::Write;
use std::os::unix::net::UnixStream;

use dot_agent_deck::config::socket_path;

fn send(stream: &mut UnixStream, json: &str) {
    writeln!(stream, "{json}").unwrap();
    println!("Sent: {json}");
    std::thread::sleep(std::time::Duration::from_secs(2));
}

fn main() {
    let path = socket_path();
    println!("Connecting to {}...", path.display());

    let mut stream =
        UnixStream::connect(&path).expect("Failed to connect — is the daemon running?");

    let now = chrono::Utc::now();
    let ts = |offset_secs: i64| -> String {
        (now + chrono::Duration::seconds(offset_secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    };

    // Session 1: start
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-1","agent_type":"claude_code","event_type":"session_start","timestamp":"{}","cwd":"/home/user/project"}}"#,
            ts(0)
        ),
    );

    // Session 2: start (OpenCode session)
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-2","agent_type":"open_code","event_type":"session_start","timestamp":"{}","cwd":"/home/user/other-project"}}"#,
            ts(1)
        ),
    );

    // Session 1: tool start
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-1","agent_type":"claude_code","event_type":"tool_start","tool_name":"Read","tool_detail":"src/main.rs","timestamp":"{}"}}"#,
            ts(2)
        ),
    );

    // Session 2 (OpenCode): waiting for input
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-2","agent_type":"open_code","event_type":"waiting_for_input","timestamp":"{}"}}"#,
            ts(3)
        ),
    );

    // Session 1: tool end
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-1","agent_type":"claude_code","event_type":"tool_end","timestamp":"{}"}}"#,
            ts(4)
        ),
    );

    // Session 1: error
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-1","agent_type":"claude_code","event_type":"error","timestamp":"{}"}}"#,
            ts(5)
        ),
    );

    // Session 2 (OpenCode): end
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-2","agent_type":"open_code","event_type":"session_end","timestamp":"{}"}}"#,
            ts(6)
        ),
    );

    // Session 1: end
    send(
        &mut stream,
        &format!(
            r#"{{"session_id":"demo-1","agent_type":"claude_code","event_type":"session_end","timestamp":"{}"}}"#,
            ts(7)
        ),
    );

    println!("Done! Sessions have ended.");
}
