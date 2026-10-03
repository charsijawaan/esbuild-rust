#!/usr/bin/env python3
"""Focused watch wire/EOF probes. Go EOF differences are observations, not passes."""
import hashlib
import json
import os
import pathlib
import select
import struct
import subprocess
import sys
import tempfile
import time


def encode(value):
    if value is None:
        return b'\0'
    if isinstance(value, bool):
        return b'\1' + bytes([value])
    if isinstance(value, int):
        return b'\2' + struct.pack('<I', value & 0xffffffff)
    if isinstance(value, str):
        value = value.encode()
        return b'\3' + struct.pack('<I', len(value)) + value
    if isinstance(value, bytes):
        return b'\4' + struct.pack('<I', len(value)) + value
    if isinstance(value, list):
        return b'\5' + struct.pack('<I', len(value)) + b''.join(map(encode, value))
    if isinstance(value, dict):
        return b'\6' + struct.pack('<I', len(value)) + b''.join(
            struct.pack('<I', len(key.encode())) + key.encode() + encode(item)
            for key, item in sorted(value.items()))
    raise TypeError(value)


def decode(data):
    offset = 0

    def word():
        nonlocal offset
        result = struct.unpack_from('<I', data, offset)[0]
        offset += 4
        return result

    def blob():
        nonlocal offset
        length = word()
        result = data[offset:offset + length]
        offset += length
        assert len(result) == length
        return result

    def visit():
        nonlocal offset
        tag = data[offset]
        offset += 1
        if tag == 0:
            return None
        if tag == 1:
            result = bool(data[offset])
            offset += 1
            return result
        if tag == 2:
            return word()
        if tag == 3:
            return blob().decode()
        if tag == 4:
            return blob()
        if tag == 5:
            return [visit() for _ in range(word())]
        if tag == 6:
            result = {}
            for _ in range(word()):
                key = blob().decode()
                result[key] = visit()
            return result
        raise ValueError(tag)

    header = word()
    result = dict(id=header >> 1, request=not (header & 1), value=visit())
    assert offset == len(data)
    return result


class Host:
    def __init__(self, binary, ping=False):
        self.process = subprocess.Popen(
            [binary, '--service=0.28.1', *(['--ping'] if ping else [])],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        assert self.frame() == b'0.28.1'

    def frame(self, seconds=5):
        deadline = time.monotonic() + seconds

        def exact(length):
            result = b''
            while len(result) < length:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([self.process.stdout], [], [], remaining)[0]:
                    raise TimeoutError('watch service frame')
                data = os.read(self.process.stdout.fileno(), length - len(result))
                if not data:
                    raise EOFError('watch service frame')
                result += data
            return result

        return exact(struct.unpack('<I', exact(4))[0])

    def packet(self):
        return decode(self.frame())

    def send(self, ident, value, request=True):
        body = struct.pack('<I', (ident << 1) | int(not request)) + encode(value)
        self.process.stdin.write(struct.pack('<I', len(body)) + body)

    def response(self, ident):
        result = self.packet()
        assert not result['request'] and result['id'] == ident, result
        return result['value']

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)


def context(input_file=None, outfile=None, stage=None):
    result = dict(command='build', context=True, key=1, write=bool(outfile),
                  entries=[['', str(input_file)]] if input_file else [],
                  nodePaths=[], absWorkingDir='', flags=['--log-level=silent', '--format=esm'])
    if not input_file:
        result['stdinContents'] = b'throw 1'
    if outfile:
        result['flags'].append('--outfile=' + str(outfile))
    if stage in ['on-start', 'on-end']:
        result['plugins'] = [dict(name='held', onStart=stage == 'on-start',
                                  onEnd=stage == 'on-end', onResolve=[], onLoad=[])]
    return result


def operation(command):
    return dict(command=command, key=1)


def wait_output(outfile, expected):
    deadline = time.monotonic() + 5
    while not outfile.exists() or outfile.read_text() != expected:
        assert time.monotonic() < deadline, 'native watch did not write real disk change'
        time.sleep(0.02)


def admission_and_unobserved(binary, root):
    host = Host(binary)
    try:
        host.send(0, operation('watch'))
        assert host.response(0) == dict(error='Cannot watch')
        input_file = root / 'input.js'
        outfile = root / 'out.js'
        input_file.write_text('throw 1')
        host.send(1, context(input_file, outfile))
        assert host.response(1) == dict(errors=[], warnings=[])
        host.send(2, operation('watch'))
        assert host.response(2) == {}
        wait_output(outfile, 'throw 1;\n')
        assert not select.select([host.process.stdout], [], [], 0.15)[0], 'unexpected unobserved onEnd'
        input_file.write_text('throw 2')
        wait_output(outfile, 'throw 2;\n')
        assert not select.select([host.process.stdout], [], [], 0.15)[0], 'unexpected background onEnd'
        host.send(3, operation('watch'))
        assert host.response(3) == dict(error='Watch mode has already been enabled')
        host.send(4, operation('rebuild'))
        end = host.packet()
        assert end['request'] and end['value']['command'] == 'on-end'
        host.send(end['id'], dict(errors=[], warnings=[]), request=False)
        assert host.response(4) == dict(errors=[], warnings=[])
        host.send(5, operation('dispose'))
        assert host.response(5) == {}
        host.send(6, operation('watch'))
        assert host.response(6) == dict(error='Cannot watch')
        host.process.stdin.close()
        assert host.process.wait(timeout=5) == 0
        return dict(passed=True)
    finally:
        host.close()


