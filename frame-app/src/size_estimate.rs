//! Runs output size estimates with the same `FFmpeg` arguments as a real conversion.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::Duration,
};

use frame_core::{
    args::{build_ffmpeg_args, build_output_path, validate_task_input},
    error::ConversionError,
    media_rules::{container_supports_audio, is_audio_only_container},
    size_estimate::{
        FULL_ENCODE_MAX_SECONDS, OutputRange, SampleMeasurement, SampleSection, SizeEstimate,
        SizeEstimateMethod, complexity_scan_args, config_for_section, estimate_from_samples,
        fixed_bitrate_estimate, output_range, plan_sample_sections, source_packet_args,
        stream_copy_estimate,
    },
    types::{ConversionConfig, ProbeMetadata},
};

use crate::{
    conversion_runner::probe_media_file,
    runtime_binaries::{ffmpeg_executable, ffprobe_executable},
};

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(40);
const SCAN_UNITS_PER_SECTION: u64 = 1_000;

static NEXT_WORK_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeEstimateProgress {
    Preparing,
    EncodingFull,
    ScanningSource,
    EncodingSample { current: u32, total: u32 },
}

/// Holds the test encodes and removes them when the estimate ends.
struct WorkDirectory {
    path: PathBuf,
}

impl WorkDirectory {
    fn create() -> Result<Self, ConversionError> {
        let sequence = NEXT_WORK_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "frame-size-estimate-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn output_path(&self, name: &str, container: &str) -> String {
        build_output_path(&self.path.to_string_lossy(), container, Some(name))
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Estimates the output size of a conversion. Returns `Ok(None)` when cancelled.
///
/// # Errors
///
/// Returns [`ConversionError`] when the settings are invalid or a tool fails.
pub fn run_size_estimate(
    file_path: &str,
    config: &ConversionConfig,
    cancel: &AtomicBool,
    report: &mut impl FnMut(SizeEstimateProgress),
) -> Result<Option<SizeEstimate>, ConversionError> {
    report(SizeEstimateProgress::Preparing);
    validate_task_input(file_path, config)?;
    let probe = probe_media_file(file_path)?;
    let work = WorkDirectory::create()?;

    if probe.media_kind == "image" {
        return estimate_full_encode(file_path, config, &probe, &work, cancel, report);
    }

    let range = output_range(config, &probe).ok_or_else(|| {
        ConversionError::InvalidInput("The source duration is unknown".to_string())
    })?;

    if config.processing_mode == "copy" {
        return estimate_stream_copy(file_path, config, &probe, range).map(Some);
    }
    if let Some(estimate) = fixed_bitrate_estimate(config, range) {
        return Ok(Some(estimate));
    }
    if range.duration_seconds <= FULL_ENCODE_MAX_SECONDS {
        return estimate_full_encode(file_path, config, &probe, &work, cancel, report);
    }

    estimate_from_test_encodes(file_path, config, &probe, range, &work, cancel, report)
}

fn estimate_full_encode(
    file_path: &str,
    config: &ConversionConfig,
    probe: &ProbeMetadata,
    work: &WorkDirectory,
    cancel: &AtomicBool,
    report: &mut impl FnMut(SizeEstimateProgress),
) -> Result<Option<SizeEstimate>, ConversionError> {
    report(SizeEstimateProgress::EncodingFull);
    let output = work.output_path("full", &config.container);
    let args = build_ffmpeg_args(file_path, &output, config, probe)?;
    if !run_ffmpeg(&args, work, cancel)? {
        return Ok(None);
    }

    Ok(Some(SizeEstimate::exact(
        fs::metadata(&output)?.len(),
        SizeEstimateMethod::FullEncode,
    )))
}

fn estimate_stream_copy(
    file_path: &str,
    config: &ConversionConfig,
    probe: &ProbeMetadata,
    range: OutputRange,
) -> Result<SizeEstimate, ConversionError> {
    let mut streams = config.selected_audio_tracks.clone();
    streams.extend(&config.selected_subtitle_tracks);
    let video_stream = probe
        .video_stream_index
        .filter(|_| !is_audio_only_container(&config.container));
    streams.extend(video_stream);

    let packets = run_ffprobe(&source_packet_args(file_path))?;
    Ok(stream_copy_estimate(
        &packets,
        &streams,
        video_stream,
        range,
        &config.container,
    ))
}

fn estimate_from_test_encodes(
    file_path: &str,
    config: &ConversionConfig,
    probe: &ProbeMetadata,
    range: OutputRange,
    work: &WorkDirectory,
    cancel: &AtomicBool,
    report: &mut impl FnMut(SizeEstimateProgress),
) -> Result<Option<SizeEstimate>, ConversionError> {
    let sections = plan_sample_sections(range);
    let section_seconds = sections[0].duration_seconds;
    let scan_output = !is_audio_only_container(&config.container);

    let full_scan_bytes = if scan_output {
        report(SizeEstimateProgress::ScanningSource);
        let scan = SampleSection {
            start_seconds: range.start_seconds,
            duration_seconds: range.duration_seconds,
        };
        match run_complexity_scan(file_path, scan, "scan-full", work, cancel)? {
            Some(bytes) => bytes,
            None => return Ok(None),
        }
    } else {
        time_based_scan_bytes(range, section_seconds)
    };

    let total = u32::try_from(sections.len()).unwrap_or(u32::MAX);
    let mut samples = Vec::with_capacity(sections.len());
    for (current, section) in (1..).zip(sections) {
        report(SizeEstimateProgress::EncodingSample { current, total });

        let name = format!("sample-{current}");
        let output = work.output_path(&name, &config.container);
        let section_config = config_for_section(config, section);
        let args = build_ffmpeg_args(file_path, &output, &section_config, probe)?;
        if !run_ffmpeg(&args, work, cancel)? {
            return Ok(None);
        }

        let scan_bytes = if scan_output {
            match run_complexity_scan(file_path, section, &format!("scan-{current}"), work, cancel)?
            {
                Some(bytes) => bytes,
                None => return Ok(None),
            }
        } else {
            SCAN_UNITS_PER_SECTION
        };

        samples.push(measure_sample(&output, config, scan_bytes)?);
    }

    estimate_from_samples(
        &samples,
        section_seconds,
        range.duration_seconds,
        full_scan_bytes,
    )
    .map(Some)
    .ok_or_else(|| ConversionError::Worker("The test encodes were empty".to_string()))
}

fn time_based_scan_bytes(range: OutputRange, section_seconds: f64) -> u64 {
    let sections = range.duration_seconds / section_seconds;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "The section count is positive and small."
    )]
    let scan_bytes = (sections * SCAN_UNITS_PER_SECTION as f64).round() as u64;
    scan_bytes
}

