#!/usr/bin/env python3
"""Mac stand-in for Lenovo USBRecoveryCreator's online half (dds.lenovo.com).

  python3 ddrs.py login '<url containing lenovoid.wust=...>'   # entitlement + list orders ('-' reads stdin)
  python3 ddrs.py order <ORDER_ID>                              # fetch recovery manifest
  python3 ddrs.py download [NAME...]                            # fetch + SHA-256 verify files

Protocol mirrors USBRecoveryCreator.Common.WebServices (decompiled).
"""
import hashlib, json, os, re, sys, uuid, urllib.error, urllib.request
import xml.etree.ElementTree as ET
from xml.sax.saxutils import quoteattr

API = 'https://dds.lenovo.com/dds/services/'
WORK = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'work')
SESSION = os.path.join(WORK, 'session.json')
ORDER_XML = os.path.join(WORK, 'order.xml')
FILES = os.path.join(WORK, 'files')
UA = 'USBRecoveryCreator-mac-port'


def load():
    with open(SESSION) as f:
        return json.load(f)


def save(s):
    os.makedirs(WORK, exist_ok=True)
    fd = os.open(SESSION, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump(s, f, indent=1)


def ddm(type_, inner):
    return ('<?xml version="1.0" encoding="utf-8"?><ddm xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" '
            'xmlns:xsd="http://www.w3.org/2001/XMLSchema" type="%s">%s</ddm>' % (type_, inner)).encode('ascii')


def attrs(**kw):
    return ''.join(' %s=%s' % (k, quoteattr(str(v))) for k, v in kw.items() if v is not None)


def post(service, body, s, bearer):
    h = {'Content-Type': 'text/xml', 'User-Agent': UA}
    if bearer:
        h['Authorization'] = 'Bearer ' + bearer
    if s.get('csrf'):
        h['X-CSRF-TOKEN'] = s['csrf']
        h['Cookie'] = 'XSRF-TOKEN=' + s['csrf']
    req = urllib.request.Request(API + service, data=body, headers=h, method='POST')
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.read().decode('utf-8'), r.headers
    except urllib.error.HTTPError as e:
        sys.exit('%s -> HTTP %s: %s' % (service, e.code, e.read()[:500].decode('utf-8', 'replace')))


def cmd_login(arg):
    arg = sys.stdin.read() if arg == '-' else arg
    m = re.search(r'lenovoid\.wust=([^&\s]+)', arg)
    wust = m.group(1) if m else arg.strip()
    s = {'clientId': hashlib.sha256(str(uuid.uuid4()).encode()).hexdigest()}  # tool sends sha256(MachineGuid)
    body = ddm('entitlementRequest', '<entitlementRequest%s><metrics%s /></entitlementRequest>' % (
        attrs(token=wust, country='US'), attrs(brand='Apple Inc. - Mac' if sys.platform == 'darwin' else 'Linux PC', machineType='Laptop', clientId=s['clientId'])))
    text, hdr = post('entitlement', body, s, None)
    # WebServices.HTTPPOSTResult: token header + first Set-Cookie split('=')[1]
    s['entToken'] = s['token'] = hdr.get('token')
    cookie = (hdr.get_all('Set-Cookie') or [''])[0]
    s['csrf'] = cookie.split('=')[1].split(';')[0] if '=' in cookie else ''
    er = ET.fromstring(text).find('entitlementResponse')
    if er is None or not s['token']:
        sys.exit('Entitlement failed: ' + text[:500])
    s['deviceId'], s['lenovoID'] = er.get('deviceId'), er.get('lenovoID')
    s['orders'] = [dict(o.attrib) for o in er.iter('order')]
    save(s)
    print('code=%s message=%s' % (er.get('code'), er.get('message')))
    for o in er.iter('order'):
        print('ORDER %s  %s  SN=%s  %s  %s' % (o.get('id'), o.get('machineType'), o.get('serialNumber'), o.get('os'), o.get('dateTime')))


def cmd_order(order_id, force=None):
    s = load()
    body = ddm('downloadRequest', '<downloadRequest%s />' % attrs(id=order_id, lenovoID=s['lenovoID'], deviceId=s['deviceId'], force=force or 0))
    text, hdr = post('download', body, s, s['entToken'])
    dr = ET.fromstring(text).find('downloadResponse')
    if dr is None:
        sys.exit('Unexpected response: ' + text[:500])
    print('code=%s message=%s' % (dr.get('code'), dr.get('message')))
    if dr.get('code') != '305' and hdr.get('token'):
        s['token'] = hdr.get('token')
    s['orderId'] = order_id
    save(s)
    with open(ORDER_XML, 'w') as f:
        f.write(text)
    os.chmod(ORDER_XML, 0o600)
    summarize(dr)


def summarize(dr):
    for p in dr.iter('partition'):
        print('PARTITION id=%s label=%s size=%sMB format=%s primary=%s' % tuple(p.get(k) for k in ('id', 'label', 'size', 'format', 'primary')))
    for f in dr.iter('file'):
        print('FILE %-28s part=%s copy=%s copypath=%s size=%s%s key=%s contents=%d' % (
            f.get('name'), f.get('partition'), f.get('copy'), f.get('copypath'), f.get('size'), f.get('stype'),
            'yes' if f.get('key') else 'no', len(f.findall('content'))))
    for f in dr.iter('manualfile'):
        print('MANUAL %s part=%s copypath=%s' % (f.get('name'), f.get('partition'), f.get('copypath')))


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''):
            h.update(b)
    return h.hexdigest()


