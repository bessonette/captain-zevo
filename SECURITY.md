# Security

Captain Zevo talks to your Docker socket, runs commands over SSH, and issues
local certificates, so reports of security problems are taken seriously.

## Reporting a problem

Please do not open a public issue. Use GitHub's private reporting instead:
on the repository page, open **Security → Report a vulnerability**. Say what
you did, what happened, and which version or commit you used.

You should get a reply within a week. Captain Zevo is in early development
and only the latest commit on `main` receives fixes.

## Known limits

- A remote host's edge Traefik serves plain HTTP on port 8480 to anyone who
  can reach that machine. Use remote hosts on networks you trust.
- HTTPS certificates come from your local mkcert authority and are trusted
  only by the machine that created them.
