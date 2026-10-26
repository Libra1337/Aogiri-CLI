#!/bin/sh
# A stand-in oh-my-pi for tests/acp_presets.rs: its version, its ACP agent
# (the fake one, $FAKE_ACP, playing omp) or its TUI. The TUI prints its
# arguments, echoes each line typed and exits on `quit`.
case "$1" in
--version) echo "omp/18.1.21"; exit 0 ;;
acp) shift; exec python3 "$FAKE_ACP" "$@" ;;
esac
echo "TUI omp $*"
while IFS= read -r line; do
  [ "$line" = quit ] && exit 0
  echo "tui: $line"
done
