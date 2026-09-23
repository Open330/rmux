#!/usr/bin/env python3
"""Does a client repaint after being switched from inside a popup?

`muxa watch` opens in a popup and its Enter closes the popup and switches the
client to another session, so the switch lands while the overlay is still
registered. The failure this checks for is the client changing session — input
goes to the new one — while the screen keeps the popup's last frame.

Usage: python3 scripts/rmux-popup-switch-check.py [rmux-binary]

Passes when the new session's marker reaches the client's tty. An isolated
server per run; no live session is touched.
"""
import fcntl, os, pty, select, shutil, struct, subprocess, sys, tempfile, termios, time

binary = sys.argv[1] if len(sys.argv) > 1 else shutil.which('rmux')

with tempfile.TemporaryDirectory(prefix="rmux-popsw-") as tmp:
    env = {k: v for k, v in os.environ.items()
           if k not in ("RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE", "TMUX_PROGRAM")}
    env["TERM"] = "xterm-256color"
    sock = tmp + "/rmux.sock"
    base = [binary, "-S", sock]

    def run(*args):
        return subprocess.run(base + list(args), env=env, capture_output=True,
                              text=True, timeout=20).stdout.strip()

    def collect(fd, seconds):
        data = b""
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([fd], [], [], .05)[0]:
                try:
                    data += os.read(fd, 65536)
                except OSError:
                    break
        return data

    run("-f", "/dev/null", "new-session", "-d", "-s", "A", "exec /bin/sh")
    run("new-session", "-d", "-s", "B", "exec /bin/sh")
    run("send-keys", "-t", "B:0.0", 'printf "MARKER_BBB\\n"', "Enter")
    run("set-option", "-ga", "terminal-features", ",xterm*:sync")
    time.sleep(0.5)

    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(binary, base + ["attach", "-t", "A"], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    collect(fd, 1.5)
    client = run("list-clients", "-F", "#{client_name}").splitlines()[0]

    try:
        # The popup paints, then its command switches the client and exits —
        # the overlay is still on the client at the moment of the switch.
        popup = (f"{binary} -S {sock} display-popup -E "
                 f"\"sh -c 'printf POPUP_CONTENT; sleep 0.6; "
                 f"{binary} -S {sock} switch-client -c {client} -t B'\"")
        subprocess.Popen(popup, shell=True, env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        painted = collect(fd, 4.0)
        session = [l.split()[1] for l in
                   run("list-clients", "-F", "#{client_name} #{client_session}").splitlines()
                   if l.split()[0] == client]
        print(f"binary={binary}")
        print(f"  client session after the popup switched it: {session}")
        print(f"  bytes painted: {len(painted)}")
        print(f"  screen shows B's marker: {b'MARKER_BBB' in painted}")
        print(f"  screen still shows the popup: {b'POPUP_CONTENT' in painted}")
    finally:
        subprocess.run(base + ["kill-server"], env=env, capture_output=True, timeout=10)
        os.close(fd)
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass

        if b"MARKER_BBB" not in painted:
            sys.exit("FAIL: the session the client switched to was never painted")
        print("PASS")
