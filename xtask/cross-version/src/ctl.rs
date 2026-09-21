//! The control channel between a run's two halves.
//!
//! The inner half runs inside the namespace and cannot see the host's `/tmp`
//! or `/run/user/<uid>` — that is the point of the namespace. But the
//! pre-connect assertion has a host-side half too: the production endpoints
//! must be unchanged, and the host's flat endpoints must still be absent. So
//! before every client it starts, the inner half asks the outer half, and waits
//! for the answer.
//!
//! The wire is the inner half's own stdio, line by line: a request is a stdout
//! line starting with [`PREFIX`], and the reply is one line on its stdin, `ok`
//! or `fail <reason>`. Every other stdout line is progress output, which the
//! outer half echoes. A closed channel is a refusal, never a pass.

use std::io::{BufRead, Write};

pub const PREFIX: &str = "@@XVER-CTL ";

/// The inner half's end.
pub struct Client {
    reader: std::io::StdinLock<'static>,
}

impl Client {
    pub fn new() -> Self {
        Self {
            reader: std::io::stdin().lock(),
        }
    }

    /// Send one request and wait for its verdict.
    pub fn request(&mut self, req: &str) -> Result<(), String> {
        {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{PREFIX}{req}").map_err(|e| format!("control channel write: {e}"))?;
            out.flush()
                .map_err(|e| format!("control channel flush: {e}"))?;
        }
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .map_err(|e| format!("control channel read: {e}"))?;
        if n == 0 {
            return Err("the outer half closed the control channel".to_string());
        }
        parse_reply(line.trim_end())
    }
}

/// `ok` → `Ok`, `fail <reason>` → `Err(reason)`, anything else → `Err`.
pub fn parse_reply(line: &str) -> Result<(), String> {
    if line == "ok" {
        return Ok(());
    }
    if let Some(reason) = line.strip_prefix("fail ") {
        return Err(reason.to_string());
    }
    Err(format!("unrecognised control reply {line:?}"))
}

/// Replies must stay one line, whatever a reason contains.
pub fn fail_reply(reason: &str) -> String {
    format!("fail {}", reason.replace(['\n', '\r'], " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_literal_ok_is_a_pass() {
        assert_eq!(parse_reply("ok"), Ok(()));
        assert_eq!(parse_reply("fail host changed"), Err("host changed".into()));
        assert!(parse_reply("okay").is_err());
        assert!(parse_reply("").is_err());
    }

    #[test]
    fn a_multi_line_reason_becomes_one_reply_line() {
        let r = fail_reply("a\nb\r\nc");
        assert!(!r.contains('\n') && !r.contains('\r'));
        assert_eq!(parse_reply(&r), Err("a b  c".into()));
    }
}
