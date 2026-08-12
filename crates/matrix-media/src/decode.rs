//! Decode path: spawn, feed, bound, reap.
//!
//! This is where the isolation the crate claims is actually enforced. Everything the
//! [`crate::limits`] module can check is checked here, and in the order that matters:
//! the cheap refusals happen before a process exists, and the deadline is enforced by
//! killing it rather than by hoping it exits.

use crate::ffmpeg;
use crate::limits::{LimitError, Limits};
use crate::{MediaError, NormalizeParams};
use matrix_frame::FrameSequence;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};

/// The bytes a decode reads, in a form that can be handed to more than one subprocess.
///
/// A decode spawns two children in turn — the probe, then the decoder — and each needs
/// its own complete read of the source. Taking a `&[u8]` forced every feeder to own a
/// copy, because a spawned feeder task must be `'static`, so one decode held the
/// caller's buffer plus one full copy per child. That is invisible at the inline cap and
/// is not at `max_source_bytes`.
///
/// [`Source::File`] exists because bytes that arrive on a transfer path are already on
/// disk: streaming them into the child costs a pipe buffer rather than the whole file.
/// The path is always one this server minted. Caller input never selects it — decoding
/// a destination a caller named is the confused-deputy pattern this crate refuses.
#[derive(Debug, Clone)]
pub enum Source {
    /// Already resident. Shared between children rather than copied for each.
    Bytes(Arc<[u8]>),
    /// A server-minted path, streamed into each child.
    File(Arc<Path>),
}

impl Source {
    /// Take ownership of resident bytes. The one copy here replaces the per-child copies.
    pub fn bytes(source: impl AsRef<[u8]>) -> Self {
        Self::Bytes(Arc::from(source.as_ref()))
    }

    /// Read from a path this server minted.
    pub fn file(path: impl AsRef<Path>) -> Self {
        Self::File(Arc::from(path.as_ref()))
    }

    /// Length in bytes, so the source ceiling can still be applied before a fork.
    ///
    /// A staged file that cannot be stated is refused rather than attempted: the ceiling
    /// is unenforceable without a length, and an unenforceable ceiling is the case it
    /// exists for.
    pub(crate) async fn byte_len(&self) -> Result<u64, MediaError> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.len() as u64),
            Self::File(path) => tokio::fs::metadata(path)
                .await
                .map(|meta| meta.len())
                .map_err(|e| MediaError::Decoder(format!("could not measure the source: {e}"))),
        }
    }

    /// Feed one child its own read of the source, bounded by the length already cleared.
    ///
    /// `bound` is the byte count [`Limits::check_source_bytes`] passed. Applying it again
    /// here is what makes that ceiling a fact about the bytes the decoder receives rather
    /// than about a stat taken earlier: a path is measured once but opened once per child,
    /// so a source that grew in between would otherwise reach the decoder unbounded and
    /// with dimensions and duration that were never probed.
    ///
    /// Read and write failures are deliberately not the same thing. A child that exits
    /// early — refusing the source, or having taken the frames its argv bounded it to —
    /// closes the pipe mid-write, and that broken pipe is an ordinary outcome; the decode
    /// is judged on the child's exit status and the output collected. Failing to *read*
    /// the source is not ordinary: it hands the decoder a prefix, and a prefix decoded
    /// successfully is a truncated sequence reported as a whole one, which is exactly what
    /// this crate refuses elsewhere as `media_truncated_output`.
    fn spawn_feeder(
        &self,
        mut stdin: ChildStdin,
        bound: u64,
    ) -> tokio::task::JoinHandle<Result<(), MediaError>> {
        let source = self.clone();
        tokio::spawn(async move {
            let outcome = match source {
                Source::Bytes(bytes) => {
                    let end = usize::try_from(bound)
                        .unwrap_or(usize::MAX)
                        .min(bytes.len());
                    let _ = stdin.write_all(&bytes[..end]).await;
                    Ok(())
                }
                Source::File(path) => match tokio::fs::File::open(&*path).await {
                    Ok(file) => feed_bounded(file, &mut stdin, bound).await,
                    Err(e) => Err(MediaError::Decoder(format!(
                        "could not open the source: {e}"
                    ))),
                },
            };
            let _ = stdin.shutdown().await;
            outcome
        })
    }
}

/// Stream at most `bound` bytes into a child, keeping the two failure kinds apart.
async fn feed_bounded(
    file: tokio::fs::File,
    stdin: &mut ChildStdin,
    bound: u64,
) -> Result<(), MediaError> {
    let mut reader = file.take(bound);
    let mut buf = vec![0u8; 64 * 1024];
    let mut delivered = 0u64;
    loop {
        let read = reader
            .read(&mut buf)
            .await
            .map_err(|e| MediaError::Decoder(format!("could not read the source: {e}")))?;
        if read == 0 {
            // Ending early is the same defect as growing, from the other side: the child
            // was handed a prefix of what was measured, and a prefix it decodes cleanly
            // becomes a truncated sequence published as a whole one.
            return if delivered == bound {
                Ok(())
            } else {
                Err(MediaError::Decoder(format!(
                    "source ended after {delivered} of {bound} measured bytes"
                )))
            };
        }
        delivered += read as u64;
        // The child stopping its read is ordinary; only the read side above is a failure.
        if stdin.write_all(&buf[..read]).await.is_err() {
            return Ok(());
        }
    }
}

