//! Shared engine state and the playback supervisor.
//!
//! `2026-07-28` is stateless: the transport builds a fresh handler per request, so
//! nothing that must outlive a request can live in the handler. Everything durable is
//! held here behind an [`Arc`] the handler factory clones, and every handle a caller
//! receives is a key into this state rather than a session.

use matrix_device::{DdpSender, WledClient};
use matrix_frame::{Canvas, FrameSequence, Rate};
use matrix_playout::{FpsFeedback, Playout, PowerBudget};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("no asset with handle {0}")]
    UnknownAsset(String),

    #[error("nothing is playing")]
    NothingPlaying,

    #[error("the panel is unreachable: {0}")]
    Device(String),

    #[error("playback failed: {0}")]
    Playback(String),

    #[error("the decode queue is full; retry after current ingests finish")]
    Busy,
}

impl EngineError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownAsset(_) => "matrix_unknown_asset",
            Self::NothingPlaying => "matrix_nothing_playing",
            Self::Device(_) => "matrix_device_unreachable",
            Self::Playback(_) => "matrix_playback_failed",
            Self::Busy => "matrix_busy",
        }
    }
}

/// A normalized asset held in memory, addressed by an opaque handle.
#[derive(Debug, Clone)]
pub struct Asset {
    pub handle: String,
    pub sequence: FrameSequence,
    pub source_bytes: u64,
    pub media_type: String,
}

/// The describable part of an asset, without its frames.
///
/// Listing and status are read paths; cloning a `FrameSequence` to answer them would
/// transiently duplicate every resident frame buffer — a full store is on the order of
/// the whole aggregate memory budget — to report a handful of numbers.
#[derive(Debug, Clone)]
pub struct AssetMeta {
    pub handle: String,
    pub frames: usize,
    pub fps: u16,
    pub duration: Duration,
    pub source_bytes: u64,
    pub media_type: String,
}

impl AssetMeta {
    fn of(asset: &Asset) -> Self {
        Self {
            handle: asset.handle.clone(),
            frames: asset.sequence.len(),
            fps: asset.sequence.rate().fps(),
            duration: asset.sequence.duration(),
            source_bytes: asset.source_bytes,
            media_type: asset.media_type.clone(),
        }
    }
}

/// What is currently on the panel.
#[derive(Debug)]
struct Playback {
    handle: String,
    asset: String,
    task: JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
}

/// Resident assets and their insertion order, so eviction is oldest-first.
#[derive(Debug, Default)]
struct AssetStore {
    by_handle: HashMap<String, Asset>,
    order: std::collections::VecDeque<String>,
}

/// Ceiling on resident normalized assets.
///
/// Per-ingest limits bound one asset; without an aggregate bound, repeated small
/// submissions accumulate resident multi-megabyte sequences until available memory is
/// exhausted. Eight assets at the default per-asset normalized ceiling remains under
/// 200 MiB before playback and process overhead.
const MAX_RESIDENT_ASSETS: usize = 8;

/// How long a submission may wait for a decode slot before being refused.
///
/// Each decode is deadline-bounded, so this covers riding out one full decode ahead in
/// the queue; a longer queue means the server is saturated and the caller should hear
/// that rather than hold a request slot indefinitely.
const DECODE_QUEUE_DEADLINE: Duration = Duration::from_secs(45);

/// Everything that survives a request.
#[derive(Debug)]
pub struct Engine {
    pub canvas: Canvas,
    pub target_rate: Rate,
    wled: WledClient,
    ddp_target: SocketAddr,
    feedback: FpsFeedback,
    assets: Mutex<AssetStore>,
    playback: Mutex<Option<Playback>>,
    counter: AtomicU64,
    /// Distinguishes this process's handles from a predecessor's.
    ///
    /// A counter alone restarts at 1, so a well-formed handle held across a restart
    /// would silently alias a different asset or playback. With a per-process token in
    /// every handle, a stale handle fails the lookup and the caller gets the unknown-
    /// handle error instead of someone else's asset.
    instance: String,
    /// Aggregate bound on concurrent decodes.
    ///
    /// Each decode is individually bounded — deadline, address space, output ceiling —
    /// but nothing in the engine crates limits how many run at once, and each one is a
    /// subprocess competing with the playout pump for CPU. Two concurrent decodes keep
    /// a second caller from waiting on the first without letting a burst of submissions
    /// multiply the per-decode budgets.
    decodes: tokio::sync::Semaphore,
    /// Submissions currently waiting for a decode permit; see `acquire_decode_slot`.
    decode_waiters: AtomicU64,
}

/// Waiters allowed behind the two decode permits before submissions are refused
/// outright: enough to absorb a small burst, small enough that a caller's payload never
/// sits parked behind more than two full decode deadlines.
const MAX_DECODE_WAITERS: u64 = 4;

