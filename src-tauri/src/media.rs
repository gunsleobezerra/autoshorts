use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::hardware;
use crate::models::MediaProbe;

pub fn command_exists(name: &str) -> bool {
    Command::new(name).arg("-version").output().is_ok()
}

/// Parse a `-progress pipe:1` key/value pair into a position in seconds.
///
/// ffmpeg reports `out_time_us` and `out_time_ms`; despite the name, both are
/// expressed in microseconds on current builds, so both are divided by 1e6.
fn parse_ffmpeg_progress_time(key: &str, value: &str) -> Option<f64> {
    match key {
        "out_time_us" | "out_time_ms" => value.trim().parse::<f64>().ok().map(|us| us / 1_000_000.0),
        "out_time" => parse_clock_timestamp(value.trim()),
        _ => None,
    }
}

fn parse_clock_timestamp(token: &str) -> Option<f64> {
    let mut parts = token.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// Run ffmpeg while streaming `-progress pipe:1` output into `on_progress`.
///
/// The callback is borrowed so a caller can retry the same job with a different
/// encoder (GPU first, CPU on failure) while keeping its progress reporting.
fn run_ffmpeg_with_progress(
    args: &[String],
    total_duration: Option<f64>,
    working_dir: Option<&Path>,
    on_progress: &mut dyn FnMut(f64),
) -> Result<()> {
    let mut command = Command::new("ffmpeg");
    command.args(["-progress", "pipe:1", "-nostats"]).args(args);
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ffmpeg")?;

    // Drain stderr on a helper thread so a full pipe never deadlocks the render.
    let stderr_handle = {
        let mut stderr = child.stderr.take().context("capturing ffmpeg stderr")?;
        std::thread::spawn(move || {
            let mut buffer = String::new();
            let _ = stderr.read_to_string(&mut buffer);
            buffer
        })
    };

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        let mut last_percentage = 0.0_f64;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key == "progress" && value.trim() == "end" {
                if last_percentage < 100.0 {
                    last_percentage = 100.0;
                    on_progress(100.0);
                }
                continue;
            }
            let Some(total) = total_duration else { continue };
            if total <= 0.0 {
                continue;
            }
            if let Some(position) = parse_ffmpeg_progress_time(key, value) {
                let percentage = (position / total * 100.0).clamp(0.0, 99.0);
                if percentage > last_percentage + 0.5 {
                    last_percentage = percentage;
                    on_progress(percentage);
                }
            }
        }
    }

    let status = child.wait().context("waiting for ffmpeg")?;
    let stderr = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        return Err(anyhow!("ffmpeg failed: {}", stderr.trim()));
    }

    on_progress(100.0);
    Ok(())
}

