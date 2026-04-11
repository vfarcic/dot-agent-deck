use std::time::Duration;

use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error ({status}): {body}")]
    ApiError { status: u16, body: String },
    #[error("Missing API key: set {env_var} environment variable")]
    MissingApiKey { env_var: String },
    #[error("Unexpected response format")]
    UnexpectedFormat,
    #[error("Unsupported provider: {0}")]
    UnsupportedProvider(String),
}

pub struct LlmRequest {
    pub system_prompt: String,
    pub user_message: String,
    pub provider: String,
    pub model: String,
}

pub async fn call_llm(request: &LlmRequest) -> Result<String, LlmError> {
    match request.provider.as_str() {
        "anthropic" => call_anthropic(request).await,
        "openai" => call_openai(request).await,
        "ollama" => call_ollama(request).await,
        other => Err(LlmError::UnsupportedProvider(other.to_string())),
    }
}

async fn call_anthropic(request: &LlmRequest) -> Result<String, LlmError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| LlmError::MissingApiKey {
        env_var: "ANTHROPIC_API_KEY".to_string(),
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let body = json!({
        "model": request.model,
        "max_tokens": 1024,
        "system": [{"type": "text", "text": request.system_prompt}],
        "messages": [{"role": "user", "content": request.user_message}]
    });

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(LlmError::ApiError { status, body });
    }

    let json: serde_json::Value = resp.json().await?;
    json["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or(LlmError::UnexpectedFormat)
}

async fn call_openai(request: &LlmRequest) -> Result<String, LlmError> {
    let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| LlmError::MissingApiKey {
        env_var: "OPENAI_API_KEY".to_string(),
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let body = json!({
        "model": request.model,
        "max_tokens": 1024,
        "messages": [
            {"role": "system", "content": request.system_prompt},
            {"role": "user", "content": request.user_message}
        ]
    });

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(LlmError::ApiError { status, body });
    }

    let json: serde_json::Value = resp.json().await?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or(LlmError::UnexpectedFormat)
}

async fn call_ollama(request: &LlmRequest) -> Result<String, LlmError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    let prompt = format!("{}\n\n{}", request.system_prompt, request.user_message);
    let body = json!({
        "model": request.model,
        "prompt": prompt,
        "stream": false
    });

    let resp = client
        .post("http://localhost:11434/api/generate")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(LlmError::ApiError { status, body });
    }

    let json: serde_json::Value = resp.json().await?;
    json["response"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or(LlmError::UnexpectedFormat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_anthropic_api_key() {
        // SAFETY: test is single-threaded; no other code reads this var concurrently.
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        let request = LlmRequest {
            system_prompt: "test".to_string(),
            user_message: "test".to_string(),
            provider: "anthropic".to_string(),
            model: "test".to_string(),
        };
        let result = call_llm(&request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("ANTHROPIC_API_KEY"),
            "error should mention ANTHROPIC_API_KEY: {err}"
        );

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("ANTHROPIC_API_KEY", v);
            }
        }
    }

    #[tokio::test]
    async fn missing_openai_api_key() {
        // SAFETY: test is single-threaded; no other code reads this var concurrently.
        let prev = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }

        let request = LlmRequest {
            system_prompt: "test".to_string(),
            user_message: "test".to_string(),
            provider: "openai".to_string(),
            model: "test".to_string(),
        };
        let result = call_llm(&request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("OPENAI_API_KEY"),
            "error should mention OPENAI_API_KEY: {err}"
        );

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("OPENAI_API_KEY", v);
            }
        }
    }

    #[tokio::test]
    async fn unsupported_provider() {
        let request = LlmRequest {
            system_prompt: "test".to_string(),
            user_message: "test".to_string(),
            provider: "unknown".to_string(),
            model: "test".to_string(),
        };
        let result = call_llm(&request).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown"));
    }
}
