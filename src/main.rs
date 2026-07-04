//! X11 <-> Wayland Clipboard Bridge
//!
//! This program synchronizes clipboard content between X11 and Wayland compositors.

use clip_bridge::{
    ClipboardContent, ClipboardType, SyncEvent,
    wayland::{GlobalData, WaylandState},
    x11::X11State,
};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd;
// ============================================================================
// Main Application
// ============================================================================
//
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc as std_mpsc;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;
use wayland_client::{Connection, EventQueue};

use tokio::{sync::mpsc as tokio_mpsc, task::JoinHandle};

type ClipboardSetRequest = (ClipboardContent, ClipboardType);

fn forward_content(
    direction: &str,
    content: ClipboardContent,
    clipboard_type: ClipboardType,
    clipboard_cache: &mut Option<ClipboardContent>,
    primary_cache: &mut Option<ClipboardContent>,
    tx: &std_mpsc::Sender<ClipboardSetRequest>,
    wake_fd: &OwnedFd,
) {
    let cache = match clipboard_type {
        ClipboardType::Clipboard => clipboard_cache,
        ClipboardType::Primary => primary_cache,
    };

    match content {
        ClipboardContent::Empty => {
            debug!("[Sync] {} empty content: {:?}", direction, clipboard_type);
            *cache = None;
        }
        content if cache.as_ref() != Some(&content) => {
            info!(
                "[Sync] {} {:?}: mime={:?}, len={}",
                direction,
                clipboard_type,
                content.primary_mime_type(),
                content.len()
            );
            *cache = Some(content.clone());
            if let Err(e) = tx.send((content, clipboard_type)) {
                error!("[Sync] Failed to forward clipboard content: {}", e);
            } else if let Err(e) = unistd::write(wake_fd, &[1]) {
                error!("[Sync] Failed to wake clipboard event loop: {}", e);
            }
        }
        content => {
            debug!(
                "[Sync] {} {:?} unchanged, skipping: mime={:?}, len={}",
                direction,
                clipboard_type,
                content.primary_mime_type(),
                content.len()
            );
        }
    }
}

fn drain_wake_pipe(label: &str, wake_read: &OwnedFd) {
    let mut buffer = [0u8; 64];
    match unistd::read(wake_read, &mut buffer) {
        Ok(bytes) => debug!("[{}] Drained {} wake bytes", label, bytes),
        Err(e) => debug!("[{}] Failed to drain wake pipe: {}", label, e),
    }
}

fn drain_wayland_set_requests(
    wayland_state: &mut WaylandState,
    set_wayland_clipboard_rx: &std_mpsc::Receiver<ClipboardSetRequest>,
) -> bool {
    let mut did_work = false;

    loop {
        match set_wayland_clipboard_rx.try_recv() {
            Ok((content, clipboard_type)) => {
                wayland_state.set_clipboard_content(content, clipboard_type);
                did_work = true;
            }
            Err(std_mpsc::TryRecvError::Empty) => break,
            Err(std_mpsc::TryRecvError::Disconnected) => {
                debug!("[Wayland] Set clipboard channel disconnected");
                break;
            }
        }
    }

    did_work
}

