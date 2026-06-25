use crate::BackendError;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::{fs, path::Path};

const ZHIPU_CHAT_COMPLETIONS_URL: &str = "https://open.bigmodel.cn/api/paas/v4/chat/completions";
const ZHIPU_MODEL: &str = "glm-4.6v-flash";
const DEEPSEEK_CHAT_COMPLETIONS_URL: &str = "https://api.deepseek.com/chat/completions";
pub const DEEPSEEK_MODEL_FLASH: &str = "deepseek-v4-flash";
const CAPTION_INSTRUCTION: &str = "Describe this image concisely for an AI scriptwriter who may need to reference it later. Focus on visible subject, pose, clothing, expression, framing, and notable visual details. Return plain text only.";

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: ChatContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    part_type: String,
    #[serde(default)]
    text: Option<String>,
}

pub async fn caption_asset(image_path: &Path, api_key: &str) -> Result<String, BackendError> {
    ensure_api_key(api_key, "智谱")?;
    let image_bytes = fs::read(image_path)?;
    let image_mime = mime_type_for_path(image_path);
    let image_data_uri = format!(
        "data:{image_mime};base64,{}",
        STANDARD.encode(image_bytes)
    );

    let client = Client::new();
    let response = client
        .post(ZHIPU_CHAT_COMPLETIONS_URL)
        .bearer_auth(api_key)
        .json(&json!({
            "model": ZHIPU_MODEL,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": CAPTION_INSTRUCTION
                        },
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": image_data_uri
                            }
                        }
                    ]
                }
            ]
        }))
        .send()
        .await
        .map_err(|error| BackendError::message(format!("调用智谱接口失败: {error}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| BackendError::message(format!("读取智谱响应失败: {error}")))?;
    if !status.is_success() {
        return Err(BackendError::message(format!("智谱接口返回错误({status}): {body}")));
    }
    let payload: ChatCompletionResponse = serde_json::from_str(&body)
        .map_err(|error| BackendError::message(format!("解析智谱响应失败: {error}; body={body}")))?;

    extract_message_text(payload, "智谱")
}

pub async fn generate_script(prompt: &str, api_key: &str, model: &str) -> Result<String, BackendError> {
    ensure_api_key(api_key, "DeepSeek")?;

    let client = Client::new();
    let response = client
        .post(DEEPSEEK_CHAT_COMPLETIONS_URL)
        .bearer_auth(api_key)
        .json(&json!({
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "stream": false
        }))
        .send()
        .await
        .map_err(|error| BackendError::message(format!("调用 DeepSeek 接口失败: {error}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| BackendError::message(format!("读取 DeepSeek 响应失败: {error}")))?;
    if !status.is_success() {
        return Err(BackendError::message(format!("DeepSeek 接口返回错误({status}): {body}")));
    }
    let payload: ChatCompletionResponse = serde_json::from_str(&body)
        .map_err(|error| BackendError::message(format!("解析 DeepSeek 响应失败: {error}; body={body}")))?;

    extract_message_text(payload, "DeepSeek")
}

fn ensure_api_key(api_key: &str, provider: &str) -> Result<(), BackendError> {
    if api_key.trim().is_empty() {
        return Err(BackendError::message(format!("{provider} API Key 未配置")));
    }
    Ok(())
}

fn mime_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    }
}

#[cfg(test)]
mod smoke_test {
    use super::*;

    // Temporary manual smoke test, gated on env vars so no key is hardcoded in source.
    // Run with: VVN_SMOKE_ZHIPU_KEY=... VVN_SMOKE_DEEPSEEK_KEY=... VVN_SMOKE_IMAGE=... cargo test smoke_test -- --nocapture --ignored
    #[tokio::test]
    #[ignore]
    async fn real_provider_smoke_test() {
        let zhipu_key = std::env::var("VVN_SMOKE_ZHIPU_KEY").expect("set VVN_SMOKE_ZHIPU_KEY");
        let deepseek_key = std::env::var("VVN_SMOKE_DEEPSEEK_KEY").expect("set VVN_SMOKE_DEEPSEEK_KEY");
        let image_path = std::env::var("VVN_SMOKE_IMAGE").expect("set VVN_SMOKE_IMAGE");

        let caption = caption_asset(Path::new(&image_path), &zhipu_key)
            .await
            .expect("caption_asset failed");
        println!("=== caption_asset result ===\n{caption}\n");
        assert!(!caption.trim().is_empty());

        let script = generate_script("用一句话打招呼", &deepseek_key, DEEPSEEK_MODEL_FLASH)
            .await
            .expect("generate_script failed");
        println!("=== generate_script result ===\n{script}\n");
        assert!(!script.trim().is_empty());
    }
}

fn extract_message_text(payload: ChatCompletionResponse, provider: &str) -> Result<String, BackendError> {
    let choice = payload
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| BackendError::message(format!("{provider} 响应缺少 choices")))?;

    let text = match choice.message.content {
        ChatContent::Text(text) => text,
        ChatContent::Parts(parts) => parts
            .into_iter()
            .filter(|part| part.part_type == "text")
            .filter_map(|part| part.text)
            .collect::<Vec<_>>()
            .join("\n"),
    };

    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(BackendError::message(format!("{provider} 返回了空文本")));
    }
    Ok(text)
}
