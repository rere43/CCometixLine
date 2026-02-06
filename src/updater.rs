use serde::{Deserialize, Serialize};

#[cfg(feature = "self-update")]
use chrono::{DateTime, Utc};

#[cfg(feature = "self-update")]
const UPDATE_CHECK_INTERVAL_HOURS: i64 = 1;

/// Guard for `update_pid` lock staleness.
///
/// The statusline process is short-lived; if it crashes mid-check, `update_pid` can remain in the
/// persisted state file. PIDs can be reused quickly on Windows, causing the lock to "stick" and
/// the cache to never refresh. Treat the lock as stale after a short TTL.
#[cfg(feature = "self-update")]
const UPDATE_PID_LOCK_TTL_SECS: i64 = 5 * 60;

/// Update status enum
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum UpdateStatus {
    /// Idle state, no update activity
    #[default]
    Idle,
    /// Currently checking for updates
    Checking,
    /// New version found, manual update required
    Ready {
        version: String,
        found_at: DateTime<Utc>,
    },
    /// Downloading new version
    Downloading { progress: u8 },
    /// Currently installing update
    Installing,
    /// Update completed successfully
    Completed {
        version: String,
        #[cfg(feature = "self-update")]
        completed_at: DateTime<Utc>,
    },
    /// Update failed with error
    Failed { error: String },
}

/// Update state persistence structure
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct UpdateState {
    pub status: UpdateStatus,
    #[cfg(feature = "self-update")]
    pub last_check: Option<DateTime<Utc>>,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_pid: Option<u32>,
}

impl UpdateState {
    #[cfg(feature = "self-update")]
    fn is_update_pid_lock_stale(&self) -> bool {
        let Some(last_check) = self.last_check else {
            return true;
        };

        let now = Utc::now();
        let age_secs = now.signed_duration_since(last_check).num_seconds();

        // If clock moved backwards, consider the lock invalid to avoid permanently skipping checks.
        !(0..=UPDATE_PID_LOCK_TTL_SECS).contains(&age_secs)
    }

    /// Get status bar display text
    pub fn status_text(&self) -> Option<String> {
        match &self.status {
            #[cfg(feature = "self-update")]
            UpdateStatus::Ready { version, .. } => Some(format!("\u{f06b0} Update v{}!", version)),
            #[cfg(not(feature = "self-update"))]
            UpdateStatus::Ready { version, .. } => Some(format!("\u{f06b0} Update v{}!", version)),
            UpdateStatus::Downloading { progress } => Some(format!("\u{f01da} {}%", progress)),
            UpdateStatus::Installing => Some("\u{f01da} Installing...".to_string()),
            #[cfg(feature = "self-update")]
            UpdateStatus::Completed {
                version,
                completed_at,
            } => {
                // Show update completion within 10 seconds
                let now = Utc::now();
                let seconds_passed = now.signed_duration_since(*completed_at).num_seconds();
                if seconds_passed < 10 {
                    Some(format!("\u{f058} Updated v{}!", version))
                } else {
                    None
                }
            }
            #[cfg(not(feature = "self-update"))]
            UpdateStatus::Completed { version, .. } => {
                Some(format!("\u{f058} Updated v{}!", version))
            }
            _ => None,
        }
    }

    /// Load update state from config directory and trigger auto-check if needed
    pub fn load() -> Self {
        #[cfg(feature = "self-update")]
        {
            let config_dir = dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("ccline");

            let state_file = config_dir.join(".update_state.json");

            let mut state = if let Ok(content) = std::fs::read_to_string(&state_file) {
                if let Ok(state) = serde_json::from_str::<UpdateState>(&content) {
                    state
                } else {
                    UpdateState {
                        current_version: env!("CARGO_PKG_VERSION").to_string(),
                        ..Default::default()
                    }
                }
            } else {
                UpdateState {
                    current_version: env!("CARGO_PKG_VERSION").to_string(),
                    ..Default::default()
                }
            };

            // Ensure current version is always accurate (binary may have been updated).
            state.current_version = env!("CARGO_PKG_VERSION").to_string();

            // Clear stale "Ready" notification when the installed version already matches/exceeds it.
            if let UpdateStatus::Ready { version, .. } = &state.status {
                if let Ok(false) = crate::updater::github::is_newer_release_version(
                    version,
                    &state.current_version,
                ) {
                    state.status = UpdateStatus::Idle;
                }
            }

            // Trigger update check if needed (interval) or if the persisted PID lock looks stale.
            let should_attempt_check = state.should_check_update()
                || (state.update_pid.is_some() && state.is_update_pid_lock_stale());

            if should_attempt_check {
                // Check if another update process is running
                let should_start_check = match state.update_pid {
                    None => true,
                    Some(pid) => {
                        let running = Self::is_process_running(pid);
                        let stale = state.is_update_pid_lock_stale();

                        // If the lock is stale or the PID no longer exists, treat it as unlocked.
                        if stale || !running {
                            state.update_pid = None;
                            true
                        } else {
                            false
                        }
                    }
                };

                if should_start_check {
                    // Perform synchronous update check for simplicity and reliability
                    use crate::updater::github::check_for_updates;

                    state.update_pid = Some(std::process::id());
                    state.last_check = Some(chrono::Utc::now());
                    let _ = state.save();

                    // Perform update check
                    match check_for_updates() {
                        Ok(Some(release)) => {
                            if release.find_asset_for_platform().is_some() {
                                // Set Ready status with timestamp, user must run --update manually
                                state.status = UpdateStatus::Ready {
                                    version: release.version(),
                                    found_at: chrono::Utc::now(),
                                };
                            } else {
                                state.status = UpdateStatus::Failed {
                                    error: "No compatible asset found".to_string(),
                                };
                            }
                            state.latest_version = Some(release.version());
                        }
                        Ok(None) => {
                            state.status = UpdateStatus::Idle;
                        }
                        Err(_) => {
                            state.status = UpdateStatus::Idle;
                        }
                    }

                    // Clear PID and save final state
                    state.update_pid = None;
                    let _ = state.save();
                }
            }

            state
        }

        #[cfg(not(feature = "self-update"))]
        UpdateState {
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            ..Default::default()
        }
    }

