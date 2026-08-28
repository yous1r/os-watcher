# 历史记录 500 MiB 上限实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 将 SQLite 主数据库限制在 500 MiB，满容量时淘汰全局最旧指标并保留最新指标。

**架构：** `Database` 在连接建立时按 SQLite 实际页大小设置 `max_page_count`。启动时收缩已有超限文件；运行时只有 `SQLITE_FULL` 会触发批量淘汰并重试。现有基于保留时间的清理保持不变。

**技术栈：** Rust 2021、Tokio、SQLx 0.8 SQLite、tempfile、cargo test

---

## 文件结构

- 修改 `src/storage.rs`：容量常量、连接级 PRAGMA、启动收缩、最旧记录淘汰、满容量重试和单元测试。
- 修改 `config.full.example.toml`：记录 Web 节点 500 MiB 固定硬上限。
- 修改 `config.node.example.toml`：同步记录固定硬上限。

不修改 `src/main.rs`、`src/gossip.rs` 或配置数据结构；现有调用方继续使用 `Database::new` 和 `store_metrics`。

### 任务 1：最旧指标淘汰原语

**文件：**
- 修改：`src/storage.rs:1-191`
- 测试：`src/storage.rs` 文件末尾新增 `tests` 模块

- [ ] **步骤 1：编写失败的淘汰顺序测试**

