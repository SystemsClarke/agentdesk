# Permission rules

Claude Code's auto-mode classifier sometimes refuses something John asked for. An agent used to stop and wait, or hand John a
command to type. Now it proposes a rule, and John decides in the window.

1. **Agent:** `propose_permission_rule {rule, reason, blocked_action?, scope?: user|project, project_dir?}`. For example
   `Bash(python tools/trigger_pipeline.py:*)`. This records a proposal and opens a question thread that rings John (toast, Slack).
   It writes nothing. The same pending rule is not proposed twice; a rule the file already allows is answered "already allowed".
2. **John:** *Permission rules* (`X` on the main menu). `A` approves, `R` rejects, `V` reverts an approved rule, `H` shows the decided
   ones, `Enter` reads the thread. A rule with no argument list (`Bash`, `Bash(*)`) is marked broad.
3. **On approve** the core copies the settings file to `permission_backups\` in the data folder, adds the rule to `permissions.allow`
   (once), keeps every other key, writes the file atomically, and replies on the thread so the agent can retry. The user scope is
   `%USERPROFILE%\.claude\settings.json` (`AGENTDESK_CLAUDE_SETTINGS` overrides it); a project rule goes to
   `<project_dir>\.claude\settings.json`. The file is rewritten as indented JSON, so its formatting may change; its content does not.

Only John's window approves: `ui:permission_decide` and `ui:permission_revert` refuse an agent's session. Every proposal stays in the
`permission_proposals` table (who asked, why, the blocked action, when decided, which file and backup), which is the audit log.