fn run_complexity_scan(
    file_path: &str,
    section: SampleSection,
    name: &str,
    work: &WorkDirectory,
    cancel: &AtomicBool,
) -> Result<Option<u64>, ConversionError> {
    let output = work.path.join(format!("{name}.mkv"));
    let args = complexity_scan_args(
        file_path,
        &output.to_string_lossy(),
        section.start_seconds,
        section.duration_seconds,
    );
    if !run_ffmpeg(&args, work, cancel)? {
        return Ok(None);
    }

    let bytes = fs::metadata(&output)?.len();
    let _ = fs::remove_file(&output);
    Ok(Some(bytes))
}

fn measure_sample(
    output: &str,
    config: &ConversionConfig,
    scan_bytes: u64,
) -> Result<SampleMeasurement, ConversionError> {
    let encoded_bytes = fs::metadata(output)?.len();
    let has_audio =
        container_supports_audio(&config.container) && !config.selected_audio_tracks.is_empty();
    let video_bytes = if is_audio_only_container(&config.container) {
        encoded_bytes
    } else if has_audio {
        video_packet_bytes(output)?.min(encoded_bytes)
    } else {
        encoded_bytes
    };
    let _ = fs::remove_file(output);

    Ok(SampleMeasurement {
        video_bytes,
        other_bytes: encoded_bytes - video_bytes,
        scan_bytes,
    })
}

fn video_packet_bytes(path: &str) -> Result<u64, ConversionError> {
    let packets = run_ffprobe(&[
        "-v".to_string(),
        "error".to_string(),
        "-select_streams".to_string(),
        "v:0".to_string(),
        "-show_entries".to_string(),
        "packet=size".to_string(),
        "-of".to_string(),
        "csv=p=0".to_string(),
        path.to_string(),
    ])?;

    Ok(packets
        .lines()
        .filter_map(|line| line.trim().trim_end_matches(',').parse::<u64>().ok())
        .sum())
}

fn run_ffprobe(args: &[String]) -> Result<String, ConversionError> {
    let output = Command::new(ffprobe_executable())
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(ConversionError::Worker(format!(
            "ffprobe exited with status {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `FFmpeg` to completion. Returns `Ok(false)` when cancelled.
fn run_ffmpeg(
    args: &[String],
    work: &WorkDirectory,
    cancel: &AtomicBool,
) -> Result<bool, ConversionError> {
    let log_path = work.path.join("ffmpeg.log");
    let mut child = Command::new(ffmpeg_executable())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&log_path)?)
        .spawn()?;

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(true);
            }
            return Err(ConversionError::Worker(last_log_line(&log_path)));
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn last_log_line(path: &Path) -> String {
    fs::read(path)
        .ok()
        .and_then(|log| {
            String::from_utf8_lossy(&log)
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .unwrap_or_else(|| "ffmpeg failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_directory_is_removed_when_dropped() {
        let path = {
            let work = WorkDirectory::create().expect("work directory");
            fs::write(work.path.join("sample.mp4"), b"sample").expect("sample file");
            work.path.clone()
        };

        assert!(!path.exists());
    }

    #[test]
    fn time_based_scan_scales_with_the_range() {
        let range = OutputRange {
            start_seconds: 0.0,
            duration_seconds: 120.0,
        };

        assert_eq!(time_based_scan_bytes(range, 4.0), 30_000);
    }

    #[test]
    fn last_log_line_skips_trailing_blank_lines() {
        let work = WorkDirectory::create().expect("work directory");
        let log = work.path.join("ffmpeg.log");
        fs::write(&log, "first\nConversion failed!\n\n").expect("log");

        assert_eq!(last_log_line(&log), "Conversion failed!");
    }
}
