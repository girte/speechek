# Security Policy

## Reporting a vulnerability

Please report security issues **privately** through GitHub's Private Vulnerability Reporting:

- Open **Security → Report a vulnerability** on the repository, or go directly to
  `https://github.com/girte/speechek/security/advisories/new`.

Do **not** open a public issue for a vulnerability, and do not post exploit details publicly before a fix is available.

A useful report includes:

- the Speechek version and the Windows build/architecture;
- what you observed and the impact;
- steps to reproduce or a proof of concept;
- any suggested remediation.

## Please do not send

- Gemini API keys or any credentials;
- `secrets.bin`, `settings.json` or other profile data;
- audio recordings or personal content;
- large logs containing private information.

If a proof of concept needs secrets or recorded speech, reproduce it with the built-in `test-provider` debug build and fake keys instead.

## Scope

Speechek is a local Windows application that talks to the Gemini API using your own keys. Security-relevant areas include: the DPAPI key store, the embedded local HTTP/WebSocket server, clipboard insertion, the overlay/settings windows, global hotkeys, and the installer/uninstaller.

Google's Gemini API and account security are outside this project's scope; report those through Google's own channels.

## What to expect

This is a small volunteer project. Reports are handled as time and availability allow — there is no response-time or fix-time commitment, and no bug bounty. Confirmed issues are fixed in a new version; because released installers are immutable, a fix is always a new release rather than a replacement file. Credit is given in the advisory unless you ask to stay anonymous.
