#!/usr/bin/env python3
"""Hermetic GitHub contribution API; Git repositories hold the real branch tips."""
import json, os, pathlib, subprocess, sys, urllib.parse
root = pathlib.Path(os.environ['GH_FAKE_DIR'])
args = sys.argv[1:]
if args[:2] == ['pr', 'checks']:
    gates_file = root / 'gates.json'
    print(json.dumps(json.loads(gates_file.read_text()).get('prChecks', [])) if gates_file.exists() else '[]')
    sys.exit(0)
assert args[0] == 'api', args
endpoint = next(a for a in args if a.startswith('repos/') or a == 'graphql')
method = args[args.index('--method') + 1] if '--method' in args else 'GET'
with (root / 'calls').open('a', newline='\n') as f:
    f.write(method + ' ' + endpoint + '\n')
settings = json.loads((root / 'settings.json').read_text())
source, target = settings['source'], settings['target']
if (root / 'require-credentials').exists():
    expected = 'synthetic-source' if endpoint == f'repos/{source}' or endpoint.startswith(f'repos/{source}/') else 'synthetic-target'
    assert os.environ.get('GH_TOKEN') == expected, 'wrong repository credential'

prfile = root / 'pr.json'
payload = json.loads(sys.stdin.read()) if '--input' in args else {}
def sha(repo, branch):
    return subprocess.check_output(['git', '--git-dir', settings[repo], 'rev-parse', branch], text=True).strip()
def pr():
    p = json.loads(prfile.read_text())
    if p['state'] == 'open':
        p['head']['sha'] = sha('fork', p['head']['ref'])
    override = root / 'override.json'
    if override.exists():
        for key, value in json.loads(override.read_text()).items():
            fields = key.split('.')
            d = p
            for field in fields[:-1]: d = d[field]
            d[fields[-1]] = value
    return p
path, _, query = endpoint.partition('?')
gates = json.loads((root / 'gates.json').read_text()) if (root / 'gates.json').exists() else None
if path == f'repos/{source}' and source != target:
    value = {'id': 2, 'full_name': source, 'fork': True, 'parent': {'id': 1}}
elif path == f'repos/{target}':
    value = {'id': 1, 'full_name': target, 'fork': False}
    if gates and 'push' in gates:
        value['permissions'] = {'push': gates['push']}
elif gates is not None and path == 'graphql':
    value = {'data': {'repository': {'pullRequest': {'reviewDecision': gates.get('reviewDecision')}}}}
elif gates is not None and path.startswith(f'repos/{target}/rules/branches/'):
    value = gates.get('rules', [])
elif gates is not None and path.startswith(f'repos/{target}/branches/'):
    value = {'protection': gates.get('protection', {})}
elif gates is not None and path == f'repos/{target}/actions/runs':
    value = {'workflow_runs': [{'name': name} for name in gates.get('awaiting', [])]}
elif gates is not None and path.endswith('/check-runs'):
    value = {'check_runs': gates.get('checkRuns', [])}
elif gates is not None and path.endswith('/status'):
    value = {'state': 'pending', 'statuses': []}
elif gates is not None and path == f'repos/{target}/pulls/7/commits':
    value = gates.get('commits', [])
elif '/git/ref/heads/' in path:
    repo, branch = path.split('/git/ref/heads/')
    value = {'object': {'sha': sha('fork' if repo == f'repos/{source}' else 'upstream', urllib.parse.unquote(branch))}}
elif path == f'repos/{target}/pulls' and method == 'GET':
    value = [pr()] if prfile.exists() else []
elif path == f'repos/{target}/pulls' and method == 'POST':
    assert payload['head'] == ('knit/contribution' if source == target else source.split('/')[0] + ':knit/contribution')
    assert payload.get('head_repo') == (None if source == target else source.split('/')[1])
    value = {'number': 7, 'html_url': f'https://github.com/{target}/pull/7', 'state': 'open',
             'body': payload['body'], 'draft': payload['draft'],
             'head': {'repo': {'full_name': source}, 'ref': 'knit/contribution', 'sha': sha('fork', 'knit/contribution')},
             'base': {'repo': {'full_name': target}, 'ref': payload['base']}}
    prfile.write_text(json.dumps(value))
    (root / 'create.json').write_text(json.dumps(payload))
elif path == f'repos/{target}/pulls/7':
    value = pr()
    if method == 'PATCH':
        if 'body' in payload: value['body'] = payload['body']
        if 'base' in payload: value['base']['ref'] = payload['base']
        prfile.write_text(json.dumps(value))
else:
    raise AssertionError((method, endpoint, args))
print(value['html_url'] if '--jq' in args else json.dumps(value))
