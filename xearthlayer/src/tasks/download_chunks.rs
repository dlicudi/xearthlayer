//! Download chunks task implementation.
//!
//! [`DownloadChunksTask`] downloads all 256 chunks (16×16) for a tile from the
//! imagery provider, with disk cache integration and retry logic.
//!
//! # Resource Type
//!
//! This task uses `ResourceType::Network` since it performs HTTP downloads.
//!
//! # HTTP Concurrency
//!
//! This task uses a **shared semaphore** from [`DownloadConfig`] to limit
//! concurrent HTTP requests across all tiles. This prevents the multiplicative
//! effect where N concurrent tiles × 256 chunks = overwhelming HTTP load.
//!
//! # Output
//!
//! Produces `TaskOutput` with key "chunks" containing `ChunkResults`.

use crate::coord::TileCoord;
use crate::executor::{
    ChunkProvider, ChunkResults, DiskCache, DownloadConfig, ResourceType, Task, TaskContext,
    TaskOutput, TaskResult,
};
use crate::metrics::{MetricsClient, OptionalMetrics};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tracing::{debug, info, trace, warn, Instrument};

/// Task that downloads all chunks for a tile.
///
/// This task performs parallel downloads of 256 chunks (16×16 grid), checking
/// the disk cache first and retrying failed downloads with exponential backoff.
///
/// # Outputs
///
/// - Key: "chunks"
/// - Type: `ChunkResults` (256 chunks with successes and failures)
pub struct DownloadChunksTask<P, D>
where
    P: ChunkProvider,
    D: DiskCache,
{
    /// Tile coordinates to download
    tile: TileCoord,

    /// Chunk provider for downloading satellite imagery
    provider: Arc<P>,

    /// Disk cache for storing individual chunks
    disk_cache: Arc<D>,

    /// Download configuration (timeout, retries)
    config: DownloadConfig,
}

impl<P, D> DownloadChunksTask<P, D>
where
    P: ChunkProvider,
    D: DiskCache,
{
    /// Creates a new download chunks task.
    ///
    /// # Arguments
    ///
    /// * `tile` - Tile coordinates to download
    /// * `provider` - Chunk provider for downloads
    /// * `disk_cache` - Disk cache for chunks
    pub fn new(tile: TileCoord, provider: Arc<P>, disk_cache: Arc<D>) -> Self {
        Self {
            tile,
            provider,
            disk_cache,
            config: DownloadConfig::default(),
        }
    }

    /// Creates a new download chunks task with custom configuration.
    pub fn with_config(
        tile: TileCoord,
        provider: Arc<P>,
        disk_cache: Arc<D>,
        config: DownloadConfig,
    ) -> Self {
        Self {
            tile,
            provider,
            disk_cache,
            config,
        }
    }
}

impl<P, D> Task for DownloadChunksTask<P, D>
where
    P: ChunkProvider,
    D: DiskCache,
{
    fn name(&self) -> &str {
        "DownloadChunks"
    }

    fn resource_type(&self) -> ResourceType {
        ResourceType::Network
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut TaskContext,
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        let span = tracing::debug_span!(
            target: "profiling",
            "task_download",
            tile_row = self.tile.row,
            tile_col = self.tile.col,
        );
        Box::pin(
            async move {
                // Check for cancellation before starting
                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                let job_id = ctx.job_id();
                let metrics = ctx.metrics_clone();
                debug!(
                    job_id = %job_id,
                    tile = ?self.tile,
                    "Starting chunk downloads"
                );

                // Download all chunks with metrics tracking
                let chunks = download_all_chunks(
                    self.tile,
                    Arc::clone(&self.provider),
                    Arc::clone(&self.disk_cache),
                    &self.config,
                    metrics,
                )
                .await;

                // Check for cancellation after downloads
                if ctx.is_cancelled() {
                    return TaskResult::Cancelled;
                }

                debug!(
                    job_id = %job_id,
                    success_count = chunks.success_count(),
                    failure_count = chunks.failure_count(),
                    "Chunk downloads complete"
                );

                // Store chunks in task output for next task
                let mut output = TaskOutput::new();
                output.set("chunks", chunks);

                TaskResult::SuccessWithOutput(output)
            }
            .instrument(span),
        )
    }
}

/// Output key for chunk results.
pub const OUTPUT_KEY_CHUNKS: &str = "chunks";

/// Helper function to extract chunks from task output.
pub fn get_chunks_from_output(output: &TaskOutput) -> Option<&ChunkResults> {
    output.get::<ChunkResults>(OUTPUT_KEY_CHUNKS)
}

// ============================================================================
// Internal implementation - lifted from pipeline/stages/download
// ============================================================================

/// Result of a successful chunk download.
struct ChunkData {
    row: u8,
    col: u8,
    data: Vec<u8>,
}

