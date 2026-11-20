#!/bin/sh
# Small POSIX stdio MCP fixture for CLI and valet tests.
set -eu
if [ "$1" = 'exit early' ]; then
    exit 0
fi
case "$1" in
    'stdio fixture' | 'valet fixture') ;;
    'hang') exec sleep 30 ;;
    *) exit 2 ;;
esac

trace() {
    if [ "$1" = 'valet fixture' ]; then
        printf '%s\t%s\t%s\n' "$2" "$PWD" "${GLLMH_TURN:-}" >> "${GLLMH_TEST_MCP_TRACE:?}"
    fi
}

IFS= read -r request
case "$request" in
    *'"method":"initialize"'*) ;;
    *) exit 2 ;;
esac
case "$request" in
    *'"name":"gllmh"'*) ;;
    *) exit 2 ;;
esac
case "$request" in
    *'"protocolVersion":"2025-06-18"'*) ;;
    *) exit 2 ;;
esac
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"stdio-fixture","version":"1"}}}'

IFS= read -r request
test "$request" = '{"jsonrpc":"2.0","method":"notifications/initialized"}'

IFS= read -r request
case "$request" in
    *'"method":"tools/list"'*)
        trace "$1" list
        if [ "$1" = 'valet fixture' ]; then
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}],"nextCursor":"page 2"}}'
        else
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}}],"nextCursor":"page 2"}}'
        fi
        IFS= read -r request
        case "$request" in
            *'"method":"tools/list"'*'"cursor":"page 2"'*) ;;
            *) exit 2 ;;
        esac
        if [ "$1" = 'valet fixture' ]; then
            printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"fail","inputSchema":{"type":"object"}},{"name":"bad.name","inputSchema":{"type":"object"}},{"name":"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789","inputSchema":{"type":"object"}},{"name":"dup","inputSchema":{"type":"object"}},{"name":"dup","inputSchema":{"type":"object"}}]}}'
        else
            printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"fail","inputSchema":{"type":"object"}}]}}'
        fi
        ;;
    *'"method":"tools/call"'*'"name":"echo"'*)
        trace "$1" echo
        case "$request" in
            *'"text":"hello world"'*) ;;
            *) exit 2 ;;
        esac
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hello "},{"type":"text","text":"world"},{"type":"image","mimeType":"image/png","data":"AA=="}]}}'
        ;;
    *'"method":"tools/call"'*'"name":"fail"'*)
        trace "$1" fail
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"fixture failure"}],"isError":true}}'
        ;;
    *) exit 2 ;;
esac
