use std::path::PathBuf;

use crate::error::Result;

const GITHUB_REPO: &str = "Epiphytic/captain-hook";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the `self-update` subcommand.
/// If `check_only` is true, just check for updates without installing.
pub async fn run(check_only: bool) -> Result<()> {
    let latest = fetch_latest_version().await?;
    let latest_tag = latest.trim_start_matches('v');

    if latest_tag == CURRENT_VERSION {
        println!("captain-hook {} is up to date.", CURRENT_VERSION);
        return Ok(());
    }

    println!(
        "captain-hook: update available {} -> {}",
        CURRENT_VERSION, latest_tag
    );

    if check_only {
        println!("Run `captain-hook self-update` to install.");
        return Ok(());
    }

    // Determine platform
    let target = detect_target()?;
    let archive_name = format!("captain-hook-v{}-{}.tar.gz", latest_tag, target);
    let sha_name = format!("{}.sha256", archive_name);

    let base_url = format!(
        "https://github.com/{}/releases/download/v{}",
        GITHUB_REPO, latest_tag
    );

    let client = reqwest::Client::new();

    // Download archive
    println!("Downloading {}...", archive_name);
    let archive_url = format!("{}/{}", base_url, archive_name);
    let archive_bytes = client
        .get(&archive_url)
        .send()
        .await
        .map_err(|e| io_err(format!("Download failed: {}", e)))?
        .error_for_status()
        .map_err(|e| io_err(format!("Download failed: {}", e)))?
        .bytes()
        .await
        .map_err(|e| io_err(format!("Download failed: {}", e)))?;

    // Download checksum
    let sha_url = format!("{}/{}", base_url, sha_name);
    let sha_text = client
        .get(&sha_url)
        .send()
        .await
        .map_err(|e| io_err(format!("Checksum download failed: {}", e)))?
        .error_for_status()
        .map_err(|e| io_err(format!("Checksum download failed: {}", e)))?
        .text()
        .await
        .map_err(|e| io_err(format!("Checksum download failed: {}", e)))?;

    // Verify SHA-256
    let expected_hash = sha_text
        .split_whitespace()
        .next()
        .ok_or_else(|| io_err("Invalid checksum file format".into()))?;

    use sha2::{Digest, Sha256};
    let actual_hash = format!("{:x}", Sha256::digest(&archive_bytes));

    if actual_hash != expected_hash {
        return Err(io_err(format!(
            "Checksum mismatch: expected {}, got {}",
            expected_hash, actual_hash
        )));
    }
    println!("Checksum verified.");

    // Extract binary from tar.gz
    let decoder = flate2::read::GzDecoder::new(&archive_bytes[..]);
    let mut archive = tar::Archive::new(decoder);

    let tmp_dir =
        tempfile::tempdir().map_err(|e| io_err(format!("Failed to create temp dir: {}", e)))?;
    archive
        .unpack(tmp_dir.path())
        .map_err(|e| io_err(format!("Failed to extract archive: {}", e)))?;

    let extracted_binary = tmp_dir.path().join("captain-hook");
    if !extracted_binary.exists() {
        return Err(io_err("Binary not found in archive".into()));
    }

    // Find current binary location
    let current_exe = std::env::current_exe()
        .map_err(|e| io_err(format!("Failed to determine current binary path: {}", e)))?;

    // Replace binary
    println!("Installing to {}...", current_exe.display());
    let backup = current_exe.with_extension("old");

    // Move current to backup, copy new, remove backup
    if current_exe.exists() {
        std::fs::rename(&current_exe, &backup)
            .map_err(|e| io_err(format!("Failed to create backup: {}", e)))?;
    }

    match std::fs::copy(&extracted_binary, &current_exe) {
        Ok(_) => {
            // Set executable permission
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&current_exe, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| io_err(format!("Failed to set permissions: {}", e)))?;
            }

            // Remove backup
            let _ = std::fs::remove_file(&backup);
            println!("captain-hook updated to v{}.", latest_tag);
        }
        Err(e) => {
            // Restore backup on failure
            if backup.exists() {
                let _ = std::fs::rename(&backup, &current_exe);
            }
            return Err(io_err(format!("Failed to install binary: {}", e)));
        }
    }

    Ok(())
}

/// Check for updates periodically (once per day) and print a stderr warning.
/// Called from the hot path (check subcommand). Non-blocking.
pub fn check_update_hint() {
    let config_dir = crate::config::dirs_global();
    let check_file = config_dir.join("update-check.json");

    // Read last check timestamp
    if let Ok(contents) = std::fs::read_to_string(&check_file) {
        if let Ok(last_check) = serde_json::from_str::<UpdateCheck>(&contents) {
            let elapsed = chrono::Utc::now()
                .signed_duration_since(last_check.checked_at)
                .num_hours();
            if elapsed < 24 {
                // Already checked recently, and we may have a cached result
                if let Some(ref latest) = last_check.latest_version {
                    let latest_tag = latest.trim_start_matches('v');
                    if latest_tag != CURRENT_VERSION {
                        eprintln!(
                            "captain-hook: update available v{} -> v{} (run `captain-hook self-update`)",
                            CURRENT_VERSION, latest_tag
                        );
                    }
                }
                return;
            }
        }
    }

    // Spawn a background task to check (non-blocking)
    let check_file = check_file.clone();
    tokio::spawn(async move {
        if let Ok(latest) = fetch_latest_version().await {
            let check = UpdateCheck {
                checked_at: chrono::Utc::now(),
                latest_version: Some(latest.clone()),
                current_version: CURRENT_VERSION.to_string(),
            };
            let _ = std::fs::create_dir_all(check_file.parent().unwrap_or(&PathBuf::from(".")));
            let _ = std::fs::write(
                &check_file,
                serde_json::to_string(&check).unwrap_or_default(),
            );

            let latest_tag = latest.trim_start_matches('v');
            if latest_tag != CURRENT_VERSION {
                eprintln!(
                    "captain-hook: update available v{} -> v{} (run `captain-hook self-update`)",
                    CURRENT_VERSION, latest_tag
                );
            }
        }
    });
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UpdateCheck {
    checked_at: chrono::DateTime<chrono::Utc>,
    latest_version: Option<String>,
    current_version: String,
}

async fn fetch_latest_version() -> std::result::Result<String, crate::error::CaptainHookError> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    let client = reqwest::Client::builder()
        .user_agent("captain-hook-updater")
        .build()
        .map_err(|e| io_err(format!("HTTP client error: {}", e)))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| io_err(format!("GitHub API request failed: {}", e)))?
        .error_for_status()
        .map_err(|e| io_err(format!("GitHub API error: {}", e)))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| io_err(format!("Failed to parse GitHub API response: {}", e)))?;

    body["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| io_err("No tag_name in GitHub release".into()))
}

fn detect_target() -> std::result::Result<&'static str, crate::error::CaptainHookError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok("aarch64-apple-darwin")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
    )))]
    {
        Err(io_err(
            "Unsupported platform for self-update. Use `cargo install captain-hook` instead."
                .into(),
        ))
    }
}

fn io_err(msg: String) -> crate::error::CaptainHookError {
    crate::error::CaptainHookError::Io(std::io::Error::other(msg))
}
