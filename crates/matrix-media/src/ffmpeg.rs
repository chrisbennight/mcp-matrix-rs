//! FFmpeg invocation for animated and video sources.
//!
//! FFmpeg runs as a subprocess rather than a linked library. The isolation is the
//! point: this is where hostile caller-supplied media is parsed, and a separate process
//! can be given a hard deadline and killed, where a library fault takes the server with
//! it.
//!
//! Argument construction is a pure function so the exact argv is a testable contract.
//! Nothing here interpolates a caller value into a shell — there is no shell.

use crate::limits::Limits;
use matrix_frame::{BYTES_PER_PIXEL, Canvas, Rate};

/// Scaling filter.
///
/// `area` averages every source pixel that falls inside a destination pixel, which is
/// the correct choice for the reduction ratios here — a 1920-wide source into a 64-wide
/// canvas is a 30x reduction, where bilinear undersamples and lanczos rings on hard
/// edges.
///
/// The averaging happens in the input transfer space rather than in linear light, so
/// output is dimmer than a gamma-correct downscale would produce. Correcting it needs
/// `zscale`, which requires an FFmpeg built against zimg; see `DECISIONS.md`.
const SCALE_FLAGS: &str = "area";

/// Build the argv for decoding a source into raw RGB24 frames on stdout.
///
/// One filtergraph does frame-rate resampling, scaling, and pixel-format conversion in
/// a single pass, so nothing downstream re-walks the data. The output is a bare stream
/// of `canvas.byte_len()`-sized frames with no container and no framing, which is what
/// makes the reader a fixed-size chunk loop.
///
/// The source arrives on stdin. A caller-supplied path is never passed, so there is no
/// path for a filename to be interpreted as an option or a protocol.
pub fn decode_argv(canvas: Canvas, rate: Rate, limits: &Limits) -> Vec<String> {
    let filter = format!(
        "fps={},scale={}:{}:flags={},format=rgb24",
        rate.fps(),
        canvas.width(),
        canvas.height(),
        SCALE_FLAGS
    );

    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        // Never prompt; a subprocess waiting on stdin would sit until the deadline.
        "-nostdin".into(),
        // Confine input to the pipe. FFmpeg's demuxers follow references — a concat
        // script, an HLS manifest, an image2 pattern — and without this a caller's
        // bytes can name a local file or a network URL for FFmpeg to open on their
        // behalf. Reading from stdin prevents argv injection; this prevents the
        // demuxer becoming the fetch destination instead.
        "-protocol_whitelist".into(),
        "pipe".into(),
        // Refuse anything that is not a real decodable stream rather than probing
        // indefinitely on a malformed source.
        "-err_detect".into(),
        "explode".into(),
        "-i".into(),
        "pipe:0".into(),
        // Decode the same stream the probe measured. Automatic selection picks the
        // "best" video stream, which need not be v:0, so a source carrying a small
        // first stream and a huge second one would be cleared by the probe and then
        // decoded from the stream the probe never saw.
        "-map".into(),
        "0:v:0".into(),
        "-vf".into(),
        filter,
        // Video only: an audio stream in a submitted MP4 is not renderable on a panel
        // and decoding it is work we would throw away.
        "-an".into(),
        "-sn".into(),
        "-dn".into(),
        // The only output bound, deliberately. An input-side `-t` stops FFmpeg reading
        // at the duration limit, so it would never emit the extra frame that reveals an
        // over-limit source — the truncation would be silent and indistinguishable from
        // a source that simply ended. Asking for one frame beyond the effective cap and
        // refusing when it arrives is what makes the overflow observable. `-frames:v`
        // still stops the decode promptly, so dropping `-t` costs nothing in work done.
        "-frames:v".into(),
        limits
            .effective_frame_cap(rate.fps())
            .saturating_add(1)
            .to_string(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "pipe:1".into(),
    ]
}

/// Build the argv for probing a source's real dimensions.
///
/// The declared-dimension check can only bound what a caller claims. A small payload
/// can declare an enormous frame internally, and FFmpeg allocates for that before the
/// scale filter reduces anything, so the limit is worth nothing without reading the
/// dimensions out of the media itself.
///
/// Confined to the pipe protocol for the same reason the decode is.
pub fn probe_argv() -> Vec<String> {
    vec![
        "-v".into(),
        "error".into(),
        "-protocol_whitelist".into(),
        "pipe".into(),
        "-select_streams".into(),
        "v:0".into(),
        // Dimensions and duration in one call. A second probe to learn the duration
        // would double the process cost of the cheapest check in the pipeline.
        "-show_entries".into(),
        "stream=width,height:format=duration".into(),
        "-of".into(),
        "csv=p=0:s=x".into(),
        "pipe:0".into(),
    ]
}

