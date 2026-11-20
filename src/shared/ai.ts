/**
 * Chaldi IPC contract. There is deliberately no provider/key configuration here:
 * every model call is made by the authenticated Chald Friends server, which owns
 * the key, the prompt and the per-account quota.
 */

export type AiRole = 'user' | 'assistant' | 'tool';

export interface AiToolCall {
  id: string;
  name: string;
  arguments: string; // JSON string
}

export interface AiMessage {
  id: string;
  role: AiRole;
  content: string;
  /** tool calls requested by the assistant */
  toolCalls?: AiToolCall[];
  /** for role=tool */
  toolCallId?: string;
  toolName?: string;
  /** unix ms */
  at: number;
}

export interface AiChatRequest {
  messages: {
    role: AiRole;
    content: string;
    toolCalls?: AiToolCall[];
    toolCallId?: string;
    toolName?: string;
  }[];
}

export interface AiChatStep {
  content: string;
  toolCalls: AiToolCall[];
  toolResults: { toolCallId: string; name: string; result: string }[];
  finalContent?: string;
}

/** A question with clickable answers that the agent puts to the user (tool `ask_user`). */
export interface AiPoll {
  id: string;
  question: string;
  options: string[];
  multiple: boolean;
}

/** Per-account quota as reported by /chaldfriends/v1/ai/status (and every chat reply). */
export interface AiLimit {
  enabled: boolean;
  limit: number;
  remaining: number;
  resetInSec: number;
}

export interface AiChatResponse {
  steps: AiChatStep[];
  reply: string;
  poll?: AiPoll;
  /** server-issued id that binds all tool rounds of this turn to the signed-in account */
  turnId?: string | null;
  limit: number | null;
  remaining: number | null;
  resetInSec: number | null;
}

/** Live agent step for the UI (tool start / tool result / text). */
export type AiProgressKind = 'thinking' | 'tool_start' | 'tool_result' | 'text';

export interface AiProgressEvent {
  kind: AiProgressKind;
  /** Russian label for the UI, e.g. «Ищу моды» */
  label: string;
  toolName?: string;
  toolCallId?: string;
  /** short or full payload (tool args / result / assistant text) */
  detail?: string;
}
