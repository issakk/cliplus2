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

**做完就推。** 改动完成、本地自查通过后，标准收尾就是 commit + push 到 main——不要把改动留在本地攒着。push 即触发 CI：它跑 `cargo test` + `cargo build --release`，把 rustc 诊断挂成 check annotation，上传 `ClipPlus-win-x64` artifact，**push 到 main 绿了会自动把 exe 发布到 `latest` release**——固定下载地址 `https://github.com/issakk/cliplus2/releases/latest/download/ClipPlus.exe`，未登录也能下（artifact 要点进 run 页面且需登录）。要 exe 就从 release 下。

推完用 `gh run watch <run-id> --exit-status`（或 `gh run list` 找到 run）盯到结束：**CI 绿了才算做完**；红了就修，修完再推，同样盯到绿。

- CI 配置在 `.github/workflows/rust.yml`。
- 本地只做不编译的检查：读代码、`git diff` 复核、括号配平检查 `python tools/brace_check.py`（只查括号配平，挡不住类型错误；本机没有可用的 python 时，等价逻辑用 Node 跑一遍即可），有 rustfmt 就再跑 `rustfmt --check`（只验证语法解析）。

