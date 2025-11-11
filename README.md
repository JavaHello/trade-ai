我的推广链接[Okx](https://www.gtohfmmy.com/join/48126459)

## 工具简介

`trade-ai` 是一个基于 Rust 的终端可视化工具，负责：

- 通过 OKX WebSocket 实时订阅合约 `mark-price`
- 在 TUI 中绘制价格曲线并展示涨跌幅
- 监控自定义上下限并通过桌面通知（macOS、Linux）提醒
- 预加载一定窗口的历史数据，方便刚启动时快速回看走势
- 在交易视图中可直接向 OKX 下单，指令会通过 Tokio channel 发送到 OKX 交易 API

## 环境要求

- 已安装 Rust（建议 `rustup` + stable toolchain）
- 能访问 OKX API（默认公开接口即可，无需 API Key）
- 若需要桌面通知：macOS 需开启 `osascript` 权限，Linux 需 `notify-send/xdg-open`

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
默认为 `cross`，可按需切换为 `isolated` 或 `cash`。一旦配置完成，交易页面的委托将直接发送到 OKX
实盘/模拟账户（取决于 API 权限），请谨慎操作。

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

- 超出阈值时会广播 `Notify` 指令，由 `notify.rs` 调用桌面通知
- macOS 使用 `osascript`，Linux 使用 `notify-rust` 并在点击时尝试打开对应交易对页面
- 为避免刷屏，通知间隔默认 10 秒

## 常见问题

- **没有画面**：终端需支持 ANSI，推荐 24 bit 色彩的终端模拟器
- **历史数据为空**：OKX K 线接口有速率限制，可稍等自动退避重试
- **通知无效**：确认系统通知权限、命令是否存在（例如 Linux 需要 `notify-send`、`xdg-open`）
