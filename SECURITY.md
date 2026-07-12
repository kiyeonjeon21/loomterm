# Security Policy

Loomterm currently supports its latest public preview release. Report a
security issue through GitHub's private vulnerability reporting for this
repository instead of opening a public issue.

Loomterm is a local runtime for trusted coding agents. Registered workspaces
limit command selection and working directories, but they are not an operating
system sandbox. Commands inherit the filesystem, network, and process
permissions of the user running `loomd`.

Session recordings include terminal output. They do not include raw keyboard
input or hidden model reasoning, but visible prompts, paths, secrets printed by
commands, and agent responses can appear in exports. Review and redact every
recording before sharing it.