def regenerate(s, f):
    body = ddm('regenerateUrlRequest', '<regenerateUrlRequest%s><file%s /></regenerateUrlRequest>' % (
        attrs(id=s['orderId'], deviceId=s['deviceId']), attrs(name=f.get('name'), source=f.get('source'), copypath=f.get('copypath'))))
    text, _ = post('regenerateUrl', body, s, s['token'])
    nf = ET.fromstring(text).find('regenerateUrlResponse/file')
    if nf is None:
        sys.exit('Could not refresh download link (session expired?) - run login + order again: ' + text[:300])
    return nf.get('source')


def fetch(s, f, dst, tag=''):
    src = f.get('source')
    for attempt in range(3):
        have = os.path.getsize(dst + '.tmp') if os.path.exists(dst + '.tmp') else 0
        req = urllib.request.Request(src + '&SSID=' + s['deviceId'], headers={'Range': 'bytes=%d-' % have, 'User-Agent': UA})
        try:
            with urllib.request.urlopen(req, timeout=60) as r, open(dst + '.tmp', 'ab' if r.status == 206 else 'wb') as out:
                total = have + int(r.headers.get('Content-Length', 0)) if r.status == 206 else int(r.headers.get('Content-Length', 0))
                done = have if r.status == 206 else 0
                for b in iter(lambda: r.read(1 << 20), b''):
                    out.write(b)
                    done += len(b)
                    print('\r%s%s %.1f/%.1f GB' % (tag, f.get('name'), done / 2**30, total / 2**30), end='', flush=True)
            print()
            return
        except urllib.error.HTTPError as e:
            if e.code == 416:
                return
            if e.code in (401, 403) and attempt < 2:
                src = regenerate(s, f)
                continue
            raise
        except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
            print('\n  network hiccup (%s), resuming...' % e)
    sys.exit('Giving up on ' + f.get('name'))


def cmd_download(only=()):
    s = load()
    dr = ET.parse(ORDER_XML).getroot().find('downloadResponse')
    os.makedirs(FILES, exist_ok=True)
    todo = sorted((f for f in dr.iter('file') if f.get('source') and (not only or f.get('name') in only)),
                  key=lambda f: not f.get('name').upper().endswith('.RMF'))  # layout file first, so it can be checked early
    keep = {os.path.basename(f.get('name')) + x for f in dr.iter('file') for x in ('', '.tmp')}
    stale = [n for n in os.listdir(FILES) if n not in keep]
    if stale:
        print('Removing %d files left over from an earlier order' % len(stale))
        for n in stale:
            os.remove(os.path.join(FILES, n))
    for i, f in enumerate(todo, 1):
        tag = '[%d/%d] ' % (i, len(todo))
        dst = os.path.join(FILES, os.path.basename(f.get('name')))
        if os.path.exists(dst) and sha256(dst) == f.get('check', '').lower():
            print(tag + 'OK (already) ' + f.get('name'))
            continue
        fetch(s, f, dst, tag)
        got = sha256(dst + '.tmp')
        if got != f.get('check', '').lower():
            os.rename(dst + '.tmp', dst + '.bad')
            sys.exit('SHA-256 mismatch for %s (got %s, want %s) - saved as .bad' % (f.get('name'), got, f.get('check')))
        os.rename(dst + '.tmp', dst)
        print(tag + 'OK ' + f.get('name'))
    print('All files downloaded and verified in ' + FILES)


if __name__ == '__main__':
    a = sys.argv[1:]
    if a[:1] == ['login'] and len(a) == 2:
        cmd_login(a[1])
    elif a[:1] == ['order'] and len(a) in (2, 3):
        cmd_order(a[1], a[2] if len(a) == 3 else None)
    elif a[:1] == ['download']:
        cmd_download(a[1:])
    else:
        sys.exit(__doc__)
