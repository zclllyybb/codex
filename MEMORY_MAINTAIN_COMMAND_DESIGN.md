# `codex memory maintain --wait --json` 设计文档

本文说明如何在当前 Codex 代码库中提供一个专门执行 memory 整理的批处理入口：

```bash
codex memory maintain --wait --json
```

目标是：命令返回 `completed` 时，当前可见输入集合的 rollout metadata backfill、Phase 1 memory 抽取、Phase 2 全局 consolidation、workspace baseline reset、job 状态落库、内部线程清理都已经完成。这个入口不依赖 dummy turn，不等待后台 startup task 自己触发，也不通过固定 sleep 猜测后台是否结束。

本文对应当前 checkout 的实现落点：

- CLI 入口：`codex-rs/cli/src/main.rs`
- memory 维护编排：`codex-rs/memories/write/src/maintenance.rs`
- Phase 1 drain：`codex-rs/memories/write/src/phase1.rs`
- Phase 2 waitable consolidation：`codex-rs/memories/write/src/phase2.rs`
- rollout backfill：`codex-rs/rollout/src/metadata.rs`
- state job claim：`codex-rs/state/src/runtime/backfill.rs`、`codex-rs/state/src/runtime/memories.rs`
- agent status watch：`codex-rs/core/src/codex_thread.rs`

## 背景结论

Codex 原来的 memory 写入路径是面向交互式启动和用户 turn 的后台任务，不是可等待的批处理入口。

- `codex-rs/memories/write/src/start.rs` 会启动 detached startup task。
- 原 Phase 1 只 claim 一批 startup 候选，不保证 drain 到固定点。
- 原 Phase 2 会再启动 detached consolidation handler，`phase2::run` 返回不代表 consolidation agent 已经完成。
- 原 Phase 2 claim 会尊重 startup 成功 cooldown；显式维护命令不能把 cooldown 当作“已整理完成”。
- rollout metadata backfill 原先有 complete gate；专用 memory 机器后续同步来的旧路径 session 可能需要 full refresh 才不会漏。

因此，可靠的专用 memory 进程必须是一个正式同步入口，主动推进每一层状态，并在无法完成时返回明确的 blocked/failed 状态。

## 成功语义

`codex memory maintain --wait --json` 成功返回时应满足：

1. 已对 `$CODEX_HOME/sessions` 和 `$CODEX_HOME/archived_sessions` 执行维护模式 backfill。
2. 所有本次 eligible 的 thread 都完成 Phase 1，或被记录为 no-output success。
3. Phase 1 产生的新增/删除/更新信号已进入 Phase 2 输入。
4. Phase 2 global consolidation agent 已到 final status。
5. memory workspace baseline 已 reset。
6. global Phase 2 job 已写入成功状态。
7. 维护命令创建的内部 coordinator thread 已从 `ThreadManager` 移除并 shutdown。
8. state DB 已 close。

如果任一条件无法满足，命令不能 0 退出；必须打印 JSON report 或 human status，并以非 0 结束。

## 当前 CLI 形状

当前最小可靠入口在 `codex-rs/cli/src/main.rs`：

```text
codex memory maintain --wait [--json]
```

约束：

- `--wait` 必填；不带 `--wait` 直接报错，因为本命令的存在意义就是“返回即完成”。
- `--json` 输出结构化 report；不带时输出一行 human status。
- remote mode 被拒绝；memory 维护必须在本地 `$CODEX_HOME` 和 SQLite 上运行。

当前实现暂不暴露额外 tuning 参数。维护策略固定为：

- `BackfillMode::Full`
- Phase 1 每批最多 claim `128`
- `min_rollout_idle_hours = 0`
- Phase 2 使用维护模式 bypass startup success cooldown
- 锁冲突、rate limit 不足、running/retry blocker 都 fail-fast/block-fast，不主动 sleep

后续如果要把这些策略外显，可以在 `MemoryMaintainCommand` 上增加参数，再映射到 `MemoryMaintenanceOptions`。

## CLI 执行流程

`run_memory_maintain_command` 做以下事情：

1. 加载正常 Codex config 和 root `-c` overrides。
2. 强制：
   - `config.ephemeral = false`
   - `config.memories.generate_memories = false`
   - `config.memories.use_memories = false`
3. 初始化 `AuthManager`。
4. 直接初始化 `StateRuntime::init(config.sqlite_home, provider)`，避免 startup backfill gate。
5. 构造本地 `ThreadManager`：
   - 使用 `thread_store_from_config(..., Some(state_db))`
   - 使用 `EnvironmentManager::from_codex_home`
   - 使用 `empty_extension_registry()`
   - 使用 `SessionSource::Internal(InternalSessionSource::MemoryConsolidation)`
