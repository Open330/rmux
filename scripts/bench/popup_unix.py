#!/usr/bin/env python3
"""Replay one recorded muxa watch output against two isolated rmux builds.

Linux only (/proc CPU accounting). Each rmux needs its matching sibling
rmux-daemon. Never sources user config or connects to a user's rmux server.
The optional status bar uses muxa's two real status-line commands; CPU samples
cover the daemon and attach client, not those subprocesses or the replay child.
The captured terminal contents live only in a temporary directory.
"""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import platform
import select
import shlex
import struct
import subprocess
import tempfile
import termios
import time


def spawn(argv, env, rows=40):
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(argv[0], argv, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, 140, 0, 0))
    return pid, fd


def collect(fd, seconds):
    end = time.monotonic() + seconds
    chunks = []
    while time.monotonic() < end:
        if select.select([fd], [], [], min(.05, max(0, end-time.monotonic())))[0]:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            chunks.append(chunk)
    return b''.join(chunks)


def ticks(pid):
    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    return int(fields[11]) + int(fields[12])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True, type=Path)
    parser.add_argument('--candidate', required=True, type=Path)
    parser.add_argument('--muxa', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--repeats', type=int, default=2)
    args = parser.parse_args()
    env = dict(os.environ, TERM='xterm-256color')
    for key in ('RMUX', 'RMUX_PANE', 'TMUX', 'TMUX_PANE', 'TERM_PROGRAM', 'TERM_PROGRAM_VERSION'):
        env.pop(key, None)
    samples = []
    metadata = dict(platform=platform.platform(), clock_ticks=os.sysconf('SC_CLK_TCK'),
                    baseline_sha256=hashlib.sha256(args.baseline.read_bytes()).hexdigest(),
                    candidate_sha256=hashlib.sha256(args.candidate.read_bytes()).hexdigest(),
                    baseline_daemon_sha256=hashlib.sha256(args.baseline.with_name('rmux-daemon').read_bytes()).hexdigest(),
                    candidate_daemon_sha256=hashlib.sha256(args.candidate.with_name('rmux-daemon').read_bytes()).hexdigest(),
                    muxa_sha256=hashlib.sha256(args.muxa.read_bytes()).hexdigest())
    with tempfile.TemporaryDirectory(prefix='rmux-popup-bench-') as tmp:
        socket = str(Path(tmp) / 'socket')
        binary = str(args.baseline.resolve())
        def run(*argv):
            return subprocess.run([binary, '-S', socket, *argv], env=env,
                                  text=True, capture_output=True, check=True, timeout=15).stdout.strip()
        events = []
        run('-f', '/dev/null', 'new-session', '-d', '-s', 'bench', '/bin/sh')
        record_pid = record_fd = None
        try:
            record_env = dict(env, RMUX=socket+',0,0', RMUX_PANE='%0')
            record_pid, record_fd = spawn([str(args.muxa.resolve()), 'watch'], record_env, rows=39)
            start = time.monotonic()
            while time.monotonic() - start < 11:
                if select.select([record_fd], [], [], .05)[0]:
                    data = os.read(record_fd, 65536)
                    events.append((time.monotonic()-start, base64.b64encode(data).decode()))
            os.write(record_fd, b'q')
            collect(record_fd, .2)
        finally:
            run('kill-server')
            if record_pid:
                os.close(record_fd)
                os.waitpid(record_pid, 0)
        assert any(b'muxa watch' in base64.b64decode(data) for _, data in events), 'watch did not render'
        recording = Path(tmp) / 'recording.json'
        recording.write_text(json.dumps(events))
        replay = Path(tmp) / 'replay.py'
        replay.write_text('''import base64,json,os,sys,time
start=time.monotonic()
for at,data in json.load(open(sys.argv[1])):
 time.sleep(max(0,at-(time.monotonic()-start)))
 data=base64.b64decode(data)
 while data:
  written=os.write(1,data);data=data[written:]
time.sleep(3)
''')
        for repeat in range(args.repeats):
            variants = [('baseline', args.baseline), ('candidate', args.candidate)]
            if repeat % 2:
                variants.reverse()
            for status in ('off', 'on'):
                for label, executable in variants:
                    binary = str(executable.resolve())
                    run('-f', '/dev/null', 'new-session', '-d', '-s', 'bench', '/bin/sh')
                    pid = fd = None
                    try:
                        run('set-option', '-g', 'status', status)
                        run('set-option', '-g', 'status-interval', '2')
                        muxa = shlex.quote(str(args.muxa.resolve()))
                        run('set-option', '-g', 'status-right-length', '140')
                        run('set-option', '-g', 'status-right',
                            f'#({muxa} status-line --needs-attention) #({muxa} status-line --pane #{{pane_id}}) | %H:%M')
                        pid, fd = spawn([binary, '-S', socket, 'attach', '-t', 'bench'], env)
                        for _ in range(50):
                            client = run('list-clients', '-F', '#{client_name}')
                            if client:
                                break
                            collect(fd, .1)
                        assert client, 'attach failed'
                        collect(fd, .3)
                        daemon = int(run('display-message', '-p', '#{pid}'))
                        command = shlex.join(['/usr/bin/python3', str(replay), str(recording)])
                        run('display-popup', '-c', client, '-B', '-E', '-w', '100%', '-h', '99%', '-x', '0', '-y', '0', command)
                        collect(fd, 2)
                        before = (ticks(daemon), ticks(pid))
                        start = time.monotonic()
                        data = collect(fd, 6)
                        elapsed = time.monotonic()-start
                        after = (ticks(daemon), ticks(pid))
                        cpu = [(b-a)/os.sysconf('SC_CLK_TCK')/elapsed*100 for a,b in zip(before,after)]
                        sample = dict(build=label, status=status, repeat=repeat, seconds=elapsed,
                                      bytes=len(data), row_writes=data.count(b'\x1b7'),
                                      sync_frames=data.count(b'\x1b[?2026h'),
                                      daemon_cpu=cpu[0], client_cpu=cpu[1])
                        samples.append(sample)
                        print(json.dumps(sample), flush=True)
                    finally:
                        run('kill-server')
                        if pid:
                            os.close(fd)
                            os.waitpid(pid, 0)
        args.output.write_text(json.dumps(dict(metadata=metadata, recorded_chunks=len(events), samples=samples), indent=2)+'\n')


if __name__ == '__main__':
    main()
