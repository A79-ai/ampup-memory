"""Turns an `advice_replay.py` run into a review sheet: each delivery with the task in force, what the agent had just
done and what it did next in the real session, so a person (or a judge, once validated against people) can label it
relevant or not, and acted on or not. Prints the run's numbers; `--sheet` writes the sheet as Markdown.

    python3 harness/evals/advice_review.py runs/advice.jsonl --sheet runs/advice.md
"""
import argparse, json, os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from advice_replay import events  # noqa: E402

def kinds(text):
    """What a delivery carried, by the headers the hooks give each part."""
    k = []
    if 'The step you are at in this procedure' in text:
        k.append('step')
    if re.search(r'^Skill: ', text, re.M):
        k.append('skill')
    if 'Advice from the team memory' in text and not k:
        k.append('advice')
    if 'What this user told you before' in text:
        k.append('remembered')
    if 'From the team memory (AmpUp Memory), facts' in text or 'From the team memory (memstore), facts' in text or 'that may bear on this' in text:
        k.append('facts')
    if text.startswith(('AmpUp Memory session:', 'memstore session:')) and len(text) < 300:
        k.append('session-line')
    return k or ['other']


def action(ev):
    if ev['kind'] == 'prompt':
        return 'USER: ' + ev['prompt'][:200].replace('\n', ' ')
    if ev['kind'] in ('pre-tool', 'post-tool'):
        i = ev['input']
        s = i.get('command') or i.get('file_path') or i.get('pattern') or i.get('description') or json.dumps(i)
        return f"{ev['tool']}: {str(s)[:160]}".replace('\n', ' ')
    return ev['kind']


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('run')
    ap.add_argument('--sheet')
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.run)]
    deliveries = [r for r in rows if 'delivered' in r]
    sums = [r for r in rows if 'summary' in r]
    cache = {}
    sheet = []
    totals = {}
    for r in deliveries:
        ks = kinds(r['delivered'])
        for k in ks:
            totals[k] = totals.get(k, 0) + 1
        if r['session'] not in cache:
            cache[r['session']] = events(open(r['session']).read().splitlines())[1]
        evs = cache[r['session']]
        n = r['n']
        task = next((e['prompt'] for e in reversed(evs[:n + 1]) if e['kind'] == 'prompt'), '')
        before = [action(e) for e in evs[max(0, n - 4):n + 1] if e['kind'] != 'pre-tool']
        after = [action(e) for e in evs[n + 1:n + 30] if e['kind'] != 'post-tool'][:8]
        sheet.append(f"## {os.path.basename(r['session'])[:8]} · event {n} ({r['event']}"
                     f"{', step end' if r['step_end'] else ''}) · {', '.join(ks)} · {len(r['delivered'])} chars\n\n"
                     f"**Task in force:** {task[:300]}\n\n" +
                     (f"**Most likely asked by:** event {r['asked']['n']} ({r['asked']['event']}): "
                      f"{r['asked']['what'][:200]}\n\n" if r.get('asked') else '') +
                     "**Just before:**\n" + ''.join(f'- {b}\n' for b in before) +
                     f"\n**Delivered:**\n```\n{r['delivered'][:2500]}\n```\n\n**Next in the real session:**\n" +
                     ''.join(f'- {x}\n' for x in after) + '\n**Label:** relevant [ ] · acted on [ ] · harmful/noise [ ]\n')
    print(f'{len(sums)} sessions, {sum(s["prompts"] for s in sums)} prompts, {sum(s["events"] for s in sums)} events')
    print('hooks', sum(s['summary']['hooks'] for s in sums), '· step ends', sum(s['summary']['step_ends'] for s in sums),
          '· deliveries', len(deliveries), '· chars injected', sum(len(r['delivered']) for r in deliveries))
    print('by kind:', totals)
    for s in sums:
        c = s['summary']
        print(f"  {os.path.basename(s['session'])[:8]}: {s['prompts']} prompts, {c['step_ends']} step ends, "
              f"{c['delivered']} deliveries, {c['chars']} chars; hook log: {s['hook_log'][-120:].strip()!r}")
    if a.sheet:
        with open(a.sheet, 'w') as f:
            f.write('# Advice replay review\n\n' + '\n'.join(sheet))
        print('sheet:', a.sheet)


if __name__ == '__main__':
    main()
