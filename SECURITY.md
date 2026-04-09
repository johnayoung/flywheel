# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Flywheel, please report it responsibly.

**Do not open a public issue.** Instead, email security concerns to the maintainer directly via the contact information in the repository profile, or use [GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability).

You can expect an initial response within 72 hours.

## Scope

Flywheel executes agent-generated code in git worktrees. The trust boundary is the agent backend -- Flywheel itself does not sandbox agent execution. Users are responsible for ensuring that the configured agent operates within acceptable risk parameters for their environment.
