//! Output file size prediction.
//!
//! Stream copies and fixed-bitrate encodes can be calculated. Quality-based encodes
//! depend on the content, so they are measured: short sections are encoded with the
//! real settings, and a fast scan of the whole range scales that result to the parts
//! of the video the sections did not cover.

use crate::container::transport_stream_profile;
use crate::media_rules::{
    container_supports_audio, is_gif_container, is_image_container, is_video_only_container,
};
use crate::types::{ConversionConfig, ProbeMetadata};
use crate::utils::parse_time;

/// Ranges this short are encoded in full, which gives the exact output size.
pub const FULL_ENCODE_MAX_SECONDS: f64 = 45.0;
const SAMPLE_SECONDS: f64 = 4.0;
const SAMPLE_COUNT: u32 = 6;
const LONG_RANGE_SAMPLE_COUNT: u32 = 10;
const LONG_RANGE_SECONDS: f64 = 300.0;

const CALCULATED_MARGIN: f64 = 0.03;
const MEASURED_MIN_MARGIN: f64 = 0.06;
const MEASURED_MAX_MARGIN: f64 = 0.2;
const MUXING_OVERHEAD: f64 = 1.005;
const TS_ENCODE_OVERHEAD: f64 = 1.07;
const TS_CELL_PAYLOAD_BYTES: f64 = 184.0;
const TS_BYTES_PER_PACKET: f64 = 106.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeEstimateMethod {
    /// Added up from the source packets that a stream copy would keep.
    SourcePackets,
    /// Worked out from the fixed video and audio bitrates.
    FixedBitrate,
    /// Encoded in full, so the size is the real output size.
    FullEncode,
    /// Scaled up from test encodes of the given number of sections.
    Sampled { samples: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SizeEstimate {
    pub bytes: u64,
    pub low_bytes: u64,
    pub high_bytes: u64,
    pub method: SizeEstimateMethod,
}

impl SizeEstimate {
    #[must_use]
    pub const fn exact(bytes: u64, method: SizeEstimateMethod) -> Self {
        Self {
            bytes,
            low_bytes: bytes,
            high_bytes: bytes,
            method,
        }
    }

    #[must_use]
    pub fn with_margin(bytes: f64, margin: f64, method: SizeEstimateMethod) -> Self {
        Self {
            bytes: rounded_bytes(bytes),
            low_bytes: rounded_bytes(bytes * (1.0 - margin)),
            high_bytes: rounded_bytes(bytes * (1.0 + margin)),
            method,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputRange {
    pub start_seconds: f64,
    pub duration_seconds: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleSection {
    pub start_seconds: f64,
    pub duration_seconds: f64,
}

/// Bytes measured for one test section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampleMeasurement {
    pub video_bytes: u64,
    pub other_bytes: u64,
    pub scan_bytes: u64,
}

/// Returns the part of the source that the conversion will keep.
#[must_use]
pub fn output_range(config: &ConversionConfig, probe: &ProbeMetadata) -> Option<OutputRange> {
    let source_duration = probe
        .duration
        .as_deref()
        .and_then(parse_time)
        .filter(|duration| duration.is_finite() && *duration > 0.0)?;
    let time = |value: Option<&String>| {
        value
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| parse_time(value))
    };
    let start = time(config.start_time.as_ref())
        .unwrap_or(0.0)
        .clamp(0.0, source_duration);
    let end = time(config.end_time.as_ref())
        .unwrap_or(source_duration)
        .clamp(start, source_duration);

    (end > start).then_some(OutputRange {
        start_seconds: start,
        duration_seconds: end - start,
    })
}

/// Spreads test sections evenly so they cover the beginning, middle, and end.
#[must_use]
pub fn plan_sample_sections(range: OutputRange) -> Vec<SampleSection> {
    let count = if range.duration_seconds >= LONG_RANGE_SECONDS {
        LONG_RANGE_SAMPLE_COUNT
    } else {
        SAMPLE_COUNT
    };
    let spare_seconds = (range.duration_seconds - SAMPLE_SECONDS).max(0.0);

    (0..count)
        .map(|index| SampleSection {
            start_seconds: range.start_seconds
                + spare_seconds * (f64::from(index) + 0.5) / f64::from(count),
            duration_seconds: SAMPLE_SECONDS.min(range.duration_seconds),
        })
        .collect()
}

/// Returns the settings for encoding only one part of the source.
#[must_use]
pub fn config_for_section(config: &ConversionConfig, section: SampleSection) -> ConversionConfig {
    ConversionConfig {
        start_time: Some(format!("{:.3}", section.start_seconds)),
        end_time: Some(format!(
            "{:.3}",
            section.start_seconds + section.duration_seconds
        )),
        ..config.clone()
    }
}

/// Builds the fast encode that compares how hard each part of the source is to compress.
#[must_use]
pub fn complexity_scan_args(
    input: &str,
    output: &str,
    start_seconds: f64,
    duration_seconds: f64,
) -> Vec<String> {
    [
        "-ss",
        &format!("{start_seconds:.3}"),
        "-i",
        input,
        "-t",
        &format!("{duration_seconds:.3}"),
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-vf",
        "scale=320:-2",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-crf",
        "26",
        "-f",
        "matroska",
        "-n",
        output,
    ]
    .map(str::to_string)
    .to_vec()
}

/// Lists each source packet so a stream copy can be added up.
#[must_use]
pub fn source_packet_args(input: &str) -> Vec<String> {
    [
        "-v",
        "error",
        "-show_entries",
        "packet=stream_index,pts_time,size,flags",
        "-of",
        "csv=p=0",
        input,
    ]
    .map(str::to_string)
    .to_vec()
}

struct SourcePacket {
    stream_index: u32,
    time: f64,
    size: u64,
    is_keyframe: bool,
}

/// Reads one `stream_index,pts_time,size,flags` line.
fn parse_source_packet(line: &str) -> Option<SourcePacket> {
    let mut fields = line.trim().trim_end_matches(',').split(',');
    Some(SourcePacket {
        stream_index: fields.next()?.parse().ok()?,
        time: fields.next()?.parse().ok()?,
        size: fields.next()?.parse().ok()?,
        is_keyframe: fields.next()?.starts_with('K'),
    })
}

/// Adds up the packets a stream copy keeps. Video has to start on a keyframe, so a
/// trimmed copy also keeps the video back to the keyframe before the start time.
#[must_use]
pub fn stream_copy_estimate(
    packet_list: &str,
    streams: &[u32],
    video_stream: Option<u32>,
    range: OutputRange,
    container: &str,
) -> SizeEstimate {
    let packets: Vec<SourcePacket> = packet_list
        .lines()
        .filter_map(parse_source_packet)
        .filter(|packet| streams.contains(&packet.stream_index))
        .collect();
    let video_start = packets
        .iter()
        .filter(|packet| Some(packet.stream_index) == video_stream && packet.is_keyframe)
        .map(|packet| packet.time)
        .filter(|time| *time <= range.start_seconds)
        .fold(0.0_f64, f64::max);
    let end_seconds = range.start_seconds + range.duration_seconds;
    let kept: Vec<&SourcePacket> = packets
        .iter()
        .filter(|packet| {
            let start = if Some(packet.stream_index) == video_stream {
                video_start
            } else {
                range.start_seconds
            };
            packet.time >= start && packet.time < end_seconds
        })
        .collect();

    let payload = bytes_to_f64(kept.iter().map(|packet| packet.size).sum());
    let packet_count = bytes_to_f64(u64::try_from(kept.len()).unwrap_or(u64::MAX));
    SizeEstimate::with_margin(
        muxed_copy_bytes(payload, packet_count, container),
        CALCULATED_MARGIN,
        SizeEstimateMethod::SourcePackets,
    )
}

/// Transport streams split each packet into small cells, so their overhead depends on
/// the number of packets. Other containers add a small index.
fn muxed_copy_bytes(payload: f64, packet_count: f64, container: &str) -> f64 {
    let Some(profile) = transport_stream_profile(container) else {
        return payload * MUXING_OVERHEAD;
    };
    let cell_bytes = f64::from(profile.packet_size());
    packet_count.mul_add(TS_BYTES_PER_PACKET, payload) * cell_bytes / TS_CELL_PAYLOAD_BYTES
}

/// Works out the size of a fixed-bitrate encode, or `None` when the output size is
/// decided by quality settings instead.
#[must_use]
pub fn fixed_bitrate_estimate(
    config: &ConversionConfig,
    range: OutputRange,
) -> Option<SizeEstimate> {
    if config.video_bitrate_mode != "bitrate"
        || is_image_container(&config.container)
        || is_gif_container(&config.container)
        || is_video_only_container(&config.container)
    {
        return None;
    }
    let video_kbps = config.video_bitrate.trim().parse::<f64>().ok()?;
    let audio_kbps = if !container_supports_audio(&config.container)
        || config.selected_audio_tracks.is_empty()
    {
        0.0
    } else {
        if config.audio_bitrate_mode != "bitrate" {
            return None;
        }
        config.audio_bitrate.trim().parse::<f64>().ok()?
            * f64::from(u32::try_from(config.selected_audio_tracks.len()).ok()?)
    };
    if video_kbps <= 0.0 {
        return None;
    }
    let overhead = if transport_stream_profile(&config.container).is_some() {
        TS_ENCODE_OVERHEAD
    } else {
        MUXING_OVERHEAD
    };

    Some(SizeEstimate::with_margin(
        (video_kbps + audio_kbps) * 1000.0 / 8.0 * range.duration_seconds * overhead,
        CALCULATED_MARGIN,
        SizeEstimateMethod::FixedBitrate,
    ))
}

/// Scales test encodes to the whole range.
///
/// Each section starts on an expensive keyframe that a full encode would not have, and
/// the scan's sections start the same way, so comparing the two cancels that out. The
/// scan's full size then accounts for the parts that were not test-encoded.
#[must_use]
pub fn estimate_from_samples(
    samples: &[SampleMeasurement],
    section_seconds: f64,
    range_seconds: f64,
    full_scan_bytes: u64,
) -> Option<SizeEstimate> {
    let video_bytes = samples.iter().map(|sample| sample.video_bytes).sum::<u64>();
    let other_bytes = samples.iter().map(|sample| sample.other_bytes).sum::<u64>();
    let scan_bytes = samples.iter().map(|sample| sample.scan_bytes).sum::<u64>();
    let sampled_seconds = section_seconds * f64::from(u32::try_from(samples.len()).ok()?);
    if scan_bytes == 0 || full_scan_bytes == 0 || sampled_seconds <= 0.0 {
        return None;
    }

    let video =
        bytes_to_f64(video_bytes) / bytes_to_f64(scan_bytes) * bytes_to_f64(full_scan_bytes);
    let other = bytes_to_f64(other_bytes) / sampled_seconds * range_seconds;

    Some(SizeEstimate::with_margin(
        video + other,
        sample_margin(samples),
        SizeEstimateMethod::Sampled {
            samples: u32::try_from(samples.len()).ok()?,
        },
    ))
}

/// Widens the reported range when the sections disagree about how the real encoder
/// compares with the scan.
fn sample_margin(samples: &[SampleMeasurement]) -> f64 {
    let ratios: Vec<f64> = samples
        .iter()
        .filter(|sample| sample.scan_bytes > 0)
        .map(|sample| bytes_to_f64(sample.video_bytes) / bytes_to_f64(sample.scan_bytes))
        .collect();
    let count = u32::try_from(ratios.len()).map_or(0.0, f64::from);
    if count < 2.0 {
        return MEASURED_MAX_MARGIN;
    }

    let mean = ratios.iter().sum::<f64>() / count;
    if mean <= 0.0 {
        return MEASURED_MAX_MARGIN;
    }
    let variance = ratios
        .iter()
        .map(|ratio| (ratio - mean).powi(2))
        .sum::<f64>()
        / (count - 1.0);
    let standard_error = variance.sqrt() / count.sqrt() / mean;

    standard_error.clamp(MEASURED_MIN_MARGIN, MEASURED_MAX_MARGIN)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "File sizes stay far below the range where f64 loses whole bytes."
)]
const fn bytes_to_f64(bytes: u64) -> f64 {
    bytes as f64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Estimates are positive and far below u64::MAX."
)]
fn rounded_bytes(bytes: f64) -> u64 {
    bytes.max(0.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> ConversionConfig {
        serde_json::from_value(serde_json::json!({
            "container": "mp4",
            "videoCodec": "libx264",
            "videoBitrateMode": "crf",
            "videoBitrate": "5000",
            "audioCodec": "aac",
            "audioBitrate": "128",
            "audioChannels": "original",
            "selectedAudioTracks": [],
            "selectedSubtitleTracks": [],
            "subtitleBurnPath": null,
            "resolution": "original",
            "customWidth": null,
            "customHeight": null,
            "scalingAlgorithm": "bicubic",
            "fps": "original",
            "crf": 23,
            "preset": "medium",
            "startTime": null,
            "endTime": null
        }))
        .expect("sample config should deserialize")
    }

    fn probe_with_duration(duration: &str) -> ProbeMetadata {
        ProbeMetadata {
            duration: Some(duration.to_string()),
            ..ProbeMetadata::default()
        }
    }

    fn range(start_seconds: f64, duration_seconds: f64) -> OutputRange {
        OutputRange {
            start_seconds,
            duration_seconds,
        }
    }

    #[test]
    fn output_range_applies_trim_and_stays_inside_the_source() {
        let config = ConversionConfig {
            start_time: Some("10".to_string()),
            end_time: Some("500".to_string()),
            ..sample_config()
        };

        assert_eq!(
            output_range(&config, &probe_with_duration("60.0")),
            Some(range(10.0, 50.0))
        );
    }

    #[test]
    fn output_range_needs_a_known_duration() {
        assert_eq!(
            output_range(&sample_config(), &ProbeMetadata::default()),
            None
        );
    }

    #[test]
    fn sample_sections_are_spread_inside_the_range() {
        let sections = plan_sample_sections(range(100.0, 60.0));

        assert_eq!(sections.len(), 6);
        assert!(sections[0].start_seconds > 100.0);
        assert!(sections[1].start_seconds - sections[0].start_seconds > 8.0);
        let last = sections[5];
        assert!(last.start_seconds + last.duration_seconds < 160.0);
    }

    #[test]
    fn long_ranges_use_more_sample_sections() {
        assert_eq!(plan_sample_sections(range(0.0, 3600.0)).len(), 10);
    }

    #[test]
    fn config_for_section_only_changes_the_time_range() {
        let config = ConversionConfig {
            container: "webp".to_string(),
            ..sample_config()
        };

        let section = config_for_section(
            &config,
            SampleSection {
                start_seconds: 12.5,
                duration_seconds: 4.0,
            },
        );

        assert_eq!(section.container, "webp");
        assert_eq!(section.start_time.as_deref(), Some("12.500"));
        assert_eq!(section.end_time.as_deref(), Some("16.500"));
    }

    #[test]
    fn stream_copy_estimate_counts_selected_streams_inside_the_range() {
        let packets =
            "1,1.0,1000,__\n1,5.0,4000,_\n2,5.0,8000,K__,\n1,N/A,32000,K\n1,20.0,16000,K\n";

        let estimate = stream_copy_estimate(packets, &[1], None, range(2.0, 10.0), "mkv");

        assert_eq!(estimate.bytes, 4020);
        assert_eq!(estimate.method, SizeEstimateMethod::SourcePackets);
    }

    #[test]
    fn trimmed_stream_copy_keeps_video_from_the_previous_keyframe() {
        let packets = "0,4.0,1000,K__\n0,5.0,100,___\n0,7.0,100,___\n1,5.0,10,K__\n1,6.0,20,K__\n";

        let estimate = stream_copy_estimate(packets, &[0, 1], Some(0), range(5.5, 10.0), "mkv");

        assert_eq!(estimate.bytes, 1226);
    }

    #[test]
    fn transport_streams_count_their_packet_overhead() {
        let estimate =
            stream_copy_estimate("0,1.0,1000,K\n", &[0], Some(0), range(0.0, 2.0), "m2ts");

        assert_eq!(estimate.bytes, 1154);
    }

    #[test]
    fn fixed_bitrate_estimate_adds_video_and_each_audio_track() {
        let config = ConversionConfig {
            video_bitrate_mode: "bitrate".to_string(),
            video_bitrate: "1000".to_string(),
            audio_bitrate_mode: "bitrate".to_string(),
            audio_bitrate: "128".to_string(),
            selected_audio_tracks: vec![1, 2],
            ..sample_config()
        };

        let estimate = fixed_bitrate_estimate(&config, range(0.0, 8.0)).expect("fixed bitrate");

        assert_eq!(estimate.bytes, 1_262_280);
    }

    #[test]
    fn quality_based_encodes_are_not_calculated() {
        assert_eq!(
            fixed_bitrate_estimate(&sample_config(), range(0.0, 8.0)),
            None
        );
    }

    #[test]
    fn estimate_from_samples_scales_video_by_the_scan_and_audio_by_time() {
        let sample = SampleMeasurement {
            video_bytes: 2_000,
            other_bytes: 100,
            scan_bytes: 1_000,
        };

        let estimate = estimate_from_samples(&[sample, sample], 4.0, 80.0, 50_000).expect("sizes");

        assert_eq!(estimate.bytes, 100_000 + 2_000);
        assert_eq!(estimate.low_bytes, 95_880);
        assert_eq!(estimate.high_bytes, 108_120);
        assert_eq!(estimate.method, SizeEstimateMethod::Sampled { samples: 2 });
    }

    #[test]
    fn disagreeing_samples_widen_the_range() {
        let samples = [
            SampleMeasurement {
                video_bytes: 1_000,
                other_bytes: 0,
                scan_bytes: 1_000,
            },
            SampleMeasurement {
                video_bytes: 3_000,
                other_bytes: 0,
                scan_bytes: 1_000,
            },
        ];

        let estimate = estimate_from_samples(&samples, 4.0, 80.0, 10_000).expect("sizes");

        assert_eq!(estimate.bytes, 20_000);
        assert_eq!(estimate.low_bytes, 16_000);
        assert_eq!(estimate.high_bytes, 24_000);
    }
}
