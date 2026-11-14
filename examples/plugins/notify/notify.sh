#!/bin/sh
# Zevo hook: a desktop notification naming each project whose container was
# just attached.
#
# Zevo sends one line of JSON on standard input, such as
#   {"protocol":1,"event":"reconcile",
#    "actions":[{"action":"attached_project","container":"shop-app-1","slug":"shop"}],
#    "projects":[...]}
# The order of the fields inside an action is not fixed.
#
# Hooks have 5 seconds, and what they print is ignored. The notifier is
# started in the background so a slow one cannot hold Zevo up.

IFS= read -r input || true

# One action per line: split the JSON at its braces, keep the
# attached_project actions, and pull out their slugs. Slugs are plain
# (letters, digits and '-'), so this does not need a JSON parser.
slugs=$(printf '%s\n' "$input" |
	tr '{}' '\n\n' |
	grep '"action":"attached_project"' |
	sed -n 's/.*"slug":"\([^"]*\)".*/\1/p' |
	tr '\n' ' ' |
	sed 's/ *$//; s/ /, /g')

[ -n "$slugs" ] || exit 0
message="Ready: $slugs"

if command -v osascript >/dev/null 2>&1; then
	# The message is passed as an argument, never pasted into the script.
	osascript \
		-e 'on run argv' \
		-e 'display notification (item 1 of argv) with title "Captain Zevo"' \
		-e 'end run' \
		"$message" >/dev/null 2>&1 &
elif command -v notify-send >/dev/null 2>&1; then
	notify-send "Captain Zevo" "$message" >/dev/null 2>&1 &
fi

exit 0