pub fn probe_media(path: &str) -> Result<MediaProbe> {
    if !command_exists("ffprobe") {
        return Err(anyhow!("ffprobe is not installed or not available on PATH"));
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            path,
        ])
        .output()
        .context("running ffprobe")?;

    if !output.status.success() {
        return Err(anyhow!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let json: Value = serde_json::from_slice(&output.stdout).context("parsing ffprobe JSON")?;
    let streams = json
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let video = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("video"));
    let audio = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("audio"));

    let duration_sec = json
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(Value::as_str)
        .and_then(|duration| duration.parse::<f64>().ok());

    Ok(MediaProbe {
        duration_sec,
        has_video: video.is_some(),
        width: video
            .and_then(|stream| stream.get("width"))
            .and_then(Value::as_i64),
        height: video
            .and_then(|stream| stream.get("height"))
            .and_then(Value::as_i64),
        video_codec: video
            .and_then(|stream| stream.get("codec_name"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        audio_codec: audio
            .and_then(|stream| stream.get("codec_name"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

/// Extract a 16 kHz mono WAV for transcription, reporting 0..=100 progress.
pub fn extract_audio(
    source_path: &str,
    project_dir: &Path,
    mut on_progress: impl FnMut(f64),
) -> Result<PathBuf> {
    if !command_exists("ffmpeg") {
        return Err(anyhow!("ffmpeg is not installed or not available on PATH"));
    }

    std::fs::create_dir_all(project_dir)?;
    let output_path = project_dir.join("transcription_audio.wav");

    let total_duration = probe_media(source_path).ok().and_then(|probe| probe.duration_sec);

    let args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        source_path.to_string(),
        "-vn".into(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "16000".into(),
        output_path.to_string_lossy().to_string(),
    ];

    run_ffmpeg_with_progress(&args, total_duration, None, &mut on_progress)?;
    Ok(output_path)
}

/// Options for rendering a single vertical clip.
pub struct ClipRenderRequest<'a> {
    pub source_path: &'a str,
    pub start_sec: f64,
    pub end_sec: f64,
    pub output_path: &'a Path,
    /// Whether the source has a video stream (probed once by the caller).
    pub has_video: bool,
    /// Optional ASS subtitle file burned into the video.
    pub ass_path: Option<&'a Path>,
}

/// Output resolution for every clip. Normalizing here keeps the encoder from
/// working on 4K frames when the source is larger than a phone screen.
const CLIP_WIDTH: i64 = 1080;
const CLIP_HEIGHT: i64 = 1920;

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Build the `ass` filter argument for a subtitle file.
///
/// The file is referenced by its bare name and ffmpeg's working directory is
/// set to the file's folder, which avoids `:`/`\` path escaping problems on
/// every platform (Windows drive letters included).
fn ass_filter_arg(ass_path: &Path) -> Option<String> {
    let name = ass_path.file_name()?.to_string_lossy().to_string();
    if name.is_empty() {
        return None;
    }
    let escaped: String = name
        .chars()
        .map(|c| match c {
            ':' | '\'' | ',' | '[' | ']' | ';' | '\\' => format!("\\{c}"),
            other => other.to_string(),
        })
        .collect();
    Some(format!("ass=filename={escaped}"))
}

/// Render a 9:16 clip, reporting clip-level 0..=100 progress.
pub fn render_flat_clip(
    request: &ClipRenderRequest<'_>,
    mut on_progress: impl FnMut(f64),
) -> Result<PathBuf> {
    if !command_exists("ffmpeg") {
        return Err(anyhow!("ffmpeg is not installed or not available on PATH"));
    }

    let output_path = absolute_path(request.output_path);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let duration_sec = (request.end_sec - request.start_sec).max(0.1);

    // Normalize the ffmpeg progress stream (absolute media time) to the
    // 0..=100 window of this specific clip. `-ss` seeks before input
    // decoding, so reported positions already start near zero here.
    let mut on_clip_progress = move |percentage: f64| on_progress(percentage.clamp(0.0, 100.0));

    // Use the GPU encoder when one was detected *and* validated at startup.
    // A GPU that disappears (driver update, eGPU unplug, WSL passthrough lost)
    // demotes the whole process to libx264 instead of failing the batch.
    let encoder = hardware::active_video_encoder();
    match run_clip_ffmpeg(
        request,
        encoder,
        &output_path,
        duration_sec,
        &mut on_clip_progress,
    ) {
        Ok(()) => Ok(output_path),
        Err(error) if encoder.is_hardware() => {
            eprintln!(
                "[autoshorts] {} render failed ({}); retrying with libx264",
                encoder.id(),
                error
            );
            hardware::mark_video_encoder_failed(encoder);
            run_clip_ffmpeg(
                request,
                hardware::VideoEncoder::Libx264,
                &output_path,
                duration_sec,
                &mut on_clip_progress,
            )?;
            Ok(output_path)
        }
        Err(error) => Err(error),
    }
}

/// Build and run the ffmpeg command for one clip with the given encoder.
fn run_clip_ffmpeg(
    request: &ClipRenderRequest<'_>,
    encoder: hardware::VideoEncoder,
    output_path: &Path,
    duration_sec: f64,
    on_progress: &mut dyn FnMut(f64),
) -> Result<()> {
    let plan = hardware::plan(encoder);

    let source_path = absolute_path(Path::new(request.source_path));
    let source_arg = source_path.to_string_lossy().to_string();
    let output_arg = output_path.to_string_lossy().to_string();
    let start = format!("{:.3}", request.start_sec);
    let duration = format!("{duration_sec:.3}");

    let mut cmd = Command::new("ffmpeg");
    cmd.args(&plan.pre_input);
    cmd.args(["-y", "-ss", &start, "-i", &source_arg, "-t", &duration]);

    let mut ass_dir: Option<PathBuf> = None;

    if request.has_video {
        let mut filters = vec![
            "crop=w='2*trunc(min(iw,ih*9/16)/2)':h='2*trunc(min(ih,iw*16/9)/2)'".to_string(),
            format!("scale={CLIP_WIDTH}:{CLIP_HEIGHT}:flags=fast_bilinear"),
        ];

        if let Some(ass_path) = request.ass_path {
            let ass_path = absolute_path(ass_path);
            if let Some(parent) = ass_path.parent() {
                ass_dir = Some(parent.to_path_buf());
            }
            if let Some(filter) = ass_filter_arg(&ass_path) {
                filters.push(filter);
            }
        }

        let mut filter_chain = filters.join(",");
        // VA-API needs frames uploaded to the device before encoding.
        filter_chain.push_str(plan.filter_suffix);

        cmd.args(["-vf", &filter_chain]);
        cmd.args(&plan.codec);
    } else {
        cmd.arg("-vn");
    }

    cmd.args(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"]);
    cmd.arg(&output_arg);

    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();

    // `-progress` output is absolute media time, but `-ss` seeks before input
    // decoding, so reported positions already start near zero for this clip.
    run_ffmpeg_with_progress(&args, Some(duration_sec), ass_dir.as_deref(), on_progress)
}
