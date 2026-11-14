#!/bin/sh
# Zevo doctor check: is the VPN up?
#
# Change HOST to a name that only resolves when you are on the VPN, such as
# an internal Git server. The default below never resolves, so until you
# change it this check always warns.
HOST="vpn-only.example.com"

# Zevo sends {"protocol":1,"request":"check"} on standard input. This check
# does not need it, but reads it anyway.
IFS= read -r _request || true

# Reply with one line of JSON. The level is ok, warn or fail. HOST is a plain
# host name, so it needs no escaping.
reply() {
	printf '{"level":"%s","detail":"%s"}\n' "$1" "$2"
}

# Whether $1 resolves, using whatever this system has. getent and
# dscacheutil go through the system resolver, so they see names a VPN client
# adds; nslookup is only a fallback.
resolves() {
	if command -v getent >/dev/null 2>&1; then
		getent hosts "$1" >/dev/null 2>&1
	elif command -v dscacheutil >/dev/null 2>&1; then
		# macOS: prints nothing for a name that does not resolve.
		dscacheutil -q host -a name "$1" 2>/dev/null | grep -q 'address'
	else
		nslookup "$1" >/dev/null 2>&1
	fi
}

# Zevo stops a check after 10 seconds and reports it as failed, so a
# resolver that never answers still gives a result.
if resolves "$HOST"; then
	reply ok "$HOST resolves"
else
	reply warn "$HOST does not resolve; connect to the VPN"
fi
