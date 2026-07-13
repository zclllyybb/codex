use crate::extensions::seed_extension_instructions;
use crate::guard;
use crate::memory_root;
use crate::phase1;
use crate::phase2;
use crate::runtime::MemoryStartupContext;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_rollout::BackfillMode;
use codex_rollout::BackfillReport;
use codex_rollout::BackfillReportStatus;
use serde::Serialize;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_STAGE1_MAINTENANCE_BATCH: usize = 128;

#[derive(Debug, Clone)]
pub struct MemoryMaintenanceOptions {
    pub backfill_mode: BackfillMode,
    pub max_stage1_claimed_per_batch: usize,
    pub min_rollout_idle_hours: i64,
}

impl Default for MemoryMaintenanceOptions {
    fn default() -> Self {
        Self {
            backfill_mode: BackfillMode::Full,
            max_stage1_claimed_per_batch: DEFAULT_STAGE1_MAINTENANCE_BATCH,
            min_rollout_idle_hours: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryMaintenanceReport {
    pub status: String,
    pub memory_root: String,
    pub lock_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backfill: Option<BackfillReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase1: Option<phase1::Phase1DrainReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase2: Option<phase2::Phase2Report>,
}

impl MemoryMaintenanceReport {
    pub fn succeeded(&self) -> bool {
        self.status == "completed"
    }

    fn with_status(mut self, status: &'static str) -> Self {
        self.status = status.to_string();
        self
    }

    fn with_failure(mut self, status: &'static str, reason: impl Into<String>) -> Self {
        self.status = status.to_string();
        self.failure_reason = Some(reason.into());
        self
    }
}

pub async fn maintain_memories(
    thread_manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    config: Arc<Config>,
    options: MemoryMaintenanceOptions,
) -> anyhow::Result<MemoryMaintenanceReport> {
    let root = memory_root(&config.codex_home);
    let lock_path = config.sqlite_home.join("memory-maintain.lock");
    let mut report = MemoryMaintenanceReport {
        status: "running".to_string(),
        memory_root: root.to_string_lossy().to_string(),
        lock_path: lock_path.to_string_lossy().to_string(),
        failure_reason: None,
        backfill: None,
        phase1: None,
        phase2: None,
    };

    let _lock = match MemoryMaintenanceLock::try_acquire(lock_path.clone())? {
        Some(lock) => lock,
        None => return Ok(report.with_status("blocked_lock_held")),
    };

    let context = Arc::new(MemoryStartupContext::new(
        thread_manager,
        Arc::clone(&auth_manager),
        thread_id,
        thread,
        config.as_ref(),
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
    ));
    let Some(state_db) = context.state_db() else {
        return Ok(report.with_failure("failed_no_state_db", "state db unavailable"));
    };

    let backfill = codex_rollout::refresh_state_from_sessions(
        state_db.as_ref(),
        &config.codex_home,
        config.model_provider_id.as_str(),
        options.backfill_mode,
    )
    .await?;
    let backfill_status = backfill.status;
    let backfill_failed = backfill.failed;
    report.backfill = Some(backfill);
    if backfill_status == BackfillReportStatus::AlreadyRunning {
        return Ok(report.with_status("blocked_backfill_running"));
    }
    if backfill_failed > 0 {
        return Ok(report.with_failure(
            "failed_backfill",
            format!("{backfill_failed} rollout metadata extraction(s) failed"),
        ));
    }

    tokio::fs::create_dir_all(&root).await?;
    seed_extension_instructions(&root).await?;
    phase1::prune(context.as_ref(), &config).await;

    if !guard::rate_limits_ok(&auth_manager, &config).await {
        return Ok(report.with_status("blocked_rate_limit"));
    }

    let phase1_report = phase1::drain_for_maintenance(
        Arc::clone(&context),
        Arc::clone(&config),
        phase1::Phase1DrainOptions {
            max_claimed_per_batch: options.max_stage1_claimed_per_batch,
            min_rollout_idle_hours: options.min_rollout_idle_hours,
        },
    )
    .await;
    let phase1_has_blockers = phase1_report.has_blockers();
    report.phase1 = Some(phase1_report);
    if phase1_has_blockers {
        return Ok(report.with_status("blocked_phase1_jobs"));
    }

    let phase2_report = phase2::run_for_maintenance(Arc::clone(&context), config).await;
    let phase2_succeeded = phase2_report.succeeded();
    let phase2_status = phase2_report.status.clone();
    report.phase2 = Some(phase2_report);
    if phase2_succeeded {
        return Ok(report.with_status("completed"));
    }

    let status = match phase2_status.as_str() {
        "skipped_running" => "blocked_phase2_running",
        "skipped_retry_unavailable" => "blocked_phase2_retry",
        _ => "failed_phase2",
    };
    Ok(report.with_status(status))
}

struct MemoryMaintenanceLock {
    _file: File,
}

impl MemoryMaintenanceLock {
    fn try_acquire(path: PathBuf) -> anyhow::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}
