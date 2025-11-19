我的推广链接[Okx](https://www.gtohfmmy.com/join/48126459)

## 工具简介

`trade-ai` 是一个基于 Rust 的终端可视化加密货币交易工具，负责：

- 通过 OKX WebSocket 实时订阅合约 `mark-price`
- 在 TUI 中绘制价格曲线并展示涨跌幅
- 预加载一定窗口的历史数据，方便刚启动时快速回看走势
- 将成交明细、AI 决策、错误信息持久化到本地 JSONL 文件，TUI 交易页可随时回看
- 在交易视图中可直接向 OKX 下单，指令会发送到 OKX 交易 API
- 可选启用 Deepseek AI，周期性读取账户/技术指标并可自动生成交易指令
  - 提示词来源: 基于 [nof1 ai 逆向](https://gist.github.com/wquguru/7d268099b8c04b7e5b6ad6fae922ae83) 修改

## 环境要求

- 能访问 OKX API（默认公开接口即可，无需 API Key）
- 交易功能需 OKX 交易 API Key/Secret/Passphrase
- 已安装 Rust（建议 `rustup` + stable toolchain） (\* 手动编译时需要)

## 运行配置（`config.json`）

程序首次启动会在项目根目录生成 `config.json`，记录运行起点时间和显示所用时区，例如：

```json
{
  "start_timestamp_ms": 1763132135841,
  "timezone": "Asia/Shanghai"
}
```

- `start_timestamp_ms`：用于策略统计与 TUI 中的“运行以来”指标，删除此文件可重新初始化。
- `timezone`：控制 TUI 中的时间格式，支持 IANA 名称（`Asia/Shanghai`）或 `UTC+08:00`、`UTC-05:00` 等固定偏移。

修改文件后重启程序即可生效；若字段缺失会回落到本地时区。

## 配置 OKX 交易

默认情况下不会发送真实订单。如需启用交易页面（`t`），需要提供以下 OKX API 参数：

```bash
cargo run --release -- \
  --okx-api-key "$OKX_API_KEY" \
  --okx-api-secret "$OKX_API_SECRET" \
  --okx-api-passphrase "$OKX_API_PASSPHRASE" \
  --okx-td-mode cross \
  ...
```

所有字段也可以通过环境变量 `OKX_API_KEY`、`OKX_API_SECRET`、`OKX_API_PASSPHRASE` 注入。`--okx-td-mode`
默认为 `cross`(目前只支持 `cross`)。一旦配置完成，交易页面的委托将直接发送到 OKX
实盘/模拟账户（取决于 API 权限），请谨慎操作。

## AI 智能分析

在同时提供 OKX 与 Deepseek API 时，`trade-ai` 会按照设定频率（默认 3 分钟）执行以下流程：

1. 抓取账户快照：持仓、挂单、资金、最近成交（来自本地 `trade_logs.jsonl`）。
2. 采集行情指标：EMA/MACD/RSI/ATR、资金费率、未平仓合约数等（`okx_analytics.rs`）。
3. 生成上下文并发送给 Deepseek，请求中文结论及结构化 JSON 决策。
4. 在 TUI 底部展示最近一条摘要，并在交易页 `AI` 面板里保留完整记录。

当 Deepseek 返回有效的 JSON 信号时，程序会做安全检查：验证交易对、数量粒度、止盈/止损方向等，然后通过内置
OKX 客户端执行下列动作：

- **建仓**：按最新 `mark-price` 生成限价单，可附带杠杆与标签。
- **保护单**：当决策提供目标价/止损价时，会自动派发止盈、止损单。
- **平仓**：若信号为 `close`，会按持仓方向发送减仓单。
- **杠杆同步**：若要求的杠杆与当前不符，会先发送 `SetLeverage`。

所有 AI 请求/响应会写入 `ai_decisions.jsonl`，TUI 启动时会加载最近 64 条方便排查。
若未提供 OKX API（即没有交易令牌），Deepseek 仍会给出文字分析，但不会触发任何下单操作。

> ⚠️ Deepseek 具备实盘下单能力。请确认 API 权限、交易模式（实盘/模拟）和杠杆限制，必要时在 OKX 侧设置更细的
> 风控（子账户、资金限额）后再开启。

可以通过环境变量或命令行参数启用：

```bash
cargo run --release -- \
  --okx-api-key "$OKX_API_KEY" \
  --okx-api-secret "$OKX_API_SECRET" \
  --okx-api-passphrase "$OKX_API_PASSPHRASE" \
  --deepseek-api-key "$DEEPSEEK_API_KEY" \
  --deepseek-interval 10m
```

支持的参数：

- `--deepseek-api-key` / `DEEPSEEK_API_KEY`：Deepseek API Key（必填）
- `--deepseek-model` / `DEEPSEEK_MODEL`：模型名称，默认 `deepseek-chat`
- `--deepseek-endpoint` / `DEEPSEEK_API_BASE`：API 基础地址，默认 `https://api.deepseek.com`
- `--deepseek-interval`：提交频率（如 `5m`、`15m`、`1h`）

Deepseek 集成仅在成功加载 OKX 账户信息后激活，若账户数据为空则会跳过本次请求。

## 快速开始

```bash
cargo run --release -- \
  --inst-id BTC-USDT-SWAP,ETH-USDT-SWAP \
  --threshold BTC-USDT-SWAP:30000:38000 \
  --threshold ETH-USDT-SWAP:1500:2300 \
  --window 1h
```

命令行参数说明：

- `--inst-id` / `-i`：要监听的交易对。可用逗号分隔或多次传入；默认 `BTC-USDT-SWAP`
- `--threshold INST:LOWER:UPPER`：阈值设定，命中后会触发通知。未配置则默认 `[0,+∞)`
- `--window`：历史数据窗口，使用 `s/m/h/d` 单位，如 `15m`、`4h`、`1d`（默认 `15m`）

## TUI 操作说明

- `q` / `Esc` / `Ctrl+C`：退出程序
- `n`：切换绝对价格 vs. 相对涨跌（%）
- `m`：切换多轴模式（仅在绝对价格下生效）
- `+` / `-`：沿 Y 轴放大 / 缩小；`0` 重置缩放
- `t`：在图表与交易页面之间切换；交易页面中可用 `↑/↓` 选择合约，`b`/`s` 买入卖出（默认填充最新价格，可手动编辑）。配置 API 后的订单会实时提交到 OKX。
- 界面底部会显示最近状态（如加载历史数据、缩放提示等）

## 通知机制

- 超出阈值时会广播 `Notify` 指令，由 `notify.rs` 处理
- 为避免刷屏，通知间隔默认 10 秒

## 日志与数据持久化

- `trade_logs.jsonl`：每次委托/撤单/成交都会记录一行 JSON，TUI 交易页的“成交日志”即来自此文件（最多加载 512 条）。
- `ai_decisions.jsonl`：保存 AI 系统提示词、用户上下文、原始 JSON 响应及推断的操作结论。
- `error_logs.jsonl`：所有 `Command::Error` 信息都会落盘，方便后台运行时查因。

## 常见问题

- **没有画面**：终端需支持 ANSI，推荐 24 bit 色彩的终端模拟器
- **历史数据为空**：OKX K 线接口有速率限制，可稍等自动退避重试
- **通知无效**：确认终端支持 `OSC 777`协议
