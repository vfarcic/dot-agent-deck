use crate::config::IdleArtConfig;
use crate::llm::{LlmError, LlmRequest};

const SYSTEM_PROMPT: &str = include_str!("../assets/idle-art-prompt.md");
const FRAME_DELIMITER: &str = "---FRAME---";
const MAX_LINES_PER_FRAME: usize = 8;
const MAX_CHARS_PER_LINE: usize = 38;
const MAX_RETRIES: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum AsciiArtError {
    #[error("LLM call failed: {0}")]
    Llm(#[from] LlmError),
    #[error("Validation failed after {MAX_RETRIES} attempts: {reason}")]
    ValidationExhausted { reason: String },
}

#[derive(Debug, Clone)]
pub struct AsciiArtResult {
    pub frames: Vec<String>,
}

pub fn parse_frames(raw: &str) -> Vec<String> {
    raw.split(FRAME_DELIMITER)
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

pub fn validate_frame(frame: &str) -> Result<(), String> {
    let lines: Vec<&str> = frame.lines().collect();
    if lines.len() > MAX_LINES_PER_FRAME {
        return Err(format!(
            "frame has {} lines, max is {MAX_LINES_PER_FRAME}",
            lines.len()
        ));
    }
    for (i, line) in lines.iter().enumerate() {
        if line.len() > MAX_CHARS_PER_LINE {
            return Err(format!(
                "line {} has {} chars, max is {MAX_CHARS_PER_LINE}",
                i + 1,
                line.len()
            ));
        }
    }
    Ok(())
}

fn validate_all_frames(frames: &[String]) -> Result<(), String> {
    if frames.is_empty() {
        return Err("no frames in output".to_string());
    }
    for (i, frame) in frames.iter().enumerate() {
        validate_frame(frame).map_err(|e| format!("frame {}: {e}", i + 1))?;
    }
    Ok(())
}

fn build_user_message(input: &str, output: &str) -> String {
    format!("{input} / {output}")
}

pub async fn generate_ascii_art(
    input: &str,
    output: &str,
    config: &IdleArtConfig,
) -> Result<AsciiArtResult, AsciiArtError> {
    let user_message = build_user_message(input, output);
    let request = LlmRequest {
        system_prompt: SYSTEM_PROMPT.to_string(),
        user_message,
        provider: config.provider.clone(),
        model: config.model.clone(),
    };

    let mut last_reason = String::new();
    for _ in 0..MAX_RETRIES {
        let raw = crate::llm::call_llm(&request).await?;
        let frames = parse_frames(&raw);
        match validate_all_frames(&frames) {
            Ok(()) => return Ok(AsciiArtResult { frames }),
            Err(reason) => {
                last_reason = reason;
                continue;
            }
        }
    }
    Err(AsciiArtError::ValidationExhausted {
        reason: last_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_embedded() {
        assert!(
            !SYSTEM_PROMPT.is_empty(),
            "embedded system prompt should not be empty"
        );
        assert!(
            SYSTEM_PROMPT.contains("ASCII"),
            "system prompt should mention ASCII"
        );
    }

    #[test]
    fn parse_single_frame() {
        let raw = "  hello\n  world  ";
        let frames = parse_frames(raw);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], "hello\n  world");
    }

    #[test]
    fn parse_multi_frames() {
        let raw =
            "frame1 line1\nframe1 line2\n---FRAME---\nframe2 line1\n---FRAME---\nframe3 line1";
        let frames = parse_frames(raw);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], "frame1 line1\nframe1 line2");
        assert_eq!(frames[1], "frame2 line1");
        assert_eq!(frames[2], "frame3 line1");
    }

    #[test]
    fn parse_trailing_delimiter() {
        let raw = "frame1\n---FRAME---\n";
        let frames = parse_frames(raw);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn parse_empty_input() {
        let frames = parse_frames("");
        assert!(frames.is_empty());
    }

    #[test]
    fn validate_valid_frame() {
        let frame = "12345678901234567890123456789012345678\nline2\nline3\nline4\nline5\nline6\nline7\nline8";
        assert!(validate_frame(frame).is_ok());
    }

    #[test]
    fn validate_too_many_lines() {
        let frame = "1\n2\n3\n4\n5\n6\n7\n8\n9";
        let err = validate_frame(frame).unwrap_err();
        assert!(err.contains("9 lines"));
    }

    #[test]
    fn validate_line_too_long() {
        let frame = "123456789012345678901234567890123456789"; // 39 chars
        let err = validate_frame(frame).unwrap_err();
        assert!(err.contains("39 chars"));
    }

    #[test]
    fn validate_empty_frame() {
        // Empty string after trim results in empty lines vec — valid (0 lines <= 8)
        assert!(validate_frame("").is_ok());
    }

    #[test]
    fn validate_all_frames_empty_vec() {
        let err = validate_all_frames(&[]).unwrap_err();
        assert!(err.contains("no frames"));
    }

    #[test]
    fn validate_all_frames_mixed() {
        let frames = vec![
            "valid frame".to_string(),
            "123456789012345678901234567890123456789".to_string(), // 39 chars — invalid
        ];
        let err = validate_all_frames(&frames).unwrap_err();
        assert!(err.contains("frame 2"));
    }

    #[test]
    fn build_user_message_format() {
        let msg = build_user_message("Fix auth tokens", "Fixed TTL, 47 tests pass");
        assert_eq!(msg, "Fix auth tokens / Fixed TTL, 47 tests pass");
    }
}
