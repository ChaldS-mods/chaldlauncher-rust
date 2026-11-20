import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { AiChatResponse, AiLimit, AiMessage, AiPoll, AiProgressEvent } from '../../shared/ai';
import { ApiError, call, errMsg, on } from '../api';
import { Icon } from '../components/Icon';
import { Markdown } from '../components/Markdown';
import { Empty } from '../components/ui';
import { useI18n } from '../i18n';
import { useStore } from '../store';

const STORAGE_KEY = 'chald.ai.sessions.v1';
const MAX_SESSIONS = 30;
const MAX_MESSAGES = 120;
const MAX_BUBBLES = 100;

function rid(): string {
  return Math.random().toString(36).slice(2, 10);
}

function formatReset(sec: number): string {
  if (!Number.isFinite(sec) || sec <= 0) return '';
  if (sec < 60) return `${Math.ceil(sec)} с`;
  const minutes = Math.ceil(sec / 60);
  if (minutes < 60) return `${minutes} мин`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest ? `${hours} ч ${rest} мин` : `${hours} ч`;
}

type Bubble =
  | { id: string; kind: 'user'; text: string }
  | { id: string; kind: 'assistant'; text: string }
  | { id: string; kind: 'thinking'; text: string }
  | { id: string; kind: 'poll'; poll: AiPoll; selected: string[]; /** null = still open; [] = skipped */ answered: string[] | null }
  | {
      id: string;
      kind: 'tool';
      label: string;
      toolName: string;
      status: 'running' | 'done';
      detail?: string;
      open?: boolean;
    };

interface ChatSession {
  id: string;
  createdAt: number;
  updatedAt: number;
  /** Full structured conversation sent back on the next turn. */
  messages: AiMessage[];
  /** Display-only transcript (bubbles, polls, tool cards). */
  bubbles: Bubble[];
}

function makeSession(): ChatSession {
  const now = Date.now();
  return { id: rid(), createdAt: now, updatedAt: now, messages: [], bubbles: [] };
}

function isMessage(value: unknown): value is AiMessage {
  if (!value || typeof value !== 'object') return false;
  const message = value as Partial<AiMessage>;
  return typeof message.id === 'string'
    && (message.role === 'user' || message.role === 'assistant' || message.role === 'tool')
    && typeof message.content === 'string'
    && typeof message.at === 'number';
}

function isBubble(value: unknown): value is Bubble {
  if (!value || typeof value !== 'object') return false;
  const bubble = value as { id?: unknown; kind?: unknown };
  return typeof bubble.id === 'string'
    && (
      bubble.kind === 'user' || bubble.kind === 'assistant' || bubble.kind === 'thinking'
      || bubble.kind === 'poll' || bubble.kind === 'tool'
    );
}

function loadSessions(): ChatSession[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((value): value is Record<string, unknown> => !!value && typeof value === 'object')
      .map((value) => ({
        id: typeof value.id === 'string' && value.id ? value.id : rid(),
        createdAt: typeof value.createdAt === 'number' ? value.createdAt : Date.now(),
        updatedAt: typeof value.updatedAt === 'number' ? value.updatedAt : Date.now(),
        messages: Array.isArray(value.messages) ? value.messages.filter(isMessage).slice(-MAX_MESSAGES) : [],
        bubbles: Array.isArray(value.bubbles) ? value.bubbles.filter(isBubble).slice(-MAX_BUBBLES) : [],
      }))
      .sort((a, b) => b.updatedAt - a.updatedAt)
      .slice(0, MAX_SESSIONS);
  } catch {
    return [];
  }
}

function saveSessions(sessions: ChatSession[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(sessions));
  } catch {
    // Storage can be full or blocked. Keep only the essentials and try once more.
    try {
      const trimmed = sessions.slice(0, 10).map((session) => ({
        ...session,
        messages: session.messages.slice(-40),
        bubbles: session.bubbles.slice(-40),
      }));
      localStorage.setItem(STORAGE_KEY, JSON.stringify(trimmed));
    } catch { /* storage unavailable — chats stay in memory for this run */ }
  }
}

