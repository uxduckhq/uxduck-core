//! Wait outside the engine for a styled window, including a reused instance.
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

pub(super) async fn wait(
    receipt: &Path,
    mut exited: impl FnMut() -> Result<Option<ExitStatus>, String>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match std::fs::read(receipt) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("The desktop app sent an invalid startup result: {e}"))?;
                if value["ready"] == true {
                    return Ok(());
                }
                return Err(value["error"]
                    .as_str()
                    .unwrap_or("The desktop app did not confirm its window opened.")
                    .to_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Could not read the desktop startup result: {error}"
                ));
            }
        }
        // A duplicate Electron process exits successfully before the original
        // instance has shown its window. Its exit alone is never confirmation.
        if let Some(status) = exited()?
            && !status.success()
        {
            return Err(format!(
                "The desktop app exited before opening a window ({status})."
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "The desktop app did not open a window within {} seconds.",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waits_for_visibility_and_preserves_the_shell_error() {
        let tmp = tempfile::tempdir().unwrap();
        let receipt = tmp.path().join("result.json");
        std::fs::write(&receipt, r#"{"ready":true}"#).unwrap();
        assert!(
            wait(&receipt, || panic!("already visible"), Duration::ZERO)
                .await
                .is_ok()
        );
        std::fs::write(&receipt, r#"{"error":"Renderer crashed"}"#).unwrap();
        assert_eq!(
            wait(&receipt, || Ok(None), Duration::ZERO)
                .await
                .unwrap_err(),
            "Renderer crashed"
        );
        std::fs::write(&receipt, "invalid").unwrap();
        assert!(
            wait(&receipt, || Ok(None), Duration::ZERO)
                .await
                .unwrap_err()
                .contains("invalid startup result")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn catches_early_crashes_and_never_mistakes_a_clean_exit_for_a_window() {
        let tmp = tempfile::tempdir().unwrap();
        let receipt = tmp.path().join("result.json");
        let failed = std::process::Command::new("sh")
            .args(["-c", "exit 17"])
            .status()
            .unwrap();
        assert!(
            wait(&receipt, || Ok(Some(failed)), Duration::ZERO)
                .await
                .unwrap_err()
                .contains("exited before opening")
        );
        let clean = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .unwrap();
        assert!(
            wait(&receipt, || Ok(Some(clean)), Duration::ZERO)
                .await
                .unwrap_err()
                .contains("did not open a window")
        );
        // A second instance exits first; its existing window confirms later.
        let send = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            std::fs::write(&receipt, r#"{"ready":true}"#).unwrap();
        };
        let (result, ()) = tokio::join!(
            wait(&receipt, || Ok(Some(clean)), Duration::from_secs(1)),
            send
        );
        assert!(result.is_ok());
    }
}
