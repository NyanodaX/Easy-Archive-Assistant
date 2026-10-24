"""Generate a reproducible inventory and retained license texts from installed dependencies.

Usage: python scripts/dependency-notices.py METADATA_JSON NEW_OUTPUT_DIRECTORY
Cargo metadata must be filtered for the distribution platform. Never writes outside
the newly created output directory or downloads source/executes dependency scripts.
"""
import json
from pathlib import Path
import sys

metadata_path, output = Path(sys.argv[1]), Path(sys.argv[2])
raw = metadata_path.read_bytes()
metadata = json.loads(raw.decode('utf-16' if raw[:2] in (b'\xff\xfe', b'\xfe\xff') else 'utf-8-sig'))
output.mkdir(parents=True, exist_ok=False)
licenses_dir = output / 'licenses'
licenses_dir.mkdir()
rows = []
resolved = {node['id'] for node in metadata['resolve']['nodes']}

def retain_texts(folder, label, license_file=None):
    files = []
    for path in folder.iterdir():
        if path.is_file() and path.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE', 'COPYRIGHT', 'AUTHORS')):
            files.append(path)
    if license_file:
        named = (folder / license_file).resolve()
        if folder.resolve() not in named.parents:
            raise ValueError('license file escaped dependency source')
        if named.is_file() and named not in files:
            files.append(named)
    destination = licenses_dir / label
    destination.mkdir()
    retained = []
    for i, path in enumerate(sorted(files)):
        if path.stat().st_size > 2 * 1024 * 1024:
            raise ValueError('unexpectedly large license file')
        target = destination / f'{i}-{path.name}'
        target.write_bytes(path.read_bytes())
        retained.append(str(target.relative_to(output)).replace('\\', '/'))
    return retained

for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
    if not package['source'] or package['id'] not in resolved:
        continue
    label = f"cargo-{package['name']}-{package['version']}"
    files = retain_texts(Path(package['manifest_path']).parent, label, package.get('license_file'))
    supplemental = {
        'alloc-stdlib': ['alloc-stdlib-BSD.txt'],
        'selectors': ['MPL-2.0.txt'],
        'webview2-com': ['webview2-rs-MIT.txt'],
        'webview2-com-macros': ['webview2-macros-MIT.txt'],
        'webview2-com-sys': ['webview2-rs-MIT.txt', 'WebView2-SDK-0-LICENSE.txt', 'WebView2-SDK-1-NOTICE.txt'],
    }
    files.extend('supplemental/' + name for name in supplemental.get(package['name'], []))
    rows.append({'ecosystem': 'Cargo', 'name': package['name'], 'version': package['version'], 'license': package.get('license'), 'repository': package.get('repository'), 'source_archive': f"https://crates.io/api/v1/crates/{package['name']}/{package['version']}/download", 'retained_license_files': files})

# The only JavaScript production dependency is bundled @tauri-apps/api.
root = Path(__file__).resolve().parent.parent
for name in json.loads((root / 'package.json').read_text(encoding='utf-8'))['dependencies']:
    folder = root / 'node_modules' / name
    package = json.loads((folder / 'package.json').read_text(encoding='utf-8'))
    files = retain_texts(folder, f"npm-{name.replace('/', '-')}-{package['version']}")
    rows.append({'ecosystem': 'npm (bundled)', 'name': name, 'version': package['version'], 'license': package.get('license'), 'repository': package.get('repository'), 'retained_license_files': files})

(output / 'DEPENDENCIES.json').write_text(json.dumps(rows, ensure_ascii=False, indent=2)+'\n', encoding='utf-8')
supplement = root / 'third-party'
if supplement.exists():
    import shutil
    shutil.copytree(supplement, output / 'supplemental')
toolchain = root / '.tools' / 'rustup' / 'toolchains' / 'stable-x86_64-pc-windows-gnullvm' / 'share' / 'doc' / 'rust' / 'licenses'
if toolchain.exists():
    import shutil
    shutil.copytree(toolchain, output / 'rust-runtime-licenses')
mingw = root / '.tools' / 'llvm-mingw-20261020-ucrt-x86_64'
if mingw.exists():
    import shutil
    runtime = output / 'llvm-mingw-runtime-licenses'
    runtime.mkdir()
    shutil.copy2(mingw / 'LICENSE.TXT', runtime / 'LLVM-LICENSE.txt')
    for path in (mingw / 'x86_64-w64-mingw32' / 'share' / 'mingw32').glob('COPYING*'):
        shutil.copy2(path, runtime / path.name)
summary = ['# Third-party notices / 第三方许可', '', 'Generated from the locked Windows dependency graph and installed production JS package. Includes build-only dependencies conservatively. Developer tools, operating-system DLLs, and the separately installed WebView2 runtime are not bundled here. WebView2Loader.dll is redistributed with the webview2-com-sys license texts below.', '', 'This inventory retains declared license expressions and supplied texts; it is not a legal compatibility opinion. 项目自身许可证另见根目录；以下仅为第三方组件清单。', '', '| Component | Version | Declared license |', '|---|---|---|']
for row in rows:
    summary.append(f"| {row['name']} | {row['version']} | {row['license'] or 'SEE RETAINED TEXT'} |")
summary.extend(['', 'Full source/repository references, exact source archive download links and retained notice paths: [DEPENDENCIES.json](DEPENDENCIES.json). Dependencies are unmodified. For MPL-2.0 components (including selectors), the corresponding source code is available at the exact source archive links; the MPL-2.0 text is retained in supplemental/. Cargo packages omitting upstream root licenses are supplemented there with commit-pinned upstream texts. The Microsoft WebView2 SDK license and NOTICE apply to WebView2Loader.dll.', '', '未修改的依赖对应源码可通过上述精确版本链接获取；MPL-2.0、未随 crate 携带的上游许可和 WebView2 SDK 许可/NOTICE 保留在 supplemental 目录。', ''])
(output / 'THIRD_PARTY_NOTICES.md').write_text('\n'.join(summary), encoding='utf-8')
print(json.dumps({'components':len(rows),'output':str(output),'without_retained_text': [r['name'] for r in rows if not r['retained_license_files']]}))
