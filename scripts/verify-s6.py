"""Local S6 fixtures/reports only; sizes are decimal MB, never committed."""
import argparse
import json
import pathlib
import subprocess
import time
import tomllib


def generate(path, size, nested):
    unit = 10_000 if nested else 1000
    count = size // unit
    suffix = b'","meta":{"enabled":true,"items":[0,1]}}' if nested else b'"'
    prefix = b'{"payload":"' if nested else b'"'
    padding = unit - 1 - len(prefix) - len(suffix)
    with path.open('wb') as out:
        out.write(b'[')
        for i in range(count):
            marker = b'NEEDLE' if i in (0, count // 2, count - 1) else b''
            length = padding - (i == count - 1)
            out.write(prefix + marker + b'x' * (length - len(marker)) + suffix)
            if i < count - 1:
                out.write(b',')
        out.write(b']')
    assert path.stat().st_size == size
    return count


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--sizes', type=int, nargs='+', default=[100, 300, 500])
    parser.add_argument('--report-name', default='report.json')
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    directory = root / 'target' / 's6-local'
    directory.mkdir(parents=True, exist_ok=True)
    exe = root / 'target' / 'release' / 'examples' / 'search_bench.exe'
    version = tomllib.loads((root / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    report = {'version': version, 'units': 'decimal MB', 'results': []}
    for nested in (False, True):
        for mb in args.sizes:
            path = directory / f'{"nested" if nested else "scalar"}-{mb}MB.json'
            count = generate(path, mb * 1_000_000, nested)
            for repeat in range(args.rounds):
                started = time.monotonic()
                run = subprocess.run([str(exe), str(path), str(count)], cwd=root,
                                     check=True, capture_output=True, text=True)
                row = json.loads(run.stdout)
                row.update(nested=nested, repeat=repeat + 1)
                report['results'].append(row)
                (directory / args.report_name).write_text(json.dumps(report, indent=2), encoding='utf-8')
                print(f'{path.name} round {repeat + 1}: {time.monotonic() - started:.2f}s', flush=True)


if __name__ == '__main__':
    main()
