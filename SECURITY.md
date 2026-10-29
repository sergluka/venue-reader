# Security policy

## Supported versions

There are no user releases yet. During pre-release development, report problems
against the current default branch. After the first release, security fixes
target the latest released version; older versions do not receive backports.

## Report a vulnerability

Do not disclose vulnerability details in a public issue or pull request.
Use [GitHub private vulnerability reporting](https://github.com/sergluka/venue-reader/security/advisories/new)
when it is available. The form requires a public repository with private
vulnerability reporting enabled; it is not enabled during private release
preparation.

While the repository is private, use an existing private channel with the
maintainer. If no private channel is available, open an issue requesting one
without including vulnerability details.

Include the affected version or source revision, operating system, installation
method, potential impact and reproduction steps using synthetic data. Do not
send real FIX logs, private dictionaries, credentials or customer information,
even with names and values removed. Debug and trace logs can contain document
contents and should not be attached by default.

For ordinary bugs, use the [bug report form](https://github.com/sergluka/venue-reader/issues/new?template=bug_report.yml).
The [security reference](https://github.com/sergluka/venue-reader/wiki/Security)
describes offline analysis, file-access permissions and logging.
