"""Replays real Claude Code sessions through the memstore hooks and records what each hook would have put in the
agent's context: the advice a session gets, when it gets it, and how long the server took.

A session's transcript is fed to `memstore-client hook <event>` one event at a time, as Claude Code would have fired
them (start, each prompt, each tool call before and after, each turn's end, the end), with the transcript file
growing to what the agent had seen at that moment. Gaps between events keep their real length up to `--max-gap`
seconds, so a call the hook ran on in a copy of itself has the time it had in the session to land; the hooks'
ask-again marker is aged by the whole real gap, so a step's end asks as often as it did live.

Nothing is remembered: MEMSTORE_CAPTURE=off, which the hooks send as `keep: false` so the server keeps nothing the
replayed prompts say either (a server older than that keeps them as the signed-in user's notes: don't replay against
one). The session id is new. Use the server and org the hook client is signed in to (`memstore-client doctor`).

    python3 harness/evals/advice_replay.py list ~/.claude/projects/-Users-me-myrepo/*.jsonl
    python3 harness/evals/advice_replay.py run --out runs/advice.jsonl SESSION.jsonl ...

`run` writes one JSON line per hook that delivered something, and a line per session with its counts.
"""
import argparse, datetime, json, os, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
CLIENT = os.path.join(HERE, '..', '..', 'target', 'debug', 'memstore-client')
STEP_ENDS = ['git commit', 'git push', 'gh pr create', 'cargo test', 'cargo build', 'pytest', 'npm test',
             'npm run build', 'pnpm test', 'go test', 'make test']


def when(e):
    t = e.get('timestamp')
    return datetime.datetime.fromisoformat(t.replace('Z', '+00:00')).timestamp() if t else None


# User entries Claude Code writes itself, which fire no UserPromptSubmit: a background task's notice, a deferred
# tool's load note, an interruption.
NOT_TYPED = ('<local-command', '<command-name>', '<task-notification>', 'Tool loaded.', '[Request interrupted')


def typed(e):
    """What the person typed in a user entry, or None: not a tool result, a meta entry, a local command or what
    the harness wrote in the person's turn."""
    if e.get('type') != 'user' or e.get('isMeta') or e.get('isCompactSummary'):
        return None
    c = e.get('message', {}).get('content')
    if isinstance(c, list):
        c = '\n'.join(b.get('text', '') for b in c if b.get('type') == 'text')
    if not isinstance(c, str) or not c.strip() or c.lstrip().startswith(NOT_TYPED):
        return None
    return c


def events(lines):
    """The hook events of a transcript, each with the number of its lines the agent had seen when it fired."""
    entries = [json.loads(l) for l in lines]
    calls, out, turn_open = {}, [], False
    for i, e in enumerate(entries):
        t = when(e)
        p = typed(e)
        if p is not None:
            if turn_open:
                out.append(dict(kind='stop', upto=i, at=t))
            out.append(dict(kind='prompt', upto=i + 1, at=t, prompt=p))
            turn_open = True
            continue
        content = e.get('message', {}).get('content')
        if not isinstance(content, list):
            continue
        for b in content:
            if e.get('type') == 'assistant' and b.get('type') == 'tool_use':
                calls[b['id']] = b
                out.append(dict(kind='pre-tool', upto=i + 1, at=t, tool=b['name'], input=b.get('input', {})))
            elif e.get('type') == 'user' and b.get('type') == 'tool_result' and b.get('tool_use_id') in calls:
                use = calls.pop(b['tool_use_id'])
                res = e.get('toolUseResult')
                if not isinstance(res, dict):
                    text = b.get('content')
                    if isinstance(text, list):
                        text = '\n'.join(x.get('text', '') for x in text if isinstance(x, dict))
                    res = {'stdout': text or ''}
                if b.get('is_error'):
                    res = dict(res, is_error=True)
                out.append(dict(kind='post-tool', upto=i + 1, at=t, tool=use['name'], input=use.get('input', {}),
                                response=res))
    if turn_open:
        out.append(dict(kind='stop', upto=len(entries), at=when(entries[-1]) if entries else None))
    return entries, out


def ends_step(ev):
    if ev['kind'] != 'post-tool' or ev['response'].get('is_error'):
        return False
    args = json.dumps(ev['input']).lower()
    if ev['tool'] in ('TodoWrite', 'TaskUpdate'):
        return 'completed' in args
    return ev['tool'] == 'Bash' and any(c in args for c in STEP_ENDS)


def list_sessions(paths):
    for p in paths:
        try:
            lines = open(p).read().splitlines()
            entries, evs = events(lines)
        except Exception as e:
            print(f'{p}: unreadable ({e})', file=sys.stderr)
            continue
        prompts = [e for e in evs if e['kind'] == 'prompt']
        if not prompts:
            continue
        ts = [e['at'] for e in evs if e['at']]
        cwd = next((e.get('cwd') for e in entries if e.get('cwd')), '')
        print(json.dumps({'path': p, 'cwd': cwd, 'prompts': len(prompts),
                          'tools': sum(e['kind'] == 'post-tool' for e in evs),
                          'step_ends': sum(ends_step(e) for e in evs),
                          'minutes': round((max(ts) - min(ts)) / 60) if ts else 0,
                          'first': prompts[0]['prompt'][:160]}))


