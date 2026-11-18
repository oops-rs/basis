# Stdio MCP fixture for the declared-tool -> MCP -> JavaScript acceptance test.
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*)
            result='{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}' ;;
        *'"method":"tools/list"'*)
            result='{"tools":[{"name":"lookup","description":"Look up a ticket","inputSchema":{"type":"object","properties":{"ticket":{"type":"string"}},"required":["ticket"]}}]}' ;;
        *'"method":"tools/call"'*'"ticket":"0042"'*)
            result='{"content":[{"type":"text","text":"found 0042"}]}' ;;
        *'"method":"tools/call"'*)
            result='{"content":[{"type":"text","text":"wrong arguments"}],"isError":true}' ;;
        *) continue ;;
    esac
    id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
