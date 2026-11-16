use std::os::fd::{AsFd, AsRawFd};

use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use nix::unistd::ftruncate;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};

use super::wayland::AppState;

// ── Screencopy frame state ────────────────────────────────────

/// Per-frame state for the screencopy buffer negotiation.
/// Stored directly in [`AppState`] so the dispatch handler can mutate it.
#[derive(Debug, Default)]
pub struct CaptureFrame {
    /// Format + dimensions advertised by the compositor.
    pub format: Option<wl_shm::Format>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,

    /// Whether the frame is ready (pixels written).
    pub ready: bool,
    /// Whether the frame capture failed.
    pub failed: bool,
}

/// Tear down an incomplete screencopy request before another capture is
/// attempted. A timed-out frame can otherwise remain alive on the connection
/// and deliver late events into the next request's shared CaptureFrame state.
fn discard_incomplete_capture(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    frame: &ZwlrScreencopyFrameV1,
    buffer: &WlBuffer,
    pool: &WlShmPool,
) {
    buffer.destroy();
    pool.destroy();
    frame.destroy();

    // Drain events queued before the destructor requests take effect. Reset
    // only afterwards so late events from this frame cannot mark a new frame
    // as ready or failed.
    let _ = event_queue.roundtrip(state);
    state.capture_frame = CaptureFrame::default();
}

// ── Capture logic ─────────────────────────────────────────────

/// Capture the full output (no region) for freeze-frame mode.
///
/// Returns the same tuple as [`capture_region`].
pub fn capture_full_output(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    conn: &Connection,
    qh: &QueueHandle<AppState>,
) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)> {
    capture_with_timeout_retries("freeze-frame", event_queue, state, conn, |queue, state, conn| {
        capture_full_output_once(queue, state, conn, qh)
    })
}

fn capture_full_output_once(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    conn: &Connection,
    qh: &QueueHandle<AppState>,
) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)> {
    let manager = state
        .screencopy_manager
        .as_ref()
        .context("Screencopy manager not bound")?
        .clone();

    let output = state
        .outputs
        .first()
        .context("No output available")?
        .wl_output
        .clone();

    state.capture_frame = CaptureFrame::default();
    state.capture_generation = state.capture_generation.wrapping_add(1);
    let generation = state.capture_generation;

    let frame = manager.capture_output(0, &output, qh, generation);

    event_queue
        .roundtrip(state)
        .context("Roundtrip failed while negotiating screencopy frame")?;

    let fmt = state
        .capture_frame
        .format
        .context("Compositor did not send buffer format")?;
    let buf_width = state.capture_frame.width;
    let buf_height = state.capture_frame.height;
    let buf_stride = state.capture_frame.stride;
    let buf_size = (buf_stride * buf_height) as usize;

    let shm_name = format!("/voidsnap-freeze-{}", std::process::id());
    let fd = nix::sys::mman::shm_open(
        shm_name.as_str(),
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_TRUNC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .context("shm_open for freeze buffer failed")?;
    let _ = nix::sys::mman::shm_unlink(shm_name.as_str());

    ftruncate(&fd, buf_size as i64).context("ftruncate on freeze SHM fd failed")?;

    let mmap = unsafe {
        memmap2::MmapOptions::new()
            .len(buf_size)
            .map_mut(&fd)
            .context("mmap of freeze SHM buffer failed")?
    };

    let shm = state.shm.as_ref().context("wl_shm not bound")?;
    let pool = shm.create_pool(fd.as_fd(), buf_size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        buf_width as i32,
        buf_height as i32,
        buf_stride as i32,
        fmt,
        qh,
        (),
    );

    frame.copy(&buffer);

    if let Err(error) = wait_for_capture(conn, event_queue, state) {
        discard_incomplete_capture(event_queue, state, &frame, &buffer, &pool);
        return Err(error).context("Freeze-frame capture did not complete");
    }

    let pixels = mmap[..buf_size].to_vec();

    buffer.destroy();
    pool.destroy();
    frame.destroy();

    Ok((pixels, buf_width, buf_height, buf_stride, fmt))
}

/// Capture the selected region of the given output via wlr-screencopy.
///
/// Returns the raw pixel buffer (format as reported by the compositor,
/// typically XRGB8888 or ARGB8888) along with width, height, stride, and format.
pub fn capture_region(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    conn: &Connection,
    qh: &QueueHandle<AppState>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)> {
    capture_with_timeout_retries("region capture", event_queue, state, conn, |queue, state, conn| {
        capture_region_once(queue, state, conn, qh, x, y, width, height)
    })
}

fn capture_region_once(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    conn: &Connection,
    qh: &QueueHandle<AppState>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)> {
    let manager = state
        .screencopy_manager
        .as_ref()
        .context("Screencopy manager not bound")?
        .clone();

    let output = state
        .outputs
        .first()
        .context("No output available")?
        .wl_output
        .clone();

    // Give this request a unique token so delayed events from an older frame
    // cannot complete or fail the current capture.
    state.capture_frame = CaptureFrame::default();
    state.capture_generation = state.capture_generation.wrapping_add(1);
    let generation = state.capture_generation;

    // Request a frame for the selected region (overlay_cursor = false → don't include cursor).
    let frame = manager.capture_output_region(
        0, // overlay_cursor: 0 = no, 1 = yes
        &output,
        x,
        y,
        width,
        height,
        qh,
        generation,
    );

    // Roundtrip to get the buffer event with format info.
    event_queue
        .roundtrip(state)
        .context("Roundtrip failed while negotiating screencopy frame")?;

    // Read back the negotiated format from AppState.
    let fmt = state
        .capture_frame
        .format
        .context("Compositor did not send buffer format")?;
    let buf_width = state.capture_frame.width;
    let buf_height = state.capture_frame.height;
    let buf_stride = state.capture_frame.stride;
    let buf_size = (buf_stride * buf_height) as usize;

    // Allocate SHM for the capture buffer.
    let shm_name = format!("/voidsnap-capture-{}", std::process::id());
    let fd = nix::sys::mman::shm_open(
        shm_name.as_str(),
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_TRUNC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .context("shm_open for capture buffer failed")?;
    let _ = nix::sys::mman::shm_unlink(shm_name.as_str());

    ftruncate(&fd, buf_size as i64).context("ftruncate on capture SHM fd failed")?;

    let mmap = unsafe {
        memmap2::MmapOptions::new()
            .len(buf_size)
            .map_mut(&fd)
            .context("mmap of capture SHM buffer failed")?
    };

    // Create wl_shm_pool + wl_buffer.
    let shm = state.shm.as_ref().context("wl_shm not bound")?;
    let pool = shm.create_pool(fd.as_fd(), buf_size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        buf_width as i32,
        buf_height as i32,
        buf_stride as i32,
        fmt,
        qh,
        (),
    );

    // Tell the compositor to copy pixels into our buffer.
    frame.copy(&buffer);

    if let Err(error) = wait_for_capture(conn, event_queue, state) {
        discard_incomplete_capture(event_queue, state, &frame, &buffer, &pool);
        return Err(error).context("Screencopy capture did not complete");
    }

    // Copy the pixels out.
    let pixels = mmap[..buf_size].to_vec();

    // Clean up the wl objects.
    buffer.destroy();
    pool.destroy();
    frame.destroy();

    Ok((pixels, buf_width, buf_height, buf_stride, fmt))
}


/// Wait for the compositor's Ready/Failed event using a real wall-clock
/// deadline. Repeated roundtrips can finish in milliseconds without waiting
/// for the compositor to schedule and complete a new output frame.
fn wait_for_capture(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
) -> Result<()> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let deadline = std::time::Instant::now() + TIMEOUT;

    loop {
        event_queue
            .dispatch_pending(state)
            .context("Failed to dispatch pending screencopy events")?;

        if state.capture_frame.failed {
            anyhow::bail!("Compositor reported screencopy failure");
        }
        if state.capture_frame.ready {
            return Ok(());
        }

        conn.flush().context("Failed to flush screencopy request")?;
        let Some(guard) = conn.prepare_read() else {
            continue;
        };

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            drop(guard);
            anyhow::bail!("Screencopy timed out after {}ms waiting for Ready or Failed", TIMEOUT.as_millis());
        }

        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd points to one valid pollfd for the duration of this call.
        let poll_result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if poll_result == 0 {
            drop(guard);
            anyhow::bail!("Screencopy timed out after {}ms waiting for Ready or Failed", TIMEOUT.as_millis());
        }
        if poll_result < 0 {
            let error = std::io::Error::last_os_error();
            drop(guard);
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Polling Wayland connection failed");
        }

        guard.read().context("Reading Wayland screencopy events failed")?;
    }
}

