# Security

Please report security problems privately with GitHub's "Report a vulnerability" button on the Security tab,
not in a public issue. Include the PackSync version, your operating system and steps to reproduce.

Things worth reporting:

- tokens, keys or passwords showing up in a log, an error message, a diagnostics report or a snapshot;
- cloud data that makes PackSync read, write or delete files outside the instance folder, or crash;
- PackSync deleting or overwriting data it was not asked to;
- any way for the PackSync cloud, or anyone in between, to read or silently change encrypted data, or to
  learn a pairing code or passkey secret.

Not vulnerabilities: data in your own Google Drive or R2 bucket is not encrypted by PackSync (the app says
so), the PackSync cloud can see object sizes and times, and a malicious mod in a modpack is out of scope.
