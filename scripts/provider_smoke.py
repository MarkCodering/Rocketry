#!/usr/bin/env python3
"""Real CLI/TUI against local protocol fixtures. Never uses live provider keys."""
import fcntl, http.server, json, os, pathlib, pty, select, signal, sqlite3, struct, subprocess, sys, tempfile, termios, threading, time
binary = str(pathlib.Path(sys.argv[1] if len(sys.argv)>1 else 'target/debug/rocketry').resolve())
requests=[]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def do_POST(self):
        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append((self.path,body,dict(self.headers)))
        if self.path.endswith('/responses'):
            frames=[{'type':'response.output_text.delta','delta':'fixture response'}, {'type':'response.completed','response':{'output':[{'type':'message','role':'assistant','content':[{'type':'output_text','text':'fixture response'}]}],'usage':{'input_tokens':4,'output_tokens':2}}}]
        elif self.path.endswith('/messages'):
            frames=[{'type':'message_start','message':{'usage':{'input_tokens':4}}}, {'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}}, {'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'fixture response'}}, {'type':'content_block_stop','index':0}, {'type':'message_delta','delta':{'stop_reason':'end_turn'},'usage':{'output_tokens':2}}, {'type':'message_stop'}]
        else:
            frames=[{'choices':[{'delta':{'content':'fixture response'},'finish_reason':'stop'}],'usage':{'prompt_tokens':4,'completion_tokens':2}}]
        payload=''.join('data: '+json.dumps(frame)+'\n\n' for frame in frames).encode()
        self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(payload)));self.end_headers();self.wfile.write(payload)
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
threading.Thread(target=server.serve_forever,daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix='rocketry-provider-smoke-') as data:
        env=dict(os.environ,TERM='xterm-256color')
        for name in ['OPENAI','ANTHROPIC','OLLAMA']:
            env[name+'_API_KEY']='fixture-secret-'+name
            env[name+'_BASE_URL']=f'http://127.0.0.1:{server.server_port}/v1'
            env[name+'_MODEL']='profile-default'
        base=[binary,'--config',str(pathlib.Path(data)/'absent.toml'),'--data-dir',data]
        for provider,suffix in [('openai','responses'),('anthropic','messages'),('ollama','chat/completions')]:
            result=subprocess.run(base+['--agent',provider,'--model','chosen-'+provider,'run','fixture test','--json'],env=env,capture_output=True,text=True,timeout=15)
            assert result.returncode==0,result.stderr
            path,body,headers=requests[-1]
            assert path=='/v1/'+suffix and body['model']=='chosen-'+provider,(path,body)
            credential=headers.get('x-api-key') if provider=='anthropic' else headers.get('authorization') or headers.get('Authorization')
            assert credential and 'fixture-secret-'+provider.upper() in credential
            assert 'fixture-secret-' not in result.stdout+result.stderr
        # Drive actual TUI model selection and dispatch through the same fixture.
        master,slave=pty.openpty();os.set_blocking(master,False)
        before=termios.tcgetattr(slave)
        fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',32,110,0,0))
        def setup(): os.setsid();fcntl.ioctl(slave,termios.TIOCSCTTY,0)
        proc=subprocess.Popen(base+['--agent','openai'],stdin=slave,stdout=slave,stderr=slave,env=env,preexec_fn=setup)
        capture=bytearray()
        def drain(duration=.1):
            end=time.monotonic()+duration
            while time.monotonic()<end:
                ready,_,_=select.select([master],[],[],.03)
                if ready:
                    try:capture.extend(os.read(master,65536))
                    except OSError:break
        def wait_for(check,seconds=10):
            end=time.monotonic()+seconds
            while time.monotonic()<end:
                drain()
                if check():return
                assert proc.poll() is None,capture[-2000:].decode(errors='replace')
            raise AssertionError('TUI condition timed out')
        try:
            wait_for(lambda:b'R O C K E T R Y' in capture)
            os.write(master,b'/model tui-selected\r');drain(.3)
            assert b'tui-selected' in capture
            os.write(master,b'TUI fixture test\r')
            wait_for(lambda:len(requests)==4)
            assert requests[-1][1]['model']=='tui-selected'
            def completed():
                with sqlite3.connect(pathlib.Path(data)/'rocketry.sqlite3') as db:
                    rows=[json.loads(r[0]) for r in db.execute('SELECT data FROM runs ORDER BY rowid')]
                return len(rows)==4 and rows[-1]['status']=='completed' and rows[-1]['model']=='tui-selected'
            wait_for(completed)
            drain(.3);os.write(master,b'/quit\r');drain(.3)
            proc.wait(timeout=5)
            assert proc.returncode==0
            assert termios.tcgetattr(slave)==before
            assert b'fixture-secret-' not in capture
        finally:
            pathlib.Path('target').mkdir(exist_ok=True)
            pathlib.Path('target/provider-tui.ansi').write_bytes(capture)
            if proc.poll() is None:os.killpg(proc.pid,signal.SIGKILL);proc.wait(timeout=5)
            os.close(master);os.close(slave)
        # Long-term memory survives separate CLI processes and is operator-manageable.
        for args in [['memory','put','style','concise evidence'],['memory','search','concise'],['memory','forget','style'],['memory','list']]:
            result=subprocess.run(base+args,env=env,capture_output=True,text=True,timeout=10)
            assert result.returncode==0,result.stderr
            value=json.loads(result.stdout)
            if args[1]=='search': assert value[0]['value']=='concise evidence'
            if args[1]=='list': assert value==[]
        print(json.dumps({'environment_profiles':3,'authenticated_fixture_requests':4,'cli_model_override':True,'tui_model_override':True,'model_persisted':True,'secrets_hidden':True,'memory_cli_lifecycle':True,'terminal_restored':True},indent=2))
finally:
    server.shutdown();server.server_close()
