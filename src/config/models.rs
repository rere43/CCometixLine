use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::RwLock;
use std::time::SystemTime;

/// Model alias entry for exact model ID matching
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAlias {
    /// Exact model ID to match
    pub id: String,
    /// Display name to show in statusline
    pub display_name: String,
    /// Optional context limit override
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model aliases for exact ID matching (highest priority)
    #[serde(default, rename = "aliases")]
    pub model_aliases: Vec<ModelAlias>,
    /// Model patterns for fuzzy matching (fallback)
    #[serde(default, rename = "models")]
    pub model_entries: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub pattern: String,
    pub display_name: String,
    pub context_limit: u32,
}

impl ModelConfig {
    /// Load model configuration from TOML file
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let config: ModelConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load model configuration with fallback locations
    pub fn load() -> Self {
        let mut model_config = Self::default();

        // First, try to create default models.toml if it doesn't exist
        if let Some(home_dir) = dirs::home_dir() {
            let user_models_path = home_dir.join(".claude").join("ccline").join("models.toml");
            if !user_models_path.exists() {
                let _ = Self::create_default_file(&user_models_path);
            }
        }

        // Try loading from user config directory first, then local
        let config_paths = [
            dirs::home_dir().map(|d| d.join(".claude").join("ccline").join("models.toml")),
            Some(Path::new("models.toml").to_path_buf()),
        ];

        for path in config_paths.iter().flatten() {
            if path.exists() {
                if let Ok(config) = Self::load_from_file(path) {
                    // Merge aliases (user config takes priority)
                    let mut merged_aliases = config.model_aliases;
                    merged_aliases.extend(model_config.model_aliases);
                    model_config.model_aliases = merged_aliases;

                    // Prepend external models to built-in ones for priority
                    let mut merged_entries = config.model_entries;
                    merged_entries.extend(model_config.model_entries);
                    model_config.model_entries = merged_entries;
                    return model_config;
                }
            }
        }

        // Fallback to default configuration if no file found
        model_config
    }

    /// Get context limit for a model based on ID matching
    /// Priority: exact alias match > pattern match > default
    pub fn get_context_limit(&self, model_id: &str) -> u32 {
        // First, check exact alias match
        for alias in &self.model_aliases {
            if alias.id == model_id {
                if let Some(limit) = alias.context_limit {
                    return limit;
                }
            }
        }

        let model_lower = model_id.to_lowercase();

        // Check model entries (pattern matching)
        for entry in &self.model_entries {
            if model_lower.contains(&entry.pattern.to_lowercase()) {
                return entry.context_limit;
            }
        }

        200_000
    }

    /// Get display name for a model based on ID matching
    /// Priority: exact alias match > pattern match > None (use fallback)
    pub fn get_display_name(&self, model_id: &str) -> Option<String> {
        // First, check exact alias match (highest priority)
        for alias in &self.model_aliases {
            if alias.id == model_id {
                return Some(alias.display_name.clone());
            }
        }

        let model_lower = model_id.to_lowercase();

        // Check model entries (pattern matching)
        for entry in &self.model_entries {
            if model_lower.contains(&entry.pattern.to_lowercase()) {
                return Some(entry.display_name.clone());
            }
        }

        None
    }

    /// Create default model configuration file with minimal template
    pub fn create_default_file<P: AsRef<Path>>(path: P) -> Result<(), Box<dyn std::error::Error>> {
        // Create parent directory if it doesn't exist
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }

        // Create template content with examples
        let template_content = r#"# CCometixLine Model Configuration
# This file defines model display names and context limits for different LLM models
# File location: ~/.claude/ccline/models.toml

# =============================================================================
# Model Aliases (Exact Match - Highest Priority)
# =============================================================================
# Use aliases for exact model ID matching. This is useful when you want to
# customize the display name for a specific model ID.
#
# Example:
# [[aliases]]
# id = "gemini-claude-opus-4-5-thinking"      # Exact model ID to match
# display_name = "Opus 4.5"                    # Display name in statusline
# context_limit = 200000                       # Optional: override context limit

# [[aliases]]
# id = "my-custom-model-v1"
# display_name = "Custom Model"
# context_limit = 128000

# =============================================================================
# Model Patterns (Fuzzy Match - Fallback)
# =============================================================================
# Use patterns for fuzzy matching. The pattern is matched using "contains".
# Order matters: first match wins, so put more specific patterns first.
#
# Example:
# [[models]]
# pattern = "glm-4.5"
# display_name = "GLM-4.5"
# context_limit = 128000
"#;

        fs::write(path, template_content)?;
        Ok(())
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model_aliases: vec![],
            model_entries: vec![],
        }
    }
}

/// Cached model config with mtime tracking
struct CachedModelConfig {
    config: ModelConfig,
    mtime: Option<SystemTime>,
    path: Option<std::path::PathBuf>,
}

static MODEL_CONFIG_CACHE: Lazy<RwLock<Option<CachedModelConfig>>> =
    Lazy::new(|| RwLock::new(None));

impl ModelConfig {
    /// Load with caching - only reloads when file mtime changes
    pub fn load_cached() -> Self {
        let config_path =
            dirs::home_dir().map(|d| d.join(".claude").join("ccline").join("models.toml"));

        let current_mtime = config_path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());

        // Try to use cached version
        if let Ok(cache_guard) = MODEL_CONFIG_CACHE.read() {
            if let Some(cached) = cache_guard.as_ref() {
                // Check if mtime matches (or both are None)
                if cached.mtime == current_mtime && cached.path == config_path {
                    return cached.config.clone();
                }
            }
        }

        // Reload from disk
        let config = Self::load();

        // Update cache
        if let Ok(mut cache_guard) = MODEL_CONFIG_CACHE.write() {
            *cache_guard = Some(CachedModelConfig {
                config: config.clone(),
                mtime: current_mtime,
                path: config_path,
            });
        }

        config
    }
}
