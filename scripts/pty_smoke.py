#!/usr/bin/env python3
"""Exercise the real terminal binary; no provider credentials or Python packages needed."""
import fcntl, json, os, pathlib, pty, select, signal, sqlite3, struct, subprocess, sys, tempfile, termios, time
binary = str(pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/rocketry').resolve())
out = pathlib.Path('target/pty'); out.mkdir(parents=True, exist_ok=True)
import faulthandler
faulthandler.dump_traceback_later(60, exit=True)
reports = []
for width, height in [(80,24),(120,40),(180,50)]:
    with tempfile.TemporaryDirectory(prefix='rocketry-pty-') as data:
        master, slave = pty.openpty()
        os.set_blocking(master, False)
        before = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH',height,width,0,0))
        def setup():
            os.setsid(); fcntl.ioctl(slave,termios.TIOCSCTTY,0)
        env = dict(os.environ, TERM='xterm-256color', COLORTERM='truecolor')
        wrapper = '"$@"; code=$?; printf "\\nROCKETRY_EXIT:%s\\n" "$code"; sleep 1; exit "$code"'
        proc = subprocess.Popen(['/bin/sh','-c',wrapper,'rocketry-pty-wrapper',binary,'--demo','--backend','host','--data-dir',data],stdin=slave,stdout=slave,stderr=slave,preexec_fn=setup,env=env)
        capture = bytearray()
        def drain(seconds=.1):
            end=time.monotonic()+seconds
            while time.monotonic()<end:
                ready,_,_=select.select([master],[],[],min(.05,max(0,end-time.monotonic())))
                if ready:
                    try: capture.extend(os.read(master,65536))
                    except OSError: break
        def send(data):
            while data:
                try:
                    n=os.write(master,data)
                    data=data[n:]
                except BlockingIOError:
                    drain(.02)
        def statuses():
            db=pathlib.Path(data)/'rocketry.sqlite3'
            if not db.exists():return []
            with sqlite3.connect(f'file:{db}?mode=ro',uri=True) as c:
                return [json.loads(r[0])['status'] for r in c.execute('SELECT data FROM runs ORDER BY rowid')]
        def wait_for(fn,seconds=10):
            end=time.monotonic()+seconds
            while time.monotonic()<end:
                drain(.05)
                if proc.poll() is not None:raise AssertionError(capture[-2000:].decode(errors='replace'))
                if fn():return
            raise AssertionError('timed out: '+repr(statuses()))
        try:
            wait_for(lambda:b'R O C K E T R Y' in capture or b'MISSION CONTROL' in capture)
            send(b'\x0b'); drain(.1); send(b'\x1b'); drain(.1)
            send(b'\x1b[200~'+ 'Inspect 火箭 workspace'.encode()+b'\x1b[201~'); drain(.1);send(b'\r')
            wait_for(lambda:statuses()==['completed'])
            drain(.2)
            fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',height+1,width+1,0,0));os.kill(proc.pid,signal.SIGWINCH);drain(.1)
            send(b'\x06');send(b'workspace\r');drain(.1)
            send(b'\x0e');drain(.1)
            send(b'\x1b[200~'+b'long mission '*200+b'\x1b[201~\r')
            wait_for(lambda:len(statuses())==2)
            drain(.15);send(b'\x03');drain(.1);send(b'\r')
            wait_for(lambda:statuses()[-1]=='cancelled')
            drain(.8)
            send(b'\x11')
            wait_for(lambda:b'ROCKETRY_EXIT:0' in capture,seconds=5)
            after=termios.tcgetattr(slave)
            assert after==before, 'terminal mode was not restored'
            assert b'\x1b[?1049l' in capture, 'alternate screen was not restored'
            proc.wait(timeout=5)
            assert proc.returncode==0
            with sqlite3.connect(pathlib.Path(data)/'rocketry.sqlite3') as c:
                messages=[json.loads(r[0]) for r in c.execute('SELECT data FROM messages')]
            assert messages[0]['text']=='Inspect 火箭 workspace', 'bracketed Unicode paste changed'
            print(f'{width}x{height}: verified', flush=True)
            reports.append({'terminal':f'{width}x{height}','unicode_paste':True,'resize':True,'completed_run':True,'cancelled_run':True,'terminal_restored':True})
        finally:
            if proc.poll() is None:os.killpg(proc.pid,signal.SIGKILL);proc.wait(timeout=5)
            (out/f'{width}x{height}.ansi').write_bytes(capture)
            os.close(master);os.close(slave)
(out/'report.json').write_text(json.dumps(reports,indent=2)+'\n')
print(json.dumps(reports,indent=2))
