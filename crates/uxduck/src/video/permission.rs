//! Explicit selection requests access as UXDuck.app. Restored capture only checks.
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    NotDetermined,
    Restricted,
    Denied,
    Granted,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Access {
    pub camera: Status,
    pub microphone: Status,
}

impl Access {
    pub fn camera_error(&self) -> Option<String> {
        match self.camera {
            Status::Granted => None,
            Status::NotDetermined => Some("Camera permission has not been requested. Choose Try camera again to let macOS ask for access.".into()),
            Status::Restricted => Some("Camera access is restricted on this Mac. Check System Settings → Privacy & Security → Camera, or ask this Mac's administrator.".into()),
            Status::Denied => Some("Camera access is off for UXDuck. Allow UXDuck in System Settings → Privacy & Security → Camera, then choose Try camera again.".into()),
            Status::Unavailable => Some("macOS could not determine camera access. Reopen UXDuck and try the camera again.".into()),
        }
    }
    pub fn microphone_note(&self) -> Option<String> {
        (self.microphone != Status::Granted).then(|| {
            "Microphone access is off or restricted for UXDuck. Video continues without sound. Check System Settings → Privacy & Security → Microphone, then choose Try camera again.".into()
        })
    }
}

type Answer = Result<Access, String>;
pub type Receiver = watch::Receiver<Option<Answer>>;

fn checked_answer(access: Access, requested: bool) -> Answer {
    if requested && access.camera == Status::NotDetermined {
        // A request was attempted. Do not describe a TCC/app-policy failure
        // as though the person simply hasn't opened the permission flow yet.
        return Err("macOS did not complete UXDuck's camera permission request. Reopen UXDuck and try again. If no permission question appears, reinstall UXDuck.".into());
    }
    Ok(access)
}

pub fn required() -> bool {
    cfg!(target_os = "macos") || std::env::var_os("UXDUCK_CAPTURE_PERMISSIONS").is_some()
}

/// Owned by the selected source, independently of whether it has a viewer yet.
/// Removing/disabling/replacing it cancels the request and cannot start capture.
#[derive(Debug)]
pub struct Selection {
    pub answer: Receiver,
    task: tokio::task::JoinHandle<()>,
}
impl Selection {
    pub fn start(runtime: &tokio::runtime::Handle, audio: bool) -> Self {
        let (sender, answer) = watch::channel(None);
        let task = runtime.spawn(async move {
            let _ = sender.send(Some(query(true, audio).await));
        });
        Self { answer, task }
    }
}
impl Drop for Selection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn selected(answer: &mut Receiver) -> Answer {
    loop {
        if let Some(result) = answer.borrow_and_update().clone() {
            return result;
        }
        answer.changed().await.map_err(|_| {
            "Camera permission request was cancelled. Choose the camera again.".to_owned()
        })?;
    }
}

/// The override is a process-only test seam, like `UXDUCK_CAPTURE_FFMPEG`. Normal
/// macOS builds use the outer app's own executable and never Electron's identity.
pub async fn query(request: bool, audio: bool) -> Answer {
    let fixture = std::env::var_os("UXDUCK_CAPTURE_PERMISSIONS");
    if !cfg!(target_os = "macos") && fixture.is_none() {
        return Ok(Access {
            camera: Status::Granted,
            microphone: Status::Granted,
        });
    }
    let binary = match fixture {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            #[cfg(target_os = "macos")]
            {
                crate::tray::capture_permission_binary().map_err(|why| why.to_string())?
            }
            #[cfg(not(target_os = "macos"))]
            {
                unreachable!("non-macOS permission requests use an explicit fixture")
            }
        }
    };
    let mut command = tokio::process::Command::new(binary);
    command.args([
        "--capture-permission",
        if request { "request" } else { "status" },
    ]);
    if audio {
        command.arg("--capture-audio");
    }
    command
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(if request { 300 } else { 15 }), command.output())
        .await.map_err(|_| "Camera permission did not receive an answer. Choose Try camera again and respond to the macOS permission question.".to_owned())?
        .map_err(|why| format!("Could not check camera permission: {why}. Rebuild uxduck-tray or reinstall UXDuck."))?;
    if !output.status.success() {
        return Err(format!(
            "Could not check camera permission: {}. Rebuild uxduck-tray or reinstall UXDuck.",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let access = serde_json::from_slice(&output.stdout).map_err(|why| format!("The camera permission helper returned an invalid answer: {why}. Rebuild uxduck-tray or reinstall UXDuck."))?;
    checked_answer(access, request)
}

pub fn settings(microphone: bool) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err(
            "Open camera and microphone privacy settings on the computer running UXDuck.".into(),
        );
    }
    let address = if microphone {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
    } else {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera"
    };
    std::process::Command::new("/usr/bin/open")
        .arg(address)
        .spawn()
        .map(|_| ())
        .map_err(|why| format!("Could not open System Settings: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denial_restriction_and_first_use_have_actionable_messages() {
        for status in [Status::NotDetermined, Status::Denied, Status::Restricted] {
            let access = Access {
                camera: status,
                microphone: status,
            };
            assert!(access.camera_error().is_some());
            assert!(
                access
                    .microphone_note()
                    .unwrap()
                    .contains("Video continues")
            );
        }
        let granted = Access {
            camera: Status::Granted,
            microphone: Status::Granted,
        };
        assert!(granted.camera_error().is_none());
        assert!(granted.microphone_note().is_none());
    }
    #[test]
    fn an_unanswered_request_is_not_reported_as_never_requested() {
        let access = || Access {
            camera: Status::NotDetermined,
            microphone: Status::NotDetermined,
        };
        assert!(checked_answer(access(), false).is_ok());
        let error = checked_answer(access(), true).unwrap_err();
        assert!(error.contains("did not complete"));
        assert!(!error.contains("has not been requested"));
        for camera in [Status::Granted, Status::Denied, Status::Restricted] {
            let result = checked_answer(
                Access {
                    camera,
                    microphone: Status::NotDetermined,
                },
                true,
            )
            .unwrap();
            assert_eq!(result.camera, camera);
        }
    }

    #[tokio::test]
    async fn selection_waits_for_an_answer_and_cancellation_is_not_grant() {
        let (sender, mut answer) = watch::channel(None);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), selected(&mut answer))
                .await
                .is_err()
        );
        sender
            .send(Some(Ok(Access {
                camera: Status::Granted,
                microphone: Status::Denied,
            })))
            .unwrap();
        let access = selected(&mut answer).await.unwrap();
        assert!(access.camera_error().is_none());
        assert!(access.microphone_note().is_some());
        let (sender, mut answer) = watch::channel(None);
        drop(sender);
        assert!(
            selected(&mut answer)
                .await
                .unwrap_err()
                .contains("cancelled")
        );
    }
}
