# `edit` 工具匹配策略收窄需求

> **状态（2026-08-10）**：P0 + P1 已落地于 `crates/one-tools/src/edit_diff.rs`。
> - 任一策略多候选且 `replace_all=false` → 立即 `Multiple`（禁止 fallback 绕过）
> - `BlockAnchor`：相似度 ≥ 0.80、次优 margin ≥ 0.10、弱/相同锚点拒绝；`replace_all` 跳过 block-anchor
> - 回归测试见 `edit_diff::tests::{multi_match_does_not_fall_through_*, block_anchor_*}`
>
> P2（高风险策略 opt-in、LSP diagnostics、版本检查等）尚未做。

## 一、问题背景

当前 One 的 `edit` 工具为了提高模型编辑成功率，加入了多级 fallback 匹配：

```text
Exact
→ Fuzzy
→ LineTrimmed
→ WhitespaceNormalized
→ IndentationFlexible
→ BlockAnchor
```

这些策略可以容忍：

- CRLF/LF 换行差异；
- 行尾空格；
- Tab 与空格差异；
- 智能引号、破折号；
- 缩进变化；
- 部分代码块内容变化。

但当前匹配范围过宽。当文件中存在多个相似位置时，工具可能自动选择错误位置进行修改。

核心风险是：

> `edit` 在无法确定唯一目标时不应猜测，否则“编辑失败”会变成“静默误改”。

## 二、当前主要问题

### 1. 前一个策略发现歧义，后一个策略仍可能绕过歧义

当前逻辑可能出现以下情况：

```text
LineTrimmed 找到多个候选
→ 放弃本级结果
→ 继续尝试 WhitespaceNormalized
→ 后者恰好找到一个候选
→ 执行编辑
```

这会导致严格策略已经发现目标不唯一，但更宽松的策略反而把候选数量“压”成一个并执行。

### 2. `BlockAnchor` 可能在多个候选中强行选择最高分

当前 block-anchor 逻辑会：

- 使用首行和末行作为锚点；
- 计算中间内容的相似度；
- 选择最高分候选；
- 只要最高分达到阈值就执行。

问题是没有要求最高分明显领先第二名。例如：

```text
候选 A：0.72
候选 B：0.70
```

虽然 A 分数更高，但不能证明 A 就是模型想修改的位置。

### 3. 部分锚点过于普通

以下内容不适合单独作为 block anchor：

```rust
}
);
return;
```

它们在文件中经常重复出现，首尾锚点不能提供足够的定位能力。

### 4. 宽松匹配策略叠加后，误匹配概率增加

多个策略叠加后，可能同时忽略：

- 行尾空白；
- 行首缩进；
- 内部空白和换行；
- 代码块首尾之外的内容；
- 部分字符差异。

最终可能把语义不同、位置不同的代码块视为同一个目标。

## 三、外部实现调研结论

### Pi

Pi 的策略比较保守：

```text
Exact
→ 轻量 fuzzy normalization
```

主要只处理：

- 行尾空白；
- Unicode 引号；
- Unicode 破折号；
- 特殊空格。

同时要求：

- 匹配必须唯一；
- 多个候选直接报错；
- 不使用“最相似候选”猜测目标。

### Claude Code

Claude Code 公开资料没有完整披露内部匹配算法，但其工具定位是：

```text
针对特定文件的定点编辑
```

公开行为体现出以下方向：

- 使用较短的 `old_string` anchor；
- 编辑目标必须能够明确定位；
- 多处修改使用多个定点编辑；
- 文件内容发生冲突时失败，而不是重新猜测；
- 没有公开证据表明其默认使用大范围 fuzzy 或 block-anchor 匹配。

Claude Code 的 changelog 提到过：

```text
Edit tool now uses shorter old_string anchors, reducing output tokens
```

这里的重点是“更短的定位片段”，而不是允许任意短字符串模糊匹配。短 anchor 仍应经过唯一性验证。

### Grok Build

Grok Build 的 `search_replace` 被定义为：

```text
Make precise edits to files
```

工具职责倾向于：

```text
read_file / grep_search：定位和确认
search_replace：执行精确替换
```

而不是让替换工具承担大量模糊搜索、候选排序和位置猜测。

## 四、目标需求

在保留必要编辑容错能力的同时，收窄匹配范围，确保：

1. 只有唯一目标才允许自动编辑；
2. 存在歧义时必须失败；
3. 不允许更宽松策略掩盖前面发现的歧义；
4. 不允许根据相似度无条件猜测位置；
5. 所有失败路径都不能修改文件。

## 五、具体功能需求

### 需求 1：严格处理多候选

对于 `replace_all = false`：

```text
当前匹配策略产生多个有效候选
→ 立即返回 Multiple
→ 不再尝试后续更宽松策略
→ 文件不发生变化
```

应覆盖：

- `Exact`；
- `Fuzzy`；
- `LineTrimmed`；
- `WhitespaceNormalized`；
- `IndentationFlexible`；
- `BlockAnchor`。

只有当前策略完全没有候选时，才允许继续尝试下一个策略。

推荐逻辑：

```rust
if safe.len() > 1 && !replace_all {
    return Err(EditApplyError::Multiple {
        count: safe.len(),
    });
}
```

### 需求 2：保留低风险的格式容错

默认可以保留：

```text
Exact
→ 换行归一化
→ 行尾空白归一化
→ Unicode 引号/破折号归一化
```

这些容错主要解决模型复制文本时的格式变化，不应改变代码结构或语义边界。

### 需求 3：收紧高风险 fallback

以下策略应视为高风险：

