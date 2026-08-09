//! Finding remote listeners and deciding whose they are.
//!
//! Two parsers, both pure over `&str`:
//! - `lsof -nP -iTCP -sTCP:LISTEN` on macOS, whose `COMMAND` column is padded
//!   and truncated and can contain spaces, and whose `NAME` column carries the
//!   bind address as `127.0.0.1:8501`, `*:53`, or `[::1]:8501`;
//! - `ss -ltnH` on Linux, whose address column uses `0.0.0.0:5173`, `*:5173`,
//!   and `[::]:5173`, and whose `users:(("node",pid=123,fd=20))` field carries
//!   the owning pid.
//!
//! Then attribution: `ps -eo pid=,ppid=` gives a parent map, and a listener
//! belongs to a pane when walking its ancestry reaches that pane's `shell_pid`.
//!
//! The ancestry walk must terminate on any input. PID reuse can produce cycles
//! and a malformed `ps` can produce a self-parent, so the walk is both
//! cycle-guarded and depth-capped. This is invariant 5 in `ARCHITECTURE.md`.
