# Scrubs one captured transcript line into a test fixture line: `jq -c -f scrub.jq <capture>`.
# Whitelist: ids, types, timestamps, and the fields the tailer reads. Every other line keeps only
# its skeleton. Strings are clipped and the capturing user's home directory is replaced.
def clip: if length > 300 then .[0:300] + "…" else . end;
def pick(ks): with_entries(select(.key as $k | ks | index($k)));
def skeleton: pick(["type", "subtype", "uuid", "parentUuid", "id", "parentId", "timestamp", "ordinal"]);
def message: pick(["id", "role", "content", "stop_reason", "stopReason", "toolCallId", "toolName", "isError"])
  | if .content | type == "array"
    then .content |= map(del(.signature, .thinkingSignature) | if .type == "thinking" then .thinking = "" else . end)
    else . end;
def payload:
  if .type == "session_meta" then .payload |= pick(["id", "session_id", "parent_thread_id", "timestamp", "source", "thread_source", "cli_version"])
  elif .type == "response_item" then .payload |= (del(.encrypted_content, .summary, .internal_chat_message_metadata_passthrough))
  elif .type == "event_msg" and (.payload.type | IN("task_started", "task_complete")) then .
  elif .type == "event_msg" and .payload.type == "item_completed" and .payload.item.type == "UserMessage" then .payload |= pick(["type", "item"])
  else .payload |= {type: .type} | .payload |= with_entries(select(.value != null))
  end;
(if has("payload") then payload | pick(["timestamp", "type", "ordinal", "payload"])
 elif .type | IN("user", "assistant", "message") then skeleton + pick(["isMeta", "isCompactSummary", "isSidechain"]) + {message: (.message | message)}
 else skeleton
 end)
| walk(if type == "string" then (gsub("/Users/[^/\"]+"; "/home/dev") | gsub("-Users-[^-/]+-"; "-home-dev-") | clip) else . end)
