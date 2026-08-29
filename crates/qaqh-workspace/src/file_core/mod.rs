#![allow(unused_imports)]
//! file_core — 统一行号/视图/匹配/账本内核（P1 抽离）
//!
//! 目标：read / write / edit 三工具共享单真源的
//! - LF规范视图 + hash (file_shared)
//! - FileView (byte_starts/char_starts)
//! - Tier1-3 四层匹配流水线
//! - ledger 行号偏移链
//!
//! 当前为物理搬迁后权威实现，edit/* 逐步改为 re-export。

pub mod view;
pub mod matching;
pub mod hunk;
pub mod locate;
pub mod read;
pub mod ledger;

// 内核常量（原 edit/mod.rs 与 file_shared 单点）
pub(crate) const T3_THRESHOLD: f32 = 0.85;
pub(crate) const T3_MARGIN: f32 = 0.10;
pub(crate) const HINT_WINDOW: usize = 10;
pub(crate) const MAX_HUNKS: usize = 64;

pub(crate) use crate::file_shared::{CANDIDATE_MAX, CONTENT_CAP, READ_MAX_CHARS, READ_MAX_CONTEXT, READ_MAX_LINES, SNIPPET_MAX};