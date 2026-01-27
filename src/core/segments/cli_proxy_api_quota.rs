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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackedModel {
    Opus,
    Gemini3Pro,
    Gemini3Flash,
}

impl TrackedModel {
    pub fn alias_key(&self) -> &'static str {
        match self {
            Self::Opus => "opus_alias",
            Self::Gemini3Pro => "gemini3pro_alias",
            Self::Gemini3Flash => "gemini3flash_alias",
        }
    }

    pub fn color_key(&self) -> &'static str {
        match self {
            Self::Opus => "opus_color",
            Self::Gemini3Pro => "gemini3pro_color",
            Self::Gemini3Flash => "gemini3flash_color",
        }
    }

    pub fn default_alias(&self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Gemini3Pro => "3pro",
            Self::Gemini3Flash => "3flash",
        }
    }

    pub fn default_color(&self) -> AnsiColor {
        match self {
            Self::Opus => AnsiColor::Color256 { c256: 214 },
            Self::Gemini3Pro => AnsiColor::Color256 { c256: 129 },
            Self::Gemini3Flash => AnsiColor::Color256 { c256: 45 },
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Gemini3Pro => "Gemini 3 Pro",
            Self::Gemini3Flash => "Gemini 3 Flash",
        }
    }

    pub fn all() -> &'static [TrackedModel] {
        &[Self::Opus, Self::Gemini3Pro, Self::Gemini3Flash]
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
        }

        let mut agg: HashMap<TrackedModel, SumCount> = HashMap::new();
        for quota in quotas {
            let Some(model) = Self::tracked_model_for_quota(quota) else {
                continue;
            };
            let entry = agg.entry(model).or_default();
            entry.sum += quota.remaining_fraction;
            entry.count += 1;
        }

        let mut parts = Vec::new();
        for model in [
            TrackedModel::Opus,
            TrackedModel::Gemini3Pro,
            TrackedModel::Gemini3Flash,
        ] {
            let Some(entry) = agg.get(&model) else {
                continue;
            };
            if entry.count == 0 {
                continue;
            }

            let avg = entry.sum / entry.count as f64;
            let percent = (avg * 100.0).round().clamp(0.0, 100.0) as u8;
            let alias = self.get_alias(options, model);
            let color = self.get_color(options, model);
            let label = format!("{}:{}%", alias, percent);
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
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建缓存目录失败: {}", e))?;
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

    fn fetch_all_quotas(&self, host: &str, key: &str, auth_type_filter: &str) -> Vec<ModelQuota> {
        let mut all_quotas = Vec::new();

        let auth_files = match self.get_auth_files(host, key) {
            Some(files) => files,
            None => return all_quotas,
        };

        for file in auth_files {
            // Skip disabled accounts
            if file.disabled.unwrap_or(false) {
                continue;
            }

            // Apply type filter
            if auth_type_filter != "all" && file.auth_type != auth_type_filter {
                continue;
            }

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
                _ => Vec::new(),
            };

            all_quotas.extend(quotas);
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

impl CliProxyApiQuotaSegment {
    fn auto_refresh_enabled(options: &HashMap<String, serde_json::Value>) -> bool {
        options
            .get("auto_refresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    fn auto_refresh_cooldown(options: &HashMap<String, serde_json::Value>) -> u64 {
        options
            .get("auto_refresh_cooldown")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
    }

    fn auto_refresh_lock_ttl(options: &HashMap<String, serde_json::Value>) -> u64 {
        options
            .get("auto_refresh_lock_ttl")
            .and_then(|v| v.as_u64())
            .unwrap_or(600)
    }

    fn refresh_lock_path() -> Option<std::path::PathBuf> {
        Self::get_cache_path().map(|p| p.with_file_name(".cli_proxy_api_quota_refresh.lock"))
    }

    fn refresh_stamp_path() -> Option<std::path::PathBuf> {
        Self::get_cache_path().map(|p| p.with_file_name(".cli_proxy_api_quota_refresh.stamp"))
    }

    fn maybe_spawn_refresh(&self, options: &HashMap<String, serde_json::Value>) {
        if !Self::auto_refresh_enabled(options) {
            return;
        }

        let cooldown = Self::auto_refresh_cooldown(options);
        let lock_ttl = Self::auto_refresh_lock_ttl(options);
        let now = SystemTime::now();

        // If a refresh is already in progress, don't spawn another.
        if let Some(lock_path) = Self::refresh_lock_path() {
            if let Ok(meta) = std::fs::metadata(&lock_path) {
                if let Ok(modified) = meta.modified() {
                    if now
                        .duration_since(modified)
                        .unwrap_or(Duration::ZERO)
                        .as_secs()
                        <= lock_ttl
                    {
                        return;
                    }
                }

                // Stale lock: best-effort cleanup.
                let _ = std::fs::remove_file(&lock_path);
            }
        }

        let Some(stamp_path) = Self::refresh_stamp_path() else {
            return;
        };

        // Throttle spawns to avoid process storms on repeated failures.
        if let Ok(meta) = std::fs::metadata(&stamp_path) {
            if let Ok(modified) = meta.modified() {
                if now
                    .duration_since(modified)
                    .unwrap_or(Duration::ZERO)
                    .as_secs()
                    <= cooldown
                {
                    return;
                }
            }
        }

        if let Some(parent) = stamp_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&stamp_path, Utc::now().to_rfc3339());

        let Ok(exe) = std::env::current_exe() else {
            return;
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

        let _ = cmd.spawn();
    }

    fn acquire_refresh_lock(
        &self,
        options: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<RefreshLockGuard>, String> {
        let lock_ttl = Self::auto_refresh_lock_ttl(options);
        let now = SystemTime::now();

        let lock_path = Self::refresh_lock_path().ok_or("无法定位锁文件路径")?;

        if let Ok(meta) = std::fs::metadata(&lock_path) {
            if let Ok(modified) = meta.modified() {
                if now
                    .duration_since(modified)
                    .unwrap_or(Duration::ZERO)
                    .as_secs()
                    <= lock_ttl
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
        let lock_guard = match self.acquire_refresh_lock(options)? {
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

        let fetched = self.fetch_all_quotas(host, key, auth_type);
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
        if need_refresh {
            self.maybe_spawn_refresh(options);
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
