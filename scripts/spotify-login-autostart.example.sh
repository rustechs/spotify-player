#!/usr/bin/env bash
# Example KDE login autostart: start desktop Spotify and park it in the tray.
#
# Copy to a personal path (for example ~/.local/bin/spotify-autostart) and point
# a KDE autostart .desktop entry at it. Requires xdotool when ui.minimize_to_tray
# is enabled; uses taskbar iconic minimize otherwise.
#
# Do not use wmctrl hidden+iconic when minimize_to_tray is on — that leaves ghost
# windows that break tray "Show Spotify" after spotify_player wake.
set -euo pipefail

spotify_running() {
  pgrep -u "${USER}" -f '/snap/spotify/.*/usr/share/spotify/spotify$' >/dev/null 2>&1 \
    || pgrep -u "${USER}" -x spotify >/dev/null 2>&1
}

minimize_to_tray_pref_enabled() {
  local prefs
  for prefs in \
    "${HOME}/snap/spotify/current/.config/spotify/Users"/*/prefs \
    "${HOME}/.config/spotify/Users"/*/prefs; do
    [[ -f "${prefs}" ]] || continue
    grep -qx 'ui.minimize_to_tray=true' "${prefs}" && return 0
  done
  return 1
}

hide_spotify_to_tray() {
  command -v xdotool >/dev/null 2>&1 || return 1
  local ids id name
  ids="$(xdotool search --onlyvisible --class spotify 2>/dev/null || true)"
  [[ -n "${ids}" ]] || return 1
  for id in ${ids}; do
    name="$(xdotool getwindowname "${id}" 2>/dev/null || true)"
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%"${name##*[![:space:]]}"}"
    [[ "${name,,}" == "spotify" ]] && continue
    xdotool windowclose "${id}" 2>/dev/null || true
  done
  return 0
}

minimize_spotify_window() {
  if minimize_to_tray_pref_enabled; then
    hide_spotify_to_tray || return 1
    return 0
  fi

  command -v wmctrl >/dev/null 2>&1 || return 1
  local line id
  line="$(wmctrl -lx 2>/dev/null | awk 'BEGIN{IGNORECASE=1} /spotify/ {print; exit}')"
  [[ -n "${line}" ]] || return 1
  id="$(awk '{print $1}' <<<"${line}")"
  wmctrl -i -r "${id}" -b add,iconic 2>/dev/null || true
}

if spotify_running; then
  minimize_spotify_window || true
  exit 0
fi

if command -v /snap/bin/spotify >/dev/null 2>&1; then
  /snap/bin/spotify >/dev/null 2>&1 &
elif command -v spotify >/dev/null 2>&1; then
  spotify >/dev/null 2>&1 &
else
  echo "spotify-login-autostart: spotify not found on PATH" >&2
  exit 1
fi

for _ in $(seq 1 80); do
  if minimize_spotify_window; then
    exit 0
  fi
  sleep 0.25
done

exit 0