在 `src/storage.rs` 末尾加入测试模块。测试直接插入最小合法行，使断言只关注全局时间顺序：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    async fn temp_database() -> (TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();
        (dir, db)
    }

    async fn insert_raw_metric(db: &Database, timestamp: &str, hostname: &str) {
        sqlx::query(
            r#"INSERT INTO metrics_history
               (node_id, hostname, timestamp, cpu_usage, memory_usage,
                memory_used_bytes, memory_total_bytes, uptime_seconds, raw_json)
               VALUES (?, ?, ?, 0, 0, 0, 0, 0, '{}')"#,
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(hostname)
        .bind(timestamp)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn delete_oldest_metrics_orders_by_timestamp_then_id() {
        let (_dir, db) = temp_database().await;
        let same_time = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        insert_raw_metric(&db, &same_time.to_rfc3339(), "older-id").await;
        insert_raw_metric(&db, &same_time.to_rfc3339(), "newer-id").await;
        insert_raw_metric(
            &db,
            &Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap().to_rfc3339(),
            "newest-time",
        )
        .await;

        assert_eq!(db.delete_oldest_metrics_batch(2).await.unwrap(), 2);

        let remaining: Vec<String> = sqlx::query_scalar(
            "SELECT hostname FROM metrics_history ORDER BY timestamp, id",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(remaining, vec!["newest-time"]);
    }
}
```

- [ ] **步骤 2：运行测试并确认失败**

运行：

```bash
cargo test storage::tests::delete_oldest_metrics_orders_by_timestamp_then_id -- --exact
```

预期：编译失败，指出 `delete_oldest_metrics_batch` 尚不存在。

- [ ] **步骤 3：实现最小淘汰原语和时间索引**

在 `src/storage.rs` 顶部加入批量大小常量：

```rust
const EVICTION_BATCH_SIZE: i64 = 10_000;
```

本任务不改 `Database::new`；连接硬上限由任务 2 一次完成，避免临时构造路径。

在 `run_migrations` 的 `idx_metrics_node_time` 后增加：

```sql
CREATE INDEX IF NOT EXISTS idx_metrics_time
    ON metrics_history(timestamp, id);
```

在 `impl Database` 内增加：

```rust
async fn delete_oldest_metrics_batch(&self, limit: i64) -> Result<u64> {
    let result = sqlx::query(
        r#"DELETE FROM metrics_history
           WHERE id IN (
               SELECT id FROM metrics_history
               ORDER BY timestamp ASC, id ASC
               LIMIT ?
           )"#,
    )
    .bind(limit)
    .execute(&self.pool)
    .await?;
    Ok(result.rows_affected())
}
```

- [ ] **步骤 4：运行淘汰测试并确认通过**

运行：

```bash
cargo test storage::tests::delete_oldest_metrics_orders_by_timestamp_then_id -- --exact
```

预期：PASS。

- [ ] **步骤 5：提交淘汰原语**

```bash
git add src/storage.rs
git commit -m "feat: 增加最旧指标淘汰原语"
```

### 任务 2：连接硬上限和已有数据库收缩

**文件：**
- 修改：`src/storage.rs`
- 测试：`src/storage.rs::tests`

- [ ] **步骤 1：编写失败的启动收缩测试**

在测试模块加入页大小帮助函数和测试。先用 4 MiB 上限创建较大数据库，关闭后用 256 KiB 上限重新打开：

```rust
async fn database_bytes(db: &Database) -> u64 {
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    page_size as u64 * page_count as u64
}

#[tokio::test]
async fn opening_oversized_database_evicts_oldest_and_shrinks_file() {
    const LIMIT: u64 = 256 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized.db");
    let path = path.to_str().unwrap();

    let db = Database::new_with_max_size(path, 4 * 1024 * 1024).await.unwrap();
    for index in 0..80 {
        let timestamp = Utc.timestamp_opt(index, 0).unwrap().to_rfc3339();
        insert_raw_metric(&db, &timestamp, &format!("{index:03}-{}", "x".repeat(8 * 1024))).await;
    }
    assert!(database_bytes(&db).await > LIMIT);
    db.pool.close().await;

    let db = Database::new_with_max_size(path, LIMIT).await.unwrap();
    assert!(database_bytes(&db).await <= LIMIT);
    let newest: String = sqlx::query_scalar(
        "SELECT hostname FROM metrics_history ORDER BY timestamp DESC, id DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(newest.starts_with("079-"));
}
```

- [ ] **步骤 2：运行测试并确认失败**

运行：

```bash
cargo test storage::tests::opening_oversized_database_evicts_oldest_and_shrinks_file -- --exact
```

预期：编译失败，指出 `Database::new_with_max_size` 尚不存在。

- [ ] **步骤 3：实现连接级上限与启动收缩**

给 `Database` 保存测试/生产共用的字节上限，并导入 `anyhow::ensure`、`sqlx::SqliteConnection`、`tracing::warn`：

```rust
pub struct Database {
    pool: SqlitePool,
    max_size_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct PageStats {
    page_size: u64,
    page_count: u64,
    freelist_count: u64,
}

impl PageStats {
    fn file_bytes(self) -> u64 {
        self.page_size * self.page_count
    }

    fn used_bytes(self) -> u64 {
        self.page_size * self.page_count.saturating_sub(self.freelist_count)
    }
}
```

加入完整构造路径；内存数据库使用单连接，避免多个独立 `:memory:` 数据库：

```rust
const MIB: u64 = 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 500 * MIB;

pub async fn new(db_path: &str) -> Result<Self> {
    Self::new_with_max_size(db_path, MAX_DATABASE_BYTES).await
}

async fn new_with_max_size(db_path: &str, max_size_bytes: u64) -> Result<Self> {
    ensure!(max_size_bytes > 0, "database size limit must be positive");
    let url = if db_path == ":memory:" {
        "sqlite::memory:".to_string()
    } else {
        format!("sqlite://{}?mode=rwc", db_path)
    };
    let max_connections = if db_path == ":memory:" { 1 } else { 5 };
    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .idle_timeout(Duration::from_secs(30))
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _metadata| {
            Box::pin(async move {
                apply_max_page_count(connection, max_size_bytes)
                    .await
                    .map(|_| ())
            })
        })
        .connect(&url)
        .await?;

    let db = Self { pool, max_size_bytes };
    db.run_migrations().await?;
    db.enforce_startup_size_limit().await?;
    info!("Database initialized at {}", db_path);
    Ok(db)
}
```

`after_connect` 让当前及后续池连接都按实际页大小应用同一上限。现有超限库会暂时被
SQLite 钳制到当前页数，随后由启动收缩路径降低。

实现以下帮助函数；PRAGMA 数值由整数格式化，不接收外部字符串：

```rust
async fn apply_max_page_count(
    connection: &mut SqliteConnection,
    max_size_bytes: u64,
) -> sqlx::Result<u64> {
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut *connection)
        .await?;
    let max_pages = max_size_bytes / page_size as u64;
    let statement = format!("PRAGMA max_page_count = {max_pages}");
    let applied: i64 = sqlx::query_scalar(&statement)
        .fetch_one(&mut *connection)
        .await?;
    Ok(applied as u64)
}
```

`new_with_max_size` 在迁移后调用 `enforce_startup_size_limit`。该方法读取三个 PRAGMA；超限时按 90% 水位循环淘汰，执行一次 `VACUUM`，再设置并核验页数上限：

```rust
async fn page_stats(&self) -> Result<PageStats> {
    Ok(PageStats {
        page_size: sqlx::query_scalar::<_, i64>("PRAGMA page_size")
            .fetch_one(&self.pool).await? as u64,
        page_count: sqlx::query_scalar::<_, i64>("PRAGMA page_count")
            .fetch_one(&self.pool).await? as u64,
        freelist_count: sqlx::query_scalar::<_, i64>("PRAGMA freelist_count")
            .fetch_one(&self.pool).await? as u64,
    })
}

async fn enforce_startup_size_limit(&self) -> Result<()> {
    let original = self.page_stats().await?;
    if original.file_bytes() > self.max_size_bytes {
        let target = self.max_size_bytes * 9 / 10;
        let mut deleted = 0;
        while self.page_stats().await?.used_bytes() > target {
            let batch = self.delete_oldest_metrics_batch(EVICTION_BATCH_SIZE).await?;
            ensure!(batch > 0, "database exceeds size limit but has no metric history to evict");
            deleted += batch;
        }
        sqlx::query("VACUUM").execute(&self.pool).await?;
        warn!("Evicted {} old metric records while shrinking database", deleted);
    }

    let mut connection = self.pool.acquire().await?;
    let applied = apply_max_page_count(&mut connection, self.max_size_bytes).await?;
    let final_stats = self.page_stats().await?;
    let allowed_pages = self.max_size_bytes / final_stats.page_size;
    ensure!(final_stats.file_bytes() <= self.max_size_bytes, "database remains above size limit");
    ensure!(applied <= allowed_pages, "failed to apply database page limit");
    Ok(())
}
```

`SqlitePoolOptions::connect` 在 `Database::new_with_max_size` 返回前只建立初始化连接；该连接在收缩后显式重设上限，后续连接通过 `after_connect` 获得相同上限，因此无需关闭和重建连接池。

- [ ] **步骤 4：运行启动收缩测试与淘汰测试**

运行：

```bash
cargo test storage::tests
```

预期：两个现有 storage 测试均 PASS。

- [ ] **步骤 5：提交启动容量治理**

```bash
git add src/storage.rs
git commit -m "feat: 限制历史数据库容量"
```

### 任务 3：满容量时淘汰并重试最新指标

**文件：**
- 修改：`src/storage.rs:83-108`
- 测试：`src/storage.rs::tests`

- [ ] **步骤 1：编写失败的满容量行为测试**

加入真实 `SystemMetrics` 夹具和测试：

```rust
fn metrics_at(timestamp: chrono::DateTime<Utc>, payload_bytes: usize) -> SystemMetrics {
    SystemMetrics {
        timestamp,
        cpu: CpuMetrics { usage_percent: 0.0, core_usages: vec![], core_count: 1 },
        memory: MemoryMetrics {
            total_bytes: 1,
            used_bytes: 0,
            available_bytes: 1,
            usage_percent: 0.0,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
        },
        disks: vec![],
        physical_disks: vec![],
        networks: vec![],
        load_average: None,
        top_processes: vec![],
        uptime_seconds: 0,
        os_name: "test".to_string(),
        hostname: "x".repeat(payload_bytes),
    }
}

#[tokio::test]
async fn full_database_evicts_oldest_and_keeps_latest_metric() {
    const LIMIT: u64 = 256 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("full.db");
    let db = Database::new_with_max_size(path.to_str().unwrap(), LIMIT).await.unwrap();
    let node_id = uuid::Uuid::new_v4();

    for second in 0..80 {
        let metrics = metrics_at(Utc.timestamp_opt(second, 0).unwrap(), 8 * 1024);
        db.store_metrics(&node_id, &metrics).await.unwrap();
    }

    assert!(database_bytes(&db).await <= LIMIT);
    let latest_timestamp: String = sqlx::query_scalar(
        "SELECT timestamp FROM metrics_history ORDER BY timestamp DESC, id DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(latest_timestamp, Utc.timestamp_opt(79, 0).unwrap().to_rfc3339());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(count < 80, "the quota must evict at least one old metric");
}
```

- [ ] **步骤 2：运行测试并确认失败**

运行：

```bash
cargo test storage::tests::full_database_evicts_oldest_and_keeps_latest_metric -- --exact
```

预期：FAIL，现有 `store_metrics` 返回 SQLite `database or disk is full`。

- [ ] **步骤 3：实现仅针对 SQLITE_FULL 的淘汰重试**

加入错误分类函数：

```rust
fn is_database_full(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("13")
    )
}
```

`store_metrics` 只序列化一次，把现有 INSERT 放入循环。成功即返回；非容量错误直接转换为
`anyhow::Error`；容量错误先保存，淘汰一批后重试；没有记录可删时返回保存的容量错误：

```rust
loop {
    match sqlx::query(INSERT_METRICS_SQL)
        // 保留现有九个 bind，引用循环外的 node_id_str、ts 和 raw
        .execute(&self.pool)
        .await
    {
        Ok(_) => return Ok(()),
        Err(error) if is_database_full(&error) => {
            let deleted = self.delete_oldest_metrics_batch(EVICTION_BATCH_SIZE).await?;
            if deleted == 0 {
                return Err(error.into());
            }
            warn!("Database full; evicted {} oldest metric records", deleted);
        }
        Err(error) => return Err(error.into()),
    }
}
```

不匹配错误消息文本，不对锁冲突或损坏执行删除。

- [ ] **步骤 4：运行所有 storage 测试**

运行：

```bash
cargo test storage::tests
```

预期：全部 PASS，无 warning。

- [ ] **步骤 5：提交满容量重试**

```bash
git add src/storage.rs
git commit -m "feat: 满容量时保留最新指标"
```

### 任务 4：时间清理回归和配置说明

**文件：**
- 修改：`src/storage.rs`
- 修改：`config.full.example.toml:37-39`
- 修改：`config.node.example.toml:34-36`

- [ ] **步骤 1：编写失败的时间清理回归测试**

为了避免依赖真实当前时间边界，插入一条 48 小时前和一条当前记录：

```rust
#[tokio::test]
async fn cleanup_old_metrics_only_removes_expired_rows() {
    let (_dir, db) = temp_database().await;
    insert_raw_metric(&db, &(Utc::now() - chrono::Duration::hours(48)).to_rfc3339(), "expired").await;
    insert_raw_metric(&db, &Utc::now().to_rfc3339(), "current").await;

    assert_eq!(db.cleanup_old_metrics(24).await.unwrap(), 1);
    let remaining: Vec<String> = sqlx::query_scalar("SELECT hostname FROM metrics_history")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(remaining, vec!["current"]);
}
```

先临时改变断言期望为 2，运行确认测试能捕获错误，再恢复为 1；这是现有行为的特征测试，不改生产清理逻辑。

- [ ] **步骤 2：运行时间清理测试**

运行：

```bash
cargo test storage::tests::cleanup_old_metrics_only_removes_expired_rows -- --exact
```

预期：恢复正确断言后 PASS。

- [ ] **步骤 3：更新发布配置说明**

`config.full.example.toml`：

```toml
[storage]
db_path = "os-watcher.db"       # SQLite 数据库文件路径
retention_hours = 12             # 保留 12 小时；主数据库同时受 500 MiB 固定上限约束
```

`config.node.example.toml`：

```toml
[storage]
db_path = "os-watcher.db"       # SQLite 数据库文件路径
retention_hours = 12             # 若存储则保留 12 小时；主数据库同时受 500 MiB 固定上限约束
```

- [ ] **步骤 4：运行格式化和针对性测试**

运行：

```bash
cargo fmt -- --check
cargo test storage::tests
```

预期：格式检查成功，所有 storage 测试 PASS。

- [ ] **步骤 5：提交回归测试和配置说明**

```bash
git add src/storage.rs config.full.example.toml config.node.example.toml
git commit -m "test: 覆盖历史记录容量治理"
```

### 任务 5：行为级验证

**文件：**
- 不新增文件

- [ ] **步骤 1：运行完整 Rust 测试套件**

```bash
cargo test
```

预期：全部测试 PASS，无编译错误。

- [ ] **步骤 2：运行发布构建检查**

```bash
cargo check --release
```

预期：成功完成，无 error。

- [ ] **步骤 3：核对行为证据**

从 `full_database_evicts_oldest_and_keeps_latest_metric` 输出确认：

- 最新时间戳仍为第 80 次写入；
- 历史总数小于写入总数；
- `page_size * page_count <= 256 KiB`。

从 `opening_oversized_database_evicts_oldest_and_shrinks_file` 输出确认：

- 已超限文件重新打开成功；
- 物理页大小不超过测试上限；
- 收缩后保留最新记录。
