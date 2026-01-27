use super::{Segment, SegmentData};
use crate::config::{InputData, ModelConfig, SegmentId, TranscriptEntry};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct ContextWindowSegment;

const TRANSCRIPT_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const TRANSCRIPT_TAIL_MAX_LINES: usize = 4000;
const LEAF_UUID_SEARCH_MAX_FILES: usize = 20;

impl ContextWindowSegment {
    pub fn new() -> Self {
        Self
    }

    /// Get context limit for the specified model
    fn get_context_limit_for_model(model_id: &str) -> u32 {
        let model_config = ModelConfig::load_cached();
        model_config.get_context_limit(model_id)
    }
}

impl Segment for ContextWindowSegment {
    fn collect(&self, input: &InputData) -> Option<SegmentData> {
        // Dynamically determine context limit based on current model ID
        let context_limit = Self::get_context_limit_for_model(&input.model.id);

        let context_used_token_opt = parse_transcript_usage(&input.transcript_path);

        let (percentage_display, tokens_display) = match context_used_token_opt {
            Some(context_used_token) => {
                let context_used_rate = (context_used_token as f64 / context_limit as f64) * 100.0;

                let percentage = if context_used_rate.fract() == 0.0 {
                    format!("{:.0}%", context_used_rate)
                } else {
                    format!("{:.1}%", context_used_rate)
                };

                let tokens = if context_used_token >= 1000 {
                    let k_value = context_used_token as f64 / 1000.0;
                    if k_value.fract() == 0.0 {
                        format!("{}k", k_value as u32)
                    } else {
                        format!("{:.1}k", k_value)
                    }
                } else {
                    context_used_token.to_string()
                };

                (percentage, tokens)
            }
            None => {
                // No usage data available
                ("-".to_string(), "-".to_string())
            }
        };

        let mut metadata = HashMap::new();
        match context_used_token_opt {
            Some(context_used_token) => {
                let context_used_rate = (context_used_token as f64 / context_limit as f64) * 100.0;
                metadata.insert("tokens".to_string(), context_used_token.to_string());
                metadata.insert("percentage".to_string(), context_used_rate.to_string());
            }
            None => {
                metadata.insert("tokens".to_string(), "-".to_string());
                metadata.insert("percentage".to_string(), "-".to_string());
            }
        }
        metadata.insert("limit".to_string(), context_limit.to_string());
        metadata.insert("model".to_string(), input.model.id.clone());

        Some(SegmentData {
            primary: format!("{} · {} tokens", percentage_display, tokens_display),
            secondary: String::new(),
            metadata,
        })
    }

    fn id(&self) -> SegmentId {
        SegmentId::ContextWindow
    }
}

fn parse_transcript_usage<P: AsRef<Path>>(transcript_path: P) -> Option<u32> {
    let path = transcript_path.as_ref();

    if !path.exists() {
        return None;
    }

    try_parse_transcript_file(path)
}

fn try_parse_transcript_file(path: &Path) -> Option<u32> {
    let (tail, started_mid_file) = read_transcript_tail(path)?;
    let lines = tail_lines(&tail, started_mid_file, TRANSCRIPT_TAIL_MAX_LINES);

    // Check if the last line is a summary
    let last_line = lines.iter().rev().find(|l| !l.trim().is_empty())?.trim();
    if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(last_line) {
        if entry.r#type.as_deref() == Some("summary") {
            // Handle summary case: find usage by leafUuid
            if let Some(leaf_uuid) = &entry.leaf_uuid {
                // Prefer searching within the current transcript tail first to avoid
                // expensive full-project scans during Claude Code startup.
                if let Some(usage) = search_uuid_in_lines(&lines, leaf_uuid) {
                    return Some(usage);
                }

                let project_dir = path.parent()?;
                return find_usage_by_leaf_uuid(leaf_uuid, project_dir);
            }
        }
    }

    // Normal case: find the last assistant message in current file
    for line in lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
            if entry.r#type.as_deref() == Some("assistant") {
                if let Some(message) = &entry.message {
                    if let Some(raw_usage) = &message.usage {
                        let normalized = raw_usage.clone().normalize();
                        return Some(normalized.display_tokens());
                    }
                }
            }
        }
    }

    None
}

fn find_usage_by_leaf_uuid(leaf_uuid: &str, project_dir: &Path) -> Option<u32> {
    let session_files = get_recent_session_files(project_dir, LEAF_UUID_SEARCH_MAX_FILES)?;

    for path in session_files {
        if let Some(usage) = search_uuid_in_file_tail(&path, leaf_uuid) {
            return Some(usage);
        }
    }

    None
}

fn search_uuid_in_file_tail(path: &Path, target_uuid: &str) -> Option<u32> {
    let (tail, started_mid_file) = read_transcript_tail(path)?;
    let lines = tail_lines(&tail, started_mid_file, TRANSCRIPT_TAIL_MAX_LINES);
    search_uuid_in_lines(&lines, target_uuid)
}

fn search_uuid_in_lines(lines: &[&str], target_uuid: &str) -> Option<u32> {
    // Find the message with target_uuid
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
            let Some(uuid) = &entry.uuid else {
                continue;
            };

            if uuid != target_uuid {
                continue;
            }

            // Found the target message, check its type
            if entry.r#type.as_deref() == Some("assistant") {
                // Direct assistant message with usage
                if let Some(message) = &entry.message {
                    if let Some(raw_usage) = &message.usage {
                        let normalized = raw_usage.clone().normalize();
                        return Some(normalized.display_tokens());
                    }
                }
            } else if entry.r#type.as_deref() == Some("user") {
                // User message, need to find the parent assistant message
                if let Some(parent_uuid) = &entry.parent_uuid {
                    return find_assistant_message_by_uuid(lines, parent_uuid);
                }
            }

            break;
        }
    }

    None
}

fn find_assistant_message_by_uuid(lines: &[&str], target_uuid: &str) -> Option<u32> {
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
            if let Some(uuid) = &entry.uuid {
                if uuid == target_uuid && entry.r#type.as_deref() == Some("assistant") {
                    if let Some(message) = &entry.message {
                        if let Some(raw_usage) = &message.usage {
                            let normalized = raw_usage.clone().normalize();
                            return Some(normalized.display_tokens());
                        }
                    }
                }
            }
        }
    }

    None
}

fn read_transcript_tail(path: &Path) -> Option<(String, bool)> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), start > 0))
}

fn tail_lines<'a>(tail: &'a str, started_mid_file: bool, max_lines: usize) -> Vec<&'a str> {
    let tail = if started_mid_file {
        match tail.split_once('\n') {
            Some((_, rest)) => rest,
            None => "",
        }
    } else {
        tail
    };

    let mut lines: Vec<&'a str> = Vec::with_capacity(max_lines);
    for line in tail.lines().rev() {
        if lines.len() >= max_lines {
            break;
        }
        lines.push(line.trim_end_matches('\r'));
    }
    lines.reverse();
    lines
}

fn get_recent_session_files(project_dir: &Path, limit: usize) -> Option<Vec<PathBuf>> {
    let entries = fs::read_dir(project_dir).ok()?;

    let mut session_files: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            session_files.push(path);
        }
    }

    if session_files.is_empty() {
        return None;
    }

    // Sort by modification time (most recent first)
    session_files.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    session_files.reverse();

    session_files.truncate(limit);
    Some(session_files)
}