```text
WhitespaceNormalized
IndentationFlexible
BlockAnchor
```

推荐方案：

- 保留实现，但增加严格前置条件；
- 或改为显式 opt-in；
- 或只在较长、多行、上下文充分的 `old_string` 上启用。

无论采用哪种方案，都不能让它们在前序策略已经产生歧义后继续“抢救”。

### 需求 4：强化 `BlockAnchor` 的候选判断

`BlockAnchor` 至少应满足：

- `old_string` 包含至少 3 行；
- 首尾锚点不能相同；
- 首尾锚点不能过短；
- 首尾锚点不能只是普通标点或单独闭合符号；
- 实际匹配行数与输入行数的差异不超过约 `25%`；
- 匹配结果不能明显大于 `old_string`；
- 中间内容相似度达到较高阈值。

建议参数：

```rust
const BLOCK_ANCHOR_SIMILARITY: f64 = 0.80;
const BLOCK_ANCHOR_MIN_MARGIN: f64 = 0.10;
```

多候选时排序后必须满足：

```text
best_score >= 0.80
且
best_score - second_best_score >= 0.10
```

否则返回匹配失败或多候选错误。

更保守的方案是：

```text
BlockAnchor 发现多个候选时一律拒绝
```

### 需求 5：限制 `replace_all`

`replace_all = true` 只表达明确的批量替换意图。

| 场景 | 行为 |
|---|---|
| exact 匹配 + `replace_all=false` | 必须唯一 |
| exact 匹配 + `replace_all=true` | 替换所有 exact 候选 |
| 低风险 fuzzy + `replace_all=true` | 可以替换所有归一化后完全一致的候选 |
| `LineTrimmed` + `replace_all=true` | 谨慎支持，需测试 |
| `BlockAnchor` + `replace_all=true` | 禁止 |
| 依赖相似度排序的批量替换 | 禁止 |

不允许出现以下行为：

```text
replace_all=true
→ 通过 block anchor 找到相似块
→ 批量替换所有“看起来像”的代码块
```

### 需求 6：失败必须保持文件不变

以下情况都必须在写文件前返回错误：

- `old_string` 为空；
- `old_string == new_string`；
- 找不到匹配；
- 找到多个候选；
- 匹配不成比例；
- block-anchor 相似度不足；
- block-anchor 候选分数接近；
- 多个 edit 相互重叠；
- 文件内容在读取期间发生变化。

错误发生后必须保证：

```text
文件内容不变
不执行部分写入
不执行后续替换
```

## 六、推荐的最终匹配顺序

### 默认安全模式

```text
Exact
→ LF/CRLF normalization
→ Fuzzy
```

其中 `Fuzzy` 只处理：

- 行尾空白；
- Unicode 引号；
- Unicode 破折号；
- 特殊空格。

### 保留完整容错的兼容模式

如果现有 benchmark 必须保留更多容错，可以使用：

```text
Exact
→ Fuzzy
→ LineTrimmed
→ WhitespaceNormalized
→ IndentationFlexible
→ BlockAnchor
```

但必须满足：

```text
当前策略多候选
→ 立即报错
```

不能继续下钻。

## 七、验收标准

### 行为验收

以下测试必须通过：

1. exact 唯一匹配成功；
2. exact 多匹配返回 `Multiple`；
3. fuzzy 唯一匹配成功；
4. fuzzy 多匹配返回 `Multiple`；
5. 前级策略多匹配时，后级策略不能绕过；
6. block-anchor 单候选且相似度足够高时成功；
7. block-anchor 多候选且分数接近时失败；
8. 弱锚点不能触发 block-anchor 编辑；
9. `replace_all=true` 只替换明确允许的候选；
10. 所有失败场景文件内容保持不变；
11. CRLF 文件编辑后仍保持原始换行格式；
12. trailing whitespace、Tab、smart quotes 的容错测试仍然通过。

### 建议增加的回归测试

#### 歧义泄漏测试

```text
LineTrimmed 找到两个候选
WhitespaceNormalized 找到一个候选
```

期望：

```text
Multiple
```

不能执行编辑。

#### block-anchor 近似双胞胎测试

```text
候选 A：0.82
候选 B：0.79
```

如果 margin 要求为 `0.10`，期望拒绝。

#### 弱锚点测试

```text
old_string:
}
...
}
```

期望：

```text
BlockAnchor 不执行
```

#### replace_all 安全测试

```text
old_string = "return value;"
replace_all = true
```

需要确认是否明确允许批量替换；如果没有额外上下文，不能默认认为安全。

## 八、推荐实施顺序

### P0：必须修复

1. 修复 fallback 歧义泄漏；
2. 多候选直接返回错误；
3. 确保错误路径不写文件。

### P1：强烈建议

1. 为 block-anchor 增加第二名分数；
2. 增加最低领先 margin；
3. 限制弱锚点；
4. 提高相似度阈值；
5. 增加近重复代码块测试。

### P2：后续优化

1. 将高风险 fallback 改为显式 opt-in；
2. 增加编辑后的 LSP diagnostics；
3. 增加文件内容版本检查；
4. 在工具返回中明确报告匹配策略；
5. 针对不同模型统计各策略的成功率与误匹配率。

## 九、一句话需求

> 收窄 One `edit` 工具的 fuzzy 匹配：保留 CRLF、行尾空白和 Unicode 等低风险容错，但只允许唯一、明确的候选自动编辑；任何歧义、近似双候选或弱锚点场景都必须拒绝，不能由更宽松的 fallback 或最高相似度逻辑替模型猜测目标位置。
