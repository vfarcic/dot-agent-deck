"""Credential-free slave writer, controlled over fixture-owned Unix sockets."""
import errno
import json
import os
import signal
import socket
import sys
import tty

root, role = sys.argv[1:3]
tty.setraw(0)
signal.alarm(180)


def connect(name):
    connection = socket.socket(socket.AF_UNIX)
    connection.connect(root + '/' + name + '.sock')
    stream = connection.makefile('rwb')
    stream.write((json.dumps({'role': name, 'pid': os.getpid(),
                             'generation': os.environ.get('DOT_AGENT_DECK_EDITOR_GEN')}) + '\n').encode())
    stream.flush()
    return stream


def write_all(data):
    while data:
        count = os.write(1, data)
        data = data[count:]


def writer():
    # Escape the predecessor's process group, retain its slave, and survive
    # the master hangup. This process never opens a successor slave by path.
    os.setsid()
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    signal.alarm(180)
    serve(connect('writer'), True)
    os._exit(0)


def serve(stream, retained=False):
    for line in stream:
        request = json.loads(line)
        action = request['action']
        result = {'id': request['id'], 'pid': os.getpid()}
        if action == 'exit':
            stream.write((json.dumps(result) + '\n').encode())
            stream.flush()
            return
        if action == 'retain':
            child = os.fork()
            if child == 0:
                stream.close()
                writer()
            result['writer'] = child
        elif action == 'burst':
            data = bytes.fromhex(request['hex'])
            child = os.fork()
            if child == 0:
                # Three syscalls split the opening ESC, a marker and a
                # burst larger than N_TTY's buffer. No newline conversion.
                for piece in (data[:1], data[1:7], data[7:]):
                    write_all(piece)
                os._exit(0)
            reaped, status = os.waitpid(child, 0)
            result.update(child=reaped, status=status, count=len(data))
        elif action in ('write', 'fill'):
            data = bytes.fromhex(request['hex'])
            result['attempted'] = True
            result['count'] = 0
            result['errno'] = None
            try:
                if action == 'fill':
                    os.set_blocking(1, False)
                    while True:
                        result['count'] += os.write(1, data)
                else:
                    write_all(data)
                    result['count'] = len(data)
            except OSError as error:
                result['errno'] = error.errno
                if error.errno not in (errno.EIO, errno.ENXIO, errno.EBADF,
                                       errno.EAGAIN, errno.EWOULDBLOCK):
                    raise
            finally:
                if action == 'fill':
                    os.set_blocking(1, True)
        else:
            raise RuntimeError(action)
        stream.write((json.dumps(result) + '\n').encode())
        stream.flush()


serve(connect(role))
