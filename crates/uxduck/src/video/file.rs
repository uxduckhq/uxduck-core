//! File cameras use the packaged, network-disabled helper and ordinary video hub.
use super::{capture, hub::Hub, listener};
use crate::daemon::engine_task::EngineHandle;
use std::{
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;
use uxduck_core::domain::video::VideoFileInput;
use uxduck_media::source::Source;

pub fn check(path: &str) -> Result<(), String> {
    capture::program()?;
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("Choose an absolute video path on the computer running UXDuck.".into());
    }
    if !matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "m4v" | "mov" | "ts")
    ) {
        return Err("Choose an MP4 video with H.264 video, or a MOV, M4V or MPEG-TS file supported by UXDuck's media helper.".into());
    }
    let meta = path.metadata().map_err(|e| {
        format!("This video is unavailable: {e}. Locate the file to play it again.")
    })?;
    if !meta.is_file() || meta.len() == 0 {
        return Err("Choose a nonempty video file.".into());
    }
    Ok(())
}

fn args(input: &VideoFileInput) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-nostdin", "-re"]
        .map(Into::into)
        .into();
    if input.repeat {
        args.extend(["-stream_loop".into(), "-1".into()]);
    }
    // File paths are argv values, never shell text or input URLs. The helper has no network support.
    args.extend(["-i".into(), input.path.clone()]);
    if input.demo_id.is_some() {
        // The catalogue preparation makes H.264 without B frames. Relay its
        // compressed pictures without spending CPU decoding or encoding them.
        args.extend(
            [
                "-map",
                "0:v:0",
                "-an",
                "-c:v",
                "copy",
                "-bsf:v",
                "h264_mp4toannexb",
                "-flush_packets",
                "1",
                "-muxdelay",
                "0",
                "-muxpreload",
                "0",
                "-mpegts_flags",
                "+resend_headers",
                "-f",
                "mpegts",
                "pipe:1",
            ]
            .map(Into::into),
        );
        return args;
    }
    args.extend(
        [
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-sn",
            "-dn",
            "-c:a",
            "aac",
            "-threads:a",
            "1",
        ]
        .map(Into::into),
    );
    args.extend(capture::encoding_args());
    args
}

pub fn spawn(
    runtime: &tokio::runtime::Handle,
    id: String,
    input: VideoFileInput,
    epoch: u64,
    hub: Arc<Hub>,
    engine: EngineHandle,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let result = run(&id, &input, epoch, &hub, &engine).await;
        engine.post(move |e| e.file_stopped(&id, epoch, result.unwrap_or_else(|why| why)));
    })
}

async fn run(
    id: &str,
    input: &VideoFileInput,
    epoch: u64,
    hub: &Hub,
    engine: &EngineHandle,
) -> Result<String, String> {
    check(&input.path)?;
    let program = capture::program()?;
    let mut child = capture::command(&program)
        .args(args(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("The media helper could not start: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("The media helper has no video pipe")?;
    let diagnostics = tokio::spawn(capture::tail(
        child
            .stderr
            .take()
            .ok_or("The media helper has no diagnostic pipe")?,
    ));
    let mut parser = Source::new();
    let mut buffer = vec![0; 188 * 128];
    let mut pending = Vec::new();
    let mut reported = Instant::now();
    let mut before = (0, 0);
    let mut last_picture = Instant::now();
    let mut pictures = 0;
    let failure = loop {
        let limit = if pictures == 0 { 15 } else { 8 };
        let remaining = Duration::from_secs(limit).saturating_sub(last_picture.elapsed());
        let n = match tokio::time::timeout(remaining, stdout.read(&mut buffer)).await {
            Ok(Ok(0)) => break None,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => break Some(format!("The video file pipe closed: {e}")),
            Err(_) => break Some("No pictures arrived from this file. Choose an MP4 with H.264 video, or rebuild the media helper with pnpm capture:build.".into()),
        };
        pending.extend_from_slice(&buffer[..n]);
        let whole = pending.len() / 188 * 188;
        if whole == 0 {
            continue;
        }
        hub.publish_ts(id, &pending[..whole]);
        for output in parser.push(&pending[..whole]) {
            hub.publish(id, &listener::item(output));
        }
        pending.drain(..whole);
        let status = parser.status();
        if status.counters.pictures > pictures {
            last_picture = Instant::now();
        }
        if status.counters.pictures > 0
            && (pictures == 0 || reported.elapsed() >= Duration::from_secs(1))
        {
            let mut report = listener::report(
                0,
                &std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                &status,
                reported.elapsed().as_secs_f64().max(0.001),
                before.0,
                before.1,
            );
            report.id = Some(id.to_owned());
            report.sender = "recorded video".into();
            before = (status.counters.bytes, status.counters.pictures);
            reported = Instant::now();
            let config = input.clone();
            engine.post(move |e| e.file_observed(epoch, &config, report));
        }
        pictures = status.counters.pictures;
    };
    if failure.is_some() {
        let _ = child.kill().await;
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let diagnostics = diagnostics.await.unwrap_or_default();
    if let Some(why) = failure {
        return Err(why);
    }
    if !status.success() || pictures == 0 {
        return Err(format!(
            "This file could not be played. Choose an MP4 with H.264 video. {}",
            diagnostics.trim()
        ));
    }
    Ok("Recorded video ended. Enable Loop video or choose its file again to replay.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_are_single_arguments_and_loop_is_explicit() {
        let mut input = VideoFileInput {
            path: "/tmp/flight $(touch bad) ' movie.mp4".into(),
            name: "Flight".into(),
            repeat: true,
            demo_id: None,
        };
        let looping = args(&input);
        assert_eq!(looping.iter().filter(|a| *a == &input.path).count(), 1);
        assert!(looping.windows(2).any(|w| w == ["-stream_loop", "-1"]));
        assert!(looping.windows(2).any(|w| w == ["-re", "-stream_loop"]));
        input.repeat = false;
        assert!(!args(&input).iter().any(|a| a == "-stream_loop"));
    }
}
