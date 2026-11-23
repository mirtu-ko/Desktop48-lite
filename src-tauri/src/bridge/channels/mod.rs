//! 按域的通道实现。
//!
//! 每个子模块导出一个 `dispatch(ctx, channel, args, caller)`，内部是「域内通道名 + 位置参数」
//! 的 `match`。`super::dispatch_from` 先做窗口权限收敛，再按通道名路由到这里的某个域。
//!
//! 新增通道的步骤：① 在 `src/bridge.ts` 的 `Channels` 表里补类型；② 在对应域的 `match`
//! 里加一个分支；③ 把通道名加进 `super::CHANNELS`（`probe` 与文档共用这份清单）。
//! 只属于播放窗的通道还要加进 `super::FLOAT_WINDOW_CHANNELS`，否则播放窗调用会被拒。

pub(super) mod danmaku;
pub(super) mod data;
pub(super) mod float;
pub(super) mod media;
pub(super) mod net;
pub(super) mod player;
pub(super) mod rooms;
pub(super) mod settings;
pub(super) mod system;
pub(super) mod tasks;
