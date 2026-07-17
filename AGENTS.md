# AGENTS.md

Claude Code 以外のエージェント（あるいは素の LLM）でこのリポジトリを
扱う場合も、規約はすべて CLAUDE.md に集約してある。まずそれを読むこと。

- 憲法・レイヤ責務・完了条件: ./CLAUDE.md
- タスク台帳: ./ROADMAP.md
- 設計判断の記録: ./docs/adr/
- 手順書: ./.claude/skills/*/SKILL.md（エージェント非依存の内容で書いてある）

examples/ とテストは事実上の eval セット。挙動を変える前に必ず
`make check` を実行し、変えた後も緑であることを確認する。