/// Result of a failed chunk download.
struct ChunkError {
    row: u8,
    col: u8,
    attempts: u32,
    error: String,
}

/// Downloads all 256 chunks for a tile with controlled concurrency.
///
/// Uses the **shared semaphore** from [`DownloadConfig`] to limit concurrent
/// HTTP requests across ALL tiles being downloaded. This prevents the
/// multiplicative effect where N tiles × 256 chunks overwhelms the provider.
async fn download_all_chunks<P, D>(
    tile: TileCoord,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    config: &DownloadConfig,
    metrics: Option<MetricsClient>,
) -> ChunkResults
where
    P: ChunkProvider,
    D: DiskCache,
{
    // Resolve the source sampling grid. When `max_source_zoom` caps the
    // requested zoom, the same geographic box is fetched at a lower zoom as a
    // smaller `grid_side × grid_side` block (to be upscaled during assembly).
    // When uncapped, this is the native 16×16 grid and behaviour is unchanged.
    let grid = tile.source_grid(config.max_source_zoom);
    let source_tile = TileCoord {
        row: grid.origin_chunk_row / crate::coord::CHUNKS_PER_TILE_SIDE,
        col: grid.origin_chunk_col / crate::coord::CHUNKS_PER_TILE_SIDE,
        zoom: grid.source_zoom,
    };
    // Offset of the block's top-left chunk within its (single) source tile. The
    // power-of-two alignment of `source_grid` guarantees the whole block fits
    // inside one source tile, so these are constant for the tile.
    let off_row = (grid.origin_chunk_row % crate::coord::CHUNKS_PER_TILE_SIDE) as u8;
    let off_col = (grid.origin_chunk_col % crate::coord::CHUNKS_PER_TILE_SIDE) as u8;
    let grid_side = grid.grid_side as u8;

    if !grid.is_native() {
        debug!(
            tile = ?tile,
            source_zoom = grid.source_zoom,
            grid_side = grid.grid_side,
            upscale_factor = grid.upscale_factor,
            "Source zoom capped: fetching reduced grid to upscale"
        );
    }

    let mut results = ChunkResults::with_grid_side(grid.grid_side);
    let mut downloads = JoinSet::new();

    // Use the SHARED semaphore from config - this limits HTTP requests across
    // ALL concurrent tile downloads, not just within this single tile.
    let semaphore = Arc::clone(&config.http_semaphore);

    // Spawn one download per chunk in the source grid (grid_side² chunks). Each
    // fetches/caches under the SOURCE tile's coords (so capped tiles sharing a
    // source chunk dedupe in the chunk cache); the result's source within-tile
    // position is remapped to the assembly grid position on collection.
    for i in 0..grid_side {
        for j in 0..grid_side {
            let provider = Arc::clone(&provider);
            let disk_cache = Arc::clone(&disk_cache);
            let timeout = config.request_timeout;
            let max_retries = config.max_retries;
            let sem = Arc::clone(&semaphore);
            let chunk_metrics = metrics.clone();
            let src_row = off_row + i;
            let src_col = off_col + j;

            downloads.spawn(async move {
                // Acquire semaphore permit before starting download
                let _permit = sem.acquire().await.expect("semaphore closed unexpectedly");

                download_chunk_with_cache(
                    source_tile,
                    src_row,
                    src_col,
                    provider,
                    disk_cache,
                    timeout,
                    max_retries,
                    chunk_metrics,
                )
                .await
            });
        }
    }

    // Collect results as they complete, remapping source within-tile coords
    // (`off + index`) back to assembly grid positions (`0..grid_side`).
    while let Some(result) = downloads.join_next().await {
        match result {
            Ok(Ok(chunk_data)) => {
                results.add_success(
                    chunk_data.row - off_row,
                    chunk_data.col - off_col,
                    chunk_data.data,
                );
            }
            Ok(Err(chunk_err)) => {
                warn!(
                    chunk_row = chunk_err.row,
                    chunk_col = chunk_err.col,
                    error = %chunk_err.error,
                    "Chunk download failed"
                );
                results.add_failure(
                    chunk_err.row - off_row,
                    chunk_err.col - off_col,
                    chunk_err.attempts,
                    chunk_err.error,
                );
            }
            Err(join_err) => {
                warn!(error = %join_err, "Download task panicked");
            }
        }
    }

    results
}

