use std::io::Write;
use std::os::unix::net::UnixStream;

use dot_agent_deck::config::socket_path;

fn send(stream: &mut UnixStream, json: &str) {
    writeln!(stream, "{json}").unwrap();
    println!("Sent: {json}");
    std::thread::sleep(std::time::Duration::from_millis(200));
}

fn main() {
    let path = socket_path();
    println!("Connecting to {}...", path.display());

    let mut stream =
        UnixStream::connect(&path).expect("Failed to connect — is the daemon running?");

    send(
        &mut stream,
        r#"{"session_id":"demo-1","agent_type":"claude_code","event_type":"session_start","timestamp":"2026-03-22T10:00:00Z","cwd":"/home/user/project"}"#,
    );

    send(
        &mut stream,
        r#"{"session_id":"demo-1","agent_type":"claude_code","event_type":"tool_start","tool_name":"Read","tool_detail":"src/main.rs","timestamp":"2026-03-22T10:00:01Z"}"#,
    );

    send(
        &mut stream,
        r#"{"session_id":"demo-1","agent_type":"claude_code","event_type":"tool_end","timestamp":"2026-03-22T10:00:02Z"}"#,
    );

    send(
        &mut stream,
        r#"{"session_id":"demo-1","agent_type":"claude_code","event_type":"waiting_for_input","timestamp":"2026-03-22T10:00:03Z"}"#,
    );

    send(
        &mut stream,
        r#"{"session_id":"demo-1","agent_type":"claude_code","event_type":"session_end","timestamp":"2026-03-22T10:00:04Z"}"#,
    );

    println!("Done! Check daemon logs for event processing output.");
}
