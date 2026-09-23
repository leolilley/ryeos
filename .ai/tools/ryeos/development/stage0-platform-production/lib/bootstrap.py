# ryeos:signed:2026-09-22T00:01:34Z:3b95477a886f99dba3356f7530045ff95ae2f983c03fcd71992457c7145e0496:Zz+lSUWLdZnaxBYNduH0t3ovw6gEAMBa/3oHXIN9ew06hasjTBNA7CvhCWee6bg8hJj+/7ilFhyALUprnWPdAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/env python3
"""External Stage-0 transport only; never imports or grants RyeOS authority."""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile


LIB = Path('.ai/tools/ryeos/development/stage0-platform-production/lib')
INPUTS = Path('.ai/config/development/ryeos/stage0-platform-x86_64-linux.yaml')
DOCKERFILE = Path('Dockerfile.development-realizations')
SOURCES = (DOCKERFILE, INPUTS,
           Path('scripts/release/acquire-development-toolchain-stage0.sh'),
           *(LIB / name for name in ('produce.sh', 'contract.sh', 'runtime.sh',
                                    'verify-bootstrap-artifact.sh')))


def contract_value(text, key):
    values = re.findall(r'^' + re.escape(key) + r': "([^"\n]*)"$', text, re.M)
    if len(values) != 1:
        raise ValueError(f'expected exactly one quoted {key} in Stage-0 contract')
    return values[0]


def unpack_export(stream, destination, name, maximum):
    """Copy only the two regular output files; never apply tar paths/metadata."""
    expected = {name: maximum, name + '.sha256': 4096}
    seen = set()
    with tarfile.open(fileobj=stream, mode='r|') as archive:
        for member in archive:
            if member.name in ('.', './', '/') and member.isdir():
                continue
            member_name = member.name.removeprefix('./')
            if (member_name not in expected or member_name in seen
                    or not member.isfile() or member.issparse()
                    or not 0 < member.size <= expected[member_name]):
                raise ValueError(f'unexpected Stage-0 export member: {member.name!r}')
            seen.add(member_name)
            with archive.extractfile(member) as source:
                with (destination / member_name).open('xb') as target:
                    shutil.copyfileobj(source, target)
    if seen != set(expected):
        raise ValueError('Stage-0 export is missing its archive or checksum')


def bootstrap(repo, output, buildx):
    if os.geteuid() == 0:
        raise ValueError('run this driver as your user; elevate only the Buildx command')
    if output.exists() or output.is_symlink():
        raise ValueError(f'refusing to replace existing output: {output}')
    # Reserve the destination before invoking Docker. Failure leaves no partial result.
    output.mkdir(mode=0o700)
    try:
        with tempfile.TemporaryDirectory(prefix='.stage0-', dir=output) as temporary:
            work = Path(temporary)
            context = work / 'source'
            for relative in SOURCES:
                source = repo / relative
                if source.is_symlink() or not source.is_file():
                    raise ValueError(f'missing or linked bootstrap source: {source}')
                target = context / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)
            # Build and verification consume the same saved input bytes, not a
            # workspace that may change during a minutes-long publisher run.
            contract = (context / INPUTS).read_text()
            name = contract_value(contract, 'output_name')
            if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*\.tar\.gz', name):
                raise ValueError('unsafe Stage-0 output_name')
            maximum = int(contract_value(contract, 'maximum_output_bytes'))
            if maximum <= 0:
                raise ValueError('invalid maximum_output_bytes')
            command = [*buildx, 'build', '--file', str(context / DOCKERFILE),
                       '--target', 'development-toolchain-stage0-artifact',
                       '--output', 'type=tar,dest=-', str(context)]
            print('Building/exporting Stage-0; existing BuildKit cache is retained.',
                  file=sys.stderr, flush=True)
            process = subprocess.Popen(command, stdout=subprocess.PIPE)
            try:
                unpack_export(process.stdout, work, name, maximum)
                # Drain tar padding so a successful exporter cannot block on its pipe.
                while process.stdout.read(65536):
                    pass
                if process.wait() != 0:
                    raise RuntimeError('Stage-0 publisher failed')
            finally:
                process.stdout.close()
                if process.poll() is None:
                    process.terminate()
                    process.wait()
            subprocess.run([
                'bash', str(context / LIB / 'verify-bootstrap-artifact.sh'),
                '--inputs', str(context / INPUTS),
                '--producer', str(context / LIB / 'produce.sh'),
                '--archive', str(work / name),
                '--checksum', str(work / (name + '.sha256')),
            ], check=True)
            # Keep exact source inputs alongside the result for subsequent import
            # and audit. No old external-content identity is assumed or reused.
            context.rename(output / 'source')
            (work / name).rename(output / name)
            (work / (name + '.sha256')).rename(output / (name + '.sha256'))
    except BaseException:
        shutil.rmtree(output)
        raise
    print(f'Verified Stage-0 transport artifact: {output}')
    print('Not imported, bound, independently reproduced, or isolation-qualified.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path,
                        help='new user-owned directory; parent must already exist')
    parser.add_argument('buildx', nargs=argparse.REMAINDER,
                        help='after --: Buildx command prefix (default: docker buildx)')
    args = parser.parse_args()
    command = args.buildx
    if command[:1] == ['--']:
        command = command[1:]
    try:
        bootstrap(Path(__file__).resolve().parents[6], args.output.absolute(),
                  command or ['docker', 'buildx'])
    except (OSError, ValueError, RuntimeError, tarfile.TarError,
            subprocess.CalledProcessError) as error:
        parser.exit(1, f'Stage-0 bootstrap failed: {error}\n')


if __name__ == '__main__':
    main()