6. 启动一个内部 coordinator thread：
   - `ThreadSource::MemoryConsolidation`
   - `InitialHistory::New`
   - `dynamic_tools = []`
7. 调用 `codex_memories_write::maintain_memories(...)`。
8. 无论 report 成功与否，都移除并 shutdown coordinator thread。
9. close state DB。
10. 输出 report。
11. 如果 `report.succeeded() == false`，进程 exit code 为 `1`。

这里 coordinator thread 必须是非 ephemeral；否则内部 context 无法稳定拿到 thread persistence/state DB。

## 维护编排

核心编排位于 `codex-rs/memories/write/src/maintenance.rs`：

```rust
pub async fn maintain_memories(
    thread_manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    config: Arc<Config>,
    options: MemoryMaintenanceOptions,
) -> anyhow::Result<MemoryMaintenanceReport>
```

`MemoryMaintenanceOptions::default()` 当前代表专用 memory 机器的推荐默认值：

```rust
backfill_mode = BackfillMode::Full
max_stage1_claimed_per_batch = 128
min_rollout_idle_hours = 0
```

维护流程：

```text
1. acquire $CODEX_SQLITE_HOME/memory-maintain.lock
2. build MemoryStartupContext
3. run full rollout metadata backfill
4. ensure $CODEX_HOME/memories exists
5. seed extension instructions
6. prune stale Phase 1 outputs
7. check rate limits
8. drain Phase 1 until no eligible candidates remain
9. run Phase 2 in waitable maintenance mode
10. map result to completed / blocked_* / failed_*
```

进程级锁使用 `File::try_lock()`。拿不到锁时返回：

```text
blocked_lock_held
```

这里不等待锁，避免流水线机器出现隐式长时间挂起。如果需要排队语义，应由外部调度系统保证同一 memory store 同时只有一个维护任务。

## Backfill 设计

显式 backfill API 位于 `codex-rs/rollout/src/metadata.rs`：

```rust
pub enum BackfillMode {
    Incremental,
    Full,
}

pub struct BackfillReport {
    pub mode: BackfillMode,
    pub status: BackfillReportStatus,
    pub scanned: usize,
    pub upserted: usize,
    pub failed: usize,
    pub last_watermark: Option<String>,
}

pub async fn refresh_state_from_sessions(
    runtime: &StateRuntime,
    codex_home: &Path,
    default_provider: &str,
    mode: BackfillMode,
) -> anyhow::Result<BackfillReport>
```

维护模式使用 `BackfillMode::Full`，原因是多台流水线机器同步 session 时，晚到文件可能落在旧日期目录或旧文件名排序位置；只依赖 watermark incremental 有漏扫风险。

state 层新增 `try_claim_backfill_for_maintenance`，允许维护命令在 backfill 已经 complete 的情况下重新 claim 一次 backfill lease。它仍然尊重已有 running lease，避免两个进程同时刷新同一 SQLite。

Backfill report 判定：

- `Completed`：本次 backfill 完成。
- `AlreadyRunning`：有其他 backfill 持有有效 lease，维护命令返回 `blocked_backfill_running`。
- `failed > 0`：维护命令返回 `failed_backfill`。

## Phase 1 Drain 设计

Phase 1 维护 API 位于 `codex-rs/memories/write/src/phase1.rs`：

```rust
pub(crate) async fn drain_for_maintenance(
    context: Arc<MemoryStartupContext>,
    config: Arc<Config>,
    options: Phase1DrainOptions,
) -> Phase1DrainReport
```

它和 startup path 的区别是：startup path 只跑一批；maintenance path 会循环 claim，直到没有 eligible candidate。

state 层新增：

```rust
pub struct Stage1MaintenanceClaimParams {
    pub worker_id: ThreadId,
    pub claim_limit: i64,
    pub now: DateTime<Utc>,
    pub min_rollout_idle_hours: i64,
    pub lease_seconds: i64,
    pub retry_limit: i64,
}

pub struct Stage1ClaimBatch {
    pub claims: Vec<Stage1JobClaim>,
    pub skipped_running: Vec<MemoryJobSnapshot>,
    pub skipped_retry_backoff: Vec<MemoryJobSnapshot>,
    pub skipped_retry_exhausted: Vec<MemoryJobSnapshot>,
}

pub async fn claim_stage1_jobs_for_maintenance(...)
```

