# Codex Memory 多机流水线归并方案

本文针对当前 checkout 的 Codex memory 实现，描述多台流水线机器如何把会话结果汇总到一台 memory 机器，再由 memory 机器统一生成、合并、发布 memories。

核心结论：

- 跨机器只合并 rollout session 文件。
- 不要合并多台机器的 SQLite。
- 一台 memory 机器串行维护自己的 `state_5.sqlite`、`memories_1.sqlite` 和 `memories/`。
- 新环境只消费 memory 时，只需要复制 `memories/`。

## 代码依据

- Rollout 文件目录是 `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`，常量在 `codex-rs/rollout/src/lib.rs`，路径生成在 `codex-rs/rollout/src/recorder.rs`。
- Backfill 扫描 `$CODEX_HOME/sessions/` 和 `$CODEX_HOME/archived_sessions/`，见 `codex-rs/rollout/src/metadata.rs`。
- Backfill 从完整 rollout items 中恢复最新 `memory_mode`，见 `extract_metadata_from_rollout`。
- `generate_memories = false` 会把新会话写成 `memory_mode = "disabled"`，见 `codex-rs/rollout/src/recorder.rs`。
- Phase 1 只处理 `threads.memory_mode = 'enabled'` 的线程，见 `codex-rs/state/src/runtime/memories.rs`。
- Phase 2 是全局 singleton job，并写同一个 `$CODEX_HOME/memories/` workspace，见 `codex-rs/memories/write/src/phase2.rs`。
- Memory 根目录是 `$CODEX_HOME/memories`，见 `codex-rs/memories/write/src/lib.rs`。
- 读路径只从 `$CODEX_HOME/memories/memory_summary.md` 注入 prompt，见 `codex-rs/ext/memories/src/prompts.rs`。
- SQLite 文件名是 `state_5.sqlite` 和 `memories_1.sqlite`，见 `codex-rs/state/src/lib.rs`。

## 每台流水线机器要做的动作

1. 保持 session 正常记录。

   不要把 `[memories].generate_memories` 设成 `false`。这个配置会把新 session 记录为 `memory_mode = "disabled"`，后续离线 Phase 1 会跳过。

2. 如果线上机器只想收集 session，不想在线跑后台 memory：

   - 优先用代码开关或运行配置关闭 memory startup pipeline。
   - 可以关闭 `Feature::MemoryTool` 来阻止当前启动路径进入 memory pipeline。
   - 不要通过 `generate_memories = false` 达成这个目的。

3. 等 Codex 进程结束后再采集文件。

   不要复制仍在追加中的 rollout。最简单的动作是任务结束、Codex 进程退出后采集。

4. 上传完整 rollout 文件。

   上传以下路径中的文件，保持相对路径：

   ```text
   $CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl
   $CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl.zst
   $CODEX_HOME/archived_sessions/**/rollout-*.jsonl
   $CODEX_HOME/archived_sessions/**/rollout-*.jsonl.zst
   ```

   操作要求：

   - 不解析 JSONL。
   - 不截断 JSONL。
   - 不只复制第一行或最后几行。
   - 不自己追加 `session_meta`。
   - 不重写字段。
   - 不改变行顺序。
   - 不把多个 rollout 合并成一个文件。

   你要做的就是文件级复制。`memory_mode` 更新本来就在完整 rollout 文件中以 `SessionMeta` 行出现；backfill 会从完整文件中恢复最新状态。

5. 保留文件 mtime。

   Backfill 用文件修改时间作为 thread 的 `updated_at`。复制时要保留 mtime，否则 memory 机器可能认为 session 刚刚更新，受 `min_rollout_idle_hours` 影响暂时不处理。

   如果用 `rsync`，使用归档模式，例如：

   ```bash
   rsync -a "$CODEX_HOME/sessions/" "$DEST/sessions/"
   rsync -a "$CODEX_HOME/archived_sessions/" "$DEST/archived_sessions/"
   ```

   如果用对象存储，上传时把原始 mtime 作为 metadata 保存，下载恢复时设置回文件 mtime。

6. 生成 checksum manifest。

   每个上传批次生成一份 manifest，至少包含：

   ```text
   relative_path
   size_bytes
   sha256
   mtime_unix
   source_machine_id
   pipeline_run_id
   ```

   这个 manifest 是中心存储去重和冲突检测用的，不是 Codex 直接读取的输入。

7. 不上传这些运行时状态作为跨机合并输入：

   ```text
   $CODEX_SQLITE_HOME/state_5.sqlite
   $CODEX_SQLITE_HOME/memories_1.sqlite
   $CODEX_SQLITE_HOME/logs_2.sqlite
   $CODEX_SQLITE_HOME/goals_1.sqlite
   $CODEX_HOME/memories/
   ```

   这些可以作为单机调试备份，但不要拿来做多机 merge。

