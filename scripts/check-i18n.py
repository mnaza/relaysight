#!/usr/bin/env python3
import json
import re
from pathlib import Path

locales = {p.stem: json.loads(p.read_text()) for p in Path('web/locales').glob('*.json')}
base_name = 'en'
base = set(locales[base_name])
failed = False
for name, data in sorted(locales.items()):
    keys = set(data)
    missing, extra = base - keys, keys - base
    if missing or extra:
        failed = True
        print(name, 'missing=', sorted(missing), 'extra=', sorted(extra))

html = '\n'.join(Path(p).read_text() for p in ['web/index.html', 'web/app.html'])
used = set(re.findall(r'data-i18n="([^"]+)"', html)) | set(re.findall(r'data-list="([^"]+)"', html))
missing = used - base
if missing:
    failed = True
    print('HTML keys missing from en:', sorted(missing))

if failed:
    raise SystemExit(1)
print(f'i18n OK: {len(base)} keys across {len(locales)} locales')
