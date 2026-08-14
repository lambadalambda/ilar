#!/usr/bin/env python3
"""Drive the ilar TUI through a pty with a real window size."""
import fcntl, os, pty, select, struct, sys, termios, time

def run(cmd, script, cols=100, rows=30, settle=0.5, idle_done=8.0):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.execvp(cmd[0], cmd)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    out = bytearray()
    start = time.time()
    last_data = time.time()
    script_iter = iter(script)
    next_send = time.time() + settle

    while time.time() - start < 90:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out.extend(chunk)
            last_data = time.time()
        now = time.time()
        if next_send is not None and now >= next_send:
            try:
                payload = next(script_iter)
                os.write(fd, payload)
                next_send = now + settle if False else None
                # after sending a line, wait for turn to finish
                next_send = None
            except StopIteration:
                next_send = None
        # finish when idle long enough after all input sent
        if next_send is None and time.time() - last_data > idle_done:
            break
    os.write(fd, b"\x03")  # Ctrl-C
    time.sleep(0.5)
    try:
        while True:
            r, _, _ = select.select([fd], [], [], 0.2)
            if not r:
                break
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            out.extend(chunk)
    except OSError:
        pass
    try:
        os.close(fd)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    return bytes(out)

if __name__ == "__main__":
    prompt = sys.argv[1]
    binary = sys.argv[2] if len(sys.argv) > 2 else "./target/debug/ilar"
    out = run([binary], [prompt.encode() + b"\r"])
    sys.stdout.buffer.write(out)
