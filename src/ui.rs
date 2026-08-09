//! Plugin pane rendering.
//!
//! Two entrypoints: a `list` overlay of live forwards with stop keys, and a
//! `picker` popup over attributed listeners. Both are plain ANSI on a terminal
//! Herdr owns — no TUI framework, matching how other Herdr plugins stay small.
//!
//! A popup receives all terminal input including Escape and closes when the
//! command exits, so key handling is ordinary stdin reading.
//!
//! Rendering is a pure function from a snapshot to bytes, so layout, column
//! widths, and truncation are testable without a terminal.
