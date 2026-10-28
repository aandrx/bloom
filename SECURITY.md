# Security policy

## Support scope

Bloom is experimental. Security fixes target the current `main` branch. There is
no maintained release series, guaranteed response time, or promise that the
external OpenBubbles engine receives fixes through Bloom.

## Report a vulnerability privately

No verified private reporting channel is currently advertised. The repository is
private, and the GitHub reporting API returned 404 during preparation. That result
does not establish whether reporting is disabled or inaccessible. Outside
contributors cannot rely on access to this repository's issues.

Do not send vulnerability details through issues or pull requests. Until a private
channel is established and documented here, retain the details rather than post
them. Before publication, the maintainer must configure a private reporting route,
verify it is available to reporters, and replace this notice with its instructions.

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
