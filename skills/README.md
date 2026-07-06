# Agent skills for jjq

Distributable skills for AI coding agents, following the
[Agent Skills](https://agentskills.io/specification) `SKILL.md` format used
by both Claude Code and OpenAI Codex.

## using-jjq

Teaches an agent to land changes through the jjq merge queue instead of
moving trunk directly, and how to recover from conflicts and failed checks.

Install by copying (or symlinking) the skill directory into the repo where
jjq is used:

```sh
# Claude Code (project)            # Codex (project)
.claude/skills/using-jjq/          .agents/skills/using-jjq/

# Claude Code (all projects)       # Codex (all projects)
~/.claude/skills/using-jjq/        ~/.codex/skills/using-jjq/
```

The same `SKILL.md` works unmodified in both harnesses.
