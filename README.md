# Vitals — 主机探针 + 指标 TSDB + 仪表盘

Steadholme 主权基础设施栈的可观测组件（探针 / 探测器）。由两个二进制组成：

- **`vitals-agent`** — 主机探针。每隔 `SCRAPE_INTERVAL`（默认 10s）从可配置的
  proc/sys 根读取主机指标，向服务端 `/ingest` POST 一个带 `Authorization: Bearer
  INGEST_TOKEN` 的 JSON 批次。
- **`vitals-server`**（默认二进制）— 指标时序库（TSDB）+ 服务端渲染的企业级仪表盘。

技术栈与 keystone/keyward 保持一致：Rust + axum + tokio，sqlx（rustls，仅运行时查询、
无编译期宏，构建无需数据库），错误信封、`block_in_place` 同步→异步桥接、`healthcheck`
子命令等模式一一对应。

---

## 架构

```
                 ┌─────────────┐  采集主机 /proc /sys /        ┌──────────────┐
   宿主机指标 ──▶ │ vitals-agent │ ── POST /ingest (Bearer) ──▶ │ vitals-server │
   (CPU/内存/磁盘  └─────────────┘                              │  TSDB + 仪表盘 │
    /负载/网络/uptime)                                          └──────┬───────┘
                                                                       │ 标准 SQL
                                                                ┌──────▼───────┐
   浏览器 ──(经 Sluice，auth=sso，注入 X-Auth-Email)──▶ GET /   │  PostgreSQL  │
                                                                └──────────────┘
```

仪表盘**自身不做登录**：它位于网关 `auth=sso` 路由之后，由 Sluice 完成 OIDC 浏览器登录、
建立会话并注入 `X-Auth-Email` 等身份头；服务端只读取该头用于 app-bar 的「已登录为」展示，
Logout 指向 `https://id.w33d.xyz/_gw/auth/logout`。

---

## 端点（`vitals-server`）

| 方法 | 路径 | 鉴权 | 说明 |
|------|------|------|------|
| GET  | `/healthz` | 公开 | 存活探针（容器 HEALTHCHECK 使用） |
| POST | `/ingest` | Bearer `INGEST_TOKEN` | 探针上报批次，返回 `{"accepted": n}` |
| GET  | `/api/metrics?host=&metric=&since=` | 经网关 sso | 仪表盘/外部读取的 JSON 时序，默认回看 1 小时 |
| GET  | `/` | 经网关 sso | 服务端渲染的仪表盘 |

`/ingest` 请求体（agent → server）：

```json
{
  "host": "node-a",
  "samples": [
    { "metric": "cpu_pct", "value": 12.5, "ts": 1700000000 },
    { "metric": "mem_pct", "value": 48.0, "ts": 1700000000 }
  ]
}
```

指标词表：`cpu_pct`、`mem_pct`、`mem_used_bytes`、`mem_total_bytes`、`disk_pct`、
`disk_used_bytes`、`disk_total_bytes`、`load1`/`load5`/`load15`、`net_rx_bps`/`net_tx_bps`、
`uptime_secs`。首次采样无前序快照，故 CPU% 与网络速率会跳过该轮。

---

## 数据模型（TSDB · 可移植标准 SQL）

```sql
CREATE TABLE IF NOT EXISTS metric_samples (
    host   TEXT NOT NULL,
    metric TEXT NOT NULL,
    value  DOUBLE PRECISION NOT NULL,
    ts     BIGINT NOT NULL,
    PRIMARY KEY (host, metric, ts)
);
CREATE INDEX IF NOT EXISTS idx_metric_samples_host_metric_ts
    ON metric_samples (host, metric, ts);
```

仅使用 TEXT/BIGINT/DOUBLE PRECISION、复合主键、二级索引、参数化查询、
`INSERT .. ON CONFLICT DO NOTHING`、`GROUP BY/MAX` 取最新行（不用 Postgres 专有的
`DISTINCT ON`）、`DELETE` 裁剪——无 JSONB/数组/SERIAL/扩展，日后可原样跑在 FusionDB
（pgwire）之上。`VITALS_STORE=memory` 时使用内存实现（测试默认，无需数据库）。

