#!/bin/sh
# Small POSIX stdio MCP fixture. Arguments select a normal or early-exit server.
set -eu
if [ "$1" = 'exit early' ]; then
    exit 0
fi
test "$1" = 'stdio fixture'

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
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}}],"nextCursor":"page 2"}}'
        IFS= read -r request
        case "$request" in
            *'"method":"tools/list"'*'"cursor":"page 2"'*) ;;
            *) exit 2 ;;
        esac
        printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"fail","inputSchema":{"type":"object"}}]}}'
        ;;
    *'"method":"tools/call"'*'"name":"echo"'*)
        case "$request" in
            *'"text":"hello world"'*) ;;
            *) exit 2 ;;
        esac
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hello "},{"type":"text","text":"world"},{"type":"image","mimeType":"image/png","data":"AA=="}]}}'
        ;;
    *'"method":"tools/call"'*'"name":"fail"'*)
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"fixture failure"}],"isError":true}}'
        ;;
    *) exit 2 ;;
esac