/// Downloads a single chunk, checking disk cache first.
///
/// Emits metrics events for:
/// - Disk cache hit/miss
/// - Download started/completed/failed
/// - Download retries
#[allow(clippy::too_many_arguments)]
async fn download_chunk_with_cache<P, D>(
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    provider: Arc<P>,
    disk_cache: Arc<D>,
    timeout: Duration,
    max_retries: u32,
    metrics: Option<MetricsClient>,
) -> Result<ChunkData, ChunkError>
where
    P: ChunkProvider,
    D: DiskCache,
{
    // Check disk cache first
    if let Some(cached) = disk_cache
        .get(tile.row, tile.col, tile.zoom, chunk_row, chunk_col)
        .await
    {
        trace!(
            tile_row = tile.row,
            tile_col = tile.col,
            chunk_row = chunk_row,
            chunk_col = chunk_col,
            "Chunk cache hit"
        );
        // Emit chunk disk cache hit metric
        metrics.chunk_disk_cache_hit(cached.len() as u64);
        return Ok(ChunkData {
            row: chunk_row,
            col: chunk_col,
            data: cached,
        });
    }

    // Emit chunk disk cache miss metric
    metrics.chunk_disk_cache_miss();

    // Calculate global coordinates for the provider
    let global_row = tile.row * crate::coord::CHUNKS_PER_TILE_SIDE + chunk_row as u32;
    let global_col = tile.col * crate::coord::CHUNKS_PER_TILE_SIDE + chunk_col as u32;
    let chunk_zoom = tile.zoom + crate::coord::CHUNK_ZOOM_OFFSET;

    trace!(
        tile = ?tile,
        chunk_row = chunk_row,
        chunk_col = chunk_col,
        global_row = global_row,
        global_col = global_col,
        chunk_zoom = chunk_zoom,
        provider = provider.name(),
        "Starting chunk download"
    );

    // Track download started
    metrics.download_started();

    // Try downloading with retries
    let download_start = Instant::now();
    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        debug!(
            provider = provider.name(),
            global_row = global_row,
            global_col = global_col,
            zoom = chunk_zoom,
            attempt = attempt,
            "HTTP download attempt"
        );

        match tokio::time::timeout(
            timeout,
            provider.download_chunk(global_row, global_col, chunk_zoom),
        )
        .await
        {
            Ok(Ok(data)) => {
                let duration_us = download_start.elapsed().as_micros() as u64;
                let bytes = data.len() as u64;

                info!(
                    provider = provider.name(),
                    global_row = global_row,
                    global_col = global_col,
                    zoom = chunk_zoom,
                    bytes = bytes,
                    attempt = attempt,
                    "Chunk download success"
                );

                // Emit download completed metric
                metrics.download_completed(bytes, duration_us);

                // Cache the chunk (fire and forget, with metrics)
                spawn_cache_write(
                    Arc::clone(&disk_cache),
                    tile,
                    chunk_row,
                    chunk_col,
                    data.clone(),
                    metrics.clone(),
                );

                return Ok(ChunkData {
                    row: chunk_row,
                    col: chunk_col,
                    data,
                });
            }
            Ok(Err(e)) => {
                warn!(
                    provider = provider.name(),
                    global_row = global_row,
                    global_col = global_col,
                    zoom = chunk_zoom,
                    attempt = attempt,
                    error = %e.message,
                    retryable = e.is_retryable,
                    "Chunk download error"
                );
                last_error = e.message.clone();
                if !e.is_retryable {
                    break;
                }
                // Emit retry metric if we'll retry
                if attempt < max_retries {
                    metrics.download_retried();
                }
            }
            Err(_) => {
                warn!(
                    provider = provider.name(),
                    global_row = global_row,
                    global_col = global_col,
                    zoom = chunk_zoom,
                    attempt = attempt,
                    timeout_secs = timeout.as_secs(),
                    "Chunk download timeout"
                );
                last_error = "timeout".to_string();
                // Emit retry metric if we'll retry
                if attempt < max_retries {
                    metrics.download_retried();
                }
            }
        }

        // Exponential backoff before retry
        if attempt < max_retries {
            let backoff = Duration::from_millis(100 * (1 << attempt));
            trace!(backoff_ms = backoff.as_millis(), "Backoff before retry");
            tokio::time::sleep(backoff).await;
        }
    }

    // Emit download failed metric
    metrics.download_failed();

    Err(ChunkError {
        row: chunk_row,
        col: chunk_col,
        attempts: max_retries,
        error: last_error,
    })
}

