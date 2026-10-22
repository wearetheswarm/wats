#!/bin/sh
# A stand-in for the wats binary in the wrapper tests (make-fixtures.sh):
# not wats. It never touches ~/.wats or any agent config.
case "${1:-}" in
    --version) echo "wats @VERSION@" ;;
    echo-args) shift; for a in "$@"; do printf '[%s]\n' "$a"; done ;;
    cat) cat ;;
    exit) exit "$2" ;;
    env) printf 'WATS_WRAPPER=%s\n' "${WATS_WRAPPER:-}" ;;
    wait-term)
        trap 'echo "got TERM"; exit 42' TERM
        echo ready
        while :; do sleep 1 >/dev/null & wait $!; done ;;
    *) echo "fake wats: $*" ;;
esac
