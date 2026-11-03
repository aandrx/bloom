# Security policy

## Support scope

Bloom is experimental. Security fixes target the current `main` branch. There is
no maintained release series, guaranteed response time, or promise that the
external OpenBubbles engine receives fixes through Bloom.

## Reporting is not available to outside contributors yet

Bloom's repository is private, so vulnerability reporting is not available to
outside contributors right now. No working private reporting route is claimed
here, and this policy advertises no email address. The private reporting form on
<https://github.com/aandrx/bloom/security> requires a public repository with
private vulnerability reporting enabled.

Preparing this policy and the report form at
[.github/VULNERABILITY_REPORT.yml](.github/VULNERABILITY_REPORT.yml) is not
activation. Adding that form does not enable private vulnerability reporting by
itself. Activation requires separately authorized public visibility, then
enabling the feature in Settings, Advanced Security, as described in GitHub's
documentation at
<https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository>.
After enabling it, verify that "Report a vulnerability" is visible on the
security page before announcing a launch or a reporting route.

## How to report once reporting is enabled

Only after activation and that visibility check, use "Report a vulnerability" on
<https://github.com/aandrx/bloom/security> and complete the form. Do not send
vulnerability details through issues or pull requests. Until reporting is
enabled, retain the details rather than post them publicly.

A report can identify the affected commit, expected behavior, impact, and a
reproduction with synthetic data. Do not send credentials, registration files,
keychain exports, databases, raw engine frames, messages, recipient identifiers,
or unredacted logs. Private reporting does not make those files safe to share.

## Test safely

Use synthetic fixtures and private temporary directories. Do not start the real
engine or access someone else's account to demonstrate a finding. Engine startup
can modify registration state and consume incoming messages.

External engine diagnostics can contain private data even when Bloom's own
logging is bounded. Inspect and redact evidence before sharing it. Report issues
in the external engine to its maintainers without posting account data here.