/// A random-enough per-process token without a dependency: `RandomState` is seeded from
/// the platform CSPRNG per instance, and hashing nothing distills that seed to a u64.
/// This is an identifier namespace, not a security boundary — handles are not secrets.
fn instance_token() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let token = RandomState::new().build_hasher().finish();
    format!("{:08x}", token as u32)
}

impl Engine {
    pub fn new(
        canvas: Canvas,
        target_rate: Rate,
        wled: WledClient,
        ddp_target: SocketAddr,
    ) -> Arc<Self> {
        Arc::new(Self {
            canvas,
            target_rate,
            wled,
            ddp_target,
            feedback: FpsFeedback::new(),
            assets: Mutex::new(AssetStore::default()),
            playback: Mutex::new(None),
            counter: AtomicU64::new(1),
            instance: instance_token(),
            decodes: tokio::sync::Semaphore::new(2),
            decode_waiters: AtomicU64::new(0),
        })
    }

    pub fn wled(&self) -> &WledClient {
        &self.wled
    }

    pub fn feedback(&self) -> &FpsFeedback {
        &self.feedback
    }

    /// Hold a decode slot for the duration of one ingest, or refuse when the queue is
    /// saturated rather than parking the request indefinitely.
    ///
    /// The waiter population is bounded as well as the wait: permits cap running
    /// decoders, but an unbounded waiter queue would let a burst of submissions park
    /// request slots and retained payloads behind the deadline. Beyond a short queue
    /// the honest answer is busy, immediately.
    pub async fn acquire_decode_slot(
        &self,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, EngineError> {
        struct Waiting<'a>(&'a AtomicU64);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }

        if self.decode_waiters.fetch_add(1, Ordering::Relaxed) >= MAX_DECODE_WAITERS {
            self.decode_waiters.fetch_sub(1, Ordering::Relaxed);
            return Err(EngineError::Busy);
        }
        let _waiting = Waiting(&self.decode_waiters);