> Future-FusionDB：为指标形状预留的 embedding 列是后续「AI 异常检测」的接入点（暂缓）。

保留期：定时器每小时删除 `ts < now - RETENTION_HOURS*3600` 的样本（默认 168 小时 / 7 天）。

---

## 配置（环境变量）

**服务端 `vitals-server`：**

| 变量 | 默认 | 说明 |
|------|------|------|
| `BIND_ADDR` | `0.0.0.0:8300` | 监听地址 |
| `VITALS_STORE` | `memory` | `memory` \| `postgres` |
| `DATABASE_URL` | — | `VITALS_STORE=postgres` 时必填 |
| `INGEST_TOKEN` | dev 默认（务必覆盖） | `/ingest` 的 Bearer 令牌 |
| `RETENTION_HOURS` | `168` | 样本保留小时数 |

**探针 `vitals-agent`：**

| 变量 | 默认 | 说明 |
|------|------|------|
| `SERVER_URL` | `http://127.0.0.1:8300` | 服务端基址（POST 到 `{SERVER_URL}/ingest`） |
| `INGEST_TOKEN` | dev 默认 | 与服务端一致的 Bearer 令牌 |
| `SCRAPE_INTERVAL` | `10` | 采集间隔（秒） |
| `HOST_ID` | 主机名 | 样本上的主机标识；缺省取 `HOSTNAME` 或 `{HOST_PROC}/sys/kernel/hostname` |
| `HOST_PROC` | `/proc` | proc 根 |
| `HOST_SYS` | `/sys` | sys 根 |
| `HOST_ROOT` | `/` | 磁盘用量 statvfs 的根 |

---

## 构建与测试

```bash
export PATH=$PATH:/usr/local/go/bin   # 仅在需要 go 的其它组件时
cargo build --bins
cargo test                            # 默认全程不需要数据库（pg 测试自动跳过）

# 针对真实 Postgres 跑 TSDB 集成测试：
docker run --rm -d --name pg -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=vitals \
  -p 127.0.0.1:55440:5432 postgres:18-alpine
TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55440/vitals \
  cargo test --test pg_store -- --nocapture
docker rm -f pg
```

测试覆盖：`/proc` fixtures 解析为样本、`collect` 整批、ingest 鉴权（无 token 401）、
存储往返 + 保留期裁剪、仪表盘渲染（含 HTML 转义）、Postgres 全流程。

---

## 容器

多阶段、非 root（uid 10001）、glibc-only（无 OpenSSL）、内建 `healthcheck` 子命令、
`EXPOSE 8300`，镜像同时携带两个二进制。

```bash
docker build -t holdfast/vitals:dev .

# 服务端
docker run --rm -p 127.0.0.1:8300:8300 holdfast/vitals:dev

# 探针（覆盖 command，挂载宿主 /proc 等只读，上报真实宿主指标）
docker run --rm \
  -e SERVER_URL=http://vitals-server:8300 \
  -e INGEST_TOKEN=... \
  -e HOST_PROC=/host/proc -e HOST_SYS=/host/sys -e HOST_ROOT=/host/root \
  -v /proc:/host/proc:ro -v /sys:/host/sys:ro -v /:/host/root:ro \
  holdfast/vitals:dev vitals-agent
```

`vitals-agent oneshot` 做一轮采集+上报后退出，便于冒烟验证。

---

## 部署集成（deploy 注意事项）

- **网关路由**：`/vitals -> auth=sso`（仪表盘 + `/api/metrics` 经 Sluice 浏览器 SSO 网关，
  注入 `X-Auth-Email`）。服务为内部网络可达，不对公网开放。
- **服务端**：`VITALS_STORE=postgres` + `DATABASE_URL`，强随机 `INGEST_TOKEN`，端口 8300
  内部可达；HEALTHCHECK 已内建。
- **探针**：以独立容器运行 `vitals-agent`，挂载宿主 `/proc`、`/sys`、`/` 为只读
  （`HOST_PROC=/host/proc` 等），与服务端共用同一 `INGEST_TOKEN`，`SERVER_URL` 指向
  `http://vitals-server:8300`。探针容器无监听端口，应禁用 HEALTHCHECK。