    /// Check if a process with given PID is still running
    #[cfg(feature = "self-update")]
    fn is_process_running(pid: u32) -> bool {
        #[cfg(unix)]
        {
            use std::process::Command;
            if let Ok(output) = Command::new("ps").arg("-p").arg(pid.to_string()).output() {
                output.status.success()
            } else {
                false
            }
        }

        #[cfg(windows)]
        {
            use std::process::Command;
            if let Ok(output) = Command::new("tasklist")
                .arg("/FI")
                .arg(format!("PID eq {}", pid))
                .output()
            {
                String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            } else {
                false
            }
        }

        #[cfg(not(any(unix, windows)))]
        false
    }

    /// Save update state to config directory
    pub fn save(&self) -> Result<(), std::io::Error> {
        #[cfg(feature = "self-update")]
        {
            let config_dir = dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("ccline");

            std::fs::create_dir_all(&config_dir)?;
            let state_file = config_dir.join(".update_state.json");

            let content = serde_json::to_string_pretty(self)?;
            std::fs::write(&state_file, content)?;
        }

        Ok(())
    }

    /// Check if update check should be triggered
    #[cfg(feature = "self-update")]
    pub fn should_check_update(&self) -> bool {
        // Don't check if already updating
        match &self.status {
            UpdateStatus::Checking
            | UpdateStatus::Downloading { .. }
            | UpdateStatus::Installing => return false,
            _ => {}
        }

        // Check time interval
        if let Some(last_check) = self.last_check {
            let now = Utc::now();
            let hours_passed = now.signed_duration_since(last_check).num_hours();
            !(0..UPDATE_CHECK_INTERVAL_HOURS).contains(&hours_passed)
        } else {
            true
        }
    }

    #[cfg(not(feature = "self-update"))]
    pub fn should_check_update(&self) -> bool {
        false
    }
}

/// GitHub Release API response structures
#[cfg(feature = "self-update")]
pub mod github {
    use std::time::Duration;

    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct GitHubRelease {
        pub tag_name: String,
        pub name: String,
        pub body: String,
        pub draft: bool,
        pub prerelease: bool,
        pub created_at: String,
        pub published_at: String,
        pub html_url: String,
        pub assets: Vec<ReleaseAsset>,
    }

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct ReleaseAsset {
        pub name: String,
        pub size: u64,
        pub download_count: u32,
        pub browser_download_url: String,
        pub content_type: String,
    }

    impl GitHubRelease {
        /// Get the version string without 'v' prefix
        pub fn version(&self) -> String {
            self.tag_name
                .strip_prefix('v')
                .unwrap_or(&self.tag_name)
                .to_string()
        }

        /// Find asset for current platform
        pub fn find_asset_for_platform(&self) -> Option<&ReleaseAsset> {
            let platform_suffix = get_platform_asset_name();
            self.assets
                .iter()
                .find(|asset| asset.name.contains(&platform_suffix))
        }
    }