        tokio::time::timeout(DECODE_QUEUE_DEADLINE, self.decodes.acquire())
            .await
            .map_err(|_| EngineError::Busy)?
            .map_err(|_| EngineError::Busy)
    }

    fn mint(&self, prefix: &str) -> String {
        format!(
            "{prefix}_{}_{}",
            self.instance,
            self.counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A fresh asset handle, for a caller assembling an [`Asset`] it will store
    /// through [`Engine::store_prepared_asset`] once its side effects have succeeded.
    pub fn mint_asset_handle(&self) -> String {
        self.mint("asset")
    }

    pub async fn store_asset(
        &self,
        sequence: FrameSequence,
        source_bytes: u64,
        media_type: String,
    ) -> Asset {
        let asset = Asset {
            handle: self.mint("asset"),
            sequence,
            source_bytes,
            media_type,
        };
        self.store_prepared_asset(asset.clone()).await;
        asset
    }

    /// Commit an already-assembled asset under the same eviction ceiling.
    pub async fn store_prepared_asset(&self, asset: Asset) {
        let mut store = self.assets.lock().await;
        // Oldest-first eviction at the ceiling. Playback owns a clone of its sequence,
        // so evicting the asset that happens to be playing cannot rip frames from the
        // pump — its handle simply stops resolving.
        while store.order.len() >= MAX_RESIDENT_ASSETS {
            if let Some(oldest) = store.order.pop_front() {
                store.by_handle.remove(&oldest);
            }
        }
        store.order.push_back(asset.handle.clone());
        store.by_handle.insert(asset.handle.clone(), asset);
    }

    pub async fn asset(&self, handle: &str) -> Option<Asset> {
        self.assets.lock().await.by_handle.get(handle).cloned()
    }

    /// Remove one asset. True when it was resident.
    pub async fn remove_asset(&self, handle: &str) -> bool {
        let mut store = self.assets.lock().await;
        let removed = store.by_handle.remove(handle).is_some();
        if removed {
            store.order.retain(|h| h != handle);
        }
        removed
    }

    /// Metadata for every resident asset, oldest first. Never clones frame data.
    pub async fn asset_metas(&self) -> Vec<AssetMeta> {
        let store = self.assets.lock().await;
        store
            .order
            .iter()
            .filter_map(|handle| store.by_handle.get(handle).map(AssetMeta::of))
            .collect()
    }

    /// Resident asset count, for status. Never clones anything.
    pub async fn asset_count(&self) -> usize {
        self.assets.lock().await.order.len()
    }

    /// Poll the device once and publish what it reports.
    ///
    /// The published framerate is what paces the pump, so this is not diagnostic — a
    /// server that never polls leaves the pump running on stale feedback.
    pub async fn poll_device(&self) -> Result<matrix_device::DeviceInfo, EngineError> {
        let info = self
            .wled
            .info()
            .await
            .map_err(|e| EngineError::Device(format!("{}: {e}", e.code())))?;
        self.feedback.publish(info.leds.fps);
        Ok(info)
    }

    /// Start playing an asset, replacing whatever was playing.
    ///
    /// The returned handle names this playback; a later stop naming a stale handle is
    /// refused rather than cancelling whatever happens to be running now.
    pub async fn play(
        self: &Arc<Self>,
        asset_handle: &str,
        looping: bool,
    ) -> Result<String, EngineError> {
        let asset = self
            .asset(asset_handle)
            .await
            .ok_or_else(|| EngineError::UnknownAsset(asset_handle.to_string()))?;
        self.play_asset(&asset, looping).await
    }

    /// Start playing an asset the caller already holds.
    ///
    /// Exists so a path that mints its own asset can start playback before committing
    /// the asset to the store — a failed start then costs nothing and evicts nothing.
    pub async fn play_asset(
        self: &Arc<Self>,
        asset: &Asset,
        looping: bool,
    ) -> Result<String, EngineError> {
        let info = self.poll_device().await?;
        let budget = PowerBudget::from_device(info.leds.max_power_ma);

        let sender = DdpSender::connect(self.ddp_target)
            .await
            .map_err(|e| EngineError::Device(format!("{}: {e}", e.code())))?;

        let handle = self.mint("play");
        let cancel = tokio_util::sync::CancellationToken::new();
        let child = cancel.clone();
        let feedback = self.feedback.clone();
        let target_rate = self.target_rate;
        let sequence = asset.sequence.clone();
        let engine = Arc::clone(self);
        let task_handle = handle.clone();

        // Replacement, spawn, and installation happen under one lock acquisition.
        // Taking the old record and installing the new one separately lets two
        // concurrent plays each pass the stop, then overwrite each other's record —
        // the overwritten task keeps streaming with nothing pointing at it and no stop
        // can ever reach it. Spawning inside the lock also means the task's own
        // completion cleanup below cannot observe the record before it is installed.
        let mut guard = self.playback.lock().await;
        if let Some(previous) = guard.take() {
            previous.cancel.cancel();
            previous.task.abort();
        }

        let task = tokio::spawn(async move {
            let mut playout = Playout::new(sender, target_rate, budget);
            tokio::select! {
                _ = child.cancelled() => return,
                result = playout.run(&sequence, looping, &feedback, None) => {
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "playback ended on a sink failure");
                    }
                }
            }
            // A run that finished or failed must stop advertising itself as playing;
            // only explicit stop and replacement remove the record otherwise. Guarded
            // by handle so a replacement installed while this ran is left alone.
            let mut guard = engine.playback.lock().await;
            if guard.as_ref().is_some_and(|p| p.handle == task_handle) {
                *guard = None;
            }
        });

        *guard = Some(Playback {
            handle: handle.clone(),
            asset: asset.handle.clone(),
            task,
            cancel,
        });
        Ok(handle)
    }

    pub async fn stop(&self, playback_handle: Option<&str>) -> Result<String, EngineError> {
        let mut guard = self.playback.lock().await;
        let current = guard.as_ref().ok_or(EngineError::NothingPlaying)?;

        if let Some(requested) = playback_handle
            && requested != current.handle
        {
            return Err(EngineError::NothingPlaying);
        }

        let stopped = current.handle.clone();
        if let Some(previous) = guard.take() {
            previous.cancel.cancel();
            previous.task.abort();
        }
        Ok(stopped)
    }

    /// Handle and asset of what is playing, if anything.
    pub async fn playing(&self) -> Option<(String, String)> {
        self.playback
            .lock()
            .await
            .as_ref()
            .map(|p| (p.handle.clone(), p.asset.clone()))
    }

    pub async fn set_brightness(&self, level: u8) -> Result<(), EngineError> {
        self.wled
            .set_brightness(level)
            .await
            .map_err(|e| EngineError::Device(format!("{}: {e}", e.code())))
    }

    pub async fn set_power(&self, on: bool) -> Result<(), EngineError> {
        self.wled
            .set_power(on)
            .await
            .map_err(|e| EngineError::Device(format!("{}: {e}", e.code())))
    }
}