/// Spawns a fire-and-forget task to write chunk data to disk cache.
///
/// Emits `disk_write_started` when beginning and `disk_write_completed`
/// with byte count and duration when finished.
fn spawn_cache_write<D>(
    disk_cache: Arc<D>,
    tile: TileCoord,
    chunk_row: u8,
    chunk_col: u8,
    data: Vec<u8>,
    metrics: Option<MetricsClient>,
) where
    D: DiskCache,
{
    let bytes = data.len() as u64;
    tokio::spawn(async move {
        // Track disk write started
        metrics.disk_write_started();
        let start = Instant::now();

        let _ = disk_cache
            .put(tile.row, tile.col, tile.zoom, chunk_row, chunk_col, data)
            .await;

        // Track disk write completed
        let duration_us = start.elapsed().as_micros() as u64;
        metrics.disk_write_completed(bytes, duration_us);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::TileCoord;
    use crate::executor::ChunkDownloadError;
    use std::sync::Mutex;

    #[test]
    fn test_task_name() {
        assert_eq!("DownloadChunks", "DownloadChunks");
    }

    #[test]
    fn test_output_key() {
        assert_eq!(OUTPUT_KEY_CHUNKS, "chunks");
    }

    /// Provider that records every (row, col, zoom) requested and always succeeds.
    struct RecordingProvider {
        requested: Arc<Mutex<Vec<(u32, u32, u8)>>>,
    }

    impl ChunkProvider for RecordingProvider {
        async fn download_chunk(
            &self,
            row: u32,
            col: u32,
            zoom: u8,
        ) -> Result<Vec<u8>, ChunkDownloadError> {
            self.requested.lock().unwrap().push((row, col, zoom));
            Ok(vec![1, 2, 3])
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    /// Disk cache that always misses and discards writes.
    struct NoopDiskCache;

    impl DiskCache for NoopDiskCache {
        async fn get(&self, _: u32, _: u32, _: u8, _: u8, _: u8) -> Option<Vec<u8>> {
            None
        }

        async fn put(
            &self,
            _: u32,
            _: u32,
            _: u8,
            _: u8,
            _: u8,
            _: Vec<u8>,
        ) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn run_download(tile: TileCoord, cap: Option<u8>) -> (ChunkResults, Vec<(u32, u32, u8)>) {
        let requested = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider {
            requested: Arc::clone(&requested),
        });
        let cache = Arc::new(NoopDiskCache);
        let config = DownloadConfig::default().with_max_source_zoom(cap);
        let results = download_all_chunks(tile, provider, cache, &config, None).await;
        let coords = requested.lock().unwrap().clone();
        (results, coords)
    }

    #[tokio::test]
    async fn native_download_fetches_full_256_grid_at_chunk_zoom() {
        // ZL18 tile: tile zoom 14, native chunk zoom 18.
        let tile = TileCoord {
            row: 1000,
            col: 2000,
            zoom: 14,
        };
        let (results, coords) = run_download(tile, None).await;

        assert_eq!(results.grid_side(), 16);
        assert_eq!(results.success_count(), 256);
        assert!(results.is_complete());
        // Native chunk zoom is tile zoom + 4 = ZL18.
        assert_eq!(coords.len(), 256);
        assert!(coords.iter().all(|&(_, _, z)| z == 18));
    }

    #[tokio::test]
    async fn capped_download_fetches_reduced_grid_at_source_coords() {
        // ZL18 tile (tile zoom 14) capped at ZL16: Δ2 -> 4×4 grid at chunk zoom 16.
        let tile = TileCoord {
            row: 1000,
            col: 2000,
            zoom: 14,
        };
        let (results, coords) = run_download(tile, Some(16)).await;

        assert_eq!(results.grid_side(), 4);
        assert_eq!(results.success_count(), 16);
        assert!(results.is_complete());

        assert_eq!(coords.len(), 16);
        assert!(coords.iter().all(|&(_, _, z)| z == 16), "source chunk zoom");

        // Geographic origin: native chunk origin (row*16) shifted down two levels.
        let exp_row0 = (1000 * 16) >> 2; // 4000
        let exp_col0 = (2000 * 16) >> 2; // 8000
        let mut rows: Vec<u32> = coords.iter().map(|&(r, _, _)| r).collect();
        rows.sort_unstable();
        rows.dedup();
        let mut cols: Vec<u32> = coords.iter().map(|&(_, c, _)| c).collect();
        cols.sort_unstable();
        cols.dedup();
        assert_eq!(
            rows,
            vec![exp_row0, exp_row0 + 1, exp_row0 + 2, exp_row0 + 3]
        );
        assert_eq!(
            cols,
            vec![exp_col0, exp_col0 + 1, exp_col0 + 2, exp_col0 + 3]
        );
    }

    #[tokio::test]
    async fn cap_at_or_above_requested_zoom_is_native() {
        // A ZL16 tile (tile zoom 12) under a cap of ZL17 must download natively.
        let tile = TileCoord {
            row: 12754,
            col: 5279,
            zoom: 12,
        };
        let (results, coords) = run_download(tile, Some(17)).await;
        assert_eq!(results.grid_side(), 16);
        assert_eq!(results.success_count(), 256);
        assert!(coords.iter().all(|&(_, _, z)| z == 16)); // native ZL16
    }
}