`Phase1DrainReport` 聚合：

- claim 成功数
- 成功 with output 数
- 成功 no output 数
- 失败数
- claim error
- running blocker
- retry backoff blocker
- retry exhausted blocker

完成判定不能只看“这次 claim 为空”。必须确认：

- 没有 claims；
- 没有 running blocker；
- 没有 retry blocker；
- 没有 claim error。

如果存在 blocker，维护命令返回 `blocked_phase1_jobs`；如果本次执行失败，返回 `failed_phase1`。

## Phase 2 Waitable Consolidation 设计

Phase 2 维护 API 位于 `codex-rs/memories/write/src/phase2.rs`：

```rust
pub(crate) async fn run_for_maintenance(
    context: Arc<MemoryStartupContext>,
    config: Arc<Config>,
) -> Phase2Report
```

它复用 startup path 的大部分逻辑，但用两个关键差异保证“返回即完成”：

1. claim 使用 `Phase2ClaimMode::MaintenanceBypassCooldown`。
2. agent completion 使用 waitable path，而不是 detached cleanup task。

state 层新增：

```rust
pub enum Phase2ClaimMode {
    Startup,
    MaintenanceBypassCooldown,
}

pub async fn try_claim_global_phase2_job_with_mode(...)
```

`Startup` 继续尊重最近成功后的 cooldown。`MaintenanceBypassCooldown` 绕过 startup success cooldown，但仍尊重：

- 当前 running lease；
- retry/backoff；
- global job ownership；
- DB 事务一致性。

这很关键：专用维护命令的目标不是“少跑”，而是把当前输入集合整理完。只要 Phase 1 有新输入，startup cooldown 就不能阻止显式维护。

### Agent 等待机制

`codex-rs/core/src/codex_thread.rs` 新增：

```rust
pub fn subscribe_agent_status(&self) -> watch::Receiver<AgentStatus>
```

Phase 2 wait path 使用 status watch：

```text
loop:
  if current status is final => break
  select:
    status_rx.changed()
    heartbeat_interval.tick()
```

这避免了原先每秒 poll status 的不必要等待。维护命令只在这些情况下等待：

- 模型请求实际运行；
- agent status 真实变化；
- 必要 heartbeat 续租；
- SQLite / filesystem I/O。

Phase 2 final 后，同步执行：

1. reset memory workspace baseline；
2. mark global Phase 2 job succeeded/failed；
3. 等待 consolidation agent cleanup；
4. 返回 `Phase2Report`。

`Phase2Report::succeeded()` 当前接受：

- `succeeded`
- `succeeded_no_workspace_changes`

其他状态由 `maintenance.rs` 映射为 blocked 或 failed。

## JSON Report

`MemoryMaintenanceReport` 当前字段：

```rust
pub struct MemoryMaintenanceReport {
    pub status: String,
    pub memory_root: String,
    pub lock_path: String,
    pub failure_reason: Option<String>,
    pub backfill: Option<BackfillReport>,
    pub phase1: Option<Phase1DrainReport>,
    pub phase2: Option<Phase2Report>,
}
```

成功示例：

```json
{
  "status": "completed",
  "memory_root": "/path/to/.codex/memories",
  "lock_path": "/path/to/.codex/memory-maintain.lock",
  "backfill": {
    "mode": "full",
    "status": "completed",
    "scanned": 1200,
    "upserted": 1200,
    "failed": 0,
    "last_watermark": "sessions/2026/06/17/rollout-..."
  },
  "phase1": {
    "stats": {
      "claimed": 1200,
      "succeeded_with_output": 240,
      "succeeded_no_output": 960,
      "failed": 0
    },
    "claim_errors": [],
    "skipped_running": [],
    "skipped_retry_backoff": [],
    "skipped_retry_exhausted": []
  },
  "phase2": {
    "status": "succeeded",
    "input_watermark": 1780000000,
    "failure": null
  }
}
```

常见非成功状态：

- `blocked_lock_held`
- `blocked_backfill_running`
- `failed_backfill`
- `blocked_rate_limit`
- `blocked_phase1_jobs`
- `failed_phase1`
- `blocked_phase2_running`
- `blocked_phase2_retry`
- `failed_phase2`
- `failed_no_state_db`

外部流水线只应把 `status == "completed"` 视为成功。其他状态都应该报警或重试，不能当作整理完成。

## 多机流水线最佳实践

不要合并 SQLite。SQLite 是 memory 机器的派生状态，不适合作为多台流水线机器的合并对象。

