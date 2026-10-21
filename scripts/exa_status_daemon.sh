#!/usr/bin/env bash
# Run EXA status as a macOS background service (launchd LaunchAgent): starts
# at login and comes back if it stops.
#
# Usage: scripts/exa_status_daemon.sh <command>
#   start      install or update the agent, and (re)start it
#   stop       stop the service (it still starts at the next login)
#   restart    same as start
#   status     loaded? PID? answering?
#   logs [N]   last N lines of the service log (default 40)
#   uninstall  stop and remove the agent
#
# Env: EXA_STATUS_CONFIG (config file the service is started with),
# EXA_STATUS_PORT (passed to the server as --port), EXA_STATUS_LABEL. When
# the first two are unset, the installed agent's values are kept; set one to
# an empty value to drop it. The port and certificate directory are the
# server's own (`exa-status settings`), so their defaults live in one place.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/release/exa-status"
LABEL="${EXA_STATUS_LABEL:-com.ddn.exa-status}"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/$LABEL.log"
DOMAIN="gui/$(id -u)"
TARGET="$DOMAIN/$LABEL"

# A value from the installed agent's plist, or nothing.
plist_value() { /usr/libexec/PlistBuddy -c "Print :$1" "$PLIST" 2>/dev/null || true; }

if [ -z "${EXA_STATUS_CONFIG+set}" ] && [ -f "$PLIST" ]; then
  EXA_STATUS_CONFIG="$(plist_value EnvironmentVariables:EXA_STATUS_CONFIG)"
fi
if [ -z "${EXA_STATUS_PORT+set}" ] && [ -f "$PLIST" ]; then
  EXA_STATUS_PORT="$(plist_value ProgramArguments | awk '$1 == "--port" { getline; print $1 }')"
fi

CONFIG=""
if [ -n "${EXA_STATUS_CONFIG:-}" ]; then
  if [ ! -f "$EXA_STATUS_CONFIG" ]; then
    echo "EXA_STATUS_CONFIG names $EXA_STATUS_CONFIG, which does not exist." >&2
    exit 1
  fi
  CONFIG="$(cd "$(dirname "$EXA_STATUS_CONFIG")" && pwd)/$(basename "$EXA_STATUS_CONFIG")"
fi

require_binary() {
  if [ ! -x "$BIN" ]; then
    echo "No binary at $BIN; run 'cargo build --release' in $REPO first." >&2
    exit 1
  fi
}

# One value the server resolves from its config, run as the service runs it.
setting() {
  local out
  if [ -n "$CONFIG" ]; then
    out="$(cd "$REPO" && EXA_STATUS_CONFIG="$CONFIG" "$BIN" settings)"
  else
    out="$(cd "$REPO" && env -u EXA_STATUS_CONFIG "$BIN" settings)"
  fi
  printf '%s\n' "$out" | sed -n "s/^$1=//p"
}

# The port and certificate directory of the service, in PORT and CERT_DIR.
resolve() {
  require_binary
  PORT="${EXA_STATUS_PORT:-}"
  if [ -z "$PORT" ]; then
    PORT="$(setting port)"
  fi
  CERT_DIR="$(setting cert_dir)"
}

plist_text() {
  local port_args="" env_block=""
  if [ -n "${EXA_STATUS_PORT:-}" ]; then
    port_args="
    <string>--port</string>
    <string>$EXA_STATUS_PORT</string>"
  fi
  if [ -n "$CONFIG" ]; then
    env_block="
  <key>EnvironmentVariables</key>
  <dict><key>EXA_STATUS_CONFIG</key><string>$CONFIG</string></dict>"
  fi
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN</string>
    <string>serve</string>$port_args
  </array>$env_block
  <key>WorkingDirectory</key><string>$REPO</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
PLIST
}

is_loaded() { launchctl print "$TARGET" >/dev/null 2>&1; }
# bootout returns before the service is gone (the server takes a few seconds
# to stop); bootstrapping before then fails with an I/O error.
unload() {
  launchctl bootout "$TARGET" 2>/dev/null || true
  for _ in $(seq 1 20); do
    is_loaded || return 0
    sleep 0.5
  done
  echo "$LABEL did not stop within 10 seconds." >&2
  exit 1
}
# The PID listening on the port, or nothing (lsof exits 1 when nothing listens).
listener_pid() { { lsof -tiTCP:"$PORT" -sTCP:LISTEN 2>/dev/null || true; } | head -n 1; }
answering() {
  curl -s -o /dev/null -w '%{http_code}' --max-time 5 --cacert "$CERT_DIR/cert.pem" \
    "https://localhost:$PORT/api/status" 2>/dev/null || true
}

settle() {
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ "$(answering)" = "200" ] && return 0
    sleep 1
  done
}

cmd_start() {
  resolve
  mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
  local wanted
  wanted="$(plist_text)"
  if is_loaded; then
    if [ "$wanted" = "$(cat "$PLIST" 2>/dev/null || true)" ]; then
      launchctl kickstart -k "$TARGET"
    else
      # The service settings changed: load the agent again with the new ones.
      unload
      printf '%s\n' "$wanted" > "$PLIST"
      launchctl bootstrap "$DOMAIN" "$PLIST"
    fi
  else
    if [ -n "$(listener_pid)" ]; then
      echo "Port $PORT is already in use; stop that first or set EXA_STATUS_PORT." >&2
      exit 1
    fi
    printf '%s\n' "$wanted" > "$PLIST"
    launchctl bootstrap "$DOMAIN" "$PLIST"
  fi
  settle || true
  cmd_status
}

cmd_stop() {
  if is_loaded; then
    unload
  fi
  echo "Stopped $LABEL. It starts again at the next login; 'uninstall' removes it."
}

cmd_status() {
  if is_loaded; then
    resolve
    local pid
    pid="$(listener_pid)"
    echo "$LABEL loaded, pid ${pid:-none}, https://localhost:$PORT answers HTTP $(answering)"
  else
    echo "$LABEL not loaded"
  fi
}

cmd_logs() { tail -n "${1:-40}" "$LOG"; }

cmd_uninstall() {
  if is_loaded; then
    unload
  fi
  rm -f "$PLIST"
  echo "Removed $LABEL."
}

case "${1:-}" in
  start | restart) cmd_start ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  logs) cmd_logs "${2:-40}" ;;
  uninstall) cmd_uninstall ;;
  *) sed -n '2,17p' "$0"; exit 2 ;;
esac