/// How often the device is polled while something is playing.
pub const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Poll the device forever, publishing its framerate.
///
/// Spawned once at startup. Without it `FpsFeedback` never changes and the pump's rate
/// adaptation has nothing to adapt to.
pub async fn run_device_poller(engine: Arc<Engine>) {
    let mut ticker = tokio::time::interval(DEVICE_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if let Err(e) = engine.poll_device().await {
            tracing::debug!(error = %e, "device poll failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_frame::{Frame, Rgb};

    fn canvas() -> Canvas {
        Canvas::new(8, 8).expect("valid")
    }

    fn sequence(len: usize) -> FrameSequence {
        let frames = (0..len)
            .map(|i| {
                let mut f = Frame::blank(canvas());
                f.fill(Rgb::new(u8::try_from(i).unwrap_or(0), 0, 0));
                f
            })
            .collect();
        FrameSequence::new(Rate::new(25).expect("valid"), frames).expect("uniform")
    }

    fn engine() -> Arc<Engine> {
        Engine::new(
            canvas(),
            Rate::new(25).expect("valid"),
            WledClient::new("http://127.0.0.1:1", Duration::from_millis(50)).expect("valid"),
            "127.0.0.1:4048".parse().expect("valid"),
        )
    }

    #[tokio::test]
    async fn a_stored_asset_is_retrievable_by_its_handle() {
        let engine = engine();
        let asset = engine
            .store_asset(sequence(3), 1024, "image/gif".into())
            .await;

        let found = engine.asset(&asset.handle).await.expect("stored");
        assert_eq!(found.handle, asset.handle);
        assert_eq!(found.sequence.len(), 3);
        assert_eq!(found.media_type, "image/gif");
    }

    #[tokio::test]
    async fn handles_are_unique_across_assets() {
        let engine = engine();
        let a = engine.store_asset(sequence(1), 1, "a".into()).await;
        let b = engine.store_asset(sequence(1), 1, "b".into()).await;
        assert_ne!(a.handle, b.handle);
    }

    #[tokio::test]
    async fn an_unknown_asset_handle_is_refused_with_a_stable_code() {
        let err = engine()
            .play("asset_nope", false)
            .await
            .expect_err("no such asset");
        assert_eq!(err.code(), "matrix_unknown_asset");
    }

    #[tokio::test]
    async fn playing_an_asset_requires_a_reachable_device() {
        // Nothing listens on the configured port, so the device poll must fail rather
        // than starting playback against a panel that is not there.
        let engine = engine();
        let asset = engine.store_asset(sequence(2), 1, "x".into()).await;
        let err = engine
            .play(&asset.handle, false)
            .await
            .expect_err("device unreachable");
        assert_eq!(err.code(), "matrix_device_unreachable");
        assert!(engine.playing().await.is_none());
    }

    #[tokio::test]
    async fn stopping_when_nothing_plays_is_refused() {
        let err = engine().stop(None).await.expect_err("nothing playing");
        assert_eq!(err.code(), "matrix_nothing_playing");
    }

    #[tokio::test]
    async fn the_asset_store_evicts_oldest_first_at_its_ceiling() {
        let engine = engine();
        let mut handles = Vec::new();
        for _ in 0..(MAX_RESIDENT_ASSETS + 2) {
            handles.push(engine.store_asset(sequence(1), 1, "x".into()).await.handle);
        }

        assert_eq!(engine.asset_count().await, MAX_RESIDENT_ASSETS);
        assert!(
            engine.asset(&handles[0]).await.is_none(),
            "the oldest asset must be evicted"
        );
        assert!(
            engine.asset(&handles[1]).await.is_none(),
            "the second oldest must be evicted"
        );
        assert!(
            engine
                .asset(handles.last().expect("stored"))
                .await
                .is_some(),
            "the newest asset must stay resident"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_saturated_decode_queue_refuses_immediately_rather_than_parking() {
        let engine = engine();

        // Occupy both permits, then fill the waiter allowance.
        let _held_a = engine.acquire_decode_slot().await.expect("first permit");
        let _held_b = engine.acquire_decode_slot().await.expect("second permit");
        let mut waiters = Vec::new();
        for _ in 0..MAX_DECODE_WAITERS {
            let engine = engine.clone();
            waiters.push(tokio::spawn(async move {
                let _ = engine.acquire_decode_slot().await;
            }));
        }
        // Let the waiters register before the assertion.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let refused = tokio::time::timeout(Duration::from_secs(1), engine.acquire_decode_slot())
            .await
            .expect("the refusal must be immediate, not a 45-second park")
            .expect_err("the queue is full");
        assert_eq!(refused.code(), "matrix_busy");

        for waiter in waiters {
            waiter.abort();
        }
    }

    #[tokio::test]
    async fn assets_list_in_a_stable_order() {
        let engine = engine();
        for _ in 0..3 {
            engine.store_asset(sequence(1), 1, "x".into()).await;
        }
        let first = engine.asset_metas().await;
        let second = engine.asset_metas().await;
        assert_eq!(first.len(), 3);
        assert_eq!(engine.asset_count().await, 3);
        assert_eq!(
            first.iter().map(|a| &a.handle).collect::<Vec<_>>(),
            second.iter().map(|a| &a.handle).collect::<Vec<_>>()
        );
    }
}