def eof_case(binary, stage, native):
    host = Host(binary, stage == 'ping')
    try:
        host.send(0, context(stage=stage))
        assert host.response(0) == dict(errors=[], warnings=[])
        host.send(1, operation('watch'))
        watched = False
        held = stage == 'idle'
        while not watched or not held:
            packet = host.packet()
            if not packet['request']:
                assert packet['id'] == 1 and packet['value'] == {}
                watched = True
            else:
                if stage == 'on-end' and packet['value']['command'] == 'on-start':
                    host.send(packet['id'], dict(errors=[], warnings=[]), request=False)
                    continue
                assert packet['value']['command'] == stage, packet
                held = True
        host.process.stdin.close()
        if native:
            exit_code = host.process.wait(timeout=5)
            assert exit_code == 0, exit_code
            return dict(passed=True, stage=stage, exit_code=exit_code,
                        stderr=host.process.stderr.read().decode(errors='replace'))
        time.sleep(0.35)
        exit_code = host.process.poll()
        result = dict(stage=stage, observation='pinned Go EOF difference',
                      exit_code=exit_code, retained=exit_code is None, parity_pass=False)
        if exit_code is not None:
            result['stderr'] = host.process.stderr.read().decode(errors='replace')
        return result
    finally:
        host.close()


def background_stderr_and_recovery(binary, root):
    host = Host(binary)
    try:
        input_file = root / 'diagnostic.js'
        outfile = root / 'diagnostic-out.js'
        input_file.write_text('throw 1 2')
        request = context(input_file, outfile)
        request['flags'][0] = '--log-level=error'
        host.send(0, request)
        assert host.response(0) == dict(errors=[], warnings=[])
        host.send(1, operation('watch'))
        assert host.response(1) == {}
        stderr = b''
        deadline = time.monotonic() + 5
        while b'Expected ";" but found "2"' not in stderr:
            remaining = deadline - time.monotonic()
            assert remaining > 0 and select.select([host.process.stderr], [], [], remaining)[0]
            stderr += os.read(host.process.stderr.fileno(), 8192)
        assert not outfile.exists()
        input_file.write_text('throw 2')
        wait_output(outfile, 'throw 2;\n')
        assert not select.select([host.process.stdout], [], [], 0.15)[0], 'diagnostics need no host onEnd'
        host.send(2, operation('dispose'))
        assert host.response(2) == {}
        host.process.stdin.close()
        assert host.process.wait(timeout=5) == 0
        return dict(passed=True, stderr=stderr.decode(errors='replace'))
    finally:
        host.close()


def background_stdout_stays_framed(binary, root):
    host = Host(binary)
    try:
        input_file = root / 'stdout.js'
        input_file.write_text('throw 1')
        request = context(input_file)
        request['write'] = True
        host.send(0, request)
        assert host.response(0) == dict(errors=[], warnings=[])
        host.send(1, operation('watch'))
        started = False
        end = None
        while not started or end is None:
            packet = host.packet()
            if packet['request']:
                assert packet['value']['command'] == 'on-end'
                end = packet
            else:
                assert packet['id'] == 1 and packet['value'] == {}
                started = True
        assert 'outputFiles' not in end['value']
        assert end['value']['writeToStdout'] == b'throw 1;\n'
        host.send(end['id'], dict(errors=[], warnings=[]), request=False)
        input_file.write_text('throw 2')
        end = host.packet()
        assert end['request'] and end['value']['command'] == 'on-end'
        assert end['value']['writeToStdout'] == b'throw 2;\n'
        host.send(end['id'], dict(errors=[], warnings=[]), request=False)
        host.send(2, operation('dispose'))
        assert host.response(2) == {}
        host.process.stdin.close()
        assert host.process.wait(timeout=5) == 0
        return dict(passed=True)
    finally:
        host.close()


def main():
    binary, kind, report_path = sys.argv[1:]
    root = pathlib.Path(tempfile.mkdtemp(prefix='esbuild-service-watch-wire-'))
    report = dict(binary=binary, binary_sha256=hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest(),
                  worker_threads=os.environ.get('ESBUILD_WORKER_THREADS') != '0',
                  kind=kind, artifact_root=str(root), results=[], completed=False)

    def save():
        pathlib.Path(report_path).write_text(json.dumps(report, indent=2) + '\n')

    save()
    for name, run in [('admission_and_unobserved', lambda: admission_and_unobserved(binary, root)),
                      ('background_stderr_and_recovery', lambda: background_stderr_and_recovery(binary, root)),
                      ('background_stdout_stays_framed', lambda: background_stdout_stays_framed(binary, root)),
                      *[(f'eof_{stage}', lambda stage=stage: eof_case(binary, stage, kind == 'rust'))
                        for stage in ['idle', 'on-start', 'on-end', 'ping']]]:
        try:
            report['results'].append(dict(name=name, **run()))
            print('DONE', name)
        except Exception as error:
            report['results'].append(dict(name=name, passed=False, error=repr(error)))
            print('FAIL', name, repr(error))
        save()
    report['completed'] = True
    save()
    sys.exit(int(any(row.get('passed') is False for row in report['results'])))


if __name__ == '__main__':
    main()