8. 如果你依赖外部上下文污染过滤，额外处理 polluted 状态。

   当前代码把 `memory_mode = "polluted"` 写进 SQLite，不写回 rollout 文件。只收集 sessions 能恢复普通 `enabled` / `disabled`，但不能可靠恢复自动 polluted。

   可选动作：

   - 方案 A：在流水线结束时额外导出一个 `thread_id -> memory_mode` manifest，memory 机器 backfill 后应用。
   - 方案 B：改 Codex 代码，让 polluted 状态也追加一条 `SessionMeta` 到 rollout 文件。
   - 方案 C：如果你的流水线不启用外部上下文污染过滤，就不需要额外动作。

## 中心存储要做的动作

1. 按 Codex 相对路径保存 session 文件。

   推荐布局：

   ```text
   collected/
     sessions/YYYY/MM/DD/rollout-*.jsonl
     sessions/YYYY/MM/DD/rollout-*.jsonl.zst
     archived_sessions/...
     manifests/<pipeline_run_id>.jsonl
     conflicts/
   ```

2. 做幂等写入。

   对同一个 `relative_path`：

   - 文件不存在：写入。
   - 文件存在且 sha256 相同：跳过。
   - 文件存在但 sha256 不同：放入 `conflicts/`，不要覆盖原文件。

3. 不修改 session 文件内容。

   中心存储只负责收集、去重、冲突隔离，不负责整理 memory。

## Memory 机器要做的动作

1. 准备持久目录。

   Memory 机器需要持久保存：

   ```text
   $CODEX_HOME/sessions/
   $CODEX_HOME/archived_sessions/
   $CODEX_HOME/memories/
   $CODEX_SQLITE_HOME/memories_1.sqlite
   ```

   `CODEX_SQLITE_HOME` 如果未设置，通常和 `CODEX_HOME` 使用同一个根目录下的 SQLite 路径。

2. 从中心存储同步 session 文件。

   把中心存储的：

   ```text
   collected/sessions/
   collected/archived_sessions/
   ```

   同步到 memory 机器的：

   ```text
   $CODEX_HOME/sessions/
   $CODEX_HOME/archived_sessions/
   ```

   同步时继续保留 mtime。

3. 重建或刷新 `state_5.sqlite`。

   当前 backfill 一旦记录为 `Complete`，后续不会自动扫描新复制进来的 sessions。因此推荐动作是每轮维护时重建 `state_5.sqlite`，让 backfill 从当前 sessions 全量恢复 thread 索引。

   不推荐多机合并 `state_5.sqlite`。

   更好的代码改造是提供一个显式命令：

   ```bash
   codex memory maintain --force-backfill --wait --json
   ```

   这个命令应当清理或重置 backfill 状态，然后重新扫描 sessions。

4. 保留 `memories_1.sqlite`。

   `memories_1.sqlite` 是 memory 机器自己的阶段产物和 job 状态：

   - `stage1_outputs`
   - `jobs`
   - Phase 2 selection watermark

   它应该在 memory 机器上持久保留，不从流水线机器合并。

5. 保留 `memories/`。

   `memories/` 是最终可消费的 memory workspace。Phase 2 会在这个目录里维护：

   ```text
   memories/memory_summary.md
   memories/MEMORY.md
   memories/rollout_summaries/
   memories/skills/
   memories/extensions/
   ```

6. 设置 memory 机器配置。

   推荐维护配置：

   ```toml
   [memories]
   generate_memories = true
   use_memories = true
   max_rollouts_per_startup = 128
   max_rollout_age_days = 90
   min_rollout_idle_hours = 1
   max_raw_memories_for_consolidation = 4096
   min_rate_limit_remaining_percent = 0
   ```

   说明：

   - `generate_memories = true` 保证 session 不被标记为 disabled。
   - `max_rollouts_per_startup = 128` 是当前配置允许的上限。
   - `max_rollout_age_days = 90` 是当前配置允许的上限。
   - `min_rollout_idle_hours = 1` 是当前配置允许的下限。
   - `max_raw_memories_for_consolidation = 4096` 是当前配置允许的上限。
   - `min_rate_limit_remaining_percent = 0` 适合专用维护机器；如果要保护账号额度，可以设回业务阈值。

7. 串行运行 memory pipeline。

   同一套 `$CODEX_HOME/memories/` 只能有一个 Phase 2 writer。用分布式锁或作业调度系统保证同一时间只有一个 memory 维护任务运行。

   当前代码没有正式的手动 `memory/consolidate` RPC 或 CLI。现有触发路径是有用户输入的 root turn 启动后异步调用 memory pipeline。工程化落地时建议新增维护命令，而不是依赖 dummy turn。

