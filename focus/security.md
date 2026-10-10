Watch for changes that weaken security.

Attend to:
- Untrusted input reaching a sink without validation: shell commands, SQL, file paths, templates, deserialisation. Example: a user-supplied filename joined into a path with no traversal check.
- Secrets handled carelessly: keys in code, logs, error messages, or files with loose permissions.
- Weakened checks: authentication or authorisation skipped, TLS verification disabled, a sandbox or permission widened.
- Risky dependencies or downloads: new packages from unknown sources, scripts piped into a shell.

Ignore:
- Style, naming, formatting and comments.
- General bugs with no security impact; the correctness Focus owns them.
- Hardening ideas unrelated to what the Worker changed.