/// Take a finished feeder's verdict, surfacing only a failure to read the source.
///
/// Called on the path where output was collected in full. A feeder still blocked on a
/// child that has stopped reading is abandoned at the deadline rather than waited on,
/// because that block is the ordinary early-exit case rather than a decode failure.
async fn feeder_outcome(
    feeder: tokio::task::JoinHandle<Result<(), MediaError>>,
    deadline: tokio::time::Instant,
) -> Result<(), MediaError> {
    match tokio::time::timeout_at(deadline, feeder).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(joined)) => Err(MediaError::Decoder(format!(
            "source feeder failed: {joined}"
        ))),
        Err(_) => Ok(()),
    }
}

/// Apply an address-space ceiling to a child before it execs.
///
/// The deadline and the output ceiling bound time and what the parent holds. Neither
/// touches the decoder's own heap, and a source declaring an enormous frame reaches
/// FFmpeg's allocator before any of our checks run. `RLIMIT_AS` makes the kernel refuse
/// the allocation instead, which turns an exhausted container into a failed decode.
/// Linux only. macOS rejects `setrlimit(RLIMIT_AS)` outright, so applying it there
/// fails the spawn rather than bounding it. Non-Linux Unix builds retain the deadline
/// and output ceiling but have no address-space bound.
///
/// `RLIMIT_NPROC` is deliberately absent. It governs tasks for the real uid rather than
/// forks alone, so a zero limit denies FFmpeg the codec and filter worker threads it
/// uses for common media — the decode would fail before normalization on exactly the
/// arbitrary input this crate exists to accept. A limit that breaks the feature it is
/// meant to protect is not a trade worth making, and the deadline plus the address-space
/// bound already constrain a runaway child.
#[cfg(target_os = "linux")]
fn limit_address_space(command: &mut Command, bytes: u64) {
    use std::io;
    unsafe {
        command.pre_exec(move || {
            let as_limit = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &as_limit) != 0 {
                return Err(io::Error::last_os_error());
            }

            // A compromised decoder cannot gain privileges through a setuid binary it
            // manages to exec.
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn limit_address_space(_command: &mut Command, _bytes: u64) {}

/// Strip the environment the decoder inherits.
///
/// Applies everywhere, unlike the rlimits. FFmpeg needs nothing from the parent's
/// environment, and the parent's environment may contain sensitive configuration, so a
/// decoder compromised by hostile media should not inherit it. `RLIMIT_FSIZE` is
/// deliberately not set alongside this: FFmpeg's use of temporary files for a
/// pipe-to-pipe rawvideo decode cannot be verified from here, and a restriction that
/// might break every decode is worse than the narrow write vector it would close.
fn strip_environment(command: &mut Command) {
    command.env_clear();
}

/// Path to the decoder binary, overridable for a controlled runtime or test stand-in.
pub const DEFAULT_FFMPEG_BIN: &str = "ffmpeg";

/// Decode a source into a normalized frame sequence.
///
/// Refusals happen in cost order. `max_source_bytes` and the projected frame count are
/// checked before a process is spawned, so a source that cannot possibly be accepted
/// never costs a fork. `decode_timeout` is enforced by killing the child, because a
/// decoder wedged on malformed input will not honour a polite request.
///
/// `declared_duration` is what the caller believes the source runs for. It is a hint
/// used only to refuse early — ffmpeg's `-frames:v` bound and the post-decode frame
/// count are what actually constrain the output, so a source that lies about its
/// duration is still cut off.
pub async fn decode(
    source: &Source,
    declared_duration: Option<Duration>,
    declared_dimensions: Option<(u32, u32)>,
    params: &NormalizeParams,
    ffmpeg_bin: &str,
    ffprobe_bin: &str,
) -> Result<FrameSequence, MediaError> {
    decode_with_argv(
        source,
        declared_duration,
        declared_dimensions,
        params,
        (
            ffmpeg_bin,
            &ffmpeg::decode_argv(params.canvas, params.rate, &params.limits),
        ),
        (ffprobe_bin, &ffmpeg::probe_argv()),
    )
    .await
}

/// [`decode`] with both command lines supplied rather than derived.
///
/// Keeps the composition — probe first, enforce what it read, then decode — exercisable
/// against stand-in binaries. Testing the probe and the projection
/// separately leaves the order and the plumbing between them unasserted, which is how a
/// check that was never called passed for one that was.
pub(crate) async fn decode_with_argv(
    source: &Source,
    declared_duration: Option<Duration>,
    declared_dimensions: Option<(u32, u32)>,
    params: &NormalizeParams,
    decoder: (&str, &[String]),
    prober: (&str, &[String]),
) -> Result<FrameSequence, MediaError> {
    if let Some((width, height)) = declared_dimensions {
        params
            .limits
            .check_dimensions(width, height)
            .map_err(MediaError::Limit)?;
    }

    // Measured once and reused: the value the ceiling clears has to be the same value
    // both children are fed against. Measuring again would put a second, unchecked
    // reading between the refusal and the feed, which is the gap this bound closes.
    let source_bytes = source.byte_len().await?;
    params
        .limits
        .check_source_bytes(source_bytes)
        .map_err(MediaError::Limit)?;
    if let Some(duration) = declared_duration {
        params
            .limits
            .projected_frames(duration, params.rate.fps(), params.canvas.byte_len())
            .map_err(MediaError::Limit)?;
    }

    let deadline = tokio::time::Instant::now() + params.limits.decode_timeout;
    let probed = probe_with_argv(
        source,
        source_bytes,
        &params.limits,
        prober.0,
        prober.1,
        deadline,
    )
    .await?;

    if let Some(duration) = probed.duration.or(declared_duration) {
        params
            .limits
            .projected_frames(duration, params.rate.fps(), params.canvas.byte_len())
            .map_err(MediaError::Limit)?;
    }

    run_decoder_until(source, source_bytes, params, decoder.0, decoder.1, deadline).await
}

/// Read a source's real dimensions and check them against the limit.
///
/// A caller's declared dimensions bound only what the caller claims. The media itself
/// can encode a far larger frame, and FFmpeg allocates for that before the scale filter
/// reduces anything, so the limit means nothing until it is applied to what is actually
/// in the source.
///
/// A failed probe or unreadable dimension is refused before decode because the source
/// dimension cap cannot otherwise be enforced.
pub async fn probe_and_check_dimensions(
    source: &Source,
    limits: &Limits,
    ffprobe_bin: &str,
    deadline: tokio::time::Instant,
) -> Result<Probed, MediaError> {
    let source_bytes = source.byte_len().await?;
    probe_with_argv(
        source,
        source_bytes,
        limits,
        ffprobe_bin,
        &ffmpeg::probe_argv(),
        deadline,
    )
    .await
}

/// What the probe read out of the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probed {
    pub width: u32,
    pub height: u32,
    /// Absent for a still image and for containers that do not report one. The frame
    /// cap bounds those.
    pub duration: Option<Duration>,
}

/// [`probe_and_check_dimensions`] with the command line supplied rather than derived.
///
/// Crate-private for the same reason as [`run_decoder`]: it is a mechanism, not an
/// entry point.
///
/// Separated for the same reason as [`run_decoder_until`]: the spawn, parse, and
/// enforcement path is exercised against a stand-in binary, while the public entry
/// point supplies [`ffmpeg::probe_argv`]. A test that
/// reimplemented the parse and the check would stay green if this were disconnected.
pub(crate) async fn probe_with_argv(
    source: &Source,
    source_bytes: u64,
    limits: &Limits,
    bin: &str,
    argv: &[String],
    deadline: tokio::time::Instant,
) -> Result<Probed, MediaError> {
    let mut command = Command::new(bin);
    command
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    strip_environment(&mut command);
    limit_address_space(&mut command, limits.decoder_address_space_bytes);
    let mut child = command
        .spawn()
        .map_err(|e| MediaError::Decoder(format!("could not spawn {bin}: {e}")))?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let feeder = source.spawn_feeder(stdin, source_bytes);

    // The probe answers two questions — frame size and duration — in a few dozen
    // bytes, but its stdout is still shaped by the source: `-show_entries` prints per
    // stream, and a crafted container can declare thousands of streams. Collect a
    // bounded prefix and refuse anything past it rather than accumulating whatever the
    // child produces, the same discipline the decoder's stdout gets.
    const PROBE_OUTPUT_CEILING: usize = 16 * 1024;
    let collected = tokio::time::timeout_at(deadline, async {
        let mut raw: Vec<u8> = Vec::with_capacity(1024);
        let mut chunk = [0u8; 4096];
        loop {
            let n = stdout
                .read(&mut chunk)
                .await
                .map_err(|e| MediaError::Decoder(format!("reading probe output: {e}")))?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            if raw.len() > PROBE_OUTPUT_CEILING {
                return Err(MediaError::Decoder(
                    "probe produced more output than its questions can answer".into(),
                ));
            }
        }
        Ok(raw)
    })
    .await;

    let stdout_bytes = match collected {
        Err(_) => {
            feeder.abort();
            let _ = child.kill().await;
            return Err(MediaError::Limit(LimitError::DecodeTimeout(
                limits.decode_timeout,
            )));
        }
        Ok(Err(refused)) => {
            feeder.abort();
            let _ = child.kill().await;
            return Err(refused);
        }
        Ok(Ok(raw)) => raw,
    };

    // A probe that answered from a prefix answered about a source the decoder will not
    // see, so the dimensions and duration it reports would bound the wrong thing.
    feeder_outcome(feeder, deadline).await?;

    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Err(_) => {
            let _ = child.kill().await;
            return Err(MediaError::Limit(LimitError::DecodeTimeout(
                limits.decode_timeout,
            )));
        }
        Ok(Err(e)) => return Err(MediaError::Decoder(format!("probe failed: {e}"))),
        Ok(Ok(status)) => status,
    };

    // Fail closed. A probe that rejected the source, or whose output cannot be read as
    // a frame size, leaves the real dimensions unknown — and an unknown dimension is
    // exactly the case the cap exists for. Passing here would delete the limit whenever
    // the source is odd enough to confuse the probe.
    if !status.success() {
        return Err(MediaError::Decoder(format!(
            "probe rejected the source: exit {status}"
        )));
    }

    let raw = String::from_utf8_lossy(&stdout_bytes);
    let (width, height) = ffmpeg::parse_probe_dimensions(&raw).ok_or_else(|| {
        MediaError::Decoder("probe did not report a frame size for the source".into())
    })?;

    limits
        .check_dimensions(width, height)
        .map_err(MediaError::Limit)?;

    Ok(Probed {
        width,
        height,
        duration: ffmpeg::parse_probe_duration(&raw),
    })
}

/// Spawn a decoder, feed it, and reap it against a deadline the caller already started.
///
/// Crate-private and deliberately unbounded: it applies no size, duration, or dimension
/// check, having been given a source [`decode`] already cleared. Exposing it would offer
/// a public route into the decoder that skips the limit set this crate advertises as its
/// untrusted-media boundary. It takes its command line rather than deriving it so the
/// spawn, feed, deadline, and reap behaviour can be exercised against a stand-in binary.
pub(crate) async fn run_decoder_until(
    source: &Source,
    source_bytes: u64,
    params: &NormalizeParams,
    bin: &str,
    argv: &[String],
    deadline: tokio::time::Instant,
) -> Result<FrameSequence, MediaError> {
    let mut command = Command::new(bin);
    command
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    strip_environment(&mut command);
    limit_address_space(&mut command, params.limits.decoder_address_space_bytes);
    let mut child = command
        .spawn()
        .map_err(|e| MediaError::Decoder(format!("could not spawn {bin}: {e}")))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| MediaError::Decoder("decoder stdin was not piped".into()))?;

    // Feeding and reaping must be concurrent. A source larger than the pipe buffer
    // deadlocks if the parent writes it all before reading, because the child blocks
    // writing output nobody is draining.
    let feeder = source.spawn_feeder(stdin, source_bytes);

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| MediaError::Decoder("decoder stdout was not piped".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| MediaError::Decoder("decoder stderr was not piped".into()))?;

    // Hard ceiling on what the parent will hold. Accumulating the child's whole output
    // and checking the frame count afterwards makes the cap unenforceable: a decoder
    // that ignores its argv bounds exhausts memory here before any check runs. One
    // frame of headroom past the cap keeps overflow detectable.
    let frame_bytes = params.canvas.byte_len();
    let cap = params
        .limits
        .frame_cap_for_frame_size(params.rate.fps(), frame_bytes);
    if cap == 0 {
        let _ = child.kill().await;
        return Err(MediaError::Limit(LimitError::FrameExceedsMemoryCeiling {
            frame_bytes,
            limit: params.limits.max_normalized_bytes,
        }));
    }
    let ceiling = cap.saturating_add(1).saturating_mul(frame_bytes as u64);

    // stderr is drained concurrently and separately bounded, so a chatty decoder cannot
    // wedge on a full pipe while the parent is reading stdout.
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Keep a bounded prefix but keep draining: stopping the read leaves
                    // a chatty decoder blocked on a full stderr pipe until the deadline.
                    if buf.len() < 8192 {
                        let room = 8192 - buf.len();
                        buf.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        buf
    });

    let collected = tokio::time::timeout_at(deadline, async {
        // Sized up front to the ceiling plus one read. Growing by extension doubles
        // capacity, and a doubling that lands just past the ceiling holds nearly twice
        // it in allocation while the length check still passes — pre-sizing is what
        // makes the documented byte ceiling the actual allocation bound.
        let mut raw: Vec<u8> = Vec::with_capacity(
            usize::try_from(ceiling)
                .unwrap_or(usize::MAX)
                .saturating_add(64 * 1024),
        );
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = stdout
                .read(&mut chunk)
                .await
                .map_err(|e| MediaError::Decoder(format!("reading decoder output: {e}")))?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            if raw.len() as u64 > ceiling {
                return Err(MediaError::Limit(LimitError::TooManyFrames {
                    actual: raw.len() as u64 / params.canvas.byte_len().max(1) as u64,
                    limit: cap,
                }));
            }
        }
        Ok(raw)
    })
    .await;

    let raw = match collected {
        Err(_) => {
            feeder.abort();
            let _ = child.kill().await;
            return Err(MediaError::Limit(LimitError::DecodeTimeout(
                params.limits.decode_timeout,
            )));
        }
        Ok(Err(e)) => {
            feeder.abort();
            let _ = child.kill().await;
            return Err(e);
        }
        Ok(Ok(raw)) => raw,
    };

    // Checked before the exit status and before assembly: a decoder can exit 0 on a
    // prefix, and assembling that would publish a truncated sequence as a whole one.
    feeder_outcome(feeder, deadline).await?;

    let status = tokio::time::timeout_at(deadline, child.wait())
        .await
        .map_err(|_| MediaError::Limit(LimitError::DecodeTimeout(params.limits.decode_timeout)))?
        .map_err(|e| MediaError::Decoder(format!("decoder failed: {e}")))?;

    if !status.success() {
        let detail = stderr_task.await.unwrap_or_default();
        let detail = String::from_utf8_lossy(&detail);
        let detail: String = detail.chars().take(400).collect();
        return Err(MediaError::Decoder(format!(
            "decoder exited {status}: {detail}"
        )));
    }
    stderr_task.abort();

    // FFmpeg was asked for one frame beyond the cap. Receiving it means the source ran
    // past the limit and what arrived is a prefix, so it is refused rather than played.
    let produced = raw.len() as u64 / params.canvas.byte_len().max(1) as u64;
    if produced > cap {
        return Err(MediaError::Limit(LimitError::TooManyFrames {
            actual: produced,
            limit: cap,
        }));
    }

    crate::sequence_from_owned(raw, params)
}

