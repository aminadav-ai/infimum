# Security Policy

## Reporting a Vulnerability

Security — including Z3 verification soundness, `Secret<T>` taint tracking,
`infimum.passport` capability enforcement, `wasmi` fuel isolation, and `bwrap`
sandboxing — is a core pillar of Infimum.

If you discover a security vulnerability, **please do not open a public GitHub issue**.
Instead, report it privately by emailing:

- **`security@infimum-lang.org`** (or **`licensing@infimum-lang.org`**)

Please include:
- Affected component (`infimum-lang`, `infimum-web`, or `infimum-code`),
- Minimal reproduction (`.ifm` snippet, `.ifw` capsule, or command),
- Expected vs. actual security boundary behavior.

We aim to acknowledge reports within 48 hours and coordinate a fix before public disclosure.