export function AiChatPage() {
  const { t, localeTag } = useI18n();
  const toast = useStore((s) => s.toast);
  const go = useStore((s) => s.go);

  const initial = useMemo(() => {
    const loaded = loadSessions();
    if (loaded.length) return { sessions: loaded, activeId: loaded[0].id };
    const fresh = makeSession();
    return { sessions: [fresh], activeId: fresh.id };
  }, []);
  const [sessions, setSessions] = useState<ChatSession[]>(initial.sessions);
  const [activeId, setActiveId] = useState(initial.activeId);
  // Progress events are subscribed once, so they need the current chat id via a ref.
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;

  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [hasSession, setHasSession] = useState<boolean | null>(null);
  const [limit, setLimit] = useState<AiLimit | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  const active = sessions.find((session) => session.id === activeId) ?? sessions[0];
  const history = active?.messages ?? [];
  const bubbles = active?.bubbles ?? [];
  const sortedSessions = useMemo(() => [...sessions].sort((a, b) => b.updatedAt - a.updatedAt), [sessions]);

  useEffect(() => { saveSessions(sessions); }, [sessions]);

  const mutateActive = useCallback((fn: (session: ChatSession) => ChatSession) => {
    setSessions((prev) => prev.map((session) => {
      if (session.id !== activeIdRef.current) return session;
      const next = fn(session);
      return next === session ? session : { ...next, updatedAt: Date.now() };
    }));
  }, []);

  const updateBubbles = useCallback((fn: (items: Bubble[]) => Bubble[]) => {
    mutateActive((session) => ({ ...session, bubbles: fn(session.bubbles) }));
  }, [mutateActive]);

  const updateMessages = useCallback((fn: (items: AiMessage[]) => AiMessage[]) => {
    mutateActive((session) => ({ ...session, messages: fn(session.messages) }));
  }, [mutateActive]);

  const refreshLimit = useCallback(async () => {
    try {
      setLimit(await call('ai:status'));
    } catch (e) {
      setLimit(null);
      if (e instanceof ApiError && e.code === 'FRIENDS_AUTH') setHasSession(false);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const ok = await call('friends:hasSession');
        if (!alive) return;
        setHasSession(ok);
        if (ok) await refreshLimit();
        else setLimit(null);
      } catch {
        if (alive) {
          setHasSession(false);
          setLimit(null);
        }
      }
    })();
    return () => { alive = false; };
  }, [refreshLimit]);

  useEffect(() => {
    const el = bottomRef.current?.parentElement;
    if (el) el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' });
  }, [bubbles, busy]);

  useEffect(() => {
    return on('ai:progress', (ev: AiProgressEvent) => {
      updateBubbles((prev) => {
        if (ev.kind === 'thinking') {
          const without = prev.filter((b) => b.kind !== 'thinking');
          return [...without, { id: rid(), kind: 'thinking', text: ev.label || 'Чалди думает…' }];
        }
        if (ev.kind === 'tool_start') {
          const without = prev.filter((b) => b.kind !== 'thinking');
          return [
            ...without,
            {
              id: ev.toolCallId || rid(),
              kind: 'tool',
              label: ev.label,
              toolName: ev.toolName || '',
              status: 'running',
              detail: ev.detail,
              open: false,
            },
          ];
        }
        if (ev.kind === 'tool_result') {
          return prev.map((b) =>
            b.kind === 'tool' && (b.id === ev.toolCallId || b.toolName === ev.toolName) && b.status === 'running'
              ? { ...b, status: 'done' as const, detail: ev.detail ?? b.detail }
              : b,
          );
        }
        if (ev.kind === 'text' && ev.detail) {
          const without = prev.filter((b) => b.kind !== 'thinking');
          return [...without, { id: rid(), kind: 'assistant', text: ev.detail }];
        }
        return prev;
      });
    });
  }, [updateBubbles]);

  const newChat = () => {
    if (busy) return;
    const session = makeSession();
    setSessions((prev) => [session, ...prev].slice(0, MAX_SESSIONS));
    activeIdRef.current = session.id;
    setActiveId(session.id);
    setInput('');
  };

  const switchChat = (id: string) => {
    if (busy || id === activeId) return;
    activeIdRef.current = id;
    setActiveId(id);
    setInput('');
  };

  const deleteChat = (id: string) => {
    if (busy) return;
    const next = sessions.filter((session) => session.id !== id);
    if (!next.length) {
      const fresh = makeSession();
      setSessions([fresh]);
      activeIdRef.current = fresh.id;
      setActiveId(fresh.id);
    } else {
      setSessions(next);
      if (id === activeId) {
        activeIdRef.current = next[0].id;
        setActiveId(next[0].id);
      }
    }
  };

  const clearChat = () => mutateActive((session) => ({ ...session, messages: [], bubbles: [] }));

  const undoLast = async () => {
    if (busy) return;
    try {
      const result = await call('ai:undo');
      if (result.ok) toast('ok', t('ai.undone', { what: result.undone || '' }));
      else toast('info', t('ai.nothingToUndo'));
    } catch (error) {
      toast('error', errMsg(error));
    }
  };

  const answerPoll = (id: string, chosen: string[]) => {
    if (busy || !chosen.length) return;
    updateBubbles((prev) => prev.map((b) => (b.kind === 'poll' && b.id === id ? { ...b, answered: chosen } : b)));
    void send(chosen.join(', '));
  };

  const togglePollOption = (id: string, option: string) => {
    updateBubbles((prev) => prev.map((b) => {
      if (b.kind !== 'poll' || b.id !== id || b.answered) return b;
      const has = b.selected.includes(option);
      return { ...b, selected: has ? b.selected.filter((o) => o !== option) : [...b.selected, option] };
    }));
  };

  const toggleTool = (id: string) => {
    updateBubbles((prev) =>
      prev.map((b) => (b.kind === 'tool' && b.id === id ? { ...b, open: !b.open } : b)),
    );
  };

  const applyChatLimit = (res: AiChatResponse) => {
    if (res.limit === null && res.remaining === null && res.resetInSec === null) return;
    setLimit((prev) => ({
      enabled: prev?.enabled ?? true,
      limit: res.limit ?? prev?.limit ?? 0,
      remaining: res.remaining ?? prev?.remaining ?? 0,
      resetInSec: res.resetInSec ?? prev?.resetInSec ?? 0,
    }));
  };

  const send = async (override?: string) => {
    const text = (override ?? input).trim();
    if (!text || busy) return;
    try {
      if (!(await call('friends:hasSession'))) {
        setHasSession(false);
        setLimit(null);
        toast('error', t('ai.needLogin'));
        return;
      }
    } catch {
      toast('error', t('ai.needLogin'));
      return;
    }
    const userMsg: AiMessage = { id: rid(), role: 'user', content: text, at: Date.now() };
    const nextHist = [...history, userMsg];
    updateMessages(() => nextHist);
    // a typed message closes polls that were not answered with a click
    updateBubbles((b) => [
      ...b.map((x) => (x.kind === 'poll' && x.answered === null ? { ...x, answered: [] } : x)),
      { id: userMsg.id, kind: 'user', text },
    ]);
    setInput('');
    setBusy(true);
    try {
      const payload = nextHist
        .filter((m) => m.role === 'user' || m.role === 'assistant' || m.role === 'tool')
        .map((m) => ({
          role: m.role,
          content: m.content,
          toolCalls: m.toolCalls,
          toolCallId: m.toolCallId,
          toolName: m.toolName,
        }));
      const res = await call('ai:chat', { messages: payload });
      applyChatLimit(res);

      // Append structured history for the next turn (tool calls + tool results stay in the conversation).
      const extra: AiMessage[] = [];
      for (const step of res.steps) {
        if (step.toolCalls.length) {
          extra.push({
            id: rid(),
            role: 'assistant',
            content: step.content || '',
            toolCalls: step.toolCalls,
            at: Date.now(),
          });
          for (const tr of step.toolResults) {
            extra.push({
              id: rid(),
              role: 'tool',
              content: tr.result,
              toolCallId: tr.toolCallId,
              toolName: tr.name,
              at: Date.now(),
            });
          }
        }
        if (step.finalContent) {
          extra.push({ id: rid(), role: 'assistant', content: step.finalContent, at: Date.now() });
        } else if (step.content && !step.toolCalls.length) {
          extra.push({ id: rid(), role: 'assistant', content: step.content, at: Date.now() });
        }
      }
      if (extra.length) updateMessages((h) => [...h, ...extra].slice(-MAX_MESSAGES));

      if (res.poll) {
        const poll = res.poll;
        updateBubbles((prev) => [
          ...prev.filter((b) => b.kind !== 'thinking'),
          { id: poll.id, kind: 'poll', poll, selected: [], answered: null },
        ]);
      }

      // If progress events didn't deliver final text, show reply.
      updateBubbles((prev) => {
        const withoutThinking = prev.filter((b) => b.kind !== 'thinking');
        if (res.reply && !withoutThinking.some((b) => b.kind === 'assistant' && b.text.trim() === res.reply.trim())) {
          const lastUserIdx = withoutThinking.map((b) => b.kind === 'user').lastIndexOf(true);
          const after = withoutThinking.slice(lastUserIdx + 1);
          if (!after.some((b) => b.kind === 'assistant')) {
            return [...withoutThinking, { id: rid(), kind: 'assistant', text: res.reply }];
          }
        }
        return withoutThinking;
      });
    } catch (e) {
      const msg = errMsg(e);
      toast('error', msg);
      void refreshLimit(); // 429 also carries the remaining quota, but status is authoritative
      updateBubbles((b) => [
        ...b.filter((x) => x.kind !== 'thinking'),
        { id: rid(), kind: 'assistant', text: `⚠ ${msg}` },
      ]);
    } finally {
      setBusy(false);
    }
  };

  const sessionTitle = (session: ChatSession): string => {
    const first = session.messages.find((m) => m.role === 'user')?.content.trim() ?? '';
    if (!first) return t('ai.newChat');
    return first.length > 42 ? `${first.slice(0, 42)}…` : first;
  };

  if (hasSession === null) {
    return (
      <div className="page page--ai">
        <Empty icon="terminal" title="Чалди" text={t('ai.checking')} />
      </div>
    );
  }

  if (!hasSession) {
    return (
      <div className="page page--ai">
        <header className="page-head page-head--ai">
          <div className="page-head__title">
            <Icon name="terminal" size={36} />
            <div>
              <h1>Чалди</h1>
              <p>{t('ai.subtitle')}</p>
            </div>
          </div>
        </header>
        <Empty
          icon="user"
          title={t('ai.needLogin')}
          text={t('ai.needLoginHint')}
          action={<button type="button" className="btn btn--primary" onClick={() => go('friends')}>{t('nav.friends')}</button>}
        />
      </div>
    );
  }

  const resetText = limit ? formatReset(limit.resetInSec) : '';

  return (
    <div className="page page--ai">
      <header className="page-head page-head--ai">
        <div className="page-head__title">
          <Icon name="terminal" size={36} />
          <div>
            <h1>Чалди</h1>
            <p>{t('ai.subtitle')}</p>
          </div>
        </div>
        <div className="page-head__actions">
          {limit && (
            <div
              className={`ai-limit${limit.remaining === 0 ? ' is-empty' : ''}`}
              title={limit.enabled ? t('ai.limitHint') : t('ai.limitUnlimited')}
            >
              <span className="ai-limit__value">
                {limit.enabled ? `${limit.remaining}/${limit.limit}` : '∞'}
              </span>
              <span className="ai-limit__label">{t('ai.limit')}</span>
              {resetText && <span className="ai-limit__reset">{t('ai.limitReset', { time: resetText })}</span>}
            </div>
          )}
          <button type="button" className="btn" onClick={() => void undoLast()} disabled={busy} title={t('ai.undoHint')}>
            {t('ai.undo')}
          </button>
          <button type="button" className="btn" onClick={clearChat} disabled={busy || !bubbles.length}>
            {t('ai.clear')}
          </button>
          <button type="button" className="btn" onClick={() => go('friends')}>
            {t('ai.account')}
          </button>
        </div>
      </header>

      <div className="ai-layout">
        <aside className="ai-sessions" aria-label={t('ai.sessions')}>
          <div className="ai-sessions__head">
            <strong>{t('ai.sessions')}</strong>
            <button type="button" className="btn btn--sm btn--primary" onClick={newChat} disabled={busy} title={t('ai.newChat')}>
              <Icon name="plus" size={14} /> <span>{t('ai.newChat')}</span>
            </button>
          </div>
          <div className="ai-sessions__list">
            {sortedSessions.map((session) => (
              <div key={session.id} className={`ai-session${session.id === active?.id ? ' is-active' : ''}`}>
                <button type="button" className="ai-session__open" onClick={() => switchChat(session.id)} disabled={busy}>
                  <Icon name="terminal" size={14} />
                  <span className="ai-session__title">{sessionTitle(session)}</span>
                  <small className="ai-session__date">
                    {new Date(session.updatedAt).toLocaleDateString(localeTag, { day: '2-digit', month: '2-digit' })}
                  </small>
                </button>
                <button
                  type="button"
                  className="ai-session__del"
                  onClick={() => deleteChat(session.id)}
                  disabled={busy}
                  title={t('ai.deleteChat')}
                  aria-label={t('ai.deleteChat')}
                >
                  <Icon name="x" size={13} />
                </button>
              </div>
            ))}
          </div>
        </aside>

        <section className="ai-conversation">
          <div className="ai-chat__body">
            {bubbles.length === 0 ? (
              <div className="ai-chat__empty">
                <Empty icon="terminal" title="Чалди на связи" text={t('ai.emptyHint')} />
              </div>
            ) : (
              <ul className="ai-chat__list">
                {bubbles.map((b) => {
                  if (b.kind === 'user') {
                    return (
                      <li key={b.id} className="ai-bubble ai-bubble--user">
                        <div className="ai-bubble__text">{b.text}</div>
                      </li>
                    );
                  }
                  if (b.kind === 'assistant') {
                    return (
                      <li key={b.id} className="ai-bubble ai-bubble--assistant ai-bubble--md">
                        <div className="ai-bubble__who">Чалди</div>
                        <Markdown text={b.text} />
                      </li>
                    );
                  }
                  if (b.kind === 'poll') {
                    const done = b.answered !== null;
                    return (
                      <li key={b.id} className="ai-bubble ai-bubble--poll">
                        <div className="ai-bubble__who">Чалди спрашивает</div>
                        <div className="ai-poll__q">{b.poll.question}</div>
                        <div className="ai-poll__opts" role={b.poll.multiple ? 'group' : 'radiogroup'}>
                          {(Array.isArray(b.poll.options) ? b.poll.options : []).map((o) => {
                            const picked = done ? b.answered!.includes(o) : b.selected.includes(o);
                            return (
                              <button
                                key={o}
                                type="button"
                                className={`ai-poll__opt${picked ? ' is-picked' : ''}`}
                                disabled={done || busy}
                                aria-pressed={picked}
                                onClick={() => (b.poll.multiple ? togglePollOption(b.id, o) : answerPoll(b.id, [o]))}
                              >
                                {b.poll.multiple && <span className="ai-poll__box">{picked ? '✓' : ''}</span>}
                                {o}
                              </button>
                            );
                          })}
                        </div>
                        {b.poll.multiple && !done && (
                          <button type="button" className="btn btn--primary btn--sm ai-poll__send" disabled={busy || !b.selected.length} onClick={() => answerPoll(b.id, b.selected)}>
                            {t('ai.answer')}
                          </button>
                        )}
                        <div className="ai-poll__hint">
                          {done ? (b.answered!.length ? t('ai.pollSent') : t('ai.pollSkipped')) : t('ai.pollHint')}
                        </div>
                      </li>
                    );
                  }
                  if (b.kind === 'thinking') {
                    return (
                      <li key={b.id} className="ai-bubble ai-bubble--thinking">
                        <span className="ai-dot" /> {b.text}
                      </li>
                    );
                  }
                  return (
                    <li key={b.id} className={`ai-bubble ai-bubble--tool${b.status === 'running' ? ' is-running' : ''}`}>
                      <button type="button" className="ai-tool__head" onClick={() => toggleTool(b.id)}>
                        <span className="ai-tool__status">{b.status === 'running' ? '…' : '✓'}</span>
                        <span className="ai-tool__label">{b.label}</span>
                        <span className="ai-tool__chev">{b.open ? '▾' : '▸'}</span>
                      </button>
                      {b.open && typeof b.detail === 'string' && b.detail && (
                        <pre className="ai-tool__detail">{b.detail.length > 4000 ? `${b.detail.slice(0, 4000)}…` : b.detail}</pre>
                      )}
                    </li>
                  );
                })}
              </ul>
            )}
            <div ref={bottomRef} />
          </div>

          <form
            className="ai-chat__composer"
            onSubmit={(e) => {
              e.preventDefault();
              void send();
            }}
          >
            <input
              className="input ai-chat__input"
              value={input}
              disabled={busy}
              placeholder={t('ai.placeholder')}
              onChange={(e) => setInput(e.target.value)}
            />
            <button type="submit" className="btn btn--primary" disabled={busy || !input.trim()}>
              {busy ? '…' : t('ai.send')}
            </button>
          </form>
        </section>
      </div>
    </div>
  );
}
