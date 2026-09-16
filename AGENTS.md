<!-- TRELLIS:START -->
# Trellis Instructions

These instructions are for AI assistants working in this project.

This project is managed by Trellis. The working knowledge you need lives under `.trellis/`:

- `.trellis/workflow.md` — development phases, when to create tasks, skill routing
- `.trellis/spec/` — package- and layer-scoped coding guidelines (read before writing code in a given layer)
- `.trellis/workspace/` — per-developer journals and session traces
- `.trellis/tasks/` — active and archived tasks (PRDs, research, jsonl context)

If a Trellis command is available on your platform (e.g. `/trellis:finish-work`, `/trellis:continue`), prefer it over manual steps. Not every platform exposes every command.

If you're using Codex or another agent-capable tool, additional project-scoped helpers may live in:
- `.agents/skills/` — reusable Trellis skills
- `.codex/agents/` — optional custom subagents

Managed by Trellis. Edits outside this block are preserved; edits inside may be overwritten by a future `trellis update`.

<!-- TRELLIS:END -->

## 构建与验证

**不要本地编译。** 不在本机跑 `cargo build` / `cargo check` / `cargo test`，也不要本地启动 exe 去看界面。

- 编译和测试交给 CI（`.github/workflows/rust.yml`，push 就触发）：它跑 `cargo test` + `cargo build --release`，把 rustc 诊断挂成 check annotation，并上传 `ClipPlus-win-x64` artifact。要 exe 就从 artifact 下。
- 本地只做不编译的检查：读代码、`git diff` 复核、`python tools/brace_check.py`（只查括号配平，挡不住类型错误），有 rustfmt 就再跑 `rustfmt --check`（只验证语法解析）。