8. 等待完成。

   不要固定 sleep。轮询 `memories_1.sqlite`：

   ```sql
   SELECT COUNT(*)
   FROM jobs
   WHERE kind IN ('memory_stage1', 'memory_consolidate_global')
     AND status = 'running'
     AND lease_until > strftime('%s','now');
   ```

   结果为 `0` 表示没有仍持有有效 lease 的运行中 memory job。

   再查全局 Phase 2：

   ```sql
   SELECT status, finished_at, retry_at, retry_remaining, last_error,
          input_watermark, last_success_watermark
   FROM jobs
   WHERE kind = 'memory_consolidate_global'
     AND job_key = 'global';
   ```

   推荐完成条件：

   - 没有有效运行中的 `memory_stage1` / `memory_consolidate_global` job。
   - 全局 Phase 2 job 为 `done`。
   - `finished_at` 晚于本轮维护开始时间。
   - `$CODEX_HOME/memories/` git workspace clean。
   - `$CODEX_HOME/memories/phase2_workspace_diff.md` 不存在。

## 新环境只消费 memory 时要做的动作

1. 复制 memory 目录。

   从 memory 机器复制：

   ```text
   $CODEX_HOME/memories/
   ```

   到新环境：

   ```text
   $NEW_CODEX_HOME/memories/
   ```

2. 至少确认这些文件存在：

   ```text
   memories/memory_summary.md
   memories/MEMORY.md
   ```

   推荐完整保留：

   ```text
   memories/memory_summary.md
   memories/MEMORY.md
   memories/rollout_summaries/
   memories/skills/
   memories/extensions/
   ```

3. 打开 memory 读路径。

   新环境需要启用 `Feature::MemoryTool`，并设置：

   ```toml
   [memories]
   use_memories = true
   ```

4. 不需要复制这些文件：

   ```text
   sessions/
   archived_sessions/
   state_5.sqlite
   memories_1.sqlite
   logs_2.sqlite
   goals_1.sqlite
   ```

   只消费 memory 时，这些不是必须输入。

## 新环境要继续生成 memory 时要做的动作

1. 复制最终 memory workspace：

   ```text
   $CODEX_HOME/memories/
   ```

2. 复制历史 session 输入：

   ```text
   $CODEX_HOME/sessions/
   $CODEX_HOME/archived_sessions/
   ```

3. 复制 memory 机器的阶段状态：

   ```text
   $CODEX_SQLITE_HOME/memories_1.sqlite
   ```

4. 在新环境重建 `state_5.sqlite`。

   不要从多台 worker 合并 `state_5.sqlite`。新环境应从 `sessions/` 和 `archived_sessions/` 重新 backfill thread 索引。

5. 继续使用单 writer 规则。

   如果这个新环境会继续跑 Phase 2，它就应成为新的唯一 memory writer，或者接入同一个分布式锁。

## 不要做的事

- 不要把多台 worker 的 `memories_1.sqlite` 拼起来。
- 不要把多台 worker 的 `state_5.sqlite` 拼起来。
- 不要只传 `memory_summary.md` 后继续生成 memory；这只能消费，不能可靠增量维护。
- 不要用 `generate_memories = false` 来表达“稍后离线整理”。
- 不要编辑 raw rollout。
- 不要把多个 rollout JSONL 拼成一个文件。
- 不要丢 mtime。
- 不要并发运行多个 Phase 2 consolidator 写同一个 `memories/`。

## 建议补的代码功能

为了把这套方案做成稳定生产流程，建议新增一个正式维护入口：

```bash
codex memory maintain --force-backfill --wait --json
```

建议功能：

- `--codex-home <path>`：指定包含 `sessions/` 和 `memories/` 的根目录。
- `--sqlite-home <path>`：指定 SQLite 根目录。
- `--force-backfill`：重扫 sessions，刷新 `state_5.sqlite` 的 thread 索引。
- `--max-rollouts <n>`：覆盖本轮 Phase 1 最大处理数。
- `--run-phase2` / `--no-run-phase2`：控制是否跑全局 consolidation。
- `--wait`：等待 Phase 1 和 Phase 2 完成。
- `--timeout <duration>`：超过时间返回非零。
- `--json`：输出机器可读状态，包括 processed、skipped、running、last_error、shareable memory artifact path。

同时建议新增一个线上配置：

```toml
[memories]
generate_memories = true
record_only = true
```

`record_only = true` 的语义应是：

- session 仍记录为 memory eligible。
- 不启动 Phase 1 / Phase 2 后台任务。
- 不影响以后离线 memory 机器处理这些 sessions。

这个配置比把 `generate_memories` 设为 `false` 更符合流水线场景。
