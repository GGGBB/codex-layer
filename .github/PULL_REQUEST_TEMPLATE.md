## 关联 issue

Closes #

## 变更摘要

<!-- 一段话说明这个 PR 做了什么、为什么。 -->

## 行为影响面

<!-- 触及哪些路径（请求转换 / 钉定 / 压缩轮 / 流式 / CLI / 配置面）；
     有没有对 codex 客户端默认可见的行为变化。无则写「无客户端可见变化」。 -->

## 测试证据

<!-- 指令 + 输出摘要，例如：
     - cargo test：lib XX + 集成 XX，0 失败
     - python3 scripts/smoke/pinning.py：PASS
     - python3 scripts/smoke/streaming.py：PASS
     - （如涉端到端）临时 Label/端口/配置的一次往返结论 -->

## 与上游 diff 可比性

<!-- 是否触及移植区（protocol_proxy.rs、compaction_*.rs、
     prefix_divergence.rs 及对应测试）：机械移植改动是否不改行为、
     不破坏与上游的逐行 diff 可比性；机械改动与行为改动是否已分开提交。 -->

## 自查清单

- [ ] 已关联 issue，`type/*` 与 `P1`/`P2` 标签已打
- [ ] commit author 使用角色名（`codex-layer-<角色>`）
- [ ] `cargo test` 全绿，测试只增不减
- [ ] 两条冒烟脚本全绿
- [ ] 端到端自测仅用临时 Label + 临时端口 + 临时配置，未触碰本机
      `com.codex-layer.proxy`:57331 与 `com.user.mlx-ssm-env`
- [ ] 无真实密钥入库，配置示例用占位符
- [ ] 未做全仓 `cargo fmt`，移植区改动与行为改动分开提交
