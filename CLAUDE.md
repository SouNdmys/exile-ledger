# Exile Ledger

这个项目以前叫 POE Ninja Data,改名之后 crate 名字仍然保留 `pnd-` 前缀。

单人自用的暗金蹲价 + poe.ninja 热门装备/词缀监控工具(Rust + GPUI 桌面应用,
Windows 专用):盯交易网站上某件暗金的价格,出现好价就弹卡片提醒;同时看
poe.ninja 上热门 Build 用得最多的装备和词缀。不是给别人用的库:**公共 API
稳定性不需要考虑**,改 `pub fn` 签名不用顾虑下游,仓内改干净即可。

## crate 地图

由底向上:

- `pnd-domain` — 纯类型:搜索引用(URL/id 解析)、价格与货币、挂单摘要。别的都建在它上面
- `pnd-trade` — 交易站客户端:纯限速器、限速头解析、ureq 版 search/fetch/whisper、Live Search WebSocket
- `pnd-ninja` — poe.ninja 客户端:快照状态、build 搜索的最小 protobuf 解析、NDIC 字典、角色详情、经济汇率、采样分区计划、词缀聚合
- `pnd-storage` — SQLite 持久化:蹲价状态与提醒历史(`watch.sqlite`)、ninja 采样缓存(`ninja.sqlite`)
- `pnd-settings` — 版本化、原子写的 `settings.json`
- `pnd-runtime` — actor 线程:串行交易网关、轮询调度、判定去重、live 会话、ninja 采样管线,以及 `src/bin/*_probe.rs` 验证探针
- `pnd-platform-win` — 隔离的 Win32 平台服务:报警音、置顶提醒小卡片、打开交易页的 shell 调用
- `pnd-app` — GPUI 桌面壳:页面、`i18n.rs`、120ms tick 抽干 runtime 与卡片事件

`pnd-runtime` 不依赖 `pnd-platform-win`:平台层与运行时平行,由 `pnd-app` 接线。

## 常用命令

```
cargo test --workspace
cargo clippy --workspace --all-targets
cargo check --workspace --all-targets
cargo run -p pnd-app
cargo run -p pnd-runtime --bin trade_probe -- --league "Forbidden Rites" --search <你的搜索URL> --cap 20 --currency divine --rounds 3
cargo run -p pnd-runtime --bin ninja_probe -- --search --class "Gemling Legionnaire"
cargo run -p pnd-platform-win --bin alert_probe -- --corner bottom_right --auto-hide 1
```

两条基准线都必须 exit 0。**仓库没有 `[lints]` 配置**,所以 plain clippy 就是标准,
零 warning 是要求,不需要加 `-D warnings`。

环境是 Windows + PowerShell:过滤输出用 `Select-String`、`Select-Object -Last N`,
不是 `grep`/`tail`。

## 不成文约定

- 测试放在同文件底部的 `#[cfg(test)] mod xxx_tests`
- Windows 专属代码一律 `#[cfg(windows)]` 门控
- 界面文本不内联:`pnd-app` 的界面文案走 `src/i18n.rs` 里的双语 `Text` 目录,不要在业务代码里内联中文字符串
- 探针是 `pnd-runtime/src/bin/*_probe.rs` 和 `pnd-platform-win/src/bin/alert_probe.rs`,它们必须调用生产函数,不能自己复制一份逻辑
- 文档注释写"为什么",不写"这行做了什么"
- 价格一律存整数千分位(`amount_milli`),不存浮点
- 所有交易站请求都走同一条串行网关线程
- 限速预算按服务端限速头的 **50%** 走,不贴着上限跑
- 程序永远不向游戏发输入,不自动私聊、不自动传送——每个游戏内动作都是用户自己点一次

## 和我协作的方式

- **我在学 Rust,读不太懂代码。** 改动要用人话解释,把我当成不懂编程术语的普通人:
  说清这段在干什么、为什么这么改、会有什么后果。不要只丢一段代码或一个 diff 让我自己看
- **先给最小可用的写法**,防御层后加。个人单用户工具,`panic` 和 `.unwrap()` 可以接受,
  不要一上来就铺错误处理
- **修 bug 必须先写出会 FAIL 的测试**,让我看到它红,再动代码。
  如果顺序反了(先改后写测试),就故意把功能弄坏一次、确认测试会红、再恢复 ——
  没见过红的测试不算数
- 一次只做被要求的事,不要顺手重构
- **一次只做一件事,做完提交再做下一件。** 不要把多个改动混进一个 commit:
  混在一起出了问题,就分不清是哪个改动弄坏的
- **每次提交前先跑 `cargo fmt --all`。** 格式漂移不该占掉 review 的名额:
  上一轮 ultrareview 的两条 finding 全是缩进,真正的 bug 一条都没有

## docs/

设计记录是整个立项计划,不在本仓库内,而在:

`C:\Users\SouNd\.claude\plans\poe-trade-tracker-poe-alarm-poe-majestic-axolotl.md`

里面有已核实的接口事实(交易站 trade2、poe.ninja builds/economy)、设计决策表、
ninja 采样方案、crate 地图的复用来源、数据结构(`settings.json`/`watch.sqlite`/
`ninja.sqlite`)、关键模块要点、分阶段实施顺序和验证总览。改动设计决策就去改
那份文件,不要在本文件里平行维护一份。
