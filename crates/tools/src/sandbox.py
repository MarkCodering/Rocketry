# Executed inside the configured Docker image; never used on the host.
import json, os, pathlib, sys
op, args = sys.argv[1], json.loads(sys.argv[2])
root = pathlib.Path('/workspace').resolve()
raw = pathlib.Path(args.get('path', '.'))
if raw.is_absolute() or '..' in raw.parts:
    raise ValueError('path must be workspace-relative without parent traversal')
p = (root / raw).resolve()
if not p.is_relative_to(root):
    raise ValueError('path escapes workspace')
limit = 2 * 1024 * 1024
if op == 'read_file':
    with p.open('rb') as f: result = {'text': f.read(limit).decode('utf-8', errors='replace'), 'limit_bytes': limit, 'truncated': p.stat().st_size > limit}
elif op == 'write_file':
    assert len(args['content'].encode()) <= limit
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(args['content'])
    result = {'written': len(args['content'].encode()), 'path': str(raw)}
elif op == 'patch_file':
    assert p.stat().st_size <= limit
    original = p.read_text()
    assert args['old'] and original.count(args['old']) == 1, 'patch requires exactly one match'
    patched = original.replace(args['old'], args['new'], 1)
    assert len(patched.encode()) <= limit
    p.write_text(patched)
    result = {'patched': True, 'path': str(raw)}
elif op == 'list_dir':
    result = [{'name': x.name, 'directory': x.is_dir()} for x in sorted(p.iterdir())[:1000]]
elif op == 'create_dir':
    p.mkdir(parents=True, exist_ok=True)
    result = {'created': True, 'path': str(raw)}
elif op == 'move_file':
    destination = pathlib.Path(args['destination'])
    assert not destination.is_absolute() and '..' not in destination.parts
    target = (root / destination).resolve()
    assert target.is_relative_to(root) and p.is_file()
    os.link(p, target)
    p.unlink()
    result = {'moved': True, 'destination': str(destination)}
elif op == 'remove_file':
    assert p.is_file(), 'remove_file only removes files'
    p.unlink()
    result = {'removed': True, 'path': str(raw)}
elif op == 'search':
    result = {'matches': [], 'visited': 0}
    stack = [p]
    while stack and result['visited'] < 10000 and len(result['matches']) < 200:
        x = stack.pop(); result['visited'] += 1
        if x.is_symlink(): continue
        if x.is_dir():
            for y in x.iterdir():
                if result['visited'] + len(stack) >= 10000: break
                if y.name not in ('.git', 'target', 'node_modules'): stack.append(y)
        elif x.stat().st_size <= limit:
            try:
                for i, line in enumerate(x.read_text().splitlines()):
                    if args['query'] in line:
                        result['matches'].append({'path': str(x.relative_to(root)), 'line': i + 1, 'text': line[:500]})
                        if len(result['matches']) >= 200: break
            except UnicodeError: pass
else:
    raise ValueError('unknown operation')
print(json.dumps(result))
