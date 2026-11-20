# Semantic golden fixtures

这些 fixtures 是四个平台 resolver/planner 的归一化行为基线，不要求不同实现的 Inspect/Plan JSON 字段完全相同。

两套测试分别把真实 resolver/planner 结果归一化为以下稳定概念后，与同一个 fixture 比较：

- platform、media kind、title
- 排序后的 quality id
- part title
- task 的语义 kind（DASH 归一化为 `mux`）
- 安全输出文件名
- 在 fixture server 不共享 host/path 时，只比较有意义的 source basename 或忽略 transport 差异

当前覆盖：

- `kuaishou-video.json`
- `xiaohongshu-video.json`
- `douyin-video.json`
- `bilibili-video.json`

该层用于发现平台选择、排序、分P和 task 形状的行为漂移；Plan v1/v2 的实现细节仍分别由各自 contract tests 覆盖。