/// Refusals that do not need a process, exposed so a caller can reject a submission
/// before accepting its bytes.
///
/// `frame_bytes` is the target canvas's frame size, which is what makes the projection
/// byte-aware: the same declared duration is fine on a small canvas and past the memory
/// ceiling on a large one.
pub fn preflight(
    source_len: u64,
    declared_duration: Option<Duration>,
    declared_dimensions: Option<(u32, u32)>,
    fps: u16,
    frame_bytes: usize,
    limits: &Limits,
) -> Result<(), LimitError> {
    limits.check_source_bytes(source_len)?;
    if let Some((width, height)) = declared_dimensions {
        limits.check_dimensions(width, height)?;
    }
    if let Some(duration) = declared_duration {
        limits.projected_frames(duration, fps, frame_bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_frame::{Canvas, Rate};

    fn soon() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(10)
    }

    const PROBE_PAYLOAD: &[u8] = b"source";

    /// Drive the real probe path against a stand-in command.
    async fn probe(bin: &str, args: &[&str]) -> Result<Probed, MediaError> {
        let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        probe_with_argv(
            &Source::bytes(PROBE_PAYLOAD),
            PROBE_PAYLOAD.len() as u64,
            &Limits::default(),
            bin,
            &argv,
            soon(),
        )
        .await
    }

    #[tokio::test]
    async fn the_probe_refuses_a_frame_over_the_dimension_cap() {
        // `printf` writes a size past max_source_dimension and ignores stdin.
        let err = probe("printf", &["99999x99999"])
            .await
            .expect_err("over the cap");
        assert_eq!(err.code(), "media_dimensions_too_large");
    }

    #[tokio::test]
    async fn the_probe_accepts_a_frame_inside_the_cap() {
        let probed = probe("printf", &["1920x1080"])
            .await
            .expect("inside the cap");
        assert_eq!((probed.width, probed.height), (1920, 1080));
        assert_eq!(probed.duration, None, "no second line means no duration");
    }

    #[tokio::test]
    async fn a_probe_flooding_its_stdout_is_refused_rather_than_buffered() {
        // The probe is asked two questions whose answers fit in a line each. A source
        // engineered to make it print far more — thousands of declared streams — must
        // not grow a buffer in the parent. `head` ignores stdin and emits 64 KiB.
        let err = probe("head", &["-c", "65536", "/dev/zero"])
            .await
            .expect_err("a flooding probe must be refused");
        assert_eq!(err.code(), "media_decoder_failed");
    }

    #[tokio::test]
    async fn the_probe_reads_a_duration_when_the_source_reports_one() {
        let probed = probe("printf", &["1920x1080\\n8.0\\n"])
            .await
            .expect("inside the cap");
        assert_eq!(probed.duration, Some(Duration::from_secs(8)));
    }

    #[tokio::test]
    async fn decode_refuses_an_over_long_source_the_caller_never_declared() {
        // The probe stand-in reports a duration far past max_duration, and the decoder
        // stand-in would succeed if it were reached. The duration must therefore be
        // refused before decode starts.
        let params = params();
        let over = params.limits.max_duration.as_secs() + 600;
        let probe_out = format!("64x64\\n{over}\\n");

        let err = decode_with_argv(
            &Source::bytes(b"x"),
            None,
            None,
            &params,
            ("cat", &[]),
            ("printf", &[probe_out]),
        )
        .await
        .expect_err("the probed duration must refuse this before any decode");
        assert_eq!(err.code(), "media_duration_too_long");
    }

    #[tokio::test]
    async fn decode_accepts_a_source_whose_probed_duration_is_inside_the_cap() {
        // The other half: a permissible duration must not be refused, or the check
        // above would pass for the wrong reason.
        let params = NormalizeParams {
            canvas: Canvas::new(2, 2).expect("valid"),
            ..params()
        };
        let payload = vec![9u8; params.canvas.byte_len()];
        let sequence = decode_with_argv(
            &Source::bytes(&payload),
            None,
            None,
            &params,
            ("cat", &[]),
            ("printf", &["2x2\\n1.0\\n".to_string()]),
        )
        .await
        .expect("inside every cap");
        assert_eq!(sequence.len(), 1);
    }

    #[tokio::test]
    async fn the_probe_refuses_when_it_exits_nonzero() {
        let err = probe("false", &[]).await.expect_err("rejected source");
        assert_eq!(err.code(), "media_decoder_failed");
    }

    #[tokio::test]
    async fn the_probe_refuses_when_it_reports_no_size() {
        // Unknown dimensions are the case the cap exists for, so this must not pass.
        let err = probe("true", &[]).await.expect_err("no size reported");
        assert_eq!(err.code(), "media_decoder_failed");
    }

    #[tokio::test]
    async fn the_probe_refuses_when_its_binary_is_missing() {
        let err = probe("definitely-not-a-real-probe", &[])
            .await
            .expect_err("missing probe must not delete the limit");
        assert_eq!(err.code(), "media_decoder_failed");
    }

    #[tokio::test]
    async fn decode_refuses_before_a_decoder_runs_when_the_probe_refuses() {
        // decode wires the probe ahead of the decoder; a probe that cannot answer must
        // stop the ingest rather than let the decoder see the source.
        let err = decode(
            &Source::bytes(b"x"),
            None,
            None,
            &params(),
            "definitely-not-a-real-decoder",
            "definitely-not-a-real-probe",
        )
        .await
        .expect_err("probe refuses first");
        assert_eq!(err.code(), "media_decoder_failed");
    }

    fn params() -> NormalizeParams {
        NormalizeParams {
            canvas: Canvas::new(64, 64).expect("valid"),
            rate: Rate::new(25).expect("valid"),
            limits: Limits::default(),
        }
    }

    #[test]
    fn preflight_refuses_an_oversized_source_without_a_process() {
        let limits = Limits::default();
        let err = preflight(limits.max_source_bytes + 1, None, None, 25, 12_288, &limits)
            .expect_err("over the byte cap");
        assert_eq!(err.code(), "media_source_too_large");
    }

    #[test]
    fn preflight_refuses_oversized_dimensions() {
        let limits = Limits::default();
        let err = preflight(1024, None, Some((9000, 64)), 25, 12_288, &limits)
            .expect_err("over the dimension cap");
        assert_eq!(err.code(), "media_dimensions_too_large");
    }

    #[test]
    fn preflight_refuses_a_source_whose_frames_would_blow_the_budget() {
        let limits = Limits {
            max_duration: Duration::from_secs(600),
            ..Limits::default()
        };
        let err = preflight(
            1024,
            Some(Duration::from_secs(300)),
            None,
            25,
            12_288,
            &limits,
        )
        .expect_err("7500 frames over an 1800 cap");
        assert_eq!(err.code(), "media_too_many_frames");
    }

    #[test]
    fn preflight_accepts_a_source_inside_every_bound() {
        assert!(
            preflight(
                1024 * 1024,
                Some(Duration::from_secs(10)),
                Some((1920, 1080)),
                25,
                12_288,
                &Limits::default()
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn decode_refuses_an_oversized_source_before_spawning_anything() {
        let params = NormalizeParams {
            limits: Limits {
                max_source_bytes: 16,
                ..Limits::default()
            },
            ..params()
        };
        // Both binaries are absent: reaching either would surface a decoder error.
        let err = decode(
            &Source::bytes([0u8; 64]),
            None,
            None,
            &params,
            "definitely-not-a-real-binary",
            "definitely-not-a-real-probe",
        )
        .await
        .expect_err("refused before spawn");
        assert_eq!(err.code(), "media_source_too_large");
    }

    #[tokio::test]
    async fn decode_refuses_an_over_long_source_before_spawning_anything() {
        // Both binaries are absent, so a decoder error here would mean the duration
        // bound ran too late.
        let err = decode(
            &Source::bytes(b"x"),
            Some(Duration::from_secs(3600)),
            None,
            &params(),
            "definitely-not-a-real-binary",
            "definitely-not-a-real-probe",
        )
        .await
        .expect_err("refused before spawn");
        assert_eq!(err.code(), "media_duration_too_long");
    }

    #[tokio::test]
    async fn an_oversized_source_is_refused_before_the_probe_runs() {
        let params = NormalizeParams {
            limits: Limits {
                max_source_bytes: 8,
                ..Limits::default()
            },
            ..params()
        };
        // Both binaries are absent: reaching either would surface a decoder error.
        let err = decode(
            &Source::bytes([0u8; 64]),
            None,
            None,
            &params,
            "definitely-not-a-real-binary",
            "definitely-not-a-real-probe",
        )
        .await
        .expect_err("refused before any subprocess");
        assert_eq!(err.code(), "media_source_too_large");
    }

    #[tokio::test]
    async fn a_canvas_whose_frame_exceeds_the_memory_ceiling_is_refused() {
        // Not silently truncated to one frame: holding the raw buffer and the clone
        // assembly makes would exceed the documented ceiling either way.
        let params = NormalizeParams {
            canvas: Canvas::new(1024, 1024).expect("valid"),
            limits: Limits {
                max_normalized_bytes: 64 * 1024,
                ..Limits::default()
            },
            ..params()
        };
        let err = run_decoder_until(&Source::bytes(b"x"), 1, &params, "cat", &[], soon())
            .await
            .expect_err("one frame is larger than the whole ceiling");
        assert_eq!(err.code(), "media_frame_exceeds_memory_ceiling");
    }

    #[tokio::test]
    async fn a_missing_decoder_binary_is_a_decoder_error_not_a_panic() {
        let err = run_decoder_until(
            &Source::bytes(b"x"),
            1,
            &params(),
            "definitely-not-a-real-binary",
            &[],
            soon(),
        )
        .await
        .expect_err("cannot spawn");
        assert_eq!(err.code(), "media_decoder_failed");
    }

    #[tokio::test]
    async fn a_decoder_that_outlives_its_deadline_is_killed_and_reported() {
        let params = NormalizeParams {
            limits: Limits {
                decode_timeout: Duration::from_millis(150),
                ..Limits::default()
            },
            ..params()
        };
        // `sleep` stands in for a wedged decoder: it ignores stdin and never exits in
        // time, which is the shape of the failure the deadline exists for. The deadline
        // is derived from the configured timeout, as `decode` does — passing an
        // unrelated one would leave the setting untested and the test slow.
        let deadline = tokio::time::Instant::now() + params.limits.decode_timeout;
        let err = run_decoder_until(
            &Source::bytes(b"x"),
            1,
            &params,
            "sleep",
            &["30".into()],
            deadline,
        )
        .await
        .expect_err("deadline must fire");
        assert_eq!(err.code(), "media_decode_timeout");
    }

    #[tokio::test]
    async fn a_decoder_exiting_nonzero_reports_failure_rather_than_empty_output() {
        // `false` exits 1 immediately, standing in for a decoder rejecting a source.
        let err = run_decoder_until(&Source::bytes(b"x"), 1, &params(), "false", &[], soon())
            .await
            .expect_err("nonzero exit");
        assert_eq!(err.code(), "media_decoder_failed");
    }

    #[tokio::test]
    async fn a_decoder_producing_nothing_is_reported_as_no_frames() {
        // `true` exits 0 having written no output.
        let err = run_decoder_until(&Source::bytes(b"x"), 1, &params(), "true", &[], soon())
            .await
            .expect_err("no frames");
        assert_eq!(err.code(), "media_no_frames");
    }

    #[tokio::test]
    async fn a_decoder_producing_whole_frames_yields_a_sequence() {
        let params = NormalizeParams {
            canvas: Canvas::new(2, 2).expect("valid"),
            ..params()
        };
        // `cat` echoes stdin, so a payload sized to two 2x2 frames round-trips as one.
        let payload = vec![7u8; params.canvas.byte_len() * 2];
        let sequence = run_decoder_until(
            &Source::bytes(&payload),
            payload.len() as u64,
            &params,
            "cat",
            &[],
            soon(),
        )
        .await
        .expect("two whole frames");

        assert_eq!(sequence.len(), 2);
        assert!(
            sequence
                .get(0)
                .expect("frame 0")
                .as_rgb()
                .iter()
                .all(|&b| b == 7)
        );
    }

    /// A scratch file holding `bytes`, removed when the guard drops.
    struct Staged(std::path::PathBuf);

    impl Staged {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "matrix-source-{}-{}-{name}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::write(&path, bytes).expect("scratch file");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn a_file_source_decodes_to_what_the_same_bytes_decode_to_in_memory() {
        // The whole point of the file variant is that it changes how bytes reach the
        // child, not what the child receives. Decoding the same payload both ways must
        // therefore be indistinguishable in the output.
        let params = NormalizeParams {
            canvas: Canvas::new(2, 2).expect("valid"),
            ..params()
        };
        let payload = vec![5u8; params.canvas.byte_len() * 3];
        let staged = Staged::new("roundtrip", &payload);

        let from_memory = run_decoder_until(
            &Source::bytes(&payload),
            payload.len() as u64,
            &params,
            "cat",
            &[],
            soon(),
        )
        .await
        .expect("in-memory source decodes");
        let from_file = run_decoder_until(
            &Source::file(staged.path()),
            payload.len() as u64,
            &params,
            "cat",
            &[],
            soon(),
        )
        .await
        .expect("file source decodes");

        assert_eq!(from_file.len(), from_memory.len());
        for index in 0..from_memory.len() {
            assert_eq!(
                from_file.get(index).expect("frame").as_rgb(),
                from_memory.get(index).expect("frame").as_rgb(),
                "frame {index} differs between the two source forms"
            );
        }
    }

    #[tokio::test]
    async fn a_source_that_cannot_be_read_fails_the_decode_rather_than_yielding_a_prefix() {
        // A directory stats fine and opens fine on Unix, then fails on read — a stand-in
        // for any source that goes unreadable after it was measured. The decoder here
        // would exit 0 having seen nothing, so without the feeder's verdict this would be
        // indistinguishable from an ordinary empty decode and a partial read would be
        // indistinguishable from a whole one.
        let params = NormalizeParams {
            canvas: Canvas::new(2, 2).expect("valid"),
            ..params()
        };
        let unreadable = std::env::temp_dir();

        let err = run_decoder_until(
            &Source::file(&unreadable),
            4096,
            &params,
            "cat",
            &[],
            soon(),
        )
        .await
        .expect_err("an unreadable source must not decode");

        assert_eq!(err.code(), "media_decoder_failed");
        assert!(
            err.to_string().contains("could not read the source"),
            "the refusal names the read failure rather than the empty output: {err}"
        );
    }

    #[tokio::test]
    async fn a_file_source_shorter_than_its_measurement_fails_rather_than_decoding_a_prefix() {
        // The other side of the bound: a source that ended early hands the decoder a
        // prefix, and `cat` would echo it cleanly into a shorter but perfectly valid
        // sequence. Passing a bound above the file's real size is what a file truncated
        // after byte_len() looks like to the feeder.
        let params = NormalizeParams {
            canvas: Canvas::new(2, 2).expect("valid"),
            ..params()
        };
        let frame = params.canvas.byte_len();
        let staged = Staged::new("shrunk", &vec![6u8; frame]);

        let err = run_decoder_until(
            &Source::file(staged.path()),
            (frame * 3) as u64,
            &params,
            "cat",
            &[],
            soon(),
        )
        .await
        .expect_err("a short source must not decode as a whole one");

        assert_eq!(err.code(), "media_decoder_failed");
        assert!(
            err.to_string().contains("measured bytes"),
            "the refusal names the short read rather than the frame count: {err}"
        );
    }

    #[tokio::test]
    async fn a_file_source_is_fed_only_the_bytes_that_were_measured() {
        // The ceiling is checked against one measurement but the path is opened once per
        // child, so the bound has to travel with the feed. A source larger than the
        // measurement must reach the decoder truncated to it, not whole.
        let params = NormalizeParams {
            canvas: Canvas::new(2, 2).expect("valid"),
            ..params()
        };
        let frame = params.canvas.byte_len();
        let staged = Staged::new("grown", &vec![4u8; frame * 3]);

        // `cat` echoes whatever it is fed, so the frame count is the delivered byte count.
        let sequence = run_decoder_until(
            &Source::file(staged.path()),
            frame as u64,
            &params,
            "cat",
            &[],
            soon(),
        )
        .await
        .expect("the measured prefix decodes");

        assert_eq!(
            sequence.len(),
            1,
            "only the measured byte count may reach the decoder"
        );
    }

    #[tokio::test]
    async fn an_oversized_file_source_is_refused_before_the_probe_runs() {
        // The pre-spawn size ceiling has to survive the move to a file handle, or a
        // staged source would reach the decoder unbounded. Both binaries are absent, so
        // reaching either would surface a decoder error instead.
        let params = NormalizeParams {
            limits: Limits {
                max_source_bytes: 8,
                ..Limits::default()
            },
            ..params()
        };
        let staged = Staged::new("oversize", &[0u8; 64]);

        let err = decode(
            &Source::file(staged.path()),
            None,
            None,
            &params,
            "definitely-not-a-real-binary",
            "definitely-not-a-real-probe",
        )
        .await
        .expect_err("refused before spawn");
        assert_eq!(err.code(), "media_source_too_large");
    }

    #[tokio::test]
    async fn a_payload_larger_than_a_pipe_buffer_does_not_deadlock() {
        // The feeder and the reaper run concurrently; writing everything before reading
        // would wedge here, because the child blocks writing output nobody drains.
        let params = NormalizeParams {
            canvas: Canvas::new(64, 64).expect("valid"),
            ..params()
        };
        let payload = vec![3u8; params.canvas.byte_len() * 40]; // ~491 KB, well past 64 KB.
        let sequence = tokio::time::timeout(
            Duration::from_secs(20),
            run_decoder_until(
                &Source::bytes(&payload),
                payload.len() as u64,
                &params,
                "cat",
                &[],
                soon(),
            ),
        )
        .await
        .expect("must not hang")
        .expect("whole frames");

        assert_eq!(sequence.len(), 40);
    }
}