/// Retry only compositor timeouts. Each attempt creates a fresh frame and
/// waits up to the capture deadline before retrying.
fn capture_with_timeout_retries<F>(
    label: &str,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    conn: &Connection,
    mut capture: F,
) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)>
where
    F: FnMut(
        &mut wayland_client::EventQueue<AppState>,
        &mut AppState,
        &Connection,
    ) -> Result<(Vec<u8>, u32, u32, u32, wl_shm::Format)>,
{
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        let started = std::time::Instant::now();
        match capture(event_queue, state, conn) {
            Ok(result) => {
                if attempt > 1 {
                    eprintln!(
                        "selkies-desktop: {label} succeeded on attempt {attempt}/{MAX_ATTEMPTS} after {:.0}ms",
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                }
                return Ok(result);
            }
            Err(error) => {
                let elapsed = started.elapsed();
                let is_timeout = format!("{error:#}").contains("timed out");
                if !is_timeout || attempt == MAX_ATTEMPTS {
                    return Err(error).with_context(|| {
                        format!("{label} failed after {attempt} attempt(s)")
                    });
                }

                let retry_delay = std::time::Duration::from_millis(50 * attempt as u64);
                eprintln!(
                    "selkies-desktop: {label} timed out after {:.0}ms (attempt {attempt}/{MAX_ATTEMPTS}); retrying in {}ms",
                    elapsed.as_secs_f64() * 1000.0,
                    retry_delay.as_millis()
                );
                std::thread::sleep(retry_delay);
            }
        }
    }

    unreachable!("capture retry loop always returns")
}

// ── Dispatch for screencopy frame ─────────────────────────────

impl Dispatch<ZwlrScreencopyFrameV1, u64> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as wayland_client::Proxy>::Event,
        data: &u64,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Ignore delayed events from a previous request after a newer capture starts.
        if *data != state.capture_generation {
            return;
        }

        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let wayland_client::WEnum::Value(f) = format {
                    state.capture_frame.format = Some(f);
                }
                state.capture_frame.width = width;
                state.capture_frame.height = height;
                state.capture_frame.stride = stride;
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.capture_frame.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.capture_frame.failed = true;
            }
            _ => {}
        }
    }
}
