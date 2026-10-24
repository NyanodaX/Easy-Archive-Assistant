"""Validate a freshly created portable release without extracting user archives."""
import hashlib
import json
from pathlib import Path
import re
import sys
import urllib.parse
import zipfile

package = Path(sys.argv[1]).resolve()
zip_path = package.parent / (package.name + '.zip')
def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        while chunk := stream.read(8*1024*1024):
            value.update(chunk)
    return value.hexdigest()

expected = {}
for line in (package/'SHA256SUMS.txt').read_text(encoding='utf-8-sig').splitlines():
    value, name = line.split('  ', 1)
    path = (package/name).resolve()
    assert package in path.parents, name
    assert name not in expected, name
    assert digest(path) == value, name
    expected[name] = value
actual = {str(p.relative_to(package)).replace('\\','/') for p in package.rglob('*') if p.is_file()}
assert actual == set(expected) | {'SHA256SUMS.txt'}
assert {package.name+'.exe','easy-archive-assistant.exe','LICENSE','README.md','README.zh-CN.md'} <= actual
assert (package/'LICENSE').read_text(encoding='utf-8').startswith('MIT License\n')
for entry in json.loads((package/'third-party-notices/DEPENDENCIES.json').read_text(encoding='utf-8')):
    assert entry['retained_license_files'], entry['name']
    for name in entry['retained_license_files']:
        assert (package/'third-party-notices'/name).is_file(), name
supplement = package/'third-party-notices/supplemental'
for source in json.loads((supplement/'SOURCES.json').read_text(encoding='utf-8')):
    assert digest(supplement/source['file']) == source['sha256'], source['file']

links = []
documents = list(package.glob('*.md'))+list((package/'docs').glob('*.md'))+list((package/'tests/fixtures').glob('*.md'))
for doc in documents:
    for link in re.findall(r'\]\(([^)]+)\)', doc.read_text(encoding='utf-8-sig')):
        target = link.split('#')[0].strip('<>')
        if not target or urllib.parse.urlsplit(target).scheme:
            continue
        destination = (doc.parent/urllib.parse.unquote(target)).resolve()
        assert package in destination.parents or destination==package, (doc.name,target)
        assert destination.exists(), (doc.name,target)
        links.append((doc.name,target))

with zipfile.ZipFile(zip_path) as archive:
    assert archive.testzip() is None
    names = set(archive.namelist())
    assert all(package.name+'/'+name in names for name in actual)
zip_hash = digest(zip_path)
assert zip_path.with_suffix('.zip.sha256').read_text(encoding='utf-8-sig').split()[0] == zip_hash
report = {'package':str(package),'files_hashed':len(expected),'local_document_links_checked':len(links),'zip_crc_ok':True,'zip_sha256':zip_hash,'project_license':'MIT','publisher_confirmed_license':True,'code_signed':False,'clean_machine_tested':False,'os_drag_drop_tested':False,'remote_ci_run':False}
report_path = package.parent/'PACKAGE_VALIDATION.json'
with report_path.open('x',encoding='utf-8') as stream:
    json.dump(report,stream,ensure_ascii=False,indent=2)
print(json.dumps(report,ensure_ascii=True))
