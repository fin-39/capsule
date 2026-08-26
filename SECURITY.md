# Security policy

Capsule is experimental and has not received an independent security audit. It
must not be represented as a safe malware-analysis environment.

Please report a suspected vulnerability privately to the repository owner by
using the hosting platform's private security-advisory feature. Do not include
real credentials, capsule images, proprietary application files or personal
paths in a report. A small synthetic reproducer and the output of
`capsule --doctor` are preferred.

Security fixes should preserve the documented threat model in
`docs/architecture.md`. Unsupported configurations fail closed; they must not
fall back to launching an application with bare Wine or direct host access.