fn run_wayland_event_loop(
    mut event_queue: EventQueue<WaylandState>,
    mut wayland_state: WaylandState,
    set_wayland_clipboard_rx: std_mpsc::Receiver<ClipboardSetRequest>,
    wake_read: OwnedFd,
) -> Result<(), String> {
    loop {
        let handled_commands =
            drain_wayland_set_requests(&mut wayland_state, &set_wayland_clipboard_rx);
        let dispatched = event_queue
            .dispatch_pending(&mut wayland_state)
            .map_err(|e| format!("Wayland dispatch error: {}", e))?;

        if handled_commands || dispatched > 0 {
            event_queue
                .flush()
                .map_err(|e| format!("Wayland flush error: {}", e))?;
            continue;
        }

        event_queue
            .flush()
            .map_err(|e| format!("Wayland flush error: {}", e))?;

        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };

        let (wayland_ready, wake_ready) = {
            let mut poll_fds = [
                PollFd::new(read_guard.connection_fd(), PollFlags::POLLIN),
                PollFd::new(wake_read.as_fd(), PollFlags::POLLIN),
            ];

            poll(&mut poll_fds, PollTimeout::NONE)
                .map_err(|e| format!("Failed to poll Wayland event fds: {}", e))?;

            let wayland_ready = poll_fds[0]
                .revents()
                .unwrap_or_else(PollFlags::empty)
                .intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP);
            let wake_ready = poll_fds[1]
                .revents()
                .unwrap_or_else(PollFlags::empty)
                .intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP);

            (wayland_ready, wake_ready)
        };

        if wayland_ready {
            read_guard
                .read()
                .map_err(|e| format!("Wayland read error: {}", e))?;
            event_queue
                .dispatch_pending(&mut wayland_state)
                .map_err(|e| format!("Wayland dispatch error: {}", e))?;
        } else {
            drop(read_guard);
        }

        if wake_ready {
            drain_wake_pipe("Wayland", &wake_read);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    info!("Starting X11 <-> Wayland Clipboard Bridge");

    // Create channels for sync events
    let (x11_to_wayland_tx, mut x11_to_wayland_rx) = tokio_mpsc::unbounded_channel::<SyncEvent>();
    let (wayland_to_x11_tx, mut wayland_to_x11_rx) = tokio_mpsc::unbounded_channel::<SyncEvent>();

    // Create channels for setting clipboard
    let (set_x11_clipboard_tx, set_x11_clipboard_rx) = std_mpsc::channel::<ClipboardSetRequest>();
    let (set_wayland_clipboard_tx, set_wayland_clipboard_rx) =
        std_mpsc::channel::<ClipboardSetRequest>();
    let (x11_wake_read, x11_wake_write) = unistd::pipe()?;
    let (wayland_wake_read, wayland_wake_write) = unistd::pipe()?;

    // Clone for X11 thread
    let x11_sync_tx = x11_to_wayland_tx.clone();
    let wayland_sync_tx = wayland_to_x11_tx.clone();

    // Spawn X11 thread
    let x11_handle = tokio::task::spawn_blocking(move || {
        info!("[X11] Initializing X11 connection");

        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("Failed to connect to X11: {}", e))?;
        let mut x11_state = X11State::new(
            conn,
            screen_num,
            x11_sync_tx,
            set_x11_clipboard_rx,
            x11_wake_read,
        )
        .map_err(|e| format!("Failed to create X11 state: {}", e))?;

        info!("[X11] Connection established, window: {}", x11_state.window);

        // Run X11 event loop
        // Note: We don't request clipboard content here on startup.
        // Instead, we wait for XFixes selection events which indicate
        // when another application owns the selection. This avoids the
        // race condition where we request content before any app has set it.
        if let Err(e) = x11_state.run_event_loop() {
            error!("[X11] Event loop error: {}", e);
        }

        Ok::<(), String>(())
    });

    // Initialize Wayland
    info!("[Wayland] Initializing Wayland connection");

    let wayland_conn = Connection::connect_to_env()?;
    let display = wayland_conn.display();
    let mut event_queue = wayland_conn.new_event_queue();
    let qh = event_queue.handle();

    let mut wayland_state = WaylandState::new(
        qh.clone(),
        wayland_sync_tx,
        set_wayland_clipboard_tx.clone(),
    );

    // Get registry
    display.get_registry(&qh, GlobalData);

    // Roundtrip to initialize globals
    event_queue.roundtrip(&mut wayland_state)?;

    info!("[Wayland] Connection established");

    // Main sync loop
    let wayland_handle: JoinHandle<Result<(), String>> = tokio::task::spawn_blocking(move || {
        run_wayland_event_loop(
            event_queue,
            wayland_state,
            set_wayland_clipboard_rx,
            wayland_wake_read,
        )
    });

    // Handle sync events in main task
    tokio::spawn(async move {
        let mut clipboard_content: Option<ClipboardContent> = None;
        let mut primary_content: Option<ClipboardContent> = None;

        info!("[Sync] Starting sync loop");

        loop {
            tokio::select! {
                Some(event) = x11_to_wayland_rx.recv() => {
                    debug!("[Sync] Received event from X11: {:?}", event);
                    match event {
                        SyncEvent::X11ToWayland { content, clipboard_type } => {
                            forward_content(
                                "X11 -> Wayland",
                                content,
                                clipboard_type,
                                &mut clipboard_content,
                                &mut primary_content,
                                &set_wayland_clipboard_tx,
                                &wayland_wake_write,
                            );
                        }
                        _ => {
                            debug!("[Sync] Unhandled event from X11: {:?}", event);
                        }
                    }
                }
                Some(event) = wayland_to_x11_rx.recv() => {
                    debug!("[Sync] Received event from Wayland: {:?}", event);
                    match event {
                        SyncEvent::WaylandToX11 { content, clipboard_type } => {
                            forward_content(
                                "Wayland -> X11",
                                content,
                                clipboard_type,
                                &mut clipboard_content,
                                &mut primary_content,
                                &set_x11_clipboard_tx,
                                &x11_wake_write,
                            );
                        }
                        _ => {
                            debug!("[Sync] Unhandled event from Wayland: {:?}", event);
                        }
                    }
                }
            }
        }
    });

    // Wait for tasks
    let (x11_result, wayland_result) = tokio::join!(x11_handle, wayland_handle);

    if let Err(e) = x11_result {
        error!("X11 task error: {:?}", e);
    }
    if let Err(e) = wayland_result {
        error!("Wayland task error: {:?}", e);
    }

    info!("Clipboard bridge shutting down");
    Ok(())
}
