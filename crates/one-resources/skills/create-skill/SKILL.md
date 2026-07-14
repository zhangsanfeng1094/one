---
name: create-skill
description: >
  Interactively create a new Agent Skill (SKILL.md + optional scripts/references).
  Use when the user wants to create a skill, scaffold a skill, write a SKILL.md,
  add reusable agent instructions, or runs /skill:create-skill or /create-skill.
---

# Create Skill

Interactively gather requirements and create a working [Agent Skills](https://agentskills.io) package on disk for **One**.

One discovers skills from (project wins over user; user wins over this builtin):

| Scope | Paths |
|-------|--------|
| Project | `<repo>/.one/skills/<name>/`, `<repo>/.agents/skills/<name>/` |
| User | `~/.one/agent/skills/<name>/`, `~/.agents/skills/<name>/` |

## Step 1: Gather information

Ask **one question at a time** (plain conversation; no multi-select menus for free text):

1. **Skill name** — lowercase `a-z`, digits `0-9`, hyphens `-` only. Must start and end with a letter or digit. Length 2–64 (e.g. `deploy-k8s`). Validate before continuing.
2. **Scope** — two options:
   - **Project** (recommended when inside a git repo): `.one/skills/<name>/SKILL.md` (or `.agents/skills/<name>/` if the repo already uses that convention)
   - **User**: `~/.one/agent/skills/<name>/SKILL.md` (or `~/.agents/skills/<name>/` for cross-harness sharing)
   - Default: **Project** if cwd is in a git repo, else **User**. Prefer `.one/skills` for One-native projects unless the user asks for `.agents`.
3. **What it should do** — workflow description, a repeated prompt they paste often, or the task to automate.

If the current conversation already contains a clear workflow (“turn this into a skill”), extract steps from history and only fill gaps.

## Step 2: Draft the description

Write a frontmatter `description` that includes:

- What the skill does (1–2 sentences)
- Trigger phrases / keywords so the model auto-loads it from the catalog
- Optional: `/skill:<name>` force-load mention

Show the draft and let the user approve or edit before writing files.

## Step 3: Create the directory

```bash
mkdir -p <SKILL_DIR>
```

`<SKILL_DIR>` examples:

- Project: `<repo-root>/.one/skills/<name>`
- User: `~/.one/agent/skills/<name>`

Optional:

```bash
mkdir -p <SKILL_DIR>/scripts <SKILL_DIR>/references
```

## Step 4: Write SKILL.md

Create `<SKILL_DIR>/SKILL.md` with this shape:

```markdown
---
name: <skill-name>
description: <approved description from Step 2>
---

# <Title>

<imperative instructions, steps, code blocks>
```

Also write any `scripts/`, `references/`, or `assets/` files the skill needs.

Rules:

- Body is instructions for the agent, not user-facing docs.
- Prefer existing CLI tools over custom scripts when sufficient.
- Keep the body lean; put long reference material in `references/` and point to it.
- Use absolute paths in tool calls when creating files.
- Do not skip `mkdir`; writes fail without the directory.

## Step 5: Verify and confirm

1. Read back `<SKILL_DIR>/SKILL.md` to confirm content.
2. Tell the user how to use it:
   - **Auto**: model sees name/description in the skills catalog and `read`s `SKILL.md` when relevant
   - **Force**: `/skill:<name> [args]`
   - **Reload**: `/reload` if the session was already open before the skill was added
3. Remind them progressive disclosure: only catalog metadata is always in context; full body loads on demand.

## Guidelines

- `description` is critical — it drives auto-invocation from the catalog.
- Name should match the directory name when practical.
- Skills must not include malware, exploit code, or instructions that surprise the user if summarized honestly.
- Prefer imperative form in the skill body (“Run X”, “Write Y”).
