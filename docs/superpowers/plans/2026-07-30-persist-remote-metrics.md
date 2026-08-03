# 持久化远程节点指标 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 当本地节点通过 Gossip 收到其他节点的 `MetricsUpdate` 时，立即将指标落库；本地采集循环保持现有行为不变。

**架构：** 将 `Arc<Database>` 传入 `GossipService::run_with_rx` 和 `handle_message`，在 `MetricsUpdate` 分支里 `tokio::spawn` 一个后台任务调 `db.store_metrics()`，避免写库延迟阻塞 UDP 接收循环。`main.rs` 把已有的 `Arc<Database>` 传给 Gossip 任务。

**技术栈：** Rust · Tokio · SQLite (sqlx) · 现有 `storage::Database`

---

## 文件职责

| 文件 | 变更 |
|------|------|
| `src/gossip.rs` | `run_with_rx` + `handle_message` 加 `db` 参数；`MetricsUpdate` 分支落库 |
| `src/main.rs` | 把 `Arc::clone(&db)` 传给 `GossipService::run_with_rx` |

---

### 任务 1：给 `GossipService::run_with_rx` 加 `db` 参数

**文件：**
- 修改：`src/gossip.rs`

- [ ] **步骤 1：修改 `run_with_rx` 签名，新增 `db` 参数**

在 `src/gossip.rs` 顶部 use 区加：

```rust
use std::sync::Arc;
use crate::storage::Database;
```

（`std::sync::Arc` 已通过 `use std::net::SocketAddr` 所在的 use 块间接用到，但 `Arc` 本身是在 gossip.rs 里通过 `Arc::new(UdpSocket::bind…)` 直接使用的——检查文件顶部是否已有 `use std::sync::Arc`，若无则补上。）

将函数签名从：

```rust
pub async fn run_with_rx(
    state: SharedState,
    config: NetworkConfig,
) -> Result<()> {
```

改为：

```rust
pub async fn run_with_rx(
    state: SharedState,
    config: NetworkConfig,
    db: Arc<Database>,
) -> Result<()> {
```

- [ ] **步骤 2：将 `db` clone 进 recv 任务**

在 `run_with_rx` 内，找到 `// Task: Receive incoming messages` 注释上方，加：

```rust
let recv_db = Arc::clone(&db);
```

然后把 `tokio::spawn(async move {` 内的 `Self::handle_message(...)` 调用改为：

```rust
Self::handle_message(&recv_state, envelope, src, &recv_tx, max_hops, Arc::clone(&recv_db)).await;
```

- [ ] **步骤 3：修改 `handle_message` 签名，新增 `db` 参数**

将：

```rust
async fn handle_message(
    state: &SharedState,
    envelope: GossipEnvelope,
    src: SocketAddr,
    tx: &mpsc::Sender<(GossipEnvelope, Option<String>)>,
    max_hops: u8,
) {
```

改为：

```rust
async fn handle_message(
    state: &SharedState,
    envelope: GossipEnvelope,
    src: SocketAddr,
    tx: &mpsc::Sender<(GossipEnvelope, Option<String>)>,
    max_hops: u8,
    db: Arc<Database>,
) {
```

- [ ] **步骤 4：在 `MetricsUpdate` 分支里落库**

找到 `GossipMessage::MetricsUpdate { node_id, metrics } =>` 分支，在更新内存状态的代码块 **之后**、转发代码块 **之前**，插入：

```rust
// 持久化远程节点指标（后台任务，不阻塞接收循环）
{
    let db_clone = Arc::clone(&db);
    let metrics_clone = *metrics.clone();
    tokio::spawn(async move {
        if let Err(e) = db_clone.store_metrics(&node_id, &metrics_clone).await {
            tracing::warn!("Failed to persist remote metrics for {}: {}", node_id, e);
        }
    });
}
```

- [ ] **步骤 5：确认编译通过**

```bash
cargo build 2>&1 | head -40
```

预期：无 error，可能有 warning（unused import 等）。

- [ ] **步骤 6：Commit**

```bash
git add src/gossip.rs
git commit -m "feat(gossip): 收到远程节点 MetricsUpdate 时落库"
```

---

### 任务 2：更新 `main.rs` 传入 `db`

**文件：**
- 修改：`src/main.rs`

- [ ] **步骤 1：找到 Task 2 Gossip 任务的启动代码**

在 `src/main.rs` 中，找到：

```rust
// Task 2: Gossip service
let gossip_state = Arc::clone(&state);
let gossip_cfg = (*cfg).network.clone();
tokio::spawn(async move {
    if let Err(e) = GossipService::run_with_rx(gossip_state, gossip_cfg).await {
        error!("Gossip service error: {}", e);
    }
});
```

- [ ] **步骤 2：传入 `db`**

将上述代码改为：

```rust
// Task 2: Gossip service
let gossip_state = Arc::clone(&state);
let gossip_cfg = (*cfg).network.clone();
let gossip_db = Arc::clone(&db);
tokio::spawn(async move {
    if let Err(e) = GossipService::run_with_rx(gossip_state, gossip_cfg, gossip_db).await {
        error!("Gossip service error: {}", e);
    }
});
```

- [ ] **步骤 3：确认编译通过，无 error**

```bash
cargo build 2>&1 | head -40
```

预期：编译成功。

- [ ] **步骤 4：运行测试**

```bash
cargo test 2>&1 | tail -20
```

预期：所有测试 PASS。若有测试构造了 `GossipService::run_with_rx`，需同步更新其调用处（搜索：`grep -rn "run_with_rx" src/`）。

- [ ] **步骤 5：Commit**

```bash
git add src/main.rs
git commit -m "feat(main): 将 db 传入 GossipService 以落库远程指标"
```

---

### 任务 3：验证端到端行为

**文件：**
- 验证：`src/gossip.rs`、`src/storage.rs`（只读验证，无需修改）

- [ ] **步骤 1：检查 `broadcast_leave` 是否需要更新**

`broadcast_leave` 函数签名不涉及 `handle_message`，只发送 UDP，不受影响——无需修改。

```bash
grep -n "broadcast_leave" src/main.rs src/gossip.rs
```

预期：`broadcast_leave` 调用处不传 `db`，签名未变，正常。

- [ ] **步骤 2：检查现有 store_metrics 调用路径**

```bash
grep -n "store_metrics" src/
```

预期输出（含本次新增）：

```
src/main.rs:NNN:        if let Err(e) = collect_db.store_metrics(&node_id, &metrics).await {   // 本地采集
src/gossip.rs:NNN:      if let Err(e) = db_clone.store_metrics(&node_id, &metrics_clone).await {  // 远程节点
```

- [ ] **步骤 3：完整测试**

```bash
cargo test -- --nocapture 2>&1 | tail -30
```

预期：全部 PASS。

- [ ] **步骤 4：最终 Commit（如有遗留变更）**

```bash
git status
# 若有未提交内容：
git add -p
git commit -m "chore: 持久化远程节点指标收尾"
```