def running(tmp):
    """Whether a copy of the hook this replay started is still at work (its TMPDIR is ours alone)."""
    p = subprocess.run(['pgrep', '-f', CLIENT], capture_output=True, text=True)
    for pid in p.stdout.split():
        e = subprocess.run(['ps', 'eww', '-o', 'command=', '-p', pid], capture_output=True, text=True).stdout
        if f'TMPDIR={tmp}' in e:
            return True
    return False


def replay(path, out, max_gap):
    lines = open(path).read().splitlines()
    entries, evs = events(lines)
    cwd = next((e.get('cwd') for e in entries if e.get('cwd')), os.getcwd())
    tmp = tempfile.mkdtemp(prefix='advice-replay-')
    env = dict(os.environ, TMPDIR=tmp, MEMSTORE_CAPTURE='off')
    env.pop('MEMSTORE_HOOK_DETACHED', None)
    sid = f'replay-{os.path.basename(path)[:8]}-{int(time.time())}'
    tp = os.path.join(tmp, 'transcript.jsonl')
    marks = os.path.join(tmp, 'memstore-hooks')
    open(tp, 'w').close()
    written = 0
    counts = dict(hooks=0, delivered=0, chars=0, step_ends=0, slow=0)

    def hook(event, extra):
        nonlocal written
        upto = extra.pop('_upto', written)
        if upto > written:
            with open(tp, 'a') as f:
                f.write('\n'.join(lines[written:upto]) + '\n')
            written = upto
        payload = dict({'session_id': sid, 'transcript_path': tp, 'cwd': cwd}, **extra)
        t = time.time()
        p = subprocess.run([CLIENT, 'hook', event], input=json.dumps(payload), text=True, env=env,
                           capture_output=True, timeout=60)
        took = time.time() - t
        try:
            ctx = json.loads(p.stdout)['hookSpecificOutput'].get('additionalContext', '')
        except Exception:
            ctx = p.stdout.strip()
        counts['hooks'] += 1
        counts['slow'] += took > 1
        return ctx, took

    def settle(gap):
        """Let the hook's copies work for as long as the session gave them, and age the ask-again marker by it."""
        deadline = time.time() + min(gap, max_gap)
        while time.time() < deadline and running(tmp):
            time.sleep(0.3)
        if os.path.isdir(marks):
            for f in os.listdir(marks):
                if f.startswith('decided-'):
                    m = os.path.join(marks, f)
                    t = os.path.getmtime(m) - gap
                    os.utime(m, (t, t))

    hook('session-start', {'source': 'startup', 'hook_event_name': 'SessionStart'})
    for _ in range(100):
        if os.path.isdir(marks) and any(f.startswith('session-') for f in os.listdir(marks)):
            break
        time.sleep(0.2)
    prev_at = evs[0]['at'] if evs else None
    prompt_no = 0
    # The last hook that asked the memory (a prompt, a step's end, a turn's end): what a delivery most likely
    # answers, since a hook hands on what an earlier one's copy fetched.
    asked = None
    for n, ev in enumerate(evs):
        gap = max(0.0, (ev['at'] or prev_at or 0) - (prev_at or ev['at'] or 0))
        prev_at = ev['at'] or prev_at
        settle(gap)
        extra = {'_upto': ev['upto']}
        if ev['kind'] == 'prompt':
            prompt_no += 1
            extra.update(prompt=ev['prompt'], hook_event_name='UserPromptSubmit')
        elif ev['kind'] == 'stop':
            extra.update(hook_event_name='Stop', stop_hook_active=False)
        else:
            extra.update(tool_name=ev['tool'], tool_input=ev['input'],
                         hook_event_name='PreToolUse' if ev['kind'] == 'pre-tool' else 'PostToolUse')
            if ev['kind'] == 'post-tool':
                extra['tool_response'] = ev['response']
        counts['step_ends'] += ends_step(ev)
        ctx, took = hook(ev['kind'], extra)
        what = ev.get('prompt') or json.dumps(ev.get('input', ''))
        if ctx:
            counts['delivered'] += 1
            counts['chars'] += len(ctx)
            out.write(json.dumps({'session': path, 'replay_id': sid, 'n': n, 'prompt_no': prompt_no,
                                  'event': ev['kind'], 'tool': ev.get('tool'), 'what': what[:400],
                                  'step_end': ends_step(ev), 'took': round(took, 2), 'asked': asked,
                                  'delivered': ctx}) + '\n')
            out.flush()
        if ev['kind'] in ('prompt', 'stop') or ends_step(ev):
            asked = {'n': n, 'event': ev['kind'], 'what': what[:400]}
    settle(5)
    hook('end', {'_upto': len(lines), 'hook_event_name': 'SessionEnd', 'reason': 'other'})
    log = os.path.join(marks, 'log-session-end')
    errors = open(log).read()[-500:] if os.path.exists(log) else ''
    out.write(json.dumps({'session': path, 'replay_id': sid, 'summary': counts, 'prompts': prompt_no,
                          'events': len(evs), 'hook_log': errors}) + '\n')
    out.flush()
    return counts


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest='cmd', required=True)
    sub.add_parser('list').add_argument('paths', nargs='+')
    r = sub.add_parser('run')
    r.add_argument('--out', required=True)
    r.add_argument('--max-gap', type=float, default=20.0)
    r.add_argument('paths', nargs='+')
    a = ap.parse_args()
    if a.cmd == 'list':
        return list_sessions(a.paths)
    with open(a.out, 'a') as out:
        for p in a.paths:
            print(p, replay(p, out, a.max_gap), flush=True)


if __name__ == '__main__':
    main()