/// Parse `WIDTHxHEIGHT` from the first non-empty line of a probe's stdout.
pub fn parse_probe_dimensions(raw: &str) -> Option<(u32, u32)> {
    let line = raw.lines().find(|l| !l.trim().is_empty())?;
    let (w, h) = line.trim().split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Parse the container duration from the second non-empty line of a probe's stdout.
///
/// Absent for a still image and reported as `N/A` by some containers, so a missing
/// value is `None` rather than an error: the frame cap still bounds the decode, and
/// refusing every source whose duration cannot be read would reject ordinary stills.
pub fn parse_probe_duration(raw: &str) -> Option<std::time::Duration> {
    let line = raw.lines().filter(|l| !l.trim().is_empty()).nth(1)?;
    let seconds: f64 = line.trim().parse().ok()?;
    // Duration::from_secs_f64 panics for finite values past its range, and this figure
    // comes from a probe reading untrusted metadata. A century is far beyond anything
    // the caps admit, so treating a larger claim as unreadable loses nothing and keeps
    // an absurd number from unwinding the decode.
    const MAX_PLAUSIBLE_SECONDS: f64 = 100.0 * 365.0 * 24.0 * 3600.0;
    if !seconds.is_finite() || !(0.0..=MAX_PLAUSIBLE_SECONDS).contains(&seconds) {
        return None;
    }
    Some(std::time::Duration::from_secs_f64(seconds))
}

/// Split a raw RGB24 stream into whole frames.
///
/// A trailing partial frame means the decode was truncated — a killed subprocess, a
/// deadline, or a corrupt source — and is reported rather than padded, because a
/// half-frame rendered on the panel is worse than a refusal.
pub fn split_frames(raw: &[u8], canvas: Canvas) -> Result<Vec<&[u8]>, usize> {
    let frame_len = canvas.byte_len();
    let remainder = raw.len() % frame_len;
    if remainder != 0 {
        return Err(remainder);
    }
    Ok(raw.chunks_exact(frame_len).collect())
}

/// Bytes one normalized frame occupies for this canvas.
pub fn frame_stride(canvas: Canvas) -> usize {
    canvas.pixels() * BYTES_PER_PIXEL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m1() -> Canvas {
        Canvas::new(64, 64).expect("valid")
    }

    fn argv() -> Vec<String> {
        decode_argv(m1(), Rate::new(25).expect("valid"), &Limits::default())
    }

    fn value_after(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    }

    #[test]
    fn the_filtergraph_resamples_scales_and_converts_in_one_pass() {
        let filter = value_after(&argv(), "-vf").expect("a filtergraph is always present");
        assert_eq!(filter, "fps=25,scale=64:64:flags=area,format=rgb24");
    }

    #[test]
    fn the_scaler_is_area_averaging_for_heavy_downscale() {
        let filter = value_after(&argv(), "-vf").expect("filtergraph");
        assert!(
            filter.contains("flags=area"),
            "bilinear undersamples and lanczos rings at a 30x reduction"
        );
    }

    #[test]
    fn the_canvas_drives_the_scale_target() {
        let wide = Canvas::new(128, 64).expect("two panels");
        let filter = value_after(
            &decode_argv(wide, Rate::new(25).expect("valid"), &Limits::default()),
            "-vf",
        )
        .expect("filtergraph");
        assert!(filter.contains("scale=128:64:"));
    }

    #[test]
    fn the_rate_drives_the_fps_filter_not_a_constant() {
        for fps in [1u16, 15, 25, 30] {
            let filter = value_after(
                &decode_argv(m1(), Rate::new(fps).expect("valid"), &Limits::default()),
                "-vf",
            )
            .expect("filtergraph");
            assert!(filter.starts_with(&format!("fps={fps},")));
        }
    }

    #[test]
    fn the_source_is_read_from_stdin_never_from_a_caller_path() {
        let args = argv();
        assert_eq!(value_after(&args, "-i").as_deref(), Some("pipe:0"));
        assert!(
            !args.iter().any(|a| a.contains('/') && a != "pipe:0"),
            "no filesystem path may reach the argv"
        );
    }

    #[test]
    fn output_is_headerless_rgb24_on_stdout() {
        let args = argv();
        assert_eq!(value_after(&args, "-f").as_deref(), Some("rawvideo"));
        assert_eq!(value_after(&args, "-pix_fmt").as_deref(), Some("rgb24"));
        assert_eq!(args.last().map(String::as_str), Some("pipe:1"));
    }

    #[test]
    fn the_effective_cap_plus_one_is_the_only_output_bound() {
        let limits = Limits {
            max_duration: std::time::Duration::from_secs(12),
            max_frames: 300,
            ..Limits::default()
        };
        let args = decode_argv(m1(), Rate::new(25).expect("valid"), &limits);
        // 12 s at 25 fps is 300 frames, which binds before the 300-frame cap; one past
        // that is what makes an over-limit source observable.
        assert_eq!(value_after(&args, "-frames:v").as_deref(), Some("301"));
        assert!(
            !args.iter().any(|a| a == "-t"),
            "an input-side -t would truncate before the overflow frame could appear"
        );
    }

    #[test]
    fn non_video_streams_are_discarded_rather_than_decoded() {
        let args = argv();
        for flag in ["-an", "-sn", "-dn"] {
            assert!(args.iter().any(|a| a == flag), "{flag} must be present");
        }
    }

    #[test]
    fn the_decode_is_pinned_to_the_stream_the_probe_measures() {
        let args = argv();
        assert_eq!(value_after(&args, "-map").as_deref(), Some("0:v:0"));
        // The probe selects the same stream; the two must not drift apart.
        assert_eq!(
            value_after(&probe_argv(), "-select_streams").as_deref(),
            Some("v:0")
        );
    }

    #[test]
    fn input_is_confined_to_the_pipe_protocol() {
        // Without this, a caller-supplied concat script or HLS manifest makes FFmpeg
        // open a file or a URL the caller chose.
        let args = argv();
        assert_eq!(
            value_after(&args, "-protocol_whitelist").as_deref(),
            Some("pipe")
        );
    }

    #[test]
    fn the_subprocess_can_never_block_on_stdin_or_probe_forever() {
        let args = argv();
        assert!(args.iter().any(|a| a == "-nostdin"));
        assert_eq!(
            value_after(&args, "-err_detect").as_deref(),
            Some("explode")
        );
    }

    #[test]
    fn the_probe_is_confined_to_the_pipe_protocol_and_reads_stdin() {
        let args = probe_argv();
        assert_eq!(
            value_after(&args, "-protocol_whitelist").as_deref(),
            Some("pipe")
        );
        assert_eq!(args.last().map(String::as_str), Some("pipe:0"));
    }

    #[test]
    fn probe_output_parses_into_dimensions() {
        assert_eq!(parse_probe_dimensions("1920x1080\n"), Some((1920, 1080)));
        assert_eq!(parse_probe_dimensions("  64x64  "), Some((64, 64)));
        assert_eq!(parse_probe_dimensions("\n1920x1080\n"), Some((1920, 1080)));
    }

    #[test]
    fn probe_output_parses_a_duration_when_the_container_reports_one() {
        assert_eq!(
            parse_probe_duration("1920x1080\n12.5\n"),
            Some(std::time::Duration::from_millis(12_500))
        );
        assert_eq!(
            parse_probe_duration("64x64\n0.04\n"),
            Some(std::time::Duration::from_millis(40))
        );
    }

    #[test]
    fn a_source_without_a_readable_duration_yields_none_rather_than_an_error() {
        // A still image reports no duration, and some containers write N/A. The frame
        // cap still bounds those; refusing them here would reject ordinary stills.
        for raw in ["1920x1080\n", "1920x1080\nN/A\n", "1920x1080\n-1\n", ""] {
            assert_eq!(parse_probe_duration(raw), None, "input {raw:?}");
        }
    }

    #[test]
    fn an_extreme_duration_is_unreadable_rather_than_a_panic() {
        // Duration::from_secs_f64 panics past its range; this figure is untrusted.
        for raw in [
            "64x64\n1e300\n",
            "64x64\n99999999999999999999\n",
            "64x64\ninf\n",
            "64x64\nNaN\n",
        ] {
            assert_eq!(parse_probe_duration(raw), None, "input {raw:?}");
        }
    }

    #[test]
    fn the_probe_asks_for_duration_alongside_dimensions() {
        let args = probe_argv();
        assert_eq!(
            value_after(&args, "-show_entries").as_deref(),
            Some("stream=width,height:format=duration")
        );
    }

    #[test]
    fn unparseable_probe_output_yields_no_dimensions() {
        for raw in ["", "\n", "not-a-size", "1920", "axb"] {
            assert_eq!(parse_probe_dimensions(raw), None, "input {raw:?}");
        }
    }

    #[test]
    fn a_raw_stream_splits_into_whole_frames() {
        let canvas = m1();
        let raw = vec![0u8; canvas.byte_len() * 3];
        let frames = split_frames(&raw, canvas).expect("three whole frames");
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|f| f.len() == canvas.byte_len()));
    }

    #[test]
    fn a_truncated_stream_is_reported_rather_than_padded() {
        let canvas = m1();
        let raw = vec![0u8; canvas.byte_len() * 2 + 17];
        let remainder = split_frames(&raw, canvas).expect_err("a partial frame must not pass");
        assert_eq!(remainder, 17);
    }

    #[test]
    fn an_empty_stream_is_zero_frames_not_an_error() {
        let canvas = m1();
        assert_eq!(split_frames(&[], canvas).expect("empty is whole").len(), 0);
    }

    #[test]
    fn frame_stride_matches_the_canvas_buffer_size() {
        assert_eq!(frame_stride(m1()), 12_288);
        assert_eq!(frame_stride(m1()), m1().byte_len());
    }
}
