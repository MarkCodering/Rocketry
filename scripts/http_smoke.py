#!/usr/bin/env python3
"""Real server + remote CLI smoke test. Uses a disposable local database and token."""
import json, os, pathlib, secrets, signal, socket, subprocess, sys, tempfile, time, urllib.request, urllib.error
binary=str(pathlib.Path(sys.argv[1] if len(sys.argv)>1 else 'target/debug/rocketry').resolve())
with tempfile.TemporaryDirectory(prefix='rocketry-http-') as data:
    with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
    url=f'http://127.0.0.1:{port}'
    env=dict(os.environ,ROCKETRY_TOKEN=secrets.token_hex(24))
    log=open(pathlib.Path(data)/'server.log','w+')
    server=subprocess.Popen([binary,'--demo','--data-dir',data,'serve','--bind',f'127.0.0.1:{port}'],env=env,stdout=log,stderr=log)
    def get(path,auth=True):
        return urllib.request.urlopen(urllib.request.Request(url+path,headers={'Authorization':'Bearer '+env['ROCKETRY_TOKEN']} if auth else {}),timeout=10)
    try:
        for _ in range(100):
            try:get('/health',False).close();break
            except (OSError,urllib.error.URLError):time.sleep(.05)
        else:raise AssertionError('server did not become healthy')
        try:get('/v1/runs',False);raise AssertionError('unauthenticated endpoint accepted')
        except urllib.error.HTTPError as e:assert e.code==401
        result=subprocess.run([binary,'--connect',url,'--agent','demo','run','remote smoke test','--json'],env=env,capture_output=True,text=True,timeout=15)
        assert result.returncode==0,result.stderr
        events=[json.loads(line) for line in result.stdout.splitlines()]
        assert events[-1]['kind']=={'type':'status','data':'completed'}
        run_id=events[0]['run_id'];cursor=events[0]['sequence']
        stream=get(f'/v1/runs/{run_id}/stream?after={cursor}').read().decode()
        ids=[int(line[4:]) for line in stream.splitlines() if line.startswith('id: ')]
        assert ids and all(i>cursor for i in ids)
        assert json.load(get('/v1/openapi.json'))['paths']['/v1/runs']
        assert b'rocketry_active_runs 0' in get('/metrics').read()
        context=json.load(get('/v1/agents/demo/context'))
        assert context['agent']=='demo' and context['context']['total_bytes'] <= context['limit_bytes']
        inspected=subprocess.run([binary,'--connect',url,'--agent','demo','context'],env=env,capture_output=True,text=True,timeout=10)
        assert inspected.returncode==0,inspected.stderr
        assert json.loads(inspected.stdout)['agent']=='demo'

        print(json.dumps({'authentication':True,'remote_cli_run':True,'sse_reconnect':True,'openapi':True,'metrics':True},indent=2))
    finally:
        server.send_signal(signal.SIGINT)
        try:server.wait(timeout=10)
        except subprocess.TimeoutExpired:server.kill();server.wait()
        log.close()
