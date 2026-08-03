# cursor-chat-cleanup

分析与清理 Cursor 本地聊天存储(`state.vscdb`)的 Rust TUI 工具。

## 作用

Cursor 的本地聊天数据会随使用不断增长,且内置手段难以有效回收。本工具提供:

- 按体积排序的会话列表,直观看到磁盘空间去了哪里;
- 选择性删除不再需要的会话及其关联数据,并回收空间;
- 操作前自动备份。

**状态:功能完整,打磨中。**

- **TUI**(默认入口):按体积/行数/时间/名称/工作区排序,按工作区分组过滤(`w`),
  过滤与视图切换,会话详情,多选删除/修剪/清扫/GC——全部两段式
  (预览 → 确认 → 后台执行),完成后自动刷新;启动即扫 key 秒出列表,体积后台补齐;
- **CLI 子命令**:`report`(只读分析)、`sweep`(孤儿清扫)、`gc`(blob 与
  content 垃圾回收)、`delete`(按会话删除)、`trim`(保留正文的快照修剪)、
  `backup`(备份与残留检查)、`restore`(整体回滚)。

所有删除类操作默认 dry-run,`--apply`(或 TUI 内确认)才真正执行,
执行前自动生成 sidecar 备份,`restore` 可整体回滚。

### 维护模式(TUI 内按 `M`)

大库上每次操作都要重新扫描,代价很高。维护模式取得数据库的排他锁,
使内存中的扫描结果在会话期间恒为精确,从而:

- 只在进入时做一次全量扫描,之后所有预览与执行零重扫;
- checkpoint 与物理收缩推迟到退出模式时统一做一次。

代价:持锁期间**绝对不能启动 Cursor**——它打开主库失败会把主库判为损坏,
改名后拿旧备份顶替(静默回滚)。界面对此有常驻红色横幅提示。

## 免责声明

1. 本工具直接读写 Cursor 的本地数据库。请务必在 **Cursor 完全退出后**运行,并保留备份。
2. 本工具按"现状"提供,不含任何形式的担保,作者不对任何数据丢失负责。
   详见 `LICENSE-MIT` / `LICENSE-APACHE` 中的免责与责任限制条款。
3. Cursor 是 Anysphere, Inc. 的商标。本项目是独立的非官方工具,与 Anysphere 无关联,未获其背书。
   "Cursor" is a trademark of Anysphere, Inc. This project is an independent,
   unofficial tool and is not affiliated with, sponsored, or endorsed by Anysphere, Inc.

## 关于未来

本项目只是弥补当前版本 Cursor 内置清理能力的不足。官方后续版本可能改进相关机制,
届时本项目的部分或全部功能将不再必要,项目可能随之归档。

## 许可

本项目采用 **MIT OR Apache-2.0** 双许可,使用者可任选其一:

- Apache License, Version 2.0(`LICENSE-APACHE` 或 <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license(`LICENSE-MIT` 或 <http://opensource.org/licenses/MIT>)

除非你明确声明,否则你有意提交并纳入本项目的任何贡献
(依 Apache-2.0 许可证中的定义)均按上述双许可授权,不附加任何额外条款。

例外:仓库中的 `.proto` 接口定义文件不在上述双许可范围内,见其所在目录的 `NOTICE` 文件。