每台流水线机器完成任务后，只上传完整 session 文件：

```text
$CODEX_HOME/sessions/**/rollout-*.jsonl
$CODEX_HOME/sessions/**/rollout-*.jsonl.zst
$CODEX_HOME/archived_sessions/**/rollout-*.jsonl
$CODEX_HOME/archived_sessions/**/rollout-*.jsonl.zst
```

上传要求：

- 文件级复制；
- 不解析、不拼接、不截断 JSONL；
- 保留相对路径；
- 记录 size 和 sha256；
- 同一路径不同 sha256 必须隔离为 conflict，不能覆盖；
- memory 机器运行 maintain 时，输入目录应处于静止状态。

memory 机器需要持久保存：

```text
$CODEX_HOME/sessions/
$CODEX_HOME/archived_sessions/
$CODEX_HOME/memories/
$CODEX_SQLITE_HOME/memories_1.sqlite
```

`state_5.sqlite` 可以保留，但不应作为唯一真相。维护命令会从 session 文件 full backfill 刷新 state。

如果只想把整理后的 memory 带到一个新 Codex 环境使用，复制：

```text
$CODEX_HOME/memories/
```

如果新环境还要继续执行 memory 维护，则还要复制：

```text
$CODEX_HOME/sessions/
$CODEX_HOME/archived_sessions/
$CODEX_SQLITE_HOME/memories_1.sqlite
```

复制后在新环境跑一次：

```bash
codex memory maintain --wait --json
```

## 为什么没有不必要等待

本设计的等待点都是主动条件驱动：

- 进程锁：默认 try-lock，拿不到立即 blocked。
- backfill：主动扫描当前输入文件。
- Phase 1：主动 claim + 执行，循环到固定点。
- Phase 2：主动 claim + 启动 consolidation agent。
- agent completion：使用 status watch，只有 status change 或 heartbeat tick 唤醒。
- cleanup：同步 await 自己启动的 agent shutdown。

没有 dummy user turn，没有等待 startup task，没有固定 sleep 来“等后台整理完”。

## 当前验证

已在当前 checkout 跑过：

```bash
cd codex-rs
just fmt
just test -p codex-state
just test -p codex-rollout
just test -p codex-memories-write
http_proxy= HTTP_PROXY= https_proxy= HTTPS_PROXY= all_proxy= ALL_PROXY= \
  NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
  just test -p codex-cli
just bazel-lock-update
just bazel-lock-check
just fix -p codex-state -p codex-rollout -p codex-memories-write -p codex-cli -p codex-core
```

结果：

- `codex-state`：149/149 通过
- `codex-rollout`：70/70 通过
- `codex-memories-write`：36/36 通过
- `codex-cli`：282/282 通过
- `bazel-lock-check` 通过
- `git diff --check` 通过

第一次直接跑 `codex-cli` 时，`doctor::tests::mcp_check_warns_for_optional_http_reachability` 因当前 shell 设置了 `http_proxy` / `https_proxy` / `all_proxy` 而失败；该测试假设 `127.0.0.1:9` 不可达。清理 proxy env 后，单测和完整 `codex-cli` 测试均通过。按仓库规则，`just fix` 后未再重复运行测试。

因为改动触及 `codex-core`，全量 `just test` 需要单独确认后再跑。

## 后续可选增强

当前实现是最小可靠版本。后续可把以下策略做成 CLI 参数：

- `--backfill incremental|full`
- `--max-stage1-batch <N>`
- `--min-idle-hours <N>`
- `--phase2-cooldown respect|bypass`
- `--lock fail-fast|wait`
- `--rate-limit fail-fast|ignore`
- `--retry-errors fail-fast|force-once|skip`

这些不是保证语义的前置条件。当前默认策略已经适合专用 memory 机器周期性维护：full backfill、立即处理、drain Phase 1、等待 Phase 2 完成、非成功状态 fail-fast。

## 结论

基于当前代码，专用 memory 整理进程是可行的，并且已经可以按正式批处理入口实现：

```bash
codex memory maintain --wait --json
```

它的核心不是“触发一次后台整理”，而是把原来分散在 startup task、Phase 1 claim、Phase 2 detached handler、rollout backfill 中的步骤串成一个同步、有锁、有 report、可由流水线判断结果的维护流程。

流水线侧只需要保证 session 文件完整汇总到 memory 机器，并且 maintain 运行期间输入目录静止。命令返回 `completed` 后，`$CODEX_HOME/memories/` 就是可以发布给其他 Codex 环境消费的整理结果。
