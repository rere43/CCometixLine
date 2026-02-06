use super::{Segment, SegmentData};
use crate::config::{AnsiColor, InputData, SegmentId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// CLI Proxy API Quota response structures
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AuthFile {
    #[serde(rename = "type")]
    auth_type: String,
    auth_index: String,
    label: Option<String>,
    name: Option<String>,
    disabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AuthFilesResponse {
    files: Vec<AuthFile>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ApiCallResponse {
    body: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct QuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ModelInfo {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "quotaInfo")]
    quota_info: Option<QuotaInfo>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AntigravityModelsResponse {
    models: Option<HashMap<String, ModelInfo>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiBucket {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiQuotaResponse {
    buckets: Option<Vec<GeminiBucket>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CodexRateLimitWindow {
    used_percent: Option<f64>,
    reset_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CodexRateLimit {
    primary_window: Option<CodexRateLimitWindow>,
    secondary_window: Option<CodexRateLimitWindow>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CodexUsageResponse {
    rate_limit: Option<CodexRateLimit>,
    plan_type: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CodexAuthFileContent {
    account_id: Option<String>,
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackedModel {
    Opus,
    Gemini3Pro,
    Gemini3Flash,
    Codex5hr,
}

impl TrackedModel {
    pub fn alias_key(&self) -> &'static str {
        match self {
            Self::Opus => "opus_alias",
            Self::Gemini3Pro => "gemini3pro_alias",
            Self::Gemini3Flash => "gemini3flash_alias",
            Self::Codex5hr => "codex_alias",
        }
    }

    pub fn color_key(&self) -> &'static str {
        match self {
            Self::Opus => "opus_color",
            Self::Gemini3Pro => "gemini3pro_color",
            Self::Gemini3Flash => "gemini3flash_color",
            Self::Codex5hr => "codex_color",
        }
    }

    pub fn default_alias(&self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Gemini3Pro => "3pro",
            Self::Gemini3Flash => "3flash",
            Self::Codex5hr => "codex",
        }
    }

    pub fn default_color(&self) -> AnsiColor {
        match self {
            Self::Opus => AnsiColor::Color256 { c256: 214 },
            Self::Gemini3Pro => AnsiColor::Color256 { c256: 129 },
            Self::Gemini3Flash => AnsiColor::Color256 { c256: 45 },
            Self::Codex5hr => AnsiColor::Color256 { c256: 48 },
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Gemini3Pro => "Gemini 3 Pro",
            Self::Gemini3Flash => "Gemini 3 Flash",
            Self::Codex5hr => "Codex 5hr",
        }
    }

    pub fn all() -> &'static [TrackedModel] {
        &[
            Self::Opus,
            Self::Gemini3Pro,
            Self::Gemini3Flash,
            Self::Codex5hr,
        ]
    }
}

/// Cache structure for CLI Proxy API quota data
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CliProxyApiQuotaCache {
    quotas: Vec<ModelQuota>,
    cached_at: String,
}

struct RefreshLockGuard {
    path: std::path::PathBuf,
}

impl Drop for RefreshLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelQuota {
    model_id: String,
    display_name: String,
    remaining_fraction: f64,
    auth_type: String,
}

#[derive(Default)]
pub struct CliProxyApiQuotaSegment;

impl CliProxyApiQuotaSegment {
    pub fn new() -> Self {
        Self
    }

    fn antigravity_user_agent() -> String {
        let version = env!("CARGO_PKG_VERSION");
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" | "i686" => "386",
            other => other,
        };

        format!("antigravity/{} {}/{}", version, os, arch)
    }

    fn normalize_model_text(text: &str) -> String {
        let mut s = text.trim().to_lowercase();
        for suffix in ["-preview", " preview"] {
            if s.ends_with(suffix) {
                let new_len = s.len().saturating_sub(suffix.len());
                s.truncate(new_len);
                s = s.trim_end().to_string();
            }
        }
        s
    }

    fn tracked_model_for(model_id: &str, display_name: &str) -> Option<TrackedModel> {
        let id = Self::normalize_model_text(model_id);
        let name = Self::normalize_model_text(display_name);

        if id.contains("opus") || name.contains("opus") {
            return Some(TrackedModel::Opus);
        }
        if id.contains("gemini-3-pro") || name.contains("gemini 3 pro") {
            return Some(TrackedModel::Gemini3Pro);
        }
        if id.contains("gemini-3-flash") || name.contains("gemini 3 flash") {
            return Some(TrackedModel::Gemini3Flash);
        }
        if id.contains("codex-5hr") || name.contains("5 小时限额") || name.contains("5hr") {
            return Some(TrackedModel::Codex5hr);
        }

        None
    }

    fn tracked_model_for_quota(quota: &ModelQuota) -> Option<TrackedModel> {
        Self::tracked_model_for(&quota.model_id, &quota.display_name)
    }

    fn get_alias(
        &self,
        options: &HashMap<String, serde_json::Value>,
        model: TrackedModel,
    ) -> String {
        options
            .get(model.alias_key())
            .and_then(|v| v.as_str())
            .unwrap_or(model.default_alias())
            .to_string()
    }

    fn get_color(
        &self,
        options: &HashMap<String, serde_json::Value>,
        model: TrackedModel,
    ) -> AnsiColor {
        options
            .get(model.color_key())
            .and_then(|v| serde_json::from_value::<AnsiColor>(v.clone()).ok())
            .unwrap_or_else(|| model.default_color())
    }

    /// Apply ANSI foreground color to text (resets only foreground, keeps background)
    pub fn apply_foreground_color(text: &str, color: &AnsiColor) -> String {
        let prefix = match color {
            AnsiColor::Color16 { c16 } => {
                let code = if *c16 < 8 { 30 + c16 } else { 90 + (c16 - 8) };
                format!("\x1b[{}m", code)
            }
            AnsiColor::Color256 { c256 } => format!("\x1b[38;5;{}m", c256),
            AnsiColor::Rgb { r, g, b } => format!("\x1b[38;2;{};{};{}m", r, g, b),
        };
        // Use 39m to reset foreground only (keeps background intact if set)
        format!("{}{}\x1b[39m", prefix, text)
    }

    fn parse_model_order(&self, options: &HashMap<String, serde_json::Value>) -> Vec<TrackedModel> {
        let raw = options
            .get("model_order")
            .and_then(|v| v.as_str())
            .unwrap_or("0123");
        let mut seen = [false; 4];
        let mut order = Vec::new();

        for ch in raw.chars() {
            let (idx, model) = match ch {
                '0' => (0, TrackedModel::Opus),
                '1' => (1, TrackedModel::Gemini3Pro),
                '2' => (2, TrackedModel::Gemini3Flash),
                '3' => (3, TrackedModel::Codex5hr),
                _ => continue,
            };
            if seen[idx] {
                continue;
            }
            seen[idx] = true;
            order.push(model);
        }

        order
    }

    fn format_tracked_output(
        &self,
        quotas: &[ModelQuota],
        options: &HashMap<String, serde_json::Value>,
        separator: &str,
    ) -> String {
        #[derive(Default)]
        struct SumCount {
            sum: f64,
            count: u32,
            has_failure: bool,
        }

        let mut agg: HashMap<TrackedModel, SumCount> = HashMap::new();
        for quota in quotas {
            let Some(model) = Self::tracked_model_for_quota(quota) else {
                continue;
            };
            let entry = agg.entry(model).or_default();
            if quota.remaining_fraction < 0.0 {
                // Mark as having a failure
                entry.has_failure = true;
                entry.count += 1;
            } else {
                entry.sum += quota.remaining_fraction;
                entry.count += 1;
            }
        }

        let ordered_models: Vec<TrackedModel> = self
            .parse_model_order(options)
            .into_iter()
            .filter(|model| agg.get(model).map(|e| e.count > 0).unwrap_or(false))
            .collect();

        let hide_on_zero = options
            .get("hide_on_zero")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut parts = Vec::new();
        for model in ordered_models {
            let entry = agg.get(&model).unwrap();
            let alias = self.get_alias(options, model);
            let color = self.get_color(options, model);

            // Check if all entries for this model are failures
            let success_count = entry.count - if entry.has_failure { 1 } else { 0 };
            let (label, percent) = if entry.has_failure && success_count == 0 {
                // All failed
                (format!("{}:失败", alias), None)
            } else if entry.has_failure {
                // Some failed, show average of successful ones
                let avg = entry.sum / success_count as f64;
                let p = (avg * 100.0).round().clamp(0.0, 100.0) as u8;
                (format!("{}:{}%*", alias, p), Some(p))
            } else {
                // All successful
                let avg = entry.sum / entry.count as f64;
                let p = (avg * 100.0).round().clamp(0.0, 100.0) as u8;
                (format!("{}:{}%", alias, p), Some(p))
            };

            // Skip if hide_on_zero is enabled and percent is 0
            if hide_on_zero && percent == Some(0) {
                continue;
            }

            parts.push(Self::apply_foreground_color(&label, &color));
        }

        parts.join(separator)
    }

    fn get_cache_path() -> Option<std::path::PathBuf> {
        let home = dirs::home_dir()?;
        Some(
            home.join(".claude")
                .join("ccline")
                .join(".cli_proxy_api_quota_cache.json"),
        )
    }

    fn load_cache(&self) -> Option<CliProxyApiQuotaCache> {
        let cache_path = Self::get_cache_path()?;
        if !cache_path.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&cache_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn save_cache(&self, cache: &CliProxyApiQuotaCache) -> Result<(), String> {
        let cache_path = Self::get_cache_path().ok_or("无法定位用户目录，无法写入缓存")?;

        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建缓存目录失败: {}", e))?;
        }

        let json =
            serde_json::to_string_pretty(cache).map_err(|e| format!("序列化缓存失败: {}", e))?;

        std::fs::write(&cache_path, json).map_err(|e| format!("写入缓存失败: {}", e))?;

        Ok(())
    }

    fn is_cache_valid(&self, cache: &CliProxyApiQuotaCache, cache_duration: u64) -> bool {
        if let Ok(cached_at) = DateTime::parse_from_rfc3339(&cache.cached_at) {
            let now = Utc::now();
            let elapsed = now.signed_duration_since(cached_at.with_timezone(&Utc));
            elapsed.num_seconds() < cache_duration as i64
        } else {
            false
        }
    }

    fn get_auth_files(&self, host: &str, key: &str) -> Option<Vec<AuthFile>> {
        let url = format!("{}/v0/management/auth-files", host);

        let agent = ureq::AgentBuilder::new().build();
        let response = agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", key))
            .timeout(std::time::Duration::from_secs(30))
            .call()
            .ok()?;

        if response.status() == 200 {
            let resp: AuthFilesResponse = response.into_json().ok()?;
            Some(resp.files)
        } else {
            None
        }
    }

    fn api_call(
        &self,
        host: &str,
        key: &str,
        auth_index: &str,
        method: &str,
        url: &str,
        data: &str,
        extra_headers: Option<HashMap<String, String>>,
    ) -> Option<ApiCallResponse> {
        let api_url = format!("{}/v0/management/api-call", host);

        let mut headers: HashMap<String, String> = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer $TOKEN$".to_string());
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        if let Some(extra) = extra_headers {
            headers.extend(extra);
        }

        let payload = serde_json::json!({
            "authIndex": auth_index,
            "method": method,
            "url": url,
            "header": headers,
            "data": data
        });

        let agent = ureq::AgentBuilder::new().build();
        let response = agent
            .post(&api_url)
            .set("Authorization", &format!("Bearer {}", key))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(30))
            .send_json(&payload)
            .ok()?;

        if response.status() == 200 {
            response.into_json().ok()
        } else {
            None
        }
    }

    fn get_antigravity_quota(&self, host: &str, key: &str, auth_index: &str) -> Vec<ModelQuota> {
        let mut extra_headers = HashMap::new();
        extra_headers.insert("User-Agent".to_string(), Self::antigravity_user_agent());

        let result = self.api_call(
            host,
            key,
            auth_index,
            "POST",
            "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
            "{}",
            Some(extra_headers),
        );

        let mut quotas = Vec::new();

        if let Some(response) = result {
            if let Some(body) = response.body {
                if let Ok(models_resp) = serde_json::from_str::<AntigravityModelsResponse>(&body) {
                    if let Some(models) = models_resp.models {
                        for (model_id, model_info) in models {
                            if let Some(quota_info) = model_info.quota_info {
                                if let Some(remaining) = quota_info.remaining_fraction {
                                    let display_name = model_info
                                        .display_name
                                        .clone()
                                        .unwrap_or_else(|| model_id.clone());

                                    // Only keep Opus / Gemini 3 Pro / Gemini 3 Flash
                                    if Self::tracked_model_for(&model_id, &display_name).is_none() {
                                        continue;
                                    }

                                    quotas.push(ModelQuota {
                                        model_id: model_id.clone(),
                                        display_name,
                                        remaining_fraction: remaining,
                                        auth_type: "antigravity".to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        quotas
    }

    fn extract_project_from_name(&self, name: &str) -> Option<String> {
        // gemini-gaakki@gmail.com-airy-lodge-481706-r3.json -> airy-lodge-481706-r3
        let name = name.replace(".json", "");
        let parts: Vec<&str> = name.split('-').collect();
        if parts.len() >= 4 {
            for (i, part) in parts.iter().enumerate() {
                if part.contains('@') {
                    return Some(parts[i + 1..].join("-"));
                }
            }
        }
        None
    }

    fn get_gemini_cli_quota(
        &self,
        host: &str,
        key: &str,
        auth_index: &str,
        project: &str,
    ) -> Vec<ModelQuota> {
        let data = serde_json::json!({"project": project}).to_string();

        let result = self.api_call(
            host,
            key,
            auth_index,
            "POST",
            "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota",
            &data,
            None,
        );

        let mut quotas = Vec::new();

        if let Some(response) = result {
            if let Some(body) = response.body {
                if let Ok(quota_resp) = serde_json::from_str::<GeminiQuotaResponse>(&body) {
                    if let Some(buckets) = quota_resp.buckets {
                        for bucket in buckets {
                            if let (Some(model_id), Some(remaining)) =
                                (bucket.model_id, bucket.remaining_fraction)
                            {
                                // Only keep Opus / Gemini 3 Pro / Gemini 3 Flash
                                if Self::tracked_model_for(&model_id, &model_id).is_none() {
                                    continue;
                                }

                                quotas.push(ModelQuota {
                                    model_id: model_id.clone(),
                                    display_name: model_id,
                                    remaining_fraction: remaining,
                                    auth_type: "gemini-cli".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        quotas
    }

    fn codex_user_agent() -> String {
        let version = env!("CARGO_PKG_VERSION");
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" | "i686" => "386",
            other => other,
        };

        format!(
            "codex_cli_rs/{} ({} {}; {}) WindowsTerminal",
            version,
            os,
            std::env::consts::OS,
            arch
        )
    }

    fn download_auth_file(
        &self,
        host: &str,
        key: &str,
        name: &str,
    ) -> Option<CodexAuthFileContent> {
        let url = format!("{}/v0/management/auth-files/download?name={}", host, name);

        let agent = ureq::AgentBuilder::new().build();
        let response = agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", key))
            .timeout(std::time::Duration::from_secs(10))
            .call()
            .ok()?;

        if response.status() == 200 {
            response.into_json().ok()
        } else {
            None
        }
    }

    fn get_codex_quota(
        &self,
        host: &str,
        key: &str,
        auth_index: &str,
        account_id: Option<&str>,
    ) -> Vec<ModelQuota> {
        let mut extra_headers = HashMap::new();
        extra_headers.insert("User-Agent".to_string(), Self::codex_user_agent());
        if let Some(id) = account_id {
            if !id.is_empty() {
                extra_headers.insert("Chatgpt-Account-Id".to_string(), id.to_string());
            }
        }

        let result = self.api_call(
            host,
            key,
            auth_index,
            "GET",
            "https://chatgpt.com/backend-api/wham/usage",
            "",
            Some(extra_headers),
        );

        let mut quotas = Vec::new();

        match result {
            Some(response) => {
                if let Some(body) = response.body {
                    if let Ok(usage_resp) = serde_json::from_str::<CodexUsageResponse>(&body) {
                        if let Some(rate_limit) = usage_resp.rate_limit {
                            if let Some(primary) = rate_limit.primary_window {
                                if let Some(used_percent) = primary.used_percent {
                                    let remaining = (100.0 - used_percent) / 100.0;
                                    quotas.push(ModelQuota {
                                        model_id: "codex-5hr".to_string(),
                                        display_name: "5 小时限额".to_string(),
                                        remaining_fraction: remaining.clamp(0.0, 1.0),
                                        auth_type: "codex".to_string(),
                                    });
                                    return quotas;
                                }
                            }
                        }
                    }
                }
                // Response received but parsing failed - mark as failed
                quotas.push(ModelQuota {
                    model_id: "codex-5hr".to_string(),
                    display_name: "5 小时限额".to_string(),
                    remaining_fraction: -1.0, // Use -1 to indicate failure
                    auth_type: "codex".to_string(),
                });
            }
            None => {
                // Request failed - mark as failed
                quotas.push(ModelQuota {
                    model_id: "codex-5hr".to_string(),
                    display_name: "5 小时限额".to_string(),
                    remaining_fraction: -1.0, // Use -1 to indicate failure
                    auth_type: "codex".to_string(),
                });
            }
        }

        quotas
    }

    fn fetch_all_quotas(
        &self,
        host: &str,
        key: &str,
        auth_type_filter: &str,
        codex_enabled: bool,
    ) -> Vec<ModelQuota> {
        let auth_files = match self.get_auth_files(host, key) {
            Some(files) => files,
            None => return Vec::new(),
        };

        let auth_files: Vec<AuthFile> = auth_files
            .into_iter()
            .filter(|file| !file.disabled.unwrap_or(false))
            .filter(|file| auth_type_filter == "all" || file.auth_type == auth_type_filter)
            .filter(|file| codex_enabled || file.auth_type != "codex")
            .collect();

        if auth_files.is_empty() {
            return Vec::new();
        }

        let worker_count = auth_files.len().min(CPA_QUOTA_REFRESH_WORKERS).max(1);
        if worker_count == 1 {
            let mut all_quotas = Vec::new();
            for file in auth_files {
                let quotas = match file.auth_type.as_str() {
                    "antigravity" => self.get_antigravity_quota(host, key, &file.auth_index),
                    "gemini-cli" => {
                        if let Some(project) =
                            self.extract_project_from_name(file.name.as_deref().unwrap_or(""))
                        {
                            self.get_gemini_cli_quota(host, key, &file.auth_index, &project)
                        } else {
                            Vec::new()
                        }
                    }
                    "codex" if codex_enabled => {
                        let account_id = self
                            .download_auth_file(host, key, file.name.as_deref().unwrap_or(""))
                            .and_then(|content| content.account_id.or(content.chatgpt_account_id));
                        self.get_codex_quota(host, key, &file.auth_index, account_id.as_deref())
                    }
                    _ => Vec::new(),
                };

                all_quotas.extend(quotas);
            }
            return all_quotas;
        }

        let host = host.to_string();
        let key = key.to_string();

        let mut buckets: Vec<Vec<AuthFile>> = (0..worker_count).map(|_| Vec::new()).collect();
        for (idx, file) in auth_files.into_iter().enumerate() {
            buckets[idx % worker_count].push(file);
        }

        let mut handles = Vec::new();
        for bucket in buckets {
            let host = host.clone();
            let key = key.clone();
            let codex_enabled = codex_enabled;

            handles.push(std::thread::spawn(move || {
                let segment = CliProxyApiQuotaSegment::new();
                let mut all_quotas = Vec::new();

                for file in bucket {
                    let quotas = match file.auth_type.as_str() {
                        "antigravity" => {
                            segment.get_antigravity_quota(&host, &key, &file.auth_index)
                        }
                        "gemini-cli" => {
                            if let Some(project) = segment
                                .extract_project_from_name(file.name.as_deref().unwrap_or(""))
                            {
                                segment.get_gemini_cli_quota(
                                    &host,
                                    &key,
                                    &file.auth_index,
                                    &project,
                                )
                            } else {
                                Vec::new()
                            }
                        }
                        "codex" if codex_enabled => {
                            let account_id = segment
                                .download_auth_file(&host, &key, file.name.as_deref().unwrap_or(""))
                                .and_then(|content| {
                                    content.account_id.or(content.chatgpt_account_id)
                                });
                            segment.get_codex_quota(
                                &host,
                                &key,
                                &file.auth_index,
                                account_id.as_deref(),
                            )
                        }
                        _ => Vec::new(),
                    };

                    all_quotas.extend(quotas);
                }

                all_quotas
            }));
        }

        let mut all_quotas = Vec::new();
        for handle in handles {
            if let Ok(mut quotas) = handle.join() {
                all_quotas.append(&mut quotas);
            }
        }

        all_quotas
    }
}

impl Segment for CliProxyApiQuotaSegment {
    fn collect(&self, _input: &InputData) -> Option<SegmentData> {
        // This method loads config from disk - use collect_with_options for better performance
        let config = crate::config::Config::load().ok()?;
        let segment_config = config
            .segments
            .iter()
            .find(|s| s.id == SegmentId::CliProxyApiQuota)?;
        self.collect_with_options(&segment_config.options)
    }

    fn id(&self) -> SegmentId {
        SegmentId::CliProxyApiQuota
    }
}

/// Max worker threads for refreshing CPA quotas.
///
/// The refresh logic makes multiple blocking HTTP requests (via `ureq`) and is
/// performance-bound by network latency. Using a small, bounded amount of
/// parallelism provides a large latency win (similar to the management web UI)
/// while avoiding excessive concurrency.
const CPA_QUOTA_REFRESH_WORKERS: usize = 8;

/// Cooldown between spawn attempts (seconds)
const REFRESH_COOLDOWN_SECS: u64 = 60;
/// Lock file TTL - consider stale after this (seconds)
const REFRESH_LOCK_TTL_SECS: u64 = 120;

impl CliProxyApiQuotaSegment {
    fn refresh_lock_path() -> Option<std::path::PathBuf> {
        Self::get_cache_path().map(|p| p.with_file_name(".cli_proxy_api_quota_refresh.lock"))
    }

    fn refresh_stamp_path() -> Option<std::path::PathBuf> {
        Self::get_cache_path().map(|p| p.with_file_name(".cli_proxy_api_quota_refresh.stamp"))
    }

    fn is_refresh_locked(now: SystemTime) -> bool {
        if let Some(lock_path) = Self::refresh_lock_path() {
            if let Ok(meta) = std::fs::metadata(&lock_path) {
                if let Ok(modified) = meta.modified() {
                    if now
                        .duration_since(modified)
                        .unwrap_or(Duration::ZERO)
                        .as_secs()
                        <= REFRESH_LOCK_TTL_SECS
                    {
                        return true;
                    }
                }

                // Stale lock: best-effort cleanup.
                let _ = std::fs::remove_file(&lock_path);
            }
        }

        false
    }

    fn is_refresh_on_cooldown(now: SystemTime) -> bool {
        let Some(stamp_path) = Self::refresh_stamp_path() else {
            return false;
        };

        if let Ok(meta) = std::fs::metadata(&stamp_path) {
            if let Ok(modified) = meta.modified() {
                if now
                    .duration_since(modified)
                    .unwrap_or(Duration::ZERO)
                    .as_secs()
                    <= REFRESH_COOLDOWN_SECS
                {
                    return true;
                }
            }
        }

        false
    }

    fn touch_refresh_stamp() {
        let Some(stamp_path) = Self::refresh_stamp_path() else {
            return;
        };

        if let Some(parent) = stamp_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&stamp_path, Utc::now().to_rfc3339());
    }

    fn spawn_refresh_process() -> bool {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };

        let mut cmd = Command::new(exe);
        cmd.arg("--refresh-cpa-quota")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            const DETACHED_PROCESS: u32 = 0x00000008;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
        }

        cmd.spawn().is_ok()
    }

    fn try_refresh_cache_async(&self) -> bool {
        let now = SystemTime::now();
        if Self::is_refresh_locked(now) || Self::is_refresh_on_cooldown(now) {
            return false;
        }

        Self::touch_refresh_stamp();
        Self::spawn_refresh_process()
    }

    fn acquire_refresh_lock(&self) -> Result<Option<RefreshLockGuard>, String> {
        let now = SystemTime::now();

        let lock_path = Self::refresh_lock_path().ok_or("无法定位锁文件路径")?;

        if let Ok(meta) = std::fs::metadata(&lock_path) {
            if let Ok(modified) = meta.modified() {
                if now
                    .duration_since(modified)
                    .unwrap_or(Duration::ZERO)
                    .as_secs()
                    <= REFRESH_LOCK_TTL_SECS
                {
                    // Another refresh is in progress.
                    return Ok(None);
                }
            }

            // Stale lock: best-effort cleanup.
            let _ = std::fs::remove_file(&lock_path);
        }

        let file = match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(e) => return Err(format!("创建锁文件失败: {}", e)),
        };

        let mut file = file;
        let _ = writeln!(
            file,
            "pid={} started_at={}",
            std::process::id(),
            Utc::now().to_rfc3339()
        );

        Ok(Some(RefreshLockGuard { path: lock_path }))
    }

    pub fn refresh_cache_with_options(
        &self,
        options: &HashMap<String, serde_json::Value>,
    ) -> Result<usize, String> {
        let lock_guard = match self.acquire_refresh_lock()? {
            Some(lock) => lock,
            None => return Ok(0),
        };

        let host = options
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("http://localhost:8317");

        let key = options
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("nbkey");

        let auth_type = options
            .get("auth_type")
            .and_then(|v| v.as_str())
            .unwrap_or("all");

        let codex_enabled = options
            .get("codex_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let fetched = self.fetch_all_quotas(host, key, auth_type, codex_enabled);
        if fetched.is_empty() {
            return Err("未获取到任何额度数据".to_string());
        }

        let cache = CliProxyApiQuotaCache {
            quotas: fetched,
            cached_at: Utc::now().to_rfc3339(),
        };
        self.save_cache(&cache)?;

        drop(lock_guard);
        Ok(cache.quotas.len())
    }

    /// Collect quota data using provided options (avoids loading config from disk)
    /// Cache-only: never blocks on network requests.
    /// Use `ccline --refresh-cpa-quota` (or a scheduled task) to refresh the cache.
    /// When cache is missing/expired, optionally spawns a background refresh if `auto_refresh=true`.
    pub fn collect_with_options(
        &self,
        options: &HashMap<String, serde_json::Value>,
    ) -> Option<SegmentData> {
        let cache_duration = options
            .get("cache_duration")
            .and_then(|v| v.as_u64())
            .unwrap_or(180);

        let separator = options
            .get("separator")
            .and_then(|v| v.as_str())
            .unwrap_or(" | ");

        let (quotas, cache_valid, cache_present) = match self.load_cache() {
            Some(cache) => {
                let cache_valid = self.is_cache_valid(&cache, cache_duration);
                (cache.quotas, cache_valid, true)
            }
            None => (Vec::new(), false, false),
        };
        let need_refresh = !cache_present || !cache_valid;
        let quotas = quotas;

        // Always render immediately, refresh asynchronously if needed
        if need_refresh {
            self.try_refresh_cache_async();
        }

        // If no data available at all
        if quotas.is_empty() {
            let mut metadata = HashMap::new();
            metadata.insert("raw_text".to_string(), "true".to_string());
            metadata.insert("cache_missing".to_string(), "true".to_string());
            return Some(SegmentData {
                primary: "\x1b[90mcpa:--\x1b[39m".to_string(),
                secondary: String::new(),
                metadata,
            });
        }

        let primary = self.format_tracked_output(&quotas, options, separator);

        if primary.is_empty() {
            return None;
        }

        let display_primary = if cache_valid {
            primary
        } else {
            format!("\x1b[90m~\x1b[39m{}", primary)
        };

        let mut metadata = HashMap::new();
        metadata.insert("raw_text".to_string(), "true".to_string());
        if !cache_valid {
            metadata.insert("stale_cache".to_string(), "true".to_string());
        }

        Some(SegmentData {
            primary: display_primary,
            secondary: String::new(),
            metadata,
        })
    }
}
