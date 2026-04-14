#![allow(dead_code)]
//! System manager: controls forge child process and scanning pipeline lifecycle.
//! Called by the control panel API to start/stop the entire system.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Manages the forge child process and tracks pipeline health.
pub struct SystemManager {
    /// Forge child process handle
    forge_process: RwLock<Option<Child>>,
    /// Path to forge project directory
    forge_dir: PathBuf,
    /// Whether forge is connected and healthy
    pub forge_connected: AtomicBool,
    /// Whether the scanning pipeline tasks are running
    pub scanner_active: AtomicBool,
    /// Whether discovery is running
    pub discovery_active: AtomicBool,
    /// Counter: total signals received
    pub signals_received: AtomicU64,
    /// Counter: scans triggered
    pub scans_triggered: AtomicU64,
    /// Counter: profitable scans
    pub profitable_scans: AtomicU64,
    /// System start time (for uptime)
    pub started_at: RwLock<Option<Instant>>,
}

impl SystemManager {
    pub fn new(forge_dir: PathBuf) -> Self {
        Self {
            forge_process: RwLock::new(None),
            forge_dir,
            forge_connected: AtomicBool::new(false),
            scanner_active: AtomicBool::new(false),
            discovery_active: AtomicBool::new(false),
            signals_received: AtomicU64::new(0),
            scans_triggered: AtomicU64::new(0),
            profitable_scans: AtomicU64::new(0),
            started_at: RwLock::new(None),
        }
    }

    /// Start the forge process as a child.
    pub async fn start_forge(&self) -> Result<()> {
        let mut proc = self.forge_process.write().await;

        // Kill existing if running
        if let Some(ref mut child) = *proc {
            warn!("Forge already running, killing old process");
            child.kill().await.ok();
        }

        info!("Starting forge from {:?}", self.forge_dir);

        let child = Command::new("cargo")
            .arg("run")
            .current_dir(&self.forge_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true) // kill forge when arb exits
            .spawn()?;

        *proc = Some(child);
        info!("Forge process spawned");

        // Wait for forge to be ready (health check)
        let client = reqwest::Client::new();
        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > 30 {
                error!("Forge failed to start after 30 attempts");
                return Err(anyhow::anyhow!("Forge health check timeout"));
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match client.get("http://localhost:3001/api/v1/health").send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!("Forge is healthy (attempt {})", attempts);
                    self.forge_connected.store(true, Ordering::Relaxed);
                    break;
                }
                _ => {
                    if attempts % 5 == 0 {
                        info!("Waiting for forge to start... (attempt {})", attempts);
                    }
                }
            }
        }

        *self.started_at.write().await = Some(Instant::now());
        Ok(())
    }

    /// Stop the forge process.
    pub async fn stop_forge(&self) {
        let mut proc = self.forge_process.write().await;
        if let Some(ref mut child) = *proc {
            info!("Stopping forge process");
            child.kill().await.ok();
            *proc = None;
        }
        self.forge_connected.store(false, Ordering::Relaxed);
        self.scanner_active.store(false, Ordering::Relaxed);
        self.discovery_active.store(false, Ordering::Relaxed);
        *self.started_at.write().await = None;
    }

    /// Get uptime in seconds (None if not running).
    pub async fn uptime_secs(&self) -> Option<u64> {
        self.started_at
            .read()
            .await
            .map(|t| t.elapsed().as_secs())
    }

    /// Check if forge is still alive.
    pub async fn check_forge_health(&self) -> bool {
        let client = reqwest::Client::new();
        match client
            .get("http://localhost:3001/api/v1/health")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) => {
                let healthy = resp.status().is_success();
                self.forge_connected.store(healthy, Ordering::Relaxed);
                healthy
            }
            Err(_) => {
                self.forge_connected.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    /// Increment signal counter.
    pub fn record_signal(&self) {
        self.signals_received.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment scan counter.
    pub fn record_scan(&self, profitable: bool) {
        self.scans_triggered.fetch_add(1, Ordering::Relaxed);
        if profitable {
            self.profitable_scans.fetch_add(1, Ordering::Relaxed);
        }
    }
}
