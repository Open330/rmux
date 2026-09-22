#!/usr/bin/env python3
"""Check that prefix+s popups suppress background repaint and restore fresh output.

Uses a disposable server/socket; never touches existing rmux sessions.
Build rmux and rmux-daemon first. Optional --muxa exercises the actual watch
launcher; without it a static popup reproduces the bug independently of muxa.
"""
import argparse
import fcntl
import os
from pathlib import Path
import pty
import select
import shlex
import struct
import subprocess
import tempfile
import termios
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--rmux', type=Path, default=Path(__file__).resolve().parents[1] / 'target/debug/rmux')
parser.add_argument('--muxa', type=Path)
parser.add_argument('--split', action='store_true')
parser.add_argument('--burst', action='store_true', help='stress background output batching')
args = parser.parse_args()
binary = str(args.rmux.resolve())

with tempfile.TemporaryDirectory(prefix='rmux-popup-background-') as tmp:
    root = Path(tmp)
    env = dict(os.environ, TERM='xterm-256color')
    for key in ('RMUX', 'RMUX_PANE', 'TMUX', 'TMUX_PANE'):
        env.pop(key, None)
    env['PATH'] = str(Path(binary).parent) + os.pathsep + env['PATH']
    base = [binary, '-S', str(root / 'socket')]
    stop = root / 'stop'
    (root / 'config.toml').write_text('')
    producer = root / 'producer.py'
    producer.write_text('''import os,sys,time
from pathlib import Path
while not Path(sys.argv[1]).exists():
 os.write(1,b'BACKGROUND_SENTINEL\\r\\n' * int(sys.argv[2]))
 time.sleep(.05)
os.write(1,b'BACKGROUND_FINAL\\r\\n')
time.sleep(60)
''')
    background = shlex.join(['/usr/bin/python3', str(producer), str(stop), '4096' if args.burst else '1'])

    def run(*words):
        return subprocess.run(base + list(words), env=env, text=True,
                              capture_output=True, timeout=10, check=True).stdout.strip()

    pid = fd = None
    try:
        run('-f', '/dev/null', 'new-session', '-d', '-s', 'check',
            '/bin/sh' if args.split else background)
        if args.split:
            run('split-window', '-d', '-t', 'check', background)
        if args.muxa:
            popup_command = shlex.join([
                'env', 'MUXA_SOCKET=' + str(root / 'muxa.sock'),
                'MUXA_CONFIG=' + str(root / 'config.toml'),
                str(args.muxa.resolve()), 'watch', '--popup',
                '--caller-client', '#{client_name}', '--caller-pane', '#{pane_id}',
            ])
            run('bind-key', 's', 'run-shell', '-b', popup_command)
            expected = b'muxa watch'
        else:
            run('bind-key', 's', 'display-popup', '-B', '-E', '-w', '100%',
                '-h', '99%', '-x', '0', '-y', '0',
                'printf POPUP_SURFACE; sleep 60')
            expected = b'POPUP_SURFACE'
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe(binary, base + ['attach', '-t', 'check'], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))

        def collect(seconds):
            chunks = []
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([fd], [], [], .03)[0]:
                    chunks.append(os.read(fd, 65536))
            return b''.join(chunks)

        collect(1)
        os.write(fd, b'\x02s')
        assert expected in collect(3), 'prefix+s must open the popup'
        sample = collect(3)
        leaks = sample.count(b'BACKGROUND_SENTINEL')
        print(f'popup bytes={len(sample)} background leaks={leaks} split={args.split}', flush=True)
        assert leaks == 0, 'background pane was repainted underneath the popup'
        assert b'\x1b[2J' not in sample, 'idle popup must not repeatedly clear the screen'
        # Stop output before dismissal: no subsequent pane tick may repair a
        # stale base frame and accidentally make the test pass.
        stop.touch()
        final_while_open = collect(.5)
        assert b'\x1b[2J' not in final_while_open
        if not args.muxa:
            assert b'BACKGROUND_FINAL' not in final_while_open
        # watch may legitimately show the final line inside its pane preview.
        if args.muxa:
            os.write(fd, b'q')
        else:
            client = run('list-clients', '-F', '#{client_name}').splitlines()[0]
            run('display-popup', '-C', '-c', client)
        restored = collect(1)
        assert b'BACKGROUND_FINAL' in restored, 'dismissal must restore the latest transcript'
        print('PASS: prefix+s, hidden background output, fresh dismissal', flush=True)
    finally:
        subprocess.run(base + ['kill-server'], env=env, capture_output=True, timeout=10)
        if pid:
            os.close(fd)
            os.waitpid(pid, 0)
