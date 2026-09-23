# ryeos:signed:2026-09-22T04:12:04Z:c9ed71e829991337290ca5c0619a8571afbb3b2dc67aea75541ea64a273181d2:boHD+l0qK57K+Yay3eNo4UgUPpLx9EJJzAZAklQRSQHlZ4GSsD1ejiX81vrZ9gyqo709dhJGSsRPnUS1zrJxBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/env python3
"""Export exact development-owned static-link inputs; grant no RyeOS authority."""

import argparse
import hashlib
import os
from pathlib import Path
import importlib.util
import subprocess
import sys
import tarfile
import tempfile


DEFAULT_INPUTS = Path(__file__).resolve().parents[6] / '.ai/config/development/ryeos/static-link-inputs.yaml'
# Load only the adjacent development-owned parser, not ambient sys.path.
_spec = importlib.util.spec_from_file_location(
    "static_input_contract", Path(__file__).with_name("inputs.py"))
contract_parser = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(contract_parser)


def unpack_export(stream, destination, contract):
    expected = {item['source'][1:]: (name, item) for name, item in contract['inputs'].items()}
    seen = set()
    with tarfile.open(fileobj=stream, mode='r|') as archive:
        for member in archive:
            name = member.name
            if (name not in expected or name in seen or not member.isfile()
                    or member.issparse()):
                raise ValueError(f'unexpected static-link export member: {name!r}')
            target_name, item = expected[name]
            if member.size != item['bytes'] or member.mode != item['mode']:
                raise ValueError(f'static-link size/mode mismatch: {name}')
            target = destination / target_name
            target.parent.mkdir(parents=True, exist_ok=True)
            digest = hashlib.sha256()
            with archive.extractfile(member) as source, target.open('xb') as output:
                while chunk := source.read(65536):
                    digest.update(chunk)
                    output.write(chunk)
                output.flush()
                os.fsync(output.fileno())
            if digest.hexdigest() != item['sha256']:
                raise ValueError(f'static-link digest mismatch: {name}')
            target.chmod(item['mode'])
            seen.add(name)
    if seen != set(expected):
        raise ValueError('static-link export is missing inputs')


def bootstrap(inputs, output, docker_command):
    if os.geteuid() == 0:
        raise ValueError('run this driver as your user; elevate only Docker')
    contract = contract_parser.read_contract(inputs)
    # mkdir reserves a new destination without touching existing files/symlinks.
    output.mkdir(mode=0o700)
    reservation = output.stat()

    def still_reserved():
        try:
            current = output.lstat()
            return (current.st_dev, current.st_ino) == (reservation.st_dev, reservation.st_ino)
        except FileNotFoundError:
            return False

    try:
        with tempfile.TemporaryDirectory(prefix='.static-link-', dir=output.parent) as temporary:
            staging = Path(temporary) / 'payload'
            staging.mkdir(mode=0o700)
            command = [*docker_command, 'run', '--rm', '--pull=never', '--network', 'none',
                       '--read-only', '--security-opt=no-new-privileges', '--cap-drop=ALL',
                       '--platform', 'linux/amd64', '--entrypoint', '/bin/tar',
                       contract['publisher_image'], '--format=ustar', '--hard-dereference',
                       '--no-recursion', '-c', '-C', '/', '--',
                       *(item['source'][1:] for item in contract['inputs'].values())]
            process = subprocess.Popen(command, stdout=subprocess.PIPE)
            try:
                unpack_export(process.stdout, staging, contract)
                # Consume bounded trailing tar padding; never accumulate output in memory.
                remaining = 1024 * 1024
                while chunk := process.stdout.read(min(65536, remaining + 1)):
                    remaining -= len(chunk)
                    if remaining < 0 or any(chunk):
                        raise ValueError('unexpected export trailer')
                if process.wait() != 0:
                    raise RuntimeError('static-link publisher export failed')
            finally:
                process.stdout.close()
                if process.poll() is None:
                    process.terminate()
                    process.wait()
            if not still_reserved():
                raise ValueError('output reservation changed during export')
            # Rename replaces only our empty reservation, atomically exposing all inputs.
            staging.rename(output)
    except BaseException:
        if still_reserved():
            try:
                output.rmdir()
            except OSError:
                pass  # Preserve anything another actor placed in the reservation.
        raise
    print(f'Verified static-link transport inputs: {output}')
    print('Not imported, bound, or static-link qualified; existing Stage-0 is unchanged.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--inputs', type=Path, default=DEFAULT_INPUTS)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--docker-command', nargs='+', default=['sudo', 'docker'])
    args = parser.parse_args()
    try:
        bootstrap(args.inputs, args.output.absolute(), args.docker_command)
    except (OSError, ValueError, RuntimeError, KeyError, TypeError, tarfile.TarError) as error:
        parser.exit(1, f'Static-link input export failed: {error}\n')


if __name__ == '__main__':
    main()