    fn parse_release_version_for_compare(raw: &str) -> Option<[u64; 4]> {
        let s = raw.trim().trim_start_matches('v');

        // Accept "X.Y.Z-N" where N is a numeric revision used by this repo's release tags/npm.
        if let Some((base, rev)) = s.split_once('-') {
            if !rev.is_empty() && rev.chars().all(|c| c.is_ascii_digit()) {
                let base_parts: Vec<&str> = base.split('.').collect();
                if base_parts.len() == 3
                    && base_parts
                        .iter()
                        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
                {
                    return Some([
                        base_parts[0].parse().ok()?,
                        base_parts[1].parse().ok()?,
                        base_parts[2].parse().ok()?,
                        rev.parse().ok()?,
                    ]);
                }
            }
        }

        // Accept "X.Y.Z" or "X.Y.Z.W"
        let parts: Vec<&str> = s.split('.').collect();
        if !(parts.len() == 3 || parts.len() == 4) {
            return None;
        }

        let mut out = [0u64; 4];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            out[i] = p.parse().ok()?;
        }
        Some(out)
    }

    /// Compare project versions.
    ///
    /// This repo historically uses 4-part tags like `v1.0.9.4` (and npm-normalized `1.0.9-4`).
    /// These are not SemVer. We treat the 4th component as a monotonically increasing revision.
    pub fn is_newer_release_version(
        latest: &str,
        current: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if let (Some(latest_v), Some(current_v)) = (
            parse_release_version_for_compare(latest),
            parse_release_version_for_compare(current),
        ) {
            return Ok(latest_v > current_v);
        }

        let current = semver::Version::parse(current)?;
        let latest = semver::Version::parse(latest)?;
        Ok(latest > current)
    }

    /// Get the expected asset name suffix for current platform
    fn get_platform_asset_name() -> String {
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        return "windows-x64.zip".to_string();

        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        return "macos-x64.tar.gz".to_string();

        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        return "macos-arm64.tar.gz".to_string();

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            // glibc 2.35 is the watershed - use static for older systems
            if should_use_static_binary() {
                "linux-x64-static.tar.gz".to_string()
            } else {
                "linux-x64.tar.gz".to_string()
            }
        }

        #[cfg(not(any(
            all(target_os = "windows", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        )))]
        return "unknown".to_string();
    }

    /// Determine if we should use static binary based on glibc version
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn should_use_static_binary() -> bool {
        use std::process::Command;

        // Try to get glibc version
        if let Ok(output) = Command::new("ldd").arg("--version").output() {
            let version_output = String::from_utf8_lossy(&output.stdout);

            // Parse glibc version (format: "ldd (GNU libc) 2.35")
            for line in version_output.lines() {
                if line.contains("GNU libc") || line.contains("GLIBC") {
                    if let Some(version_part) = line.split_whitespace().last() {
                        if let Some((major, minor)) = parse_version(version_part) {
                            // Use dynamic binary if glibc >= 2.35, otherwise use static
                            return major < 2 || (major == 2 && minor < 35);
                        }
                    }
                    break;
                }
            }
        }

        // Default to static if we can't determine glibc version
        true
    }

    /// Parse version string like "2.35" into (major, minor)
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn parse_version(version: &str) -> Option<(u32, u32)> {
        let parts: Vec<&str> = version.split('.').collect();
        if parts.len() >= 2 {
            if let (Ok(major), Ok(minor)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return Some((major, minor));
            }
        }
        None
    }

    /// Check for updates from GitHub Releases API
    pub fn check_for_updates() -> Result<Option<GitHubRelease>, Box<dyn std::error::Error>> {
        let url = "https://api.github.com/repos/Haleclipse/CCometixLine/releases/latest";

        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build();

        let response = agent
            .get(url)
            .set(
                "User-Agent",
                &format!("CCometixLine/{}", env!("CARGO_PKG_VERSION")),
            )
            .call()?;

        if response.status() == 200 {
            let release: GitHubRelease = response.into_json()?;

            let current_version = env!("CARGO_PKG_VERSION");
            let latest_version = release.version();

            if is_newer_release_version(&latest_version, current_version)? {
                Ok(Some(release))
            } else {
                Ok(None)
            }
        } else {
            Err(format!("HTTP {}: {}", response.status(), response.status_text()).into())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_release_version_for_compare_supports_numeric() {
            assert_eq!(
                parse_release_version_for_compare("1.0.9"),
                Some([1, 0, 9, 0])
            );
            assert_eq!(
                parse_release_version_for_compare("v1.0.9.4"),
                Some([1, 0, 9, 4])
            );
        }

        #[test]
        fn parse_release_version_for_compare_supports_dash_revision() {
            assert_eq!(
                parse_release_version_for_compare("1.0.9-4"),
                Some([1, 0, 9, 4])
            );
        }

        #[test]
        fn is_newer_release_version_handles_revision_scheme() {
            assert!(is_newer_release_version("1.0.9.4", "1.0.9").unwrap());
            assert!(is_newer_release_version("1.0.9-4", "1.0.9-3").unwrap());
            assert!(is_newer_release_version("1.1.0", "1.0.9").unwrap());
            assert!(!is_newer_release_version("1.0.9", "1.0.9.4").unwrap());
        }
    }
}
