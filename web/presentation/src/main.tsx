import {
  Button, Checkbox, CheckboxGroup, defaultTheme, Item, Picker,
  Provider, Switch, TextField,
} from '@adobe/react-spectrum';
import { StrictMode, useCallback, useEffect, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import './style.css';

type Field = { id: string; label: string; kind: 'text' | 'choice' | 'multi_choice' | 'boolean'; required: boolean; initial?: unknown; options: string[] };
type Node =
  | { kind: 'text'; id: string; text: string }
  | { kind: 'code'; id: string; language: string | null; text: string }
  | { kind: 'diff'; id: string; before: string; after: string }
  | { kind: 'table'; id: string; columns: string[]; rows: string[][] }
  | { kind: 'group'; id: string; title: string; children: Node[] }
  | { kind: 'attachment'; id: string; name: string; record_sequence: number }
  | { kind: 'status'; id: string; text: string }
  | { kind: 'form'; id: string; action: string; fields: Field[] }
  | { kind: 'button'; id: string; action: string; label: string };
type View = { owner: string; run_id: number; revision: number; active: boolean; handled_actions: string[]; id: string; slot: string; title: string; fallback: string; platforms: string[]; nodes: Node[] };
type ActivityTarget = { kind: 'composer' } | { kind: 'form'; owner: string; view_id: string; node_id: string };
type Snapshot = { version: number; session_id: string; sequence: number; views: View[]; activity: { attachment: number; frontend: string; target: ActivityTarget }[] };
type State = { active_run: number | null; closed: boolean };
type Frame = { presentation: Snapshot; state: State };
type Fault = { code: string; message: string };
const token = new URLSearchParams(location.search).get('token') ?? '';
let requestCounter = 0;
const requestId = () => `web-${Date.now()}-${++requestCounter}`;
async function api<T>(route: string, body?: unknown, signal?: AbortSignal): Promise<T> {
  const response = await fetch(route, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { 'X-Eden-Token': token, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body), signal,
  });
  const data = await response.json();
  if (!data.ok) {
    const error = data.error as Fault;
    throw new Error(`${error.code}: ${error.message}`);
  }
  return data.result as T;
}
function App() {
  const [attachment, setAttachment] = useState<number | null>(null);
  const [frame, setFrame] = useState<Frame | null>(null);
  const [composer, setComposer] = useState(sessionStorage.getItem('eden-live-composer') ?? '');
  const [drafts, setDrafts] = useState<Record<string, Record<string, unknown>>>({});
  const [message, setMessage] = useState('');
  const activityTimer = useRef<number | null>(null);
  const attachmentRef = useRef<number | null>(null);
  const activeTarget = useRef<ActivityTarget | null>(null);
  useEffect(() => {
    if (!token) { setMessage('Open the explicit URL supplied by the live host.'); return; }
    const controller = new AbortController();
    let current: number | null = null;
    (async () => {
      try {
        const attached = await api<{ attachment: number }>('/attach', { frontend: 'web' }, controller.signal);
        current = attached.attachment; attachmentRef.current = current; setAttachment(current);
        const initial = await api<Frame>(`/snapshot?attachment=${current}`, undefined, controller.signal);
        setFrame(initial);
        let sequence = initial.presentation.sequence;
        while (!controller.signal.aborted) {
          const next = await api<Frame>(`/snapshot?after=${sequence}&attachment=${current}`, undefined, controller.signal);
          sequence = next.presentation.sequence;
          setFrame(next);
        }
      } catch (error) { if (!controller.signal.aborted) setMessage(String(error)); }
    })();
    return () => {
      controller.abort();
      if (current !== null) {
        void api('/detach', { attachment: current }).catch(() => {});
      }
      attachmentRef.current = null;
    };
  }, []);
  const stopActivity = useCallback(() => {
    if (activityTimer.current !== null) window.clearTimeout(activityTimer.current);
    const id = attachmentRef.current;
    const target = activeTarget.current;
    if (id !== null && target) void api('/activity', { attachment: id, target, active: false }).catch(() => {});
    activeTarget.current = null;
  }, []);
  const typing = useCallback((target: ActivityTarget) => {
    const id = attachmentRef.current;
    if (id === null) return;
    activeTarget.current = target;
    void api('/activity', { attachment: id, target, active: true }).catch(() => {});
    if (activityTimer.current !== null) window.clearTimeout(activityTimer.current);
    activityTimer.current = window.setTimeout(stopActivity, 3000);
  }, [stopActivity]);
  const changeComposer = (value: string) => {
    setComposer(value); sessionStorage.setItem('eden-live-composer', value);
    typing({ kind: 'composer' });
  };
  const send = async (mode: 'prompt' | 'steering' | 'follow_up') => {
    if (!composer.trim()) return;
    try {
      const result = mode === 'prompt'
        ? await api<{ run_id: number }>('/prompt', { request_id: requestId(), text: composer })
        : await api('/enqueue', { request_id: requestId(), kind: mode, text: composer });
      setMessage(`Accepted: ${JSON.stringify(result)}`);
      setComposer(''); sessionStorage.removeItem('eden-live-composer'); stopActivity();
    } catch (error) { setMessage(String(error)); }
  };
  const draftKey = (view: View, node: Node) => `${view.owner}/${view.id}/${node.id}`;
  const changeField = (view: View, node: Node, field: string, value: unknown) => {
    const key = draftKey(view, node);
    setDrafts(previous => ({ ...previous, [key]: { ...previous[key], [field]: value } }));
    typing({ kind: 'form', owner: view.owner, view_id: view.id, node_id: node.id });
  };
  const act = async (view: View, action: string, values: unknown) => {
    try {
      const result = await api('/action', {
        session_id: frame?.presentation.session_id, owner: view.owner, view_id: view.id,
        revision: view.revision, action, request_id: requestId(), values,
      });
      setMessage(`Action: ${JSON.stringify(result)}`); stopActivity();
    } catch (error) { setMessage(String(error)); }
  };
  const renderNode = (node: Node, view: View): React.ReactNode => {
    switch (node.kind) {
      case 'text': return <p key={node.id}>{node.text}</p>;
      case 'code': return <pre key={node.id} className="code" aria-label={`Code ${node.language ?? ''}`}>{node.text}</pre>;
      case 'diff': return <div key={node.id} className="diff"><pre className="removed">{node.before}</pre><pre className="added">{node.after}</pre></div>;
      case 'table': return <table key={node.id}><thead><tr>{node.columns.map((name, index) => <th key={`${node.id}-${index}`}>{name}</th>)}</tr></thead><tbody>{node.rows.map((row, index) => <tr key={`${node.id}-${index}`}>{row.map((cell, column) => <td key={column}>{cell}</td>)}</tr>)}</tbody></table>;
      case 'group': return <section key={node.id}><h3>{node.title}</h3>{node.children.map(child => renderNode(child, view))}</section>;
      case 'attachment': return <p key={node.id}>Attachment: {node.name} (record {node.record_sequence})</p>;
      case 'status': return <p key={node.id} role="status">{node.text}</p>;
      case 'button': return <Button key={node.id} variant="secondary" isDisabled={!view.active || view.handled_actions?.includes(node.action)} onPress={() => void act(view, node.action, null)}>{node.label}</Button>;
      case 'form': {
        const values = drafts[draftKey(view, node)] ?? {};
        const defaults = Object.fromEntries(node.fields.flatMap(field => {
          if (field.initial !== undefined && field.initial !== null) return [[field.id, field.initial]];
          if (field.kind === 'boolean') return [[field.id, false]];
          return [];
        }));
        return <form key={node.id} onSubmit={event => { event.preventDefault(); void act(view, node.action, { ...defaults, ...values }); }}>
          {node.fields.map(field => {
            const value = values[field.id] ?? field.initial;
            if (field.kind === 'text') return <TextField key={field.id} label={field.label} value={String(value ?? '')} isRequired={field.required} isDisabled={!view.active || view.handled_actions?.includes(node.action)} onChange={next => changeField(view, node, field.id, next)} />;
            if (field.kind === 'choice') return <Picker key={field.id} label={field.label} selectedKey={String(value ?? '') || null} isRequired={field.required} isDisabled={!view.active || view.handled_actions?.includes(node.action)} onSelectionChange={next => changeField(view, node, field.id, String(next))}>{field.options.map(option => <Item key={option}>{option}</Item>)}</Picker>;
            if (field.kind === 'multi_choice') return <CheckboxGroup key={field.id} label={field.label} value={Array.isArray(value) ? value as string[] : []} isRequired={field.required} isDisabled={!view.active || view.handled_actions?.includes(node.action)} onChange={next => changeField(view, node, field.id, next)}>{field.options.map(option => <Checkbox key={option} value={option}>{option}</Checkbox>)}</CheckboxGroup>;
            return <Switch key={field.id} isSelected={Boolean(value)} isDisabled={!view.active || view.handled_actions?.includes(node.action)} onChange={next => changeField(view, node, field.id, next)}>{field.label}</Switch>;
          })}
          <Button type="submit" variant="cta" isDisabled={!view.active || view.handled_actions?.includes(node.action)}>{view.handled_actions?.includes(node.action) ? 'Handled' : 'Submit'}</Button>
        </form>;
      }
    }
  };
  const others = frame?.presentation.activity.filter(item => item.attachment !== attachment) ?? [];
  return <Provider theme={defaultTheme} colorScheme="dark"><main>
    <header><h1>Eden live</h1><span>Session {frame?.presentation.session_id ?? 'connecting'}</span></header>
    <div className="status" role="status">{others.map(item => `${item.frontend} is typing in ${item.target.kind}`).join(' · ') || 'Connected to the shared session'}</div>
    <section className="views" aria-label="Live presentation">
      {frame?.presentation.views.map(view => <article key={`${view.owner}/${view.id}`}>
        <div className="view-title"><strong>{view.title}</strong><small>{view.owner} · {view.slot} · revision {view.revision}</small></div>
        {view.platforms.length > 0 && !view.platforms.includes('web') ? <p>{view.fallback} — unavailable on Web</p> : view.nodes.map(node => renderNode(node, view))}
        {!view.active && <p className="settled">Run settled; actions unavailable.</p>}
      </article>)}
      {frame?.presentation.views.length === 0 && <p>Waiting for a live view.</p>}
    </section>
    <footer><TextField label="Message" value={composer} onChange={changeComposer} width="100%" /><div className="controls">
      <Button variant="cta" onPress={() => void send('prompt')}>Send</Button>
      <Button variant="secondary" onPress={() => void send('steering')}>Steer</Button>
      <Button variant="secondary" onPress={() => void send('follow_up')}>Follow up</Button>
      <Button variant="negative" isDisabled={!frame?.state.active_run} onPress={() => void api('/cancel', { run_id: frame?.state.active_run }).then(() => setMessage('Cancellation requested')).catch(error => setMessage(String(error)))}>Cancel run</Button>
    </div><p role="status">{message}</p></footer>
  </main></Provider>;
}
const root = document.getElementById('root');
if (root) createRoot(root).render(<StrictMode><App /></StrictMode>);
