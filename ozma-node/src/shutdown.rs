// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Centralized process shutdown for `ozma-node`.
//!
//! The node owns several long-lived tasks and child processes (HTTP/QUIC
//! listeners, UDP HID, mDNS, QMP, HLS/ffmpeg, the WebRTC helper). A signal must
//! cancel all of them, wait a bounded time for them to release their
//! sockets/devices, and never let a task that ignores cancellation hold the
//! process open. This module is the single place that defines those bounds.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// How long one cooperative task stop may take before it is abandoned.
pub const TASK_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the tokio runtime may wait for tasks/blocking work after `main`
/// returns. A task that never yields (for example a driver busy-loop) must not
/// turn SIGTERM into a SIGKILL.
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Create the root shutdown token shared by every node task.
pub fn token() -> CancellationToken {
    CancellationToken::new()
}

/// Wait for `SIGINT`/`SIGTERM` and return the signal that requested shutdown.
#[cfg(unix)]
pub async fn wait_for_signal() -> &'static str {
    use tokio::signal;
    let mut sigterm =
        signal::unix::signal(signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = signal::ctrl_c() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    }
}

#[cfg(not(unix))]
pub async fn wait_for_signal() -> &'static str {
    tokio::signal::ctrl_c().await.expect("Ctrl-C handler");
    "Ctrl-C"
}

/// Await `future` for at most `timeout`, warning when it does not finish.
pub async fn with_timeout<F, T>(what: &'static str, timeout: Duration, future: F) -> Option<T>
where
    F: Future<Output = T>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(value) => Some(value),
        Err(_) => {
            warn!(
                operation = what,
                timeout_ms = timeout.as_millis() as u64,
                "shutdown step exceeded its bound; continuing"
            );
            None
        }
    }
}

/// Abort `task`, then wait at most [`TASK_STOP_TIMEOUT`] for it to finish.
///
/// Aborting is cooperative in tokio: a task that never reaches an await point
/// keeps running. The timeout keeps that task from blocking process exit; the
/// runtime is shut down with [`RUNTIME_SHUTDOWN_TIMEOUT`] immediately after.
pub async fn stop_task(task: JoinHandle<()>, what: &'static str) {
    if task.is_finished() {
        match task.await {
            Ok(()) => debug!(task = what, "background task stopped"),
            Err(error) if error.is_cancelled() => {}
            Err(error) => warn!(task = what, error = %error, "background task failed"),
        }
        return;
    }

    task.abort();
    match tokio::time::timeout(TASK_STOP_TIMEOUT, task).await {
        Ok(Ok(())) => debug!(task = what, "background task stopped"),
        Ok(Err(error)) if error.is_cancelled() => debug!(task = what, "background task cancelled"),
        Ok(Err(error)) => {
            warn!(task = what, error = %error, "background task failed during shutdown")
        }
        Err(_) => warn!(
            task = what,
            timeout_ms = TASK_STOP_TIMEOUT.as_millis() as u64,
            "background task did not stop within its bound; abandoning it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn cancellation_token_reaches_a_select_loop() {
        let token = token();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let child = token.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = child.cancelled() => {}
            }
            flag.store(true, Ordering::SeqCst);
        });

        // Let the task reach the select before cancelling.
        tokio::task::yield_now().await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("token cancellation must reach the task")
            .expect("task must not panic");
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_task_bounds_a_task_that_ignores_abort() {
        let started = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = started.clone();
        let stop_flag = stop.clone();
        let task = tokio::spawn(async move {
            flag.store(true, Ordering::SeqCst);
            // Simulates a driver loop that never yields to the scheduler, so
            // `abort()` alone cannot stop it.
            while !stop_flag.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
        });
        // Give the task a chance to enter the loop.
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(started.load(Ordering::SeqCst), "spin task never started");
        let began = std::time::Instant::now();
        stop_task(task, "test-spin-loop").await;
        assert!(started.load(Ordering::SeqCst));
        assert!(
            began.elapsed() < TASK_STOP_TIMEOUT + Duration::from_secs(1),
            "stop_task must return within its bound"
        );
        // Let the abandoned task observe its own exit flag so the test runtime
        // can shut down cleanly.
        stop.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn with_timeout_returns_none_for_a_stuck_future() {
        let result = with_timeout(
            "test-stuck",
            Duration::from_millis(20),
            std::future::pending::<()>(),
        )
        .await;
        assert!(result.is_none());
    }
}
